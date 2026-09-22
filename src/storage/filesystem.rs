//! Private, local cache directory admission. All opens walk trusted descriptors.

use super::platform::{DirectoryIdentity, local_filesystem, storage_path};
use super::{
    CACHE_FILE, CACHE_QUOTA_BYTES, CacheCapacityReport, MAINTENANCE_RESERVE_BYTES,
    MUTATION_RESERVE_BYTES, StoreError, check_work,
};
use crate::runtime::EntryClock;
use asupersync::Cx;
use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag, open, openat};
use nix::sys::stat::{FileStat, Mode, SFlag, fstat, fstatat, mkdirat};
use nix::sys::statfs::fstatfs;
use nix::sys::statvfs::fstatvfs;
use std::collections::BTreeSet;
use std::fs::File;
use std::path::{Component, Path, PathBuf};

pub(super) struct PrivateDirectory {
    pub path: PathBuf,
    handle: File,
    identity: DirectoryIdentity,
}

fn io_error(error: Errno) -> StoreError {
    match error {
        Errno::ENOENT => StoreError::Missing,
        Errno::ELOOP | Errno::ENOTDIR => StoreError::UnsafePath,
        Errno::EACCES | Errno::EPERM => StoreError::Permissions,
        _ => StoreError::Io,
    }
}

fn owned_regular(stat: &FileStat, uid: u32) -> Result<(), StoreError> {
    if SFlag::from_bits_truncate(stat.st_mode) != SFlag::S_IFREG || stat.st_nlink != 1 {
        return Err(StoreError::UnsafePath);
    }
    if stat.st_uid != uid || stat.st_mode & 0o7777 != 0o600 {
        return Err(StoreError::Permissions);
    }
    Ok(())
}

fn trusted_ancestor(stat: &FileStat, uid: u32, leaf: bool) -> Result<(), StoreError> {
    if SFlag::from_bits_truncate(stat.st_mode) != SFlag::S_IFDIR {
        return Err(StoreError::UnsafePath);
    }
    if leaf {
        if stat.st_uid != uid || stat.st_mode & 0o7777 != 0o700 {
            return Err(StoreError::Permissions);
        }
    } else if (stat.st_uid != 0 && stat.st_uid != uid)
        || (stat.st_mode & 0o022 != 0 && !(stat.st_uid == 0 && stat.st_mode & 0o1000 != 0))
    {
        // Root-owned sticky /tmp is safe for an owner-only child. Arbitrary
        // shared writable ancestors can rename the child and are refused.
        return Err(StoreError::Permissions);
    }
    Ok(())
}

/// What an owner-only refusal was actually about.
///
/// `StoreError::Permissions` stays a unit variant: it is matched in a dozen places and compared
/// directly in tests, and widening it would ripple for no benefit. This walks the same chain
/// `PrivateDirectory::open` walks and applies the same two predicates, then reports the FIRST path
/// that fails and what it must be, so a person is told which directory to chmod instead of being
/// pointed at their configuration (sr-488b).
///
/// Diagnosis only: it opens nothing for writing and changes nothing. It re-stats by path rather
/// than reusing the trusted descriptor walk, which is acceptable precisely because it is not the
/// admission decision — the refusal has already happened, and this only explains it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionRefusal {
    pub path: PathBuf,
    pub actual_mode: u32,
    pub required: RequiredMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequiredMode {
    /// The store's own directory: owned by this user and exactly 0o700.
    OwnerOnlyDirectory,
    /// The store's file: owned by this user and exactly 0o600.
    OwnerOnlyFile,
    /// An ancestor: not group- or other-writable, unless it is root-owned and sticky as /tmp is.
    NotSharedWritable,
    /// Owned by neither this user nor root.
    OwnedByAnotherUser,
}

impl PermissionRefusal {
    /// The one line a person needs: which path, what it is now, what it must be.
    pub fn describe(&self) -> String {
        match self.required {
            RequiredMode::OwnerOnlyDirectory => format!(
                "{} is mode {:04o}; the store directory must be owned by you and mode 0700",
                self.path.display(),
                self.actual_mode
            ),
            RequiredMode::OwnerOnlyFile => format!(
                "{} is mode {:04o}; the store file must be owned by you and mode 0600",
                self.path.display(),
                self.actual_mode
            ),
            RequiredMode::NotSharedWritable => format!(
                "{} is mode {:04o} and is writable by group or others; every directory above the \
                 store must be owner-only, or root-owned and sticky as /tmp is",
                self.path.display(),
                self.actual_mode
            ),
            RequiredMode::OwnedByAnotherUser => format!(
                "{} is owned by another user; the store and every directory above it must be owned \
                 by you or by root",
                self.path.display()
            ),
        }
    }

