//! Bounded owner-only export publication with atomic no-clobber guarantees.
//!
//! All destination components are checked through no-follow directory descriptors.
//! Creation, publication and cleanup use one held parent descriptor. File data
//! and the parent directory are both flushed; a post-publication failure is an
//! error even though the destination may already exist. Same-user hostile file
//! mutation is outside the owner-controlled directory boundary.

use crate::output::{CliExit, ErrorKind};
use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag, open, openat};
use nix::sys::stat::{FileStat, Mode, SFlag, fchmod, fstat, fstatat};
use nix::unistd::{UnlinkatFlags, linkat, unlinkat};
use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Default maximum export size for roster snapshots (32 MiB).
pub const DEFAULT_MAX_SNAPSHOT_BYTES: usize = 32 * crate::limits::MIB;

/// Default maximum export size for recorded cases (16 MiB).
pub const DEFAULT_MAX_CASE_BYTES: usize = 16 * crate::limits::MIB;

/// Configuration for atomic private file export.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExportConfig {
    /// Maximum allowed byte size of the exported file.
    pub max_bytes: usize,
}

impl Default for ExportConfig {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_SNAPSHOT_BYTES,
        }
    }
}

impl ExportConfig {
    pub const fn for_snapshot() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_SNAPSHOT_BYTES,
        }
    }

    pub const fn for_case() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_CASE_BYTES,
        }
    }
}

/// Errors during atomic export publication.
///
/// Contains no sensitive contents, credentials, or private text in diagnostics.
#[derive(Debug)]
pub enum ExportError {
    /// Target file or symlink already exists; overwriting is strictly forbidden.
    TargetAlreadyExists(PathBuf),
    /// Destination parent directory is invalid, missing, or not a directory.
    InvalidDirectory(String),
    /// Destination directory permissions or ownership are unsafe.
    Permissions(String),
    /// Exported data exceeds the maximum allowed byte limit.
    Oversized { len: usize, max: usize },
    /// Standard I/O failure.
    Io(std::io::Error),
    /// Publication occurred, but its directory durability was not confirmed.
    Durability(std::io::Error),
}

impl ExportError {
    pub const fn kind(&self) -> ErrorKind {
        match self {
            Self::Oversized { .. } => ErrorKind::OversizedInput,
            _ => ErrorKind::StorageFailure,
        }
    }

    pub const fn exit_code(&self) -> CliExit {
        self.kind().exit_code()
    }
}

impl fmt::Display for ExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TargetAlreadyExists(_) => {
                f.write_str("destination target already exists; refusing to overwrite")
            }
            Self::InvalidDirectory(reason) => {
                write!(f, "invalid destination directory: {reason}")
            }
            Self::Permissions(reason) => {
                write!(f, "destination directory permission failure: {reason}")
            }
            Self::Oversized { len, max } => {
                write!(
                    f,
                    "export payload exceeds size limit ({len} bytes > {max} bytes)"
                )
            }
            Self::Io(e) => write!(f, "export I/O error: {e}"),
            Self::Durability(e) => write!(
                f,
                "export published but directory durability was not confirmed: {e}"
            ),
        }
    }
}

impl std::error::Error for ExportError {}

/// Cleanup stays bound to the directory that received our temporary file.
struct PartialFileGuard<'a> {
    directory: &'a File,
    name: &'a OsStr,
    active: bool,
}

impl Drop for PartialFileGuard<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = unlinkat(self.directory, self.name, UnlinkatFlags::NoRemoveDir);
        }
    }
}

fn io_error(error: Errno) -> ExportError {
    ExportError::Io(std::io::Error::from_raw_os_error(error as i32))
}

