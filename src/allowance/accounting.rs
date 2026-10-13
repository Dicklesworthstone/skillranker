//! Private accounting files and qualified SQLite readers/writers.
//!
//! The guard still owns initialization authority. This layer never resets history,
//! repairs permissions, or initializes schema. Read-only WAL access can maintain
//! shared-memory bookkeeping. Revalidation detects ordinary replacement; hostile
//! same-uid mutation and uninterruptible kernel calls are outside this boundary.
use super::{ACCOUNTING_MAX_BYTES, AllowanceError, read_meta, state};
use crate::filesystem_policy::require_persistent;
use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag, open, openat};
use nix::sys::stat::{FileStat, Mode, SFlag, fstat, fstatat, mkdirat};
use rusqlite::{Connection, OpenFlags, TransactionBehavior, config::DbConfig, limits::Limit};
use std::fs::File;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

const FAMILY_MAX_BYTES: i64 = 64 * 1024 * 1024;

fn refused() -> AllowanceError {
    state("the accounting database or sidecars are not a private, bounded regular-file location")
}

fn directory(stat: &FileStat, leaf: bool, writable: bool) -> Result<(), AllowanceError> {
    let uid = nix::unistd::geteuid().as_raw();
    let mode = stat.st_mode & 0o7777;
    let trusted = if leaf {
        stat.st_uid == uid && (mode == 0o700 || (!writable && mode == 0o500))
    } else {
        (stat.st_uid == uid || stat.st_uid == 0)
            && (mode & 0o022 == 0 || (stat.st_uid == 0 && mode & 0o1000 != 0))
    };
    if SFlag::from_bits_truncate(stat.st_mode) != SFlag::S_IFDIR || !trusted {
        return Err(refused());
    }
    Ok(())
}

fn regular(stat: &FileStat, writable: bool) -> Result<(), AllowanceError> {
    let mode = stat.st_mode & 0o7777;
    if SFlag::from_bits_truncate(stat.st_mode) != SFlag::S_IFREG
        || stat.st_nlink != 1
        || stat.st_uid != nix::unistd::geteuid().as_raw()
        || !(mode == 0o600 || (!writable && mode == 0o400))
        || stat.st_size < 0
    {
        return Err(refused());
    }
    Ok(())
}

fn parent(path: &Path, create: bool, writable: bool) -> Result<Option<File>, AllowanceError> {
    if !path.is_absolute() || path.as_os_str().len() > 4096 {
        return Err(refused());
    }
    let parts: Vec<_> = path.components().collect();
    if !(2..=128).contains(&parts.len())
        || parts.iter().skip(1).any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(refused());
    }
    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let mut handle = open(Path::new("/"), flags, Mode::empty()).map_err(|_| refused())?;
    directory(&fstat(&handle).map_err(|_| refused())?, parts.len() == 2, writable)?;
    for (index, part) in parts.iter().enumerate().take(parts.len() - 1).skip(1) {
        handle = match openat(&handle, part.as_os_str(), flags, Mode::empty()) {
            Err(Errno::ENOENT) if !create => return Ok(None),
            Err(Errno::ENOENT) => {
                require_persistent(&handle).map_err(state)?;
                match mkdirat(&handle, part.as_os_str(), Mode::from_bits_truncate(0o700)) {
                    Ok(()) | Err(Errno::EEXIST) => {}
                    Err(_) => return Err(refused()),
                }
                openat(&handle, part.as_os_str(), flags, Mode::empty())
            }
            result => result,
        }
        .map_err(|_| refused())?;
        directory(&fstat(&handle).map_err(|_| refused())?, index + 2 == parts.len(), writable)?;
    }
    if create {
        require_persistent(&handle).map_err(state)?;
    }
    Ok(Some(handle.into()))
}

/// Retain admitted identities for the complete lifetime of the SQLite connection.
pub(super) struct Files {
    path: PathBuf,
    directory: File,
    file: File,
    writable: bool,
}