    /// The command that fixes it, which is what the old hint should have said.
    pub fn remedy(&self) -> Option<String> {
        match self.required {
            RequiredMode::OwnerOnlyDirectory => Some(format!("chmod 700 {}", self.path.display())),
            RequiredMode::OwnerOnlyFile => Some(format!("chmod 600 {}", self.path.display())),
            RequiredMode::NotSharedWritable => Some(format!("chmod go-w {}", self.path.display())),
            RequiredMode::OwnedByAnotherUser => None,
        }
    }
}

/// Names the first path in the chain that fails the owner-only rules, or `None` when the chain is
/// fine and the refusal came from somewhere else — a read-only filesystem, for instance. `None` is
/// not a claim that nothing is wrong; it means this is not the thing that was wrong.
pub fn diagnose_owner_only(path: &Path) -> Option<PermissionRefusal> {
    use std::os::unix::fs::MetadataExt;
    let uid = nix::unistd::getuid().as_raw();
    let mut walked = PathBuf::from("/");
    let components: Vec<_> = path.components().collect();
    let last = components.len().saturating_sub(1);
    for (index, component) in components.iter().enumerate() {
        match component {
            Component::RootDir => continue,
            Component::Normal(part) => walked.push(part),
            // A relative or unnormalized path is not this function's business to interpret.
            _ => return None,
        }
        let Ok(meta) = std::fs::symlink_metadata(&walked) else {
            // A path that does not exist yet is not a permission problem.
            return None;
        };
        let mode = meta.mode() & 0o7777;
        let owner = meta.uid();
        let leaf = index == last;
        if meta.is_file() {
            if owner != uid {
                return Some(PermissionRefusal {
                    path: walked,
                    actual_mode: mode,
                    required: RequiredMode::OwnedByAnotherUser,
                });
            }
            if mode != 0o600 {
                return Some(PermissionRefusal {
                    path: walked,
                    actual_mode: mode,
                    required: RequiredMode::OwnerOnlyFile,
                });
            }
            continue;
        }
        if leaf {
            if owner != uid {
                return Some(PermissionRefusal {
                    path: walked,
                    actual_mode: mode,
                    required: RequiredMode::OwnedByAnotherUser,
                });
            }
            if mode != 0o700 {
                return Some(PermissionRefusal {
                    path: walked,
                    actual_mode: mode,
                    required: RequiredMode::OwnerOnlyDirectory,
                });
            }
        } else {
            if owner != 0 && owner != uid {
                return Some(PermissionRefusal {
                    path: walked,
                    actual_mode: mode,
                    required: RequiredMode::OwnedByAnotherUser,
                });
            }
            let sticky_root = owner == 0 && mode & 0o1000 != 0;
            if mode & 0o022 != 0 && !sticky_root {
                return Some(PermissionRefusal {
                    path: walked,
                    actual_mode: mode,
                    required: RequiredMode::NotSharedWritable,
                });
            }
        }
    }
    None
}

fn recording_capacity(bytes: u64, available: u128) -> Result<(), StoreError> {
    if bytes > CACHE_QUOTA_BYTES - MAINTENANCE_RESERVE_BYTES - MUTATION_RESERVE_BYTES {
        return Err(StoreError::Quota);
    }
    if available < u128::from(MAINTENANCE_RESERVE_BYTES + MUTATION_RESERVE_BYTES) {
        return Err(StoreError::InsufficientSpace);
    }
    Ok(())
}