/// Publish owner-only data without replacing any existing destination.
///
/// The full parent chain must contain real, trusted directories, with no
/// symlinks. Root-owned sticky directories are permitted. Size checks precede
/// filesystem effects. A successful return confirms file and directory flushes.
/// An error after publication can leave the destination in place: inspect it
/// before retrying; never remove or overwrite it as rollback.
pub fn export_private_atomic(
    target_path: &Path,
    content: &[u8],
    config: ExportConfig,
) -> Result<(), ExportError> {
    if content.len() > config.max_bytes {
        return Err(ExportError::Oversized {
            len: content.len(),
            max: config.max_bytes,
        });
    }
    if target_path.as_os_str().len() > 4096 {
        return Err(ExportError::InvalidDirectory(
            "destination path exceeds bounds".into(),
        ));
    }
    let name = target_path
        .file_name()
        .ok_or_else(|| ExportError::InvalidDirectory("invalid target filename".into()))?;
    let parent = target_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent_path = if parent.is_absolute() {
        parent.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(ExportError::Io)?
            .join(parent)
    };
    let directory = open_destination_directory(&parent_path)?;
    match fstatat(&directory, name, AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(_) => return Err(ExportError::TargetAlreadyExists(target_path.to_owned())),
        Err(Errno::ENOENT) => {}
        Err(error) => return Err(io_error(error)),
    }

    // Do not copy the target filename: even a valid NAME_MAX-length target
    // needs a short temporary name. Exclusive creation is the collision guard.
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = format!(
        ".sr-partial-{}-{nanos}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let temporary = OsStr::new(&temporary);
    let fd = openat(
        &directory,
        temporary,
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::from_bits_truncate(0o600),
    )
    .map_err(io_error)?;
    let mut guard = PartialFileGuard {
        directory: &directory,
        name: temporary,
        active: true,
    };
    // Apply exact permissions to our held file, never to a target pathname
    // that another publisher could replace. This also handles restrictive umask.
    fchmod(&fd, Mode::from_bits_truncate(0o600)).map_err(io_error)?;
    let mut file = File::from(fd);
    file.write_all(content).map_err(ExportError::Io)?;
    file.sync_all().map_err(ExportError::Io)?;
    revalidate_directory(&directory, &parent_path)?;
    atomic_no_clobber_publish(&directory, temporary, name, target_path)?;
    guard.active = false;
    sync_directory(&directory)?;
    revalidate_directory(&directory, &parent_path)?;
    Ok(())
}

fn validate_directory(stat: &FileStat, leaf: bool) -> Result<(), ExportError> {
    if SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT != SFlag::S_IFDIR {
        return Err(ExportError::InvalidDirectory(
            "destination component is not a directory".into(),
        ));
    }
    let uid = nix::unistd::geteuid().as_raw();
    let root_sticky = stat.st_uid == 0 && stat.st_mode & 0o1000 != 0;
    if (stat.st_uid != uid && !(stat.st_uid == 0 && (!leaf || root_sticky)))
        || (stat.st_mode & 0o022 != 0 && !root_sticky)
    {
        return Err(ExportError::Permissions(
            "destination component is not owner-controlled".into(),
        ));
    }
    Ok(())
}

fn open_destination_directory(path: &Path) -> Result<File, ExportError> {
    if !path.is_absolute() || path.as_os_str().len() > 4096 {
        return Err(ExportError::InvalidDirectory(
            "destination path exceeds bounds".into(),
        ));
    }
    let components: Vec<_> = path.components().take(129).collect();
    if components.len() > 128 {
        return Err(ExportError::InvalidDirectory(
            "destination path exceeds bounds".into(),
        ));
    }
    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let mut handle = open(Path::new("/"), flags, Mode::empty()).map_err(directory_open_error)?;
    validate_directory(&fstat(&handle).map_err(io_error)?, components.len() == 1)?;
    for (index, component) in components.iter().enumerate().skip(1) {
        let name = match component {
            Component::Normal(name) => *name,
            Component::ParentDir => OsStr::new(".."),
            Component::CurDir => continue,
            _ => {
                return Err(ExportError::InvalidDirectory(
                    "invalid destination component".into(),
                ));
            }
        };
        handle = openat(&handle, name, flags, Mode::empty()).map_err(directory_open_error)?;
        validate_directory(
            &fstat(&handle).map_err(io_error)?,
            index + 1 == components.len(),
        )?;
    }
    Ok(handle.into())
}

fn directory_open_error(error: Errno) -> ExportError {
    match error {
        Errno::ENOENT | Errno::ENOTDIR | Errno::ELOOP => ExportError::InvalidDirectory(
            "destination directory is missing, symlinked, or not a directory".into(),
        ),
        _ => io_error(error),
    }
}

fn revalidate_directory(directory: &File, path: &Path) -> Result<(), ExportError> {
    let current = open_destination_directory(path)?;
    let before = fstat(directory).map_err(io_error)?;
    let after = fstat(&current).map_err(io_error)?;
    if (before.st_dev, before.st_ino) != (after.st_dev, after.st_ino) {
        return Err(ExportError::InvalidDirectory(
            "destination changed during export".into(),
        ));
    }
    Ok(())
}

fn sync_directory(directory: &File) -> Result<(), ExportError> {
    directory.sync_all().map_err(ExportError::Durability)
}

fn atomic_no_clobber_publish(
    directory: &File,
    from: &OsStr,
    to: &OsStr,
    target_path: &Path,
) -> Result<(), ExportError> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        use nix::fcntl::{RenameFlags, renameat2};
        match renameat2(
            directory,
            from,
            directory,
            to,
            RenameFlags::RENAME_NOREPLACE,
        ) {
            Ok(()) => return Ok(()),
            Err(Errno::EEXIST) => {
                return Err(ExportError::TargetAlreadyExists(target_path.to_owned()));
            }
            Err(Errno::EINVAL | Errno::ENOSYS) => {}
            Err(error) => return Err(io_error(error)),
        }
    }
    link_no_clobber_publish(directory, from, to, target_path)
}

