//! Private, local cache directory admission. All opens walk trusted descriptors.

use super::{
    CACHE_FILE, CACHE_QUOTA_BYTES, CacheCapacityReport, MAINTENANCE_RESERVE_BYTES,
    MUTATION_RESERVE_BYTES, StoreError, check_work,
};
use crate::runtime::EntryClock;
use asupersync::Cx;
use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag, open, openat};
use nix::sys::stat::{FileStat, Mode, SFlag, fstat, fstatat, mkdirat};
use nix::sys::statfs::{
    BTRFS_SUPER_MAGIC, EXT4_SUPER_MAGIC, FsType, TMPFS_MAGIC, XFS_SUPER_MAGIC, fstatfs,
};
use nix::sys::statvfs::fstatvfs;
use std::collections::BTreeSet;
use std::fs::File;
use std::path::{Component, Path, PathBuf};

pub(super) struct PrivateDirectory {
    pub path: PathBuf,
    handle: File,
    identity: (u64, u64),
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

fn local_filesystem(kind: FsType) -> bool {
    matches!(
        kind,
        EXT4_SUPER_MAGIC | BTRFS_SUPER_MAGIC | XFS_SUPER_MAGIC | TMPFS_MAGIC
    )
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
                    if !local_filesystem(fstatfs(&handle).map_err(io_error)?.filesystem_type()) {
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
        if !local_filesystem(fstatfs(&self.handle).map_err(io_error)?.filesystem_type()) {
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
    #[test]
    fn only_qualified_local_filesystems_are_admitted() {
        for kind in [
            EXT4_SUPER_MAGIC,
            BTRFS_SUPER_MAGIC,
            XFS_SUPER_MAGIC,
            TMPFS_MAGIC,
        ] {
            assert!(local_filesystem(kind));
        }
        for kind in [
            nix::sys::statfs::NFS_SUPER_MAGIC,
            nix::sys::statfs::FUSE_SUPER_MAGIC,
            FsType(0),
        ] {
            assert!(!local_filesystem(kind));
        }
    }
}
