//! Guard control files: one admitted directory, bounded reads, and a stable lock.
//!
//! Configuration directories may be searchable (0755), but never writable by
//! another user. Only verified macOS system aliases are expanded. This does not
//! defend against a hostile process with the same uid, which can edit the guard
//! itself. Revalidation still rejects ordinary replacement races.

use super::{AllowanceError, AllowancePaths, COUNTER, GUARD_FILE_MAX_BYTES, Guard, state};
use crate::filesystem_policy::require_persistent;
use nix::errno::Errno;
use nix::fcntl::{AtFlags, Flock, FlockArg, OFlag, open, openat, renameat};
use nix::sys::stat::{FileStat, Mode, SFlag, fstat, fstatat, mkdirat};
use nix::unistd::{UnlinkatFlags, unlinkat};
use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata};
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

fn unavailable() -> AllowanceError {
    state("the guard or lock is not an owned, safe regular-file location")
}

fn directory(stat: &FileStat, leaf: bool) -> Result<(), AllowanceError> {
    let uid = nix::unistd::geteuid().as_raw();
    let sticky_root = !leaf && stat.st_uid == 0 && stat.st_mode & 0o1000 != 0;
    if SFlag::from_bits_truncate(stat.st_mode) != SFlag::S_IFDIR
        || (stat.st_uid != uid && stat.st_uid != 0)
        || (stat.st_mode & 0o022 != 0 && !sticky_root)
    {
        return Err(unavailable());
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Kind {
    Guard,
    Lock,
}

fn regular(stat: &FileStat, kind: Kind) -> Result<(), AllowanceError> {
    let mode = stat.st_mode & 0o7777;
    let mode_ok = match kind {
        Kind::Guard => mode == 0o600,
        // Old releases created empty lock files with the process umask. Read
        // bits are harmless: no guard or credential content is stored here.
        // Keep those locks usable, but never accept shared write or exec bits.
        Kind::Lock => matches!(mode, 0o600 | 0o604 | 0o640 | 0o644),
    };
    if SFlag::from_bits_truncate(stat.st_mode) != SFlag::S_IFREG
        || stat.st_nlink != 1
        || stat.st_uid != nix::unistd::geteuid().as_raw()
        || !mode_ok
    {
        return Err(unavailable());
    }
    Ok(())
}

struct Parent {
    path: PathBuf,
    handle: File,
}

impl Parent {
    /// None means a genuinely absent component, not a dangling symlink or a
    /// permission failure. Read-only checks never create even a missing parent.
    fn open(path: &Path, create: bool) -> Result<Option<(Self, OsString)>, AllowanceError> {
        let path = crate::platform_path::storage_path(path.to_path_buf());
        if !path.is_absolute() || path.as_os_str().len() > 4096 {
            return Err(unavailable());
        }
        let parts: Vec<_> = path.components().collect();
        if !(2..=128).contains(&parts.len())
            || parts
                .iter()
                .skip(1)
                .any(|p| !matches!(p, Component::Normal(_)))
        {
            return Err(unavailable());
        }
        let name = parts.last().ok_or_else(unavailable)?.as_os_str().to_owned();
        let parent_path = path.parent().ok_or_else(unavailable)?.to_owned();
        let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
        let mut handle = open(Path::new("/"), flags, Mode::empty()).map_err(|_| unavailable())?;
        directory(
            &fstat(&handle).map_err(|_| unavailable())?,
            parts.len() == 2,
        )?;
        for (index, part) in parts.iter().enumerate().take(parts.len() - 1).skip(1) {
            let opened = match openat(&handle, part.as_os_str(), flags, Mode::empty()) {
                Err(Errno::ENOENT) if !create => return Ok(None),
                Err(Errno::ENOENT) => {
                    require_persistent(&handle).map_err(state)?;
                    match mkdirat(&handle, part.as_os_str(), Mode::from_bits_truncate(0o700)) {
                        Ok(()) | Err(Errno::EEXIST) => {}
                        Err(_) => return Err(unavailable()),
                    }
                    openat(&handle, part.as_os_str(), flags, Mode::empty())
                }
                result => result,
            }
            .map_err(|_| unavailable())?;
            directory(
                &fstat(&opened).map_err(|_| unavailable())?,
                index + 2 == parts.len(),
            )?;
            handle = opened;
        }
        if create {
            require_persistent(&handle).map_err(state)?;
        }
        Ok(Some((
            Self {
                path: parent_path,
                handle: handle.into(),
            },
            name,
        )))
    }

    fn revalidate(&self) -> Result<(), AllowanceError> {
        // Append an inert basename to reuse the descriptor walk without opening
        // a file. No filesystem object is created by this read-only walk.
        let (current, _) =
            Self::open(&self.path.join(".sr-guard-check"), false)?.ok_or_else(unavailable)?;
        let old = fstat(&self.handle).map_err(|_| unavailable())?;
        let new = fstat(&current.handle).map_err(|_| unavailable())?;
        if (old.st_dev, old.st_ino) != (new.st_dev, new.st_ino) {
            return Err(unavailable());
        }
        Ok(())
    }

    fn inspect(&self, name: &OsStr, kind: Kind) -> Result<bool, AllowanceError> {
        match fstatat(&self.handle, name, AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(stat) => {
                regular(&stat, kind)?;
                Ok(true)
            }
            Err(Errno::ENOENT) => Ok(false),
            Err(_) => Err(unavailable()),
        }
    }

    fn verify(&self, name: &OsStr, file: &File, kind: Kind) -> Result<(), AllowanceError> {
        self.revalidate()?;
        require_persistent(&self.handle).map_err(state)?;
        require_persistent(file).map_err(state)?;
        let held = fstat(file).map_err(|_| unavailable())?;
        let named =
            fstatat(&self.handle, name, AtFlags::AT_SYMLINK_NOFOLLOW).map_err(|_| unavailable())?;
        regular(&held, kind)?;
        regular(&named, kind)?;
        if (held.st_dev, held.st_ino) != (named.st_dev, named.st_ino) {
            return Err(unavailable());
        }
        Ok(())
    }
}

struct Reader {
    parent: Parent,
    name: OsString,
    file: File,
    before: Metadata,
}

impl Reader {
    fn open(path: &Path) -> Result<Option<Self>, AllowanceError> {
        let Some((parent, name)) = Parent::open(path, false)? else {
            return Ok(None);
        };
        if !parent.inspect(&name, Kind::Guard)? {
            parent.revalidate()?;
            return Ok(None);
        }
        // O_NONBLOCK prevents a raced-in FIFO from waiting for a writer;
        // O_NOFOLLOW refuses a raced-in symlink. Recheck the opened object.
        let flags = OFlag::O_RDONLY | OFlag::O_NONBLOCK | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
        let file: File = openat(&parent.handle, name.as_os_str(), flags, Mode::empty())
            .map_err(|_| unavailable())?
            .into();
        parent.verify(&name, &file, Kind::Guard)?;
        let before = file.metadata().map_err(|_| unavailable())?;
        if before.len() > GUARD_FILE_MAX_BYTES {
            return Err(state("the guard exceeds its byte limit"));
        }
        Ok(Some(Self {
            parent,
            name,
            file,
            before,
        }))
    }

    fn read(mut self) -> Result<Guard, AllowanceError> {
        let mut bytes = Vec::new();
        (&mut self.file)
            .take(GUARD_FILE_MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| state("the guard file cannot be read"))?;
        if bytes.len() as u64 > GUARD_FILE_MAX_BYTES {
            return Err(state("the guard exceeds its byte limit"));
        }
        self.parent.verify(&self.name, &self.file, Kind::Guard)?;
        let after = self.file.metadata().map_err(|_| unavailable())?;
        let version = |m: &Metadata| {
            (
                m.len(),
                m.mtime(),
                m.mtime_nsec(),
                m.ctime(),
                m.ctime_nsec(),
            )
        };
        if version(&self.before) != version(&after) || after.len() != bytes.len() as u64 {
            return Err(state("the guard changed while it was being read"));
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| state("the guard is not UTF-8"))?;
        Guard::decode(text)
    }
}

pub(super) fn read(path: &Path) -> Result<Option<Guard>, AllowanceError> {
    Reader::open(path)?.map(Reader::read).transpose()
}

/// This lock file is never renamed or unlinked by sr. Retain the same opened
/// inode across nonblocking retries; do not switch to a replacement lock.
pub(super) struct GuardLock {
    _flock: Flock<File>,
}

impl GuardLock {
    pub(super) fn acquire(
        paths: &AllowancePaths,
        budget: Duration,
    ) -> Result<Self, AllowanceError> {
        let started = Instant::now();
        let (parent, name) = Parent::open(&paths.lock_path(), true)?.ok_or_else(unavailable)?;
        parent.inspect(&name, Kind::Lock)?;
        let flags = OFlag::O_RDWR
            | OFlag::O_CREAT
            | OFlag::O_NONBLOCK
            | OFlag::O_NOFOLLOW
            | OFlag::O_CLOEXEC;
        let mut file: File = openat(
            &parent.handle,
            name.as_os_str(),
            flags,
            Mode::from_bits_truncate(0o600),
        )
        .map_err(|_| unavailable())?
        .into();
        parent.verify(&name, &file, Kind::Lock)?;
        loop {
            match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
                Ok(flock) => {
                    parent.verify(&name, &flock, Kind::Lock)?;
                    return Ok(Self { _flock: flock });
                }
                Err((returned, Errno::EWOULDBLOCK)) => {
                    file = returned;
                    let remaining = budget.saturating_sub(started.elapsed());
                    if remaining.is_zero() {
                        return Err(state("the guard lock is busy"));
                    }
                    std::thread::sleep(remaining.min(Duration::from_millis(2)));
                    // A delayed wake must not authorize work outside the lock budget.
                    if started.elapsed() >= budget {
                        return Err(state("the guard lock is busy"));
                    }
                    parent.verify(&name, &file, Kind::Lock)?;
                }
                Err(_) => return Err(state("the guard lock cannot be acquired")),
            }
        }
    }
}

/// Durable replacement under the caller's GuardLock. Failure cleanup removes
/// only our own still-named temporary inode, never a collision or final target.
pub(super) fn publish(paths: &AllowancePaths, guard: &Guard) -> Result<(), AllowanceError> {
    let bytes = guard.encode().into_bytes();
    if bytes.len() as u64 > GUARD_FILE_MAX_BYTES {
        return Err(state("the guard exceeds its byte limit"));
    }
    let (parent, name) = Parent::open(&paths.guard, false)?.ok_or_else(unavailable)?;
    parent.inspect(&name, Kind::Guard)?;
    let tmp = format!(
        "allowance.toml.sr-tmp.{}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    publish_named(&parent, &name, &tmp, &bytes)
}

fn publish_named(
    parent: &Parent,
    name: &OsStr,
    tmp: &str,
    bytes: &[u8],
) -> Result<(), AllowanceError> {
    parent.revalidate()?;
    require_persistent(&parent.handle).map_err(state)?;
    let flags = OFlag::O_WRONLY
        | OFlag::O_CREAT
        | OFlag::O_EXCL
        | OFlag::O_NONBLOCK
        | OFlag::O_NOFOLLOW
        | OFlag::O_CLOEXEC;
    let mut file: File = openat(&parent.handle, tmp, flags, Mode::from_bits_truncate(0o600))
        .map_err(|_| unavailable())?
        .into();
    let result = (|| {
        parent.verify(OsStr::new(tmp), &file, Kind::Guard)?;
        file.write_all(bytes)
            .map_err(|_| state("the guard cannot be written"))?;
        file.sync_all()
            .map_err(|_| state("the guard cannot be flushed"))?;
        parent.verify(OsStr::new(tmp), &file, Kind::Guard)?;
        parent.inspect(name, Kind::Guard)?;
        renameat(&parent.handle, tmp, &parent.handle, name)
            .map_err(|_| state("the guard cannot be replaced"))?;
        parent
            .handle
            .sync_all()
            .map_err(|_| state("the guard directory cannot be flushed"))?;
        parent.verify(name, &file, Kind::Guard)
    })();
    if result.is_err() && parent.verify(OsStr::new(tmp), &file, Kind::Guard).is_ok() {
        let _ = unlinkat(&parent.handle, tmp, UnlinkatFlags::NoRemoveDir);
    }
    result
}

#[cfg(test)]
#[path = "guard_file_tests.rs"]
mod tests;
