//! Descriptor-based authorized reads of bounded regular files.
//!
//! Every component is opened relative to an authorized root's directory
//! descriptor with `O_NOFOLLOW`, so a path that is swapped between the check
//! and the open cannot redirect the read: the object whose type is verified is
//! the object that is read. Symlinks are followed only into explicitly
//! authorized roots, under a hop bound that terminates cycles. Content hashes
//! come from exactly the bytes returned, never from size or modification time.
//!
//! Discovery walks, frontmatter parsing and roster revalidation are separate
//! boundaries; this module only opens, validates and reads one file. Callers
//! run these blocking leaves under their own deadline admission.

use crate::identity::ContentHash;
use crate::limits::ResourceLimit;
use crate::privacy::{TrustedAbsoluteRoot, WorkspaceRelativeRoot};
use crate::roster::LocalPath;
use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag, open, openat, readlinkat};
use nix::sys::stat::{FileStat, Mode, SFlag, fstat, fstatat};
use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Component, Path, PathBuf};

/// Symlink hops permitted while resolving one request; cycles end here.
pub const MAX_SYMLINK_HOPS: u32 = 16;
/// Total component openings permitted while resolving one request.
pub const MAX_RESOLUTION_STEPS: usize = 512;
/// Bound for one symlink target read from the filesystem.
pub const MAX_LINK_TARGET_BYTES: usize = 4096;

/// The type of an object that was opened, for diagnostics without paths.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileKind {
    Regular,
    Directory,
    Symlink,
    Fifo,
    Socket,
    CharacterDevice,
    BlockDevice,
    Unknown,
}

impl FileKind {
    fn of(stat: &FileStat) -> Self {
        let bits = SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT;
        match bits {
            SFlag::S_IFREG => Self::Regular,
            SFlag::S_IFDIR => Self::Directory,
            SFlag::S_IFLNK => Self::Symlink,
            SFlag::S_IFIFO => Self::Fifo,
            SFlag::S_IFSOCK => Self::Socket,
            SFlag::S_IFCHR => Self::CharacterDevice,
            SFlag::S_IFBLK => Self::BlockDevice,
            _ => Self::Unknown,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Regular => "regular file",
            Self::Directory => "directory",
            Self::Symlink => "symbolic link",
            Self::Fifo => "FIFO",
            Self::Socket => "socket",
            Self::CharacterDevice => "character device",
            Self::BlockDevice => "block device",
            Self::Unknown => "unsupported file type",
        }
    }
}

/// Filesystem identity of an opened object. Two names for one file (a hard
/// alias) share it, so callers can deduplicate without comparing paths.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct FileIdentity {
    device: u64,
    inode: u64,
}

impl FileIdentity {
    // `dev_t`/`ino_t` widths differ by platform: the casts are identity on
    // Linux and widening on macOS.
    #[allow(clippy::unnecessary_cast)]
    fn of(stat: &FileStat) -> Self {
        Self {
            device: stat.st_dev as u64,
            inode: stat.st_ino as u64,
        }
    }

    /// Build an identity from a caller's own stat of the same object, so
    /// enumeration and reads can be compared without a second open.
    pub const fn new(device: u64, inode: u64) -> Self {
        Self { device, inode }
    }

    pub const fn device(self) -> u64 {
        self.device
    }

    pub const fn inode(self) -> u64 {
        self.inode
    }
}

/// Failures are reported without paths, link targets or file content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadError {
    InvalidRelativePath,
    TooManyComponents,
    NotFound,
    PermissionDenied,
    NotADirectory,
    NotRegularFile(FileKind),
    EscapesAuthorizedRoots,
    SymlinkLoop,
    LinkTargetTooLong,
    TooLarge { limit: usize },
    RootUnavailable,
    Io,
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRelativePath => f.write_str("request is not a valid relative path"),
            Self::TooManyComponents => f.write_str("path resolution exceeded its component bound"),
            Self::NotFound => f.write_str("no such file under the authorized root"),
            Self::PermissionDenied => f.write_str("permission denied under the authorized root"),
            Self::NotADirectory => f.write_str("a path component is not a directory"),
            Self::NotRegularFile(kind) => {
                write!(
                    f,
                    "refusing to read a {}; only regular files are read",
                    kind.as_str()
                )
            }
            Self::EscapesAuthorizedRoots => f.write_str("path escapes every authorized root"),
            Self::SymlinkLoop => f.write_str("symbolic links exceeded the hop bound"),
            Self::LinkTargetTooLong => f.write_str("symbolic link target exceeds its byte bound"),
            Self::TooLarge { limit } => write!(f, "file exceeds its {limit} byte bound"),
            Self::RootUnavailable => f.write_str("authorized root is unavailable"),
            Self::Io => f.write_str("filesystem read failed"),
        }
    }
}

