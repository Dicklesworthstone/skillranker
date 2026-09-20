//! Platform-specific admission, shared by cache and ledger descriptor walks.

use nix::sys::statfs::Statfs;
use std::path::PathBuf;

pub(super) type DirectoryIdentity = (nix::libc::dev_t, nix::libc::ino_t);

#[cfg(target_os = "linux")]
pub(super) fn local_filesystem(stat: &Statfs) -> bool {
    linux_filesystem(stat.filesystem_type())
}

#[cfg(target_os = "linux")]
fn linux_filesystem(kind: nix::sys::statfs::FsType) -> bool {
    use nix::sys::statfs::{BTRFS_SUPER_MAGIC, EXT4_SUPER_MAGIC, TMPFS_MAGIC};
    // XFS has the same on-disk magic on glibc and musl; nix only exports
    // its named constant for glibc targets.
    matches!(kind, EXT4_SUPER_MAGIC | BTRFS_SUPER_MAGIC | TMPFS_MAGIC)
        || kind == nix::sys::statfs::FsType(0x5846_5342)
}

#[cfg(target_os = "macos")]
pub(super) fn local_filesystem(stat: &Statfs) -> bool {
    // Admit the native local filesystems only, not network or FUSE mounts.
    macos_filesystem(stat.filesystem_type_name())
}

#[cfg(any(target_os = "macos", test))]
fn macos_filesystem(name: &str) -> bool {
    matches!(name, "apfs" | "hfs")
}

/// macOS supplies root-owned /tmp and /var aliases. Expand only those exact
/// system aliases; arbitrary symlinks still fail the no-follow descriptor walk.
pub(super) fn storage_path(path: PathBuf) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::MetadataExt;
        use std::path::Path;
        for (alias, target) in [("/tmp", "/private/tmp"), ("/var", "/private/var")] {
            if let Ok(rest) = path.strip_prefix(alias)
                && let Ok(metadata) = std::fs::symlink_metadata(alias)
                && metadata.file_type().is_symlink()
                && metadata.uid() == 0
                && std::fs::read_link(alias).is_ok_and(|p| p == Path::new(target))
            {
                return Path::new(target).join(rest);
            }
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn only_qualified_linux_filesystems_are_admitted() {
        use nix::sys::statfs::*;
        for kind in [
            EXT4_SUPER_MAGIC,
            BTRFS_SUPER_MAGIC,
            FsType(0x5846_5342),
            TMPFS_MAGIC,
        ] {
            assert!(linux_filesystem(kind));
        }
        for kind in [NFS_SUPER_MAGIC, FUSE_SUPER_MAGIC, FsType(0)] {
            assert!(!linux_filesystem(kind));
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
}