impl Files {
    fn open(path: &Path, create: bool, writable: bool) -> Result<Option<Self>, AllowanceError> {
        let path = crate::platform_path::storage_path(path.to_path_buf());
        let Some(directory) = parent(&path, create, writable)? else {
            return Ok(None);
        };
        let name = path.file_name().ok_or_else(refused)?;
        let exists = match fstatat(&directory, name, AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(stat) => {
                regular(&stat, writable)?;
                if stat.st_size > ACCOUNTING_MAX_BYTES {
                    return Err(refused());
                }
                true
            }
            Err(Errno::ENOENT) => false,
            Err(_) => return Err(refused()),
        };
        // Orphaned WAL/journal files may contain enforcement history. Never
        // attach them to a newly created empty database or call them absence.
        for suffix in ["-wal", "-shm", "-journal"] {
            let mut sidecar = name.to_os_string();
            sidecar.push(suffix);
            match fstatat(&directory, sidecar.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
                Ok(stat) => {
                    regular(&stat, writable)?;
                    if !exists || stat.st_size > FAMILY_MAX_BYTES {
                        return Err(refused());
                    }
                }
                Err(Errno::ENOENT) => {}
                Err(_) => return Err(refused()),
            }
        }
        if !exists && !create {
            let current = parent(&path, false, writable)?.ok_or_else(refused)?;
            let held = fstat(&directory).map_err(|_| refused())?;
            let named = fstat(&current).map_err(|_| refused())?;
            if (held.st_dev, held.st_ino) != (named.st_dev, named.st_ino) {
                return Err(refused());
            }
            return Ok(None);
        }
        require_persistent(&directory).map_err(state)?;
        let flags = (if writable { OFlag::O_RDWR } else { OFlag::O_RDONLY })
            | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC;
        let opened = if exists {
            openat(&directory, name, flags, Mode::empty())
        } else {
            // Another first-time creator is not overwritten. The next invocation
            // can retry after inspecting whatever it published.
            openat(&directory, name, flags | OFlag::O_CREAT | OFlag::O_EXCL, Mode::from_bits_truncate(0o600))
        }
        .map_err(|_| refused())?;
        let files = Self { path, directory, file: opened.into(), writable };
        files.verify()?;
        Ok(Some(files))
    }

    pub(super) fn verify(&self) -> Result<(), AllowanceError> {
        let current = parent(&self.path, false, self.writable)?.ok_or_else(refused)?;
        require_persistent(&self.directory).map_err(state)?;
        require_persistent(&self.file).map_err(state)?;
        let held_dir = fstat(&self.directory).map_err(|_| refused())?;
        let named_dir = fstat(&current).map_err(|_| refused())?;
        if (held_dir.st_dev, held_dir.st_ino) != (named_dir.st_dev, named_dir.st_ino) {
            return Err(refused());
        }
        let name = self.path.file_name().ok_or_else(refused)?;
        let held = fstat(&self.file).map_err(|_| refused())?;
        let named = fstatat(&self.directory, name, AtFlags::AT_SYMLINK_NOFOLLOW).map_err(|_| refused())?;
        regular(&held, self.writable)?;
        regular(&named, self.writable)?;
        if (held.st_dev, held.st_ino) != (named.st_dev, named.st_ino)
            || held.st_size > ACCOUNTING_MAX_BYTES
        {
            return Err(refused());
        }
        let mut bytes = held.st_size;
        for suffix in ["-wal", "-shm", "-journal"] {
            let mut sidecar = name.to_os_string();
            sidecar.push(suffix);
            match fstatat(&self.directory, sidecar.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
                Ok(stat) => {
                    regular(&stat, self.writable)?;
                    // Sidecars are ordinary siblings. A separate mounted file
                    // must not bypass the held directory's filesystem policy.
                    if stat.st_dev != held_dir.st_dev {
                        return Err(refused());
                    }
                    bytes = bytes.checked_add(stat.st_size).ok_or_else(refused)?;
                    if bytes > FAMILY_MAX_BYTES {
                        return Err(refused());
                    }
                }
                Err(Errno::ENOENT) => {}
                Err(_) => return Err(refused()),
            }
        }
        Ok(())
    }
}

// SQLite must drop before the descriptors binding its pathname.
pub(super) struct Store {
    pub(super) connection: Connection,
    pub(super) files: Files,
}
impl std::ops::Deref for Store {
    type Target = Connection;
    fn deref(&self) -> &Connection { &self.connection }
}
impl std::ops::DerefMut for Store {
    fn deref_mut(&mut self) -> &mut Connection { &mut self.connection }
}

