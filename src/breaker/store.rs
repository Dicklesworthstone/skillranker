//! Private breaker database admission, separate from circuit transitions.
//!
//! Never chmod an existing file or initialize a foreign database. The exact
//! legacy breaker schema is adopted without resetting its rows. SQLite WAL
//! bookkeeping can touch shared-memory sidecars even during a read-only probe.
//! Descriptor checks exclude other-user path replacement, not a hostile process
//! already running as this user. This is not filesystem/release qualification.

use super::MAX_BREAKER_BYTES;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, config::DbConfig};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

const APPLICATION_ID: i64 = 0x5352_424b; // SRBK; never a cache, ledger or allowance.
const SCHEMA_VERSION: i64 = 1;
const DDL: &str = "CREATE TABLE breaker_state (
             origin TEXT PRIMARY KEY,
             generation INTEGER NOT NULL,
             open INTEGER NOT NULL CHECK(open IN (0, 1)),
             failures INTEGER NOT NULL,
             cooldown_ms INTEGER NOT NULL,
             open_until_ms INTEGER NOT NULL,
             probe_token TEXT,
             probe_expires_ms INTEGER NOT NULL,
             retry_after_until_ms INTEGER NOT NULL
         ) STRICT";

#[derive(Clone, Copy, Eq, PartialEq)]
enum Schema {
    Empty,
    Legacy,
    Current,
}

fn refused() -> rusqlite::Error {
    // No database paths or library detail escape through breaker degradation.
    rusqlite::Error::InvalidQuery
}

fn schema(connection: &Connection, create: bool) -> rusqlite::Result<Schema> {
    let app: i64 = connection.pragma_query_value(None, "application_id", |r| r.get(0))?;
    let version: i64 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let current = app == APPLICATION_ID && version == SCHEMA_VERSION;
    let legacy = app == 0 && version == 0;
    if !current && !legacy {
        return Err(refused());
    }
    let count: i64 = connection.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*'",
        [],
        |r| r.get(0),
    )?;
    if count == 0 && legacy && create {
        return Ok(Schema::Empty);
    }
    let ddl: Option<String> = connection
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type='table' AND name='breaker_state'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if count != 1 || ddl.as_deref() != Some(DDL) {
        return Err(refused());
    }
    Ok(if current {
        Schema::Current
    } else {
        Schema::Legacy
    })
}

pub(super) fn verify_schema(connection: &Connection) -> rusqlite::Result<()> {
    if schema(connection, false)? != Schema::Current {
        return Err(refused());
    }
    Ok(())
}

fn configure(connection: &Connection, busy: Duration) -> rusqlite::Result<()> {
    connection.busy_timeout(busy.min(Duration::from_millis(25)))?;
    for (setting, expected) in [
        (DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true),
        (DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false),
        (DbConfig::SQLITE_DBCONFIG_ENABLE_TRIGGER, false),
        (DbConfig::SQLITE_DBCONFIG_ENABLE_VIEW, false),
    ] {
        if connection.set_db_config(setting, expected)? != expected {
            return Err(refused());
        }
    }
    connection.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH, 1024 * 1024)?;
    connection.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_SQL_LENGTH, 64 * 1024)?;
    connection.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_ATTACHED, 0)?;
    connection.pragma_update(None, "temp_store", "MEMORY")?;
    Ok(())
}

// Drop SQLite before the descriptors binding the admitted pathname.
pub(super) struct Store {
    pub(super) connection: Connection,
    directory: File,
    file: File,
    path: PathBuf,
}

impl Store {
    pub(super) fn verify(&self) -> rusqlite::Result<()> {
        #[cfg(unix)]
        {
            use nix::sys::stat::fstat;
            let current = private::parent(&self.path, false)?;
            let held = fstat(&self.directory).map_err(|_| refused())?;
            let actual = fstat(&current).map_err(|_| refused())?;
            if (held.st_dev, held.st_ino) != (actual.st_dev, actual.st_ino) {
                return Err(refused());
            }
            private::verify_files(&self.directory, &self.file, &self.path)
        }
        #[cfg(not(unix))]
        {
            Err(refused())
        }
    }
}