impl std::error::Error for ReadError {}

fn map_errno(errno: Errno) -> ReadError {
    match errno {
        Errno::ENOENT => ReadError::NotFound,
        Errno::EACCES | Errno::EPERM => ReadError::PermissionDenied,
        Errno::ENOTDIR => ReadError::NotADirectory,
        Errno::ELOOP | Errno::EMLINK => ReadError::SymlinkLoop,
        Errno::ENAMETOOLONG => ReadError::InvalidRelativePath,
        Errno::EINVAL => ReadError::InvalidRelativePath,
        _ => ReadError::Io,
    }
}

/// A directory descriptor that anchors every read. Holding the descriptor binds
/// later operations to this exact directory, even if its name is replaced.
#[derive(Debug)]
pub struct AuthorizedRoot {
    directory: OwnedFd,
    absolute: PathBuf,
    identity: FileIdentity,
}

impl AuthorizedRoot {
    /// Open a trusted absolute root. Symlinks inside the granted path itself
    /// are resolved by the kernel once, here; later reads never follow a link
    /// out of the opened directory.
    pub fn open_absolute(path: &Path) -> Result<Self, ReadError> {
        if !path.is_absolute() {
            return Err(ReadError::InvalidRelativePath);
        }
        let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC;
        let directory = open(path, flags, Mode::empty()).map_err(map_errno)?;
        let stat = fstat(&directory).map_err(map_errno)?;
        if FileKind::of(&stat) != FileKind::Directory {
            return Err(ReadError::NotADirectory);
        }
        Ok(Self {
            directory,
            absolute: path.to_path_buf(),
            identity: FileIdentity::of(&stat),
        })
    }

    /// Open a root granted by trusted user configuration.
    pub fn open_trusted(root: &TrustedAbsoluteRoot) -> Result<Self, ReadError> {
        Self::open_absolute(root.as_path())
    }

    /// Open a workspace-relative root without leaving this root. Components are
    /// opened one at a time, so a replaced directory cannot redirect the walk.
    pub fn open_workspace_relative(
        &self,
        relative: &WorkspaceRelativeRoot,
    ) -> Result<Self, ReadError> {
        let roots = AuthorizedRoots::single(self.try_clone()?);
        let components = split_components(relative.as_path())?;
        if components.is_empty() {
            return self.try_clone();
        }
        let resolved = Walk::new(&roots, 0)?.resolve(components, Want::Directory)?;
        let stat = fstat(&resolved.descriptor).map_err(map_errno)?;
        Ok(Self {
            directory: resolved.descriptor,
            absolute: resolved.absolute,
            identity: FileIdentity::of(&stat),
        })
    }

    pub fn try_clone(&self) -> Result<Self, ReadError> {
        Ok(Self {
            directory: self
                .directory
                .try_clone()
                .map_err(|_| ReadError::RootUnavailable)?,
            absolute: self.absolute.clone(),
            identity: self.identity,
        })
    }

    pub fn absolute_path(&self) -> &Path {
        &self.absolute
    }

    pub const fn identity(&self) -> FileIdentity {
        self.identity
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.directory.as_fd()
    }
}

/// The complete set of roots a symlink may resolve into.
#[derive(Debug)]
pub struct AuthorizedRoots {
    roots: Vec<AuthorizedRoot>,
}

impl AuthorizedRoots {
    pub fn new(roots: Vec<AuthorizedRoot>) -> Self {
        Self { roots }
    }

    pub fn single(root: AuthorizedRoot) -> Self {
        Self { roots: vec![root] }
    }

