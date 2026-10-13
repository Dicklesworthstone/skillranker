//! Filesystem classification, not directory authority or durability certification.
//!
//! The cache/ledger admission set is unchanged. Enforced allowance state has a
//! stricter prerequisite: its guard, lock and accounting cannot live on tmpfs.
//! SQLite FULL and fsync do not make a volatile filesystem survive unmount/reboot.
//! Device/VFS sync behavior and crash qualification remain separate requirements.

#[cfg(any(target_os = "linux", target_os = "macos"))]
use nix::sys::statfs::{Statfs, fstatfs};
use std::os::fd::AsFd;

#[cfg(target_os = "linux")]
fn linux_filesystem(kind: nix::sys::statfs::FsType, allow_volatile: bool) -> bool {
    use nix::sys::statfs::{BTRFS_SUPER_MAGIC, EXT4_SUPER_MAGIC, FsType, TMPFS_MAGIC};
    // XFS has the same magic on glibc and musl; nix's named constant is glibc-only.
    matches!(kind, EXT4_SUPER_MAGIC | BTRFS_SUPER_MAGIC)
        || kind == FsType(0x5846_5342)
        || (allow_volatile && kind == TMPFS_MAGIC)
}

#[cfg(any(target_os = "macos", test))]
fn macos_filesystem(name: &str) -> bool {
    matches!(name, "apfs" | "hfs")
}

#[cfg(target_os = "linux")]
pub(crate) fn local_filesystem(stat: &Statfs) -> bool {
    linux_filesystem(stat.filesystem_type(), true)
}

#[cfg(target_os = "macos")]
pub(crate) fn local_filesystem(stat: &Statfs) -> bool {
    macos_filesystem(stat.filesystem_type_name())
}

/// Inspect the actual held object, not a pathname or a guessed mount location.
/// Unknown/network/FUSE/volatile filesystems cannot host enforced allowance
/// state. This is deliberately independent of the platform-gated storage module.
pub(crate) fn require_persistent(file: &impl AsFd) -> Result<(), &'static str> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let stat = fstatfs(file).map_err(|_| "allowance filesystem could not be inspected")?;
        #[cfg(target_os = "linux")]
        let persistent = linux_filesystem(stat.filesystem_type(), false);
        #[cfg(target_os = "macos")]
        let persistent = macos_filesystem(stat.filesystem_type_name());
        if persistent {
            return Ok(());
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = file;
    Err("allowance state requires a supported persistent local filesystem")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn only_qualified_linux_filesystems_are_admitted() {
        use nix::sys::statfs::*;
        for kind in [EXT4_SUPER_MAGIC, BTRFS_SUPER_MAGIC, FsType(0x5846_5342)] {
            assert!(linux_filesystem(kind, true));
            assert!(linux_filesystem(kind, false));
        }
        assert!(linux_filesystem(TMPFS_MAGIC, true));
        assert!(!linux_filesystem(TMPFS_MAGIC, false));
        for kind in [
            NFS_SUPER_MAGIC,
            FUSE_SUPER_MAGIC,
            FsType(0x794c_7630), // overlay: its backing storage is not established here.
            FsType(0x8584_58f6_u32 as _), // ramfs, also on signed 32-bit f_type.
            FsType(0),
        ] {
            assert!(!linux_filesystem(kind, true));
            assert!(!linux_filesystem(kind, false));
        }
    }

    #[test]
    fn only_native_macos_filesystems_are_admitted() {
        for name in ["apfs", "hfs"] {
            assert!(macos_filesystem(name));
        }
        for name in ["nfs", "smbfs", "osxfuse", "webdav", "", "APFS"] {
            assert!(!macos_filesystem(name));
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn descriptor_policy_matches_the_native_filesystem_not_its_path_name() {
        let file = std::fs::File::open(std::env::temp_dir()).unwrap();
        let stat = fstatfs(&file).unwrap();
        #[cfg(target_os = "linux")]
        let expected = linux_filesystem(stat.filesystem_type(), false);
        #[cfg(target_os = "macos")]
        let expected = macos_filesystem(stat.filesystem_type_name());
        assert_eq!(require_persistent(&file).is_ok(), expected);
    }
}