/// `None` means a private, empty initialization left by a failed creator.
/// Reading it does not initialize or disable the later failure-only retry.
pub(super) fn open(path: &Path, busy: Duration, create: bool) -> rusqlite::Result<Option<Store>> {
    #[cfg(not(unix))]
    {
        let _ = (path, busy, create);
        Err(refused())
    }
    #[cfg(unix)]
    {
        // Engine inspection is disk-free. A wrong bundle may not create paths.
        crate::sqlite_engine::linked_engine().map_err(|_| refused())?;
        let path = crate::platform_path::storage_path(path.to_path_buf());
        let directory = private::parent(&path, create)?;
        let file = private::database(&directory, &path, create)?;
        private::verify_files(&directory, &file, &path)?;
        let flags = OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW
            | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE;
        // Qualify identity before any writable SQLite open, WAL selection or DDL.
        let probe = Connection::open_with_flags(&path, flags | OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        configure(&probe, busy)?;
        let existing = schema(&probe, true)?;
        drop(probe);
        if existing == Schema::Empty && !create {
            return Ok(None);
        }
        private::verify_files(&directory, &file, &path)?;
        let connection =
            Connection::open_with_flags(&path, flags | OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        let mut store = Store {
            connection,
            directory,
            file,
            path,
        };
        store.verify()?;
        configure(&store.connection, busy)?;
        if schema(&store.connection, create)? != Schema::Current {
            let tx = store
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            // Another initializer may have won since the read-only probe.
            match schema(&tx, create)? {
                Schema::Empty => tx.execute_batch(DDL)?,
                Schema::Legacy | Schema::Current => {}
            }
            tx.pragma_update(None, "application_id", APPLICATION_ID)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            tx.commit()?;
        }
        verify_schema(&store.connection)?;
        let mode: String = store
            .connection
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        if mode != "wal" {
            return Err(refused());
        }
        store
            .connection
            .pragma_update(None, "synchronous", "NORMAL")?;
        let page_size: i64 = store
            .connection
            .pragma_query_value(None, "page_size", |r| r.get(0))?;
        if !(512..=65536).contains(&page_size) {
            return Err(refused());
        }
        let pages = MAX_BREAKER_BYTES / page_size;
        store
            .connection
            .pragma_update(None, "max_page_count", pages)?;
        let applied: i64 = store
            .connection
            .pragma_query_value(None, "max_page_count", |r| r.get(0))?;
        if applied != pages {
            return Err(refused());
        }
        store.verify()?;
        Ok(Some(store))
    }
}

#[cfg(unix)]
mod private {
    use super::*;
    use nix::errno::Errno;
    use nix::fcntl::{AtFlags, OFlag, open as open_directory, openat};
    use nix::sys::stat::{FileStat, Mode, SFlag, fchmod, fstat, fstatat, mkdirat};
    use std::path::Component;

    fn directory_mode(stat: &FileStat, leaf: bool) -> rusqlite::Result<()> {
        let uid = nix::unistd::geteuid().as_raw();
        if SFlag::from_bits_truncate(stat.st_mode) != SFlag::S_IFDIR {
            return Err(refused());
        }
        let valid = if leaf {
            stat.st_uid == uid && stat.st_mode & 0o7777 == 0o700
        } else {
            (stat.st_uid == uid || stat.st_uid == 0)
                && (stat.st_mode & 0o022 == 0 || (stat.st_uid == 0 && stat.st_mode & 0o1000 != 0))
        };
        if !valid {
            return Err(refused());
        }
        Ok(())
    }

    fn regular(stat: &FileStat) -> rusqlite::Result<()> {
        if SFlag::from_bits_truncate(stat.st_mode) != SFlag::S_IFREG
            || stat.st_nlink != 1
            || stat.st_uid != nix::unistd::geteuid().as_raw()
            || stat.st_mode & 0o7777 != 0o600
        {
            return Err(refused());
        }
        Ok(())
    }

    pub(super) fn parent(path: &Path, create: bool) -> rusqlite::Result<File> {
        if !path.is_absolute() || path.as_os_str().len() > 4096 {
            return Err(refused());
        }
        let parts: Vec<_> = path.components().collect();
        if !(2..=128).contains(&parts.len())
            || parts
                .iter()
                .skip(1)
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(refused());
        }
        let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
        let mut handle =
            open_directory(Path::new("/"), flags, Mode::empty()).map_err(|_| refused())?;
        directory_mode(&fstat(&handle).map_err(|_| refused())?, parts.len() == 2)?;
        for (index, part) in parts.iter().enumerate().take(parts.len() - 1).skip(1) {
            let name = part.as_os_str();
            handle = match openat(&handle, name, flags, Mode::empty()) {
                Err(Errno::ENOENT) if create => {
                    match mkdirat(&handle, name, Mode::from_bits_truncate(0o700)) {
                        Ok(()) | Err(Errno::EEXIST) => {}
                        Err(_) => return Err(refused()),
                    }
                    openat(&handle, name, flags, Mode::empty()).map_err(|_| refused())?
                }
                result => result.map_err(|_| refused())?,
            };
            directory_mode(
                &fstat(&handle).map_err(|_| refused())?,
                index + 2 == parts.len(),
            )?;
        }
        Ok(handle.into())
    }

    pub(super) fn database(directory: &File, path: &Path, create: bool) -> rusqlite::Result<File> {
        let name = path.file_name().ok_or_else(refused)?;
        // Refuse unsafe existing objects *before* even a nonblocking open.
        match fstatat(directory, name, AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(stat) => regular(&stat)?,
            Err(Errno::ENOENT) if create => {}
            Err(_) => return Err(refused()),
        }
        let flags = OFlag::O_RDWR | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC;
        let fd = match openat(directory, name, flags, Mode::empty()) {
            Err(Errno::ENOENT) if create => {
                match openat(
                    directory,
                    name,
                    flags | OFlag::O_CREAT | OFlag::O_EXCL,
                    Mode::from_bits_truncate(0o600),
                ) {
                    Ok(fd) => {
                        // Only our exclusively created empty file; never repair an existing one.
                        fchmod(&fd, Mode::from_bits_truncate(0o600)).map_err(|_| refused())?;
                        fd
                    }
                    Err(Errno::EEXIST) => {
                        openat(directory, name, flags, Mode::empty()).map_err(|_| refused())?
                    }
                    Err(_) => return Err(refused()),
                }
            }
            result => result.map_err(|_| refused())?,
        };
        let stat = fstat(&fd).map_err(|_| refused())?;
        regular(&stat)?;
        if stat.st_size < 0 || stat.st_size > MAX_BREAKER_BYTES {
            return Err(refused());
        }
        Ok(fd.into())
    }

    pub(super) fn verify_files(directory: &File, file: &File, path: &Path) -> rusqlite::Result<()> {
        let name = path.file_name().ok_or_else(refused)?;
        let held = fstat(file).map_err(|_| refused())?;
        regular(&held)?;
        let actual =
            fstatat(directory, name, AtFlags::AT_SYMLINK_NOFOLLOW).map_err(|_| refused())?;
        regular(&actual)?;
        if (held.st_dev, held.st_ino) != (actual.st_dev, actual.st_ino) {
            return Err(refused());
        }
        for suffix in ["-wal", "-shm", "-journal"] {
            let mut sidecar = name.to_os_string();
            sidecar.push(suffix);
            match fstatat(directory, sidecar.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
                Ok(stat) => regular(&stat)?,
                Err(Errno::ENOENT) => {}
                Err(_) => return Err(refused()),
            }
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
#[path = "store_tests.rs"]
mod tests;