    pub fn len(&self) -> usize {
        self.roots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&AuthorizedRoot> {
        self.roots.get(index)
    }

    /// Longest-prefix match of an absolute path against the authorized roots.
    fn locate(&self, absolute: &Path) -> Option<(usize, Vec<OsString>)> {
        let mut best: Option<(usize, Vec<OsString>)> = None;
        for (index, root) in self.roots.iter().enumerate() {
            let Ok(rest) = absolute.strip_prefix(&root.absolute) else {
                continue;
            };
            let depth = root.absolute.components().count();
            if best.as_ref().is_none_or(|(current, _)| {
                depth > self.roots[*current].absolute.components().count()
            }) {
                let components: Vec<OsString> = rest
                    .components()
                    .map(|component| component.as_os_str().to_os_string())
                    .collect();
                best = Some((index, components));
            }
        }
        best
    }

    /// Read a bounded regular file below `root_index`.
    pub fn read_bounded(
        &self,
        root_index: usize,
        relative: &Path,
        limit: ResourceLimit,
    ) -> Result<BoundedRead, ReadError> {
        let components = split_components(relative)?;
        if components.is_empty() {
            return Err(ReadError::InvalidRelativePath);
        }
        let resolved = Walk::new(self, root_index)?.resolve(components, Want::File)?;
        read_opened(resolved, limit)
    }

    /// Open an authorized regular file for a caller that performs bounded tail
    /// reads. The returned descriptor is the object checked by the safe walk;
    /// callers must not reopen its path after authorization.
    pub(crate) fn open_absolute_file(&self, absolute: &Path) -> Result<File, ReadError> {
        if !absolute.is_absolute() {
            return Err(ReadError::InvalidRelativePath);
        }
        let (index, components) = self
            .locate(absolute)
            .ok_or(ReadError::EscapesAuthorizedRoots)?;
        if components.is_empty() {
            return Err(ReadError::NotRegularFile(FileKind::Directory));
        }
        let mut walk = Walk::new(self, index)?;
        walk.require_existing_link_targets = true;
        let resolved = walk.resolve(components, Want::File)?;
        Ok(File::from(resolved.descriptor))
    }

    /// Read a bounded regular file named by an absolute path that must lie
    /// within an authorized root.
    pub fn read_absolute(
        &self,
        absolute: &Path,
        limit: ResourceLimit,
    ) -> Result<BoundedRead, ReadError> {
        if !absolute.is_absolute() {
            return Err(ReadError::InvalidRelativePath);
        }
        let (index, components) = self
            .locate(absolute)
            .ok_or(ReadError::EscapesAuthorizedRoots)?;
        if components.is_empty() {
            return Err(ReadError::NotRegularFile(FileKind::Directory));
        }
        let resolved = Walk::new(self, index)?.resolve(components, Want::File)?;
        read_opened(resolved, limit)
    }
}

/// Bytes read from one authorized file, with the hash of those exact bytes.
#[derive(Clone, Eq, PartialEq)]
pub struct BoundedRead {
    bytes: Vec<u8>,
    content_hash: ContentHash,
    identity: FileIdentity,
    path: LocalPath,
}

impl BoundedRead {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Derived from `bytes`, never from metadata, so a same-size same-mtime
    /// rewrite produces a different hash.
    pub fn content_hash(&self) -> &ContentHash {
        &self.content_hash
    }

    pub const fn identity(&self) -> FileIdentity {
        self.identity
    }

    /// Native path bytes of the object actually opened; kept local.
    pub fn path(&self) -> &LocalPath {
        &self.path
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl fmt::Debug for BoundedRead {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoundedRead")
            .field("bytes", &format_args!("<{} bytes>", self.bytes.len()))
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Want {
    File,
    Directory,
}

struct Resolved {
    descriptor: OwnedFd,
    stat: FileStat,
    absolute: PathBuf,
}

fn split_components(path: &Path) -> Result<Vec<OsString>, ReadError> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => components.push(part.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ReadError::InvalidRelativePath);
            }
        }
    }
    Ok(components)
}

/// A descriptor stack walk. `..` inside a symlink target pops the stack and can
/// never rise above the root the walk is currently anchored to.
struct Walk<'a> {
    roots: &'a AuthorizedRoots,
    root: usize,
    stack: Vec<OwnedFd>,
    names: Vec<OsString>,
    hops: u32,
    require_existing_link_targets: bool,
    steps: usize,
}

impl<'a> Walk<'a> {
    fn new(roots: &'a AuthorizedRoots, root: usize) -> Result<Self, ReadError> {
        let base = roots.get(root).ok_or(ReadError::RootUnavailable)?;
        Ok(Self {
            roots,
            root,
            stack: vec![
                base.directory
                    .try_clone()
                    .map_err(|_| ReadError::RootUnavailable)?,
            ],
            names: Vec::new(),
            hops: 0,
            require_existing_link_targets: false,
            steps: 0,
        })
    }

    fn top(&self) -> Result<&OwnedFd, ReadError> {
        self.stack.last().ok_or(ReadError::EscapesAuthorizedRoots)
    }

    fn absolute(&self, last: &OsStr) -> PathBuf {
        let mut path = self.roots.roots[self.root].absolute.clone();
        for name in &self.names {
            path.push(name);
        }
        path.push(last);
        path
    }