impl PrivateDirectory {
    pub fn open(
        path: PathBuf,
        create: bool,
        clock: EntryClock,
        cx: &Cx,
    ) -> Result<Self, StoreError> {
        let path = storage_path(path);
        // Bound path parsing before collecting components or making syscalls.
        if !path.is_absolute() || path.as_os_str().len() > 4096 {
            return Err(StoreError::UnsafePath);
        }
        let components: Vec<_> = path.components().collect();
        if components.len() < 2
            || components.len() > 128
            || components
                .iter()
                .skip(1)
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(StoreError::UnsafePath);
        }
        let uid = nix::unistd::geteuid().as_raw();
        let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
        let mut handle = open(Path::new("/"), flags, Mode::empty()).map_err(io_error)?;
        trusted_ancestor(&fstat(&handle).map_err(io_error)?, uid, false)?;
        for (i, component) in components.iter().enumerate().skip(1) {
            check_work(clock, cx)?;
            let name = component.as_os_str();
            let opened = match openat(&handle, name, flags, Mode::empty()) {
                Err(Errno::ENOENT) if create => {
                    if !local_filesystem(&fstatfs(&handle).map_err(io_error)?) {
                        return Err(StoreError::UnsupportedFilesystem);
                    }
                    match mkdirat(&handle, name, Mode::from_bits_truncate(0o700)) {
                        Ok(()) | Err(Errno::EEXIST) => {}
                        Err(error) => return Err(io_error(error)),
                    }
                    openat(&handle, name, flags, Mode::empty()).map_err(io_error)?
                }
                result => result.map_err(io_error)?,
            };
            trusted_ancestor(
                &fstat(&opened).map_err(io_error)?,
                uid,
                i + 1 == components.len(),
            )?;
            handle = opened;
        }
        let stat = fstat(&handle).map_err(io_error)?;
        let directory = Self {
            path,
            handle: handle.into(),
            identity: (stat.st_dev, stat.st_ino),
        };
        if create {
            directory.admit_space()?;
        } else {
            directory.inspect_files()?;
        }
        Ok(directory)
    }

    pub fn database_path(&self) -> PathBuf {
        self.path.join(CACHE_FILE)
    }

    pub fn revalidate(&self, clock: EntryClock, cx: &Cx) -> Result<(), StoreError> {
        // SQLite opens a pathname, not our directory descriptor. Reject any
        // changed ancestor or leaf before each effect; NOFOLLOW also protects
        // its main-file open. Same-uid hostile filesystem mutation is outside
        // this owner-only boundary, as it can already modify private contents.
        let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
        let mut fd = open(Path::new("/"), flags, Mode::empty()).map_err(io_error)?;
        let uid = nix::unistd::geteuid().as_raw();
        let components: Vec<_> = self.path.components().collect();
        for (i, component) in components.iter().enumerate().skip(1) {
            check_work(clock, cx)?;
            fd = openat(&fd, component.as_os_str(), flags, Mode::empty()).map_err(io_error)?;
            trusted_ancestor(
                &fstat(&fd).map_err(io_error)?,
                uid,
                i + 1 == components.len(),
            )?;
        }
        let stat = fstat(&fd).map_err(io_error)?;
        if (stat.st_dev, stat.st_ino) != self.identity {
            return Err(StoreError::StoreReplaced);
        }
        Ok(())
    }

    pub fn inspect_files(&self) -> Result<u64, StoreError> {
        let uid = nix::unistd::geteuid().as_raw();
        let mut bytes = 0_u64;
        let mut checked_names = BTreeSet::new();
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let name = format!("{CACHE_FILE}{suffix}");
            match fstatat(&self.handle, name.as_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
                Ok(stat) => {
                    owned_regular(&stat, uid)?;
                    bytes = bytes
                        .checked_add(u64::try_from(stat.st_size).map_err(|_| StoreError::Quota)?)
                        .ok_or(StoreError::Quota)?;
                    checked_names.insert(name);
                }
                Err(Errno::ENOENT) => {}
                Err(error) => return Err(io_error(error)),
            }
        }
        if let Ok(entries) = std::fs::read_dir(&self.path) {
            for entry in entries {
                let entry = entry.map_err(|_| StoreError::Io)?;
                let file_name = entry.file_name();
                let name_str = file_name.to_string_lossy();
                if checked_names.contains(name_str.as_ref()) {
                    continue;
                }
                match fstatat(
                    &self.handle,
                    name_str.as_ref(),
                    AtFlags::AT_SYMLINK_NOFOLLOW,
                ) {
                    Ok(stat) => {
                        owned_regular(&stat, uid)?;
                        bytes = bytes
                            .checked_add(
                                u64::try_from(stat.st_size).map_err(|_| StoreError::Quota)?,
                            )
                            .ok_or(StoreError::Quota)?;
                    }
                    Err(Errno::ENOENT) => {}
                    Err(error) => return Err(io_error(error)),
                }
            }
        }
        if bytes > CACHE_QUOTA_BYTES {
            return Err(StoreError::Quota);
        }
        Ok(bytes)
    }

    pub fn capacity_report(&self) -> Result<CacheCapacityReport, StoreError> {
        let occupied_bytes = self.inspect_files()?;
        let stat = fstatvfs(&self.handle).map_err(io_error)?;
        let available_disk_bytes =
            u64::try_from(u128::from(stat.blocks_available()) * u128::from(stat.fragment_size()))
                .unwrap_or(u64::MAX);

        let recording_ceiling_bytes = CACHE_QUOTA_BYTES
            .saturating_sub(MAINTENANCE_RESERVE_BYTES)
            .saturating_sub(MUTATION_RESERVE_BYTES);

        let usable_recording_bytes = recording_ceiling_bytes.saturating_sub(occupied_bytes);

        let is_recording_admitted = occupied_bytes <= recording_ceiling_bytes
            && (available_disk_bytes as u128)
                >= u128::from(MAINTENANCE_RESERVE_BYTES + MUTATION_RESERVE_BYTES);

        Ok(CacheCapacityReport {
            total_quota_bytes: CACHE_QUOTA_BYTES,
            maintenance_reserve_bytes: MAINTENANCE_RESERVE_BYTES,
            mutation_reserve_bytes: MUTATION_RESERVE_BYTES,
            recording_ceiling_bytes,
            occupied_bytes,
            usable_recording_bytes,
            available_disk_bytes,
            is_recording_admitted,
        })
    }

    pub fn admit_space(&self) -> Result<(), StoreError> {
        if !local_filesystem(&fstatfs(&self.handle).map_err(io_error)?) {
            return Err(StoreError::UnsupportedFilesystem);
        }
        let bytes = self.inspect_files()?;
        let stat = fstatvfs(&self.handle).map_err(io_error)?;
        let available = u128::from(stat.blocks_available()) * u128::from(stat.fragment_size());
        recording_capacity(bytes, available)
    }

    pub fn open_database_file(
        &self,
        create: bool,
        clock: EntryClock,
        cx: &Cx,
    ) -> Result<File, StoreError> {
        self.revalidate(clock, cx)?;
        if create {
            self.admit_space()?;
        } else {
            self.inspect_files()?;
        }
        let flags = OFlag::O_RDWR | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC;
        let fd = match openat(&self.handle, CACHE_FILE, flags, Mode::empty()) {
            Err(Errno::ENOENT) if create => match openat(
                &self.handle,
                CACHE_FILE,
                flags | OFlag::O_CREAT | OFlag::O_EXCL,
                Mode::from_bits_truncate(0o600),
            ) {
                Err(Errno::EEXIST) => {
                    openat(&self.handle, CACHE_FILE, flags, Mode::empty()).map_err(io_error)?
                }
                result => result.map_err(io_error)?,
            },
            result => result.map_err(io_error)?,
        };
        owned_regular(
            &fstat(&fd).map_err(io_error)?,
            nix::unistd::geteuid().as_raw(),
        )?;
        Ok(fd.into())
    }

    pub fn verify_database_file(
        &self,
        file: &File,
        clock: EntryClock,
        cx: &Cx,
    ) -> Result<(), StoreError> {
        self.revalidate(clock, cx)?;
        self.inspect_files()?;
        let held = fstat(file).map_err(io_error)?;
        let current =
            fstatat(&self.handle, CACHE_FILE, AtFlags::AT_SYMLINK_NOFOLLOW).map_err(io_error)?;
        if (held.st_dev, held.st_ino) != (current.st_dev, current.st_ino) {
            return Err(StoreError::StoreReplaced);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn foreign_owner_is_rejected_by_the_same_metadata_admission() {
        let file = File::open(std::env::current_exe().unwrap()).unwrap();
        let mut metadata = fstat(&file).unwrap();
        metadata.st_mode = SFlag::S_IFREG.bits() | 0o600;
        metadata.st_nlink = 1;
        let uid = nix::unistd::geteuid().as_raw();
        metadata.st_uid = uid;
        assert_eq!(owned_regular(&metadata, uid), Ok(()));
        metadata.st_uid = uid.wrapping_add(1);
        assert_eq!(owned_regular(&metadata, uid), Err(StoreError::Permissions));
    }
    #[test]
    fn capacity_keeps_both_mutation_and_maintenance_headroom() {
        let ceiling = CACHE_QUOTA_BYTES - MAINTENANCE_RESERVE_BYTES - MUTATION_RESERVE_BYTES;
        let required = u128::from(MAINTENANCE_RESERVE_BYTES + MUTATION_RESERVE_BYTES);
        assert_eq!(recording_capacity(ceiling, required), Ok(()));
        assert_eq!(
            recording_capacity(ceiling + 1, required),
            Err(StoreError::Quota)
        );
        assert_eq!(
            recording_capacity(0, required - 1),
            Err(StoreError::InsufficientSpace)
        );
    }
}