fn configure(connection: &Connection) -> Result<(), AllowanceError> {
    let fail = |_| state("the accounting SQLite connection cannot be configured");
    connection.busy_timeout(Duration::from_millis(25)).map_err(fail)?;
    for (setting, expected) in [
        (DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true),
        (DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false),
        (DbConfig::SQLITE_DBCONFIG_ENABLE_TRIGGER, false),
        (DbConfig::SQLITE_DBCONFIG_ENABLE_VIEW, false),
    ] {
        if connection.set_db_config(setting, expected).map_err(fail)? != expected {
            return Err(state("the accounting SQLite safeguards are unavailable"));
        }
    }
    for (limit, value) in [
        (Limit::SQLITE_LIMIT_LENGTH, ACCOUNTING_MAX_BYTES as i32),
        (Limit::SQLITE_LIMIT_SQL_LENGTH, 64 * 1024),
        (Limit::SQLITE_LIMIT_ATTACHED, 0),
        (Limit::SQLITE_LIMIT_WORKER_THREADS, 0),
    ] {
        connection.set_limit(limit, value).map_err(fail)?;
    }
    connection.pragma_update(None, "temp_store", "MEMORY").map_err(fail)?;
    Ok(())
}

fn flags() -> OpenFlags {
    OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE
}

pub(super) fn read<T>(
    path: &Path,
    inspect: impl FnOnce(&Connection) -> Result<T, AllowanceError>,
) -> Result<Option<T>, AllowanceError> {
    let Some(files) = Files::open(path, false, false)? else { return Ok(None); };
    let mut connection = Connection::open_with_flags(&files.path, flags() | OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|_| state("the accounting database cannot be read"))?;
    configure(&connection)?;
    connection.pragma_update(None, "query_only", true).map_err(|_| refused())?;
    files.verify()?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Deferred).map_err(|_| refused())?;
    let result = inspect(&tx)?;
    tx.commit().map_err(|_| state("the accounting read snapshot cannot be closed"))?;
    files.verify()?;
    Ok(Some(result))
}

pub(super) fn open_writer(path: &Path, create: bool) -> Result<Store, AllowanceError> {
    crate::sqlite_engine::linked_engine()
        .map_err(|_| state("the linked SQLite engine is not qualified for accounting writes"))?;
    let files = Files::open(path, create, true)?
        .ok_or_else(|| state("the accounting database is missing"))?;
    // Validate identity in a read snapshot before opening writable SQLite or
    // selecting journal mode. Empty first-activation remnants remain retryable.
    {
        let mut probe = Connection::open_with_flags(&files.path, flags() | OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|_| refused())?;
        configure(&probe)?;
        let tx = probe.transaction_with_behavior(TransactionBehavior::Deferred).map_err(|_| refused())?;
        if read_meta(&tx)?.is_none() && !create { return Err(state("the accounting database is not initialized")); }
        tx.commit().map_err(|_| refused())?;
    }
    files.verify()?;
    let connection = Connection::open_with_flags(&files.path, flags() | OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|_| state("the accounting database is unavailable"))?;
    let store = Store { connection, files };
    configure(&store.connection)?;
    store.files.verify()?;
    if read_meta(&store.connection)?.is_none() && !create {
        return Err(state("the accounting database is not initialized"));
    }
    let fail = |_| state("the accounting writer cannot guarantee durable debits");
    let mode: String = store.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0)).map_err(fail)?;
    store.pragma_update(None, "synchronous", "FULL").map_err(fail)?;
    let synchronous: i64 = store.pragma_query_value(None, "synchronous", |r| r.get(0)).map_err(fail)?;
    if mode != "wal" || synchronous != 2 { return Err(fail(rusqlite::Error::InvalidQuery)); }
    let page_size: i64 = store.pragma_query_value(None, "page_size", |r| r.get(0)).map_err(fail)?;
    if !(512..=65536).contains(&page_size) { return Err(refused()); }
    let pages = ACCOUNTING_MAX_BYTES / page_size;
    store.pragma_update(None, "max_page_count", pages).map_err(fail)?;
    let applied: i64 = store.pragma_query_value(None, "max_page_count", |r| r.get(0)).map_err(fail)?;
    if applied != pages { return Err(refused()); }
    store.files.verify()?;
    Ok(store)
}

#[cfg(test)]
#[path = "accounting_tests.rs"]
mod tests;
