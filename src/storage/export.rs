//! Bounded owner-only export publication with atomic no-clobber guarantees.
//!
//! # Embedded Invariants (sr-roadmap-l1i.3.14 / p2_atomic_export)
//! - Atomicity: Uses `renameat2(..., RENAME_NOREPLACE)` on Linux or atomic `hard_link`
//!   fallback to guarantee that an existing target is NEVER clobbered, even in the
//!   presence of filesystem races.
//! - Permissions: Files are created exclusively owner-only (`0o600` / `-rw-------`).
//! - Directory Validation: The destination directory must exist, be a directory, and
//!   satisfy safe ownership/permissions (owner-controlled or sticky `/tmp`).
//! - Durability: File contents are flushed (`sync_all`) before publication, and the
//!   parent directory is flushed after publication.
//! - Identifiable Partial Files: Temporary partial files use the pattern
//!   `.<target>.sr-partial-<pid>-<nanos>` and are deleted automatically on error.
//! - Bounded: Strictly enforces size limits before and during writes.

use crate::output::{CliExit, ErrorKind};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

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
        }
    }
}

impl std::error::Error for ExportError {}

/// Scope guard ensuring that partial/uncompleted temporary files are cleaned up on failure.
struct PartialFileGuard<'a> {
    path: &'a Path,
    active: bool,
}

impl<'a> PartialFileGuard<'a> {
    fn new(path: &'a Path) -> Self {
        Self { path, active: true }
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for PartialFileGuard<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = std::fs::remove_file(self.path);
        }
    }
}

/// Atomically publish private data to `target_path` without clobbering existing files.
///
/// # Guarantees
/// - If `target_path` exists (as regular file, directory, or symlink), returns `Err(ExportError::TargetAlreadyExists)`.
/// - If a file is raced into place concurrently, the atomic publication operation detects
///   the collision and safely aborts without overwriting.
/// - The published file has permissions `0o600` (owner read/write only).
/// - File data is flushed (`sync_all`) before publication, and the parent directory is flushed after.
/// - Unfinished partial files are cleaned up on failure.
pub fn export_private_atomic(
    target_path: &Path,
    content: &[u8],
    config: ExportConfig,
) -> Result<(), ExportError> {
    // 1. Bound size check before any filesystem operations
    if content.len() > config.max_bytes {
        return Err(ExportError::Oversized {
            len: content.len(),
            max: config.max_bytes,
        });
    }

    // 2. Validate destination directory
    let parent = target_path
        .parent()
        .ok_or_else(|| ExportError::InvalidDirectory("path has no parent directory".into()))?;

    // If parent is empty string, use current directory
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };

    validate_destination_directory(parent)?;

    // 3. Preflight check for existing target or symlink
    // Using symlink_metadata prevents following dangling/broken symlinks
    if std::fs::symlink_metadata(target_path).is_ok() {
        return Err(ExportError::TargetAlreadyExists(target_path.to_path_buf()));
    }

    // 4. Create identifiable private partial file in the same directory (same filesystem)
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    let target_file_name = target_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| ExportError::InvalidDirectory("invalid target filename".into()))?;

    let tmp_name = format!(".{target_file_name}.sr-partial-{pid}-{nanos}");
    let tmp_path = parent.join(&tmp_name);

    // Open exclusively with mode 0o600 (owner-only)
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp_path)
        .map_err(ExportError::Io)?;

    let mut guard = PartialFileGuard::new(&tmp_path);

    // 5. Write content and flush to disk
    file.write_all(content).map_err(ExportError::Io)?;
    file.sync_all().map_err(ExportError::Io)?;
    drop(file);

    // 6. Atomic no-clobber rename into destination
    atomic_no_clobber_publish(&tmp_path, target_path)?;
    guard.disarm();

    // 7. Ensure permissions are strictly 0o600
    if let Ok(meta) = std::fs::metadata(target_path)
        && meta.mode() & 0o777 != 0o600
    {
        let _ = std::fs::set_permissions(target_path, std::fs::Permissions::from_mode(0o600));
    }

    // 8. Flush parent directory to persist directory entry durability
    if let Ok(dir_file) = File::open(parent) {
        let _ = dir_file.sync_all();
    }

    Ok(())
}

fn validate_destination_directory(dir: &Path) -> Result<(), ExportError> {
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ExportError::InvalidDirectory(
                "directory does not exist".into(),
            ));
        }
        Err(e) => return Err(ExportError::Io(e)),
    };

    if !meta.is_dir() {
        return Err(ExportError::InvalidDirectory(
            "destination parent path is not a directory".into(),
        ));
    }

    // Ownership and permission check on Unix
    let uid = nix::unistd::geteuid().as_raw();
    let dir_uid = meta.uid();
    let mode = meta.mode();

    let is_owner = dir_uid == uid;
    let is_root_sticky = dir_uid == 0 && (mode & 0o1000 != 0);

    if !is_owner && !is_root_sticky {
        return Err(ExportError::Permissions(
            "destination directory is not owned by current user or root sticky".into(),
        ));
    }

    // Reject world/group-writable directory unless root-owned sticky (/tmp)
    if mode & 0o022 != 0 && !is_root_sticky {
        return Err(ExportError::Permissions(
            "destination directory has unsafe group/world write permissions".into(),
        ));
    }

    Ok(())
}

fn atomic_no_clobber_publish(from: &Path, to: &Path) -> Result<(), ExportError> {
    // Fast path on Linux GNU: renameat2 with RENAME_NOREPLACE
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        use nix::fcntl::{RenameFlags, renameat2};
        use std::os::fd::AsFd;

        let parent = to
            .parent()
            .map(|p| {
                if p.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    p
                }
            })
            .unwrap_or_else(|| Path::new("."));

        if let Ok(dir_file) = File::open(parent) {
            let from_name = from.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let to_name = to.file_name().and_then(|n| n.to_str()).unwrap_or("");

            if !from_name.is_empty() && !to_name.is_empty() {
                match renameat2(
                    dir_file.as_fd(),
                    from_name,
                    dir_file.as_fd(),
                    to_name,
                    RenameFlags::RENAME_NOREPLACE,
                ) {
                    Ok(()) => return Ok(()),
                    Err(nix::errno::Errno::EEXIST) => {
                        return Err(ExportError::TargetAlreadyExists(to.to_path_buf()));
                    }
                    Err(nix::errno::Errno::EINVAL | nix::errno::Errno::ENOSYS) => {
                        // Fall through to hard link fallback
                    }
                    Err(e) => {
                        return Err(ExportError::Io(std::io::Error::from_raw_os_error(e as i32)));
                    }
                }
            }
        }
    }

    // Portable atomic no-clobber fallback: hard_link + remove_file
    match std::fs::hard_link(from, to) {
        Ok(()) => {
            let _ = std::fs::remove_file(from);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(ExportError::TargetAlreadyExists(to.to_path_buf()))
        }
        Err(e) => Err(ExportError::Io(e)),
    }
}