    fn resolve(mut self, components: Vec<OsString>, want: Want) -> Result<Resolved, ReadError> {
        let mut queue: VecDeque<OsString> = components.into();
        while let Some(name) = queue.pop_front() {
            self.steps += 1;
            if self.steps > MAX_RESOLUTION_STEPS {
                return Err(ReadError::TooManyComponents);
            }
            if name.as_os_str() == OsStr::new(".") {
                continue;
            }
            if name.as_os_str() == OsStr::new("..") {
                if self.names.pop().is_none() {
                    return Err(ReadError::EscapesAuthorizedRoots);
                }
                self.stack.pop();
                continue;
            }
            let last = queue.is_empty();
            let mut flags = OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW;
            if last {
                // Opening a FIFO must not block waiting for a writer.
                flags |= OFlag::O_NONBLOCK;
            } else {
                flags |= OFlag::O_DIRECTORY;
            }
            match openat(self.top()?, name.as_os_str(), flags, Mode::empty()) {
                Ok(descriptor) => {
                    let stat = fstat(&descriptor).map_err(map_errno)?;
                    let kind = FileKind::of(&stat);
                    if last {
                        let acceptable = match want {
                            Want::File => kind == FileKind::Regular,
                            Want::Directory => kind == FileKind::Directory,
                        };
                        if !acceptable {
                            return Err(ReadError::NotRegularFile(kind));
                        }
                        let absolute = self.absolute(name.as_os_str());
                        return Ok(Resolved {
                            descriptor,
                            stat,
                            absolute,
                        });
                    }
                    self.stack.push(descriptor);
                    self.names.push(name);
                }
                Err(errno) => {
                    // O_NOFOLLOW reports a symlink as ELOOP (ENOTDIR on some
                    // platforms when O_DIRECTORY is also set). Confirm the type
                    // from the link itself before following it.
                    let is_link = matches!(errno, Errno::ELOOP | Errno::EMLINK | Errno::ENOTDIR)
                        && matches!(
                            fstatat(self.top()?, name.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW),
                            Ok(stat) if FileKind::of(&stat) == FileKind::Symlink
                        );
                    if !is_link {
                        if errno == Errno::ENOENT
                            && self.hops > 0
                            && self.require_existing_link_targets
                        {
                            return Err(ReadError::InvalidRelativePath);
                        }
                        return Err(map_errno(errno));
                    }
                    let target = self.follow(name.as_os_str())?;
                    for component in target.into_iter().rev() {
                        queue.push_front(component);
                    }
                }
            }
        }
        Err(ReadError::InvalidRelativePath)
    }

    /// Read one link target and re-anchor the walk. An absolute target must
    /// land inside an authorized root; a relative target stays in this walk.
    fn follow(&mut self, name: &OsStr) -> Result<Vec<OsString>, ReadError> {
        self.hops += 1;
        if self.hops > MAX_SYMLINK_HOPS {
            return Err(ReadError::SymlinkLoop);
        }
        let target = readlinkat(self.top()?, name).map_err(map_errno)?;
        if target.as_encoded_bytes().len() > MAX_LINK_TARGET_BYTES {
            return Err(ReadError::LinkTargetTooLong);
        }
        let target = PathBuf::from(target);
        if !target.is_absolute() {
            return relative_components(&target);
        }
        let (index, components) = self
            .roots
            .locate(&target)
            .ok_or(ReadError::EscapesAuthorizedRoots)?;
        let base = self.roots.get(index).ok_or(ReadError::RootUnavailable)?;
        self.root = index;
        self.stack = vec![
            base.directory
                .try_clone()
                .map_err(|_| ReadError::RootUnavailable)?,
        ];
        self.names.clear();
        Ok(components)
    }
}

fn relative_components(target: &Path) -> Result<Vec<OsString>, ReadError> {
    let mut components = Vec::new();
    for component in target.components() {
        match component {
            Component::Normal(part) => components.push(part.to_os_string()),
            Component::ParentDir => components.push(OsString::from("..")),
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => {
                return Err(ReadError::EscapesAuthorizedRoots);
            }
        }
    }
    Ok(components)
}

fn read_opened(resolved: Resolved, limit: ResourceLimit) -> Result<BoundedRead, ReadError> {
    let Resolved {
        descriptor,
        stat,
        absolute,
    } = resolved;
    let cap = limit.cap_plus_one().map_err(|_| ReadError::Io)?;
    let mut file = File::from(descriptor);
    let mut bytes = Vec::with_capacity(cap.min(64 * 1024));
    file.by_ref()
        .take(cap as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| ReadError::Io)?;
    if bytes.len() > limit.max() {
        return Err(ReadError::TooLarge { limit: limit.max() });
    }
    Ok(BoundedRead {
        content_hash: ContentHash::from_bytes(&bytes),
        bytes,
        identity: FileIdentity::of(&stat),
        path: LocalPath::new(absolute),
    })
}