fn link_no_clobber_publish(
    directory: &File,
    from: &OsStr,
    to: &OsStr,
    target_path: &Path,
) -> Result<(), ExportError> {
    match linkat(directory, from, directory, to, AtFlags::empty()) {
        Ok(()) => unlinkat(directory, from, UnlinkatFlags::NoRemoveDir).map_err(io_error),
        Err(Errno::EEXIST) => Err(ExportError::TargetAlreadyExists(target_path.to_owned())),
        Err(error) => Err(io_error(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_flush_propagates_real_descriptor_errors() {
        let valid = File::open("/tmp").unwrap();
        assert!(sync_directory(&valid).is_ok());
        // A real pipe cannot be fsynced. Exercise the production flush boundary
        // without replacing the syscall or injecting a pretend filesystem.
        let (_read, write) = nix::unistd::pipe().unwrap();
        let invalid = File::from(write);
        assert!(matches!(
            sync_directory(&invalid),
            Err(ExportError::Durability(_))
        ));
    }

    fn directory_tree() -> PathBuf {
        use std::os::unix::fs::DirBuilderExt;
        let path = Path::new("/tmp").join(format!(
            "sr-export-descriptors-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        path
    }

    #[test]
    fn held_parent_prevents_redirected_publication_and_cleanup() {
        use std::os::unix::fs::{DirBuilderExt, symlink};
        let root = directory_tree();
        let parent = root.join("parent");
        let moved = root.join("moved");
        let outside = root.join("outside");
        for path in [&parent, &outside] {
            std::fs::DirBuilder::new().mode(0o700).create(path).unwrap();
        }
        let directory = open_destination_directory(&parent).unwrap();
        std::fs::write(parent.join("temporary"), b"authorized content").unwrap();
        std::fs::write(parent.join("cleanup"), b"owned partial").unwrap();
        std::fs::write(outside.join("cleanup"), b"unrelated file").unwrap();
        // Simulate replacement between validation and publication, using real
        // filesystem operations and the actual production publication helper.
        std::fs::rename(&parent, &moved).unwrap();
        symlink(&outside, &parent).unwrap();
        atomic_no_clobber_publish(
            &directory,
            OsStr::new("temporary"),
            OsStr::new("result"),
            &parent.join("result"),
        )
        .unwrap();
        assert_eq!(
            std::fs::read(moved.join("result")).unwrap(),
            b"authorized content"
        );
        assert!(!outside.join("result").exists());
        assert!(revalidate_directory(&directory, &parent).is_err());
        drop(PartialFileGuard {
            directory: &directory,
            name: OsStr::new("cleanup"),
            active: true,
        });
        assert!(!moved.join("cleanup").exists());
        assert_eq!(
            std::fs::read(outside.join("cleanup")).unwrap(),
            b"unrelated file"
        );
    }

    #[test]
    fn real_hard_link_fallback_preserves_existing_targets() {
        let root = directory_tree();
        let directory = open_destination_directory(&root).unwrap();
        std::fs::write(root.join("first"), b"winner").unwrap();
        link_no_clobber_publish(
            &directory,
            OsStr::new("first"),
            OsStr::new("result"),
            &root.join("result"),
        )
        .unwrap();
        assert!(!root.join("first").exists());
        assert_eq!(std::fs::read(root.join("result")).unwrap(), b"winner");
        std::fs::write(root.join("second"), b"loser").unwrap();
        assert!(matches!(
            link_no_clobber_publish(
                &directory,
                OsStr::new("second"),
                OsStr::new("result"),
                &root.join("result")
            ),
            Err(ExportError::TargetAlreadyExists(_))
        ));
        assert_eq!(std::fs::read(root.join("result")).unwrap(), b"winner");
        assert_eq!(std::fs::read(root.join("second")).unwrap(), b"loser");
    }
}
