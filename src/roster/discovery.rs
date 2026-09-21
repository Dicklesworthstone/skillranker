//! Bounded enumeration of the roots a selected harness can load from.
//!
//! Discovery answers "which skill files could this harness load right now",
//! not "which skill files exist on this machine". Only roots the caller's
//! adapter declares are visited, each through its own authorized directory
//! descriptor, so one harness's roots are never unioned with another's.
//!
//! A missing optional root is normal. An unreadable root, an unsupported
//! source, a skipped entry or an exhausted bound makes the result partial, and
//! a partial result can never support a claim that no skill exists. Parsing
//! (`sr-roadmap-l1i.3.3`), identity and collisions (`.3.5`) and revalidation
//! (`.3.11`) are separate boundaries.

use crate::authorized_read::{AuthorizedRoot, FileIdentity, ReadError};
use crate::identity::{HarnessId, IdentityError, SourceId};
use crate::limits::{DISCOVERY_FILES, DISCOVERY_PARSED_BYTES, ResourceLimit, SKILL_FILE_BYTES};
use crate::roster::{LocalPath, Visibility};
use nix::dir::{Dir, Type};
use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag, openat};
use nix::sys::stat::{Mode, SFlag, fstatat};
use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Directory levels visited below a root before descent stops.
pub const MAX_ROOT_DEPTH: usize = 8;
/// The documented Claude Code skill file name.
pub const CLAUDE_SKILL_FILE: &str = "SKILL.md";
/// Documented Claude Code roots, in precedence order.
pub const CLAUDE_PROJECT_SOURCE: &str = "claude_code.project";
pub const CLAUDE_USER_SOURCE: &str = "claude_code.user";
/// Claude sources this build does not enumerate; declaring them keeps the
/// roster honestly partial instead of implying a complete inventory.
pub const CLAUDE_UNENUMERATED_SOURCES: &[&str] = &["claude_code.plugin", "claude_code.managed"];

// Claude's documented root precedence is enterprise > personal > project.
const CLAUDE_PROJECT_PRIORITY: i32 = 50;
const CLAUDE_USER_PRIORITY: i32 = 100;
const MAX_FILE_NAME_BYTES: usize = 255;

/// Which documented source a root represents.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum SourceKind {
    Project,
    User,
    Plugin,
    Managed,
    /// Explicitly configured root with no harness load contract.
    Generic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiscoveryError {
    InvalidSkillFileName,
    InvalidRootPath,
    Identity(IdentityError),
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSkillFileName => f.write_str("skill file name is not a single component"),
            Self::InvalidRootPath => f.write_str("root path is not usable"),
            Self::Identity(error) => write!(f, "invalid identity: {error}"),
        }
    }
}

impl std::error::Error for DiscoveryError {}

/// A root the selected adapter declares, before it is opened.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RootSpec {
    source: SourceId,
    kind: SourceKind,
    priority: i32,
    visibility: Visibility,
    skill_file: OsString,
}

impl RootSpec {
    pub fn new(
        source: SourceId,
        kind: SourceKind,
        priority: i32,
        visibility: Visibility,
        skill_file: &str,
    ) -> Result<Self, DiscoveryError> {
        let invalid = skill_file.is_empty()
            || skill_file.len() > MAX_FILE_NAME_BYTES
            || skill_file.contains('/')
            || skill_file.contains('\0')
            || skill_file == "."
            || skill_file == "..";
        if invalid {
            return Err(DiscoveryError::InvalidSkillFileName);
        }
        Ok(Self {
            source,
            kind,
            priority,
            visibility,
            skill_file: OsString::from(skill_file),
        })
    }

    pub fn source(&self) -> &SourceId {
        &self.source
    }
    pub const fn kind(&self) -> SourceKind {
        self.kind
    }
    pub const fn priority(&self) -> i32 {
        self.priority
    }
    pub fn visibility(&self) -> &Visibility {
        &self.visibility
    }
    pub fn skill_file(&self) -> &OsStr {
        &self.skill_file
    }
}

/// An opened root: the descriptor, not the path, anchors enumeration.
#[derive(Debug)]
pub struct PlannedRoot {
    spec: RootSpec,
    /// `None` when a configured root exists but could not be opened; the pass
    /// reports it and stays partial rather than silently skipping it.
    root: Option<AuthorizedRoot>,
    declared: PathBuf,
}

impl PlannedRoot {
    /// `Ok(None)` means the optional root is absent, which is normal.
    pub fn open(spec: RootSpec, path: &Path) -> Result<Option<Self>, DiscoveryError> {
        let declared = path.to_path_buf();
        match AuthorizedRoot::open_absolute(path) {
            Ok(root) => Ok(Some(Self {
                spec,
                root: Some(root),
                declared,
            })),
            Err(ReadError::NotFound) => Ok(None),
            Err(ReadError::InvalidRelativePath) => Err(DiscoveryError::InvalidRootPath),
            Err(_) => Ok(Some(Self {
                spec,
                root: None,
                declared,
            })),
        }
    }

    pub const fn spec(&self) -> &RootSpec {
        &self.spec
    }

    pub const fn root(&self) -> Option<&AuthorizedRoot> {
        self.root.as_ref()
    }

    /// The configured path, kept local for the caller's own diagnostics.
    pub fn declared_path(&self) -> &Path {
        &self.declared
    }
}

/// The declared plan for one harness. Building a plan for one harness never
/// adds another harness's roots.
#[derive(Debug)]
pub struct DiscoveryPlan {
    harness: HarnessId,
    roots: Vec<PlannedRoot>,
    missing: Vec<SourceId>,
    unenumerated: Vec<SourceId>,
}

impl DiscoveryPlan {
    pub fn new(harness: HarnessId) -> Self {
        Self {
            harness,
            roots: Vec::new(),
            missing: Vec::new(),
            unenumerated: Vec::new(),
        }
    }

    pub fn push_root(&mut self, root: PlannedRoot) {
        self.roots.push(root);
    }

    /// An optional root that does not exist. Normal, and not partial.
    pub fn note_missing(&mut self, source: SourceId) {
        self.missing.push(source);
    }

    /// A documented source this build does not enumerate. Always partial.
    pub fn note_unenumerated(&mut self, source: SourceId) {
        self.unenumerated.push(source);
    }

    pub const fn harness(&self) -> &HarnessId {
        &self.harness
    }
    pub fn roots(&self) -> &[PlannedRoot] {
        &self.roots
    }
    pub fn missing(&self) -> &[SourceId] {
        &self.missing
    }
    pub fn unenumerated(&self) -> &[SourceId] {
        &self.unenumerated
    }

    pub fn discover(&self) -> Discovery {
        self.discover_with(DiscoveryLimits::defaults())
    }

    pub fn discover_with(&self, limits: DiscoveryLimits) -> Discovery {
        self.discover_with_checkpoint(limits, || Ok::<(), std::convert::Infallible>(()))
            .unwrap_or_else(|never| match never {})
    }

    /// The bounded pass with caller-owned deadline/cancellation admission.
    /// Checks run before and after filesystem work and before returning even
    /// an empty result. A failed check returns its error, never a usable partial
    /// snapshot. Blocking syscalls are not forcibly interruptible.
    pub fn discover_with_checkpoint<E>(
        &self,
        limits: DiscoveryLimits,
        mut checkpoint: impl FnMut() -> Result<(), E>,
    ) -> Result<Discovery, E> {
        checkpoint()?;
        let mut discovery = Discovery::default();
        for source in &self.missing {
            checkpoint()?;
            discovery
                .diagnostics
                .push(Diagnostic::RootMissing(source.clone()));
        }
        for source in &self.unenumerated {
            checkpoint()?;
            discovery
                .diagnostics
                .push(Diagnostic::SourceNotEnumerated(source.clone()));
            discovery.partial = true;
        }
        discovery.walk_roots(&self.roots, limits, &mut checkpoint)?;
        checkpoint()?;
        discovery.candidates.sort_by(|left, right| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| left.source.as_str().cmp(right.source.as_str()))
                .then_with(|| left.relative.cmp(&right.relative))
        });
        discovery.rejected.sort_by(|left, right| {
            left.source
                .as_str()
                .cmp(right.source.as_str())
                .then_with(|| left.relative.as_path().cmp(right.relative.as_path()))
        });
        checkpoint()?;
        Ok(discovery)
    }
}

/// Ceilings applied to one discovery pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiscoveryLimits {
    entries: ResourceLimit,
    bytes: ResourceLimit,
    depth: usize,
}

impl DiscoveryLimits {
    pub const fn defaults() -> Self {
        Self {
            entries: DISCOVERY_FILES,
            bytes: DISCOVERY_PARSED_BYTES,
            depth: MAX_ROOT_DEPTH,
        }
    }

    pub const fn new(entries: ResourceLimit, bytes: ResourceLimit, depth: usize) -> Self {
        Self {
            entries,
            bytes,
            depth,
        }
    }

    pub const fn entries(self) -> ResourceLimit {
        self.entries
    }
    pub const fn bytes(self) -> ResourceLimit {
        self.bytes
    }
    pub const fn depth(self) -> usize {
        self.depth
    }
}

/// One skill file the harness could load, before parsing or identity work.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Candidate {
    source: SourceId,
    kind: SourceKind,
    priority: i32,
    visibility: Visibility,
    relative: PathBuf,
    path: LocalPath,
    identity: FileIdentity,
    size: u64,
    via_symlink: bool,
}

impl Candidate {
    pub fn source(&self) -> &SourceId {
        &self.source
    }
    pub const fn kind(&self) -> SourceKind {
        self.kind
    }
    pub const fn priority(&self) -> i32 {
        self.priority
    }
    pub fn visibility(&self) -> &Visibility {
        &self.visibility
    }
    /// Path below its root, for the authorized read that follows.
    pub fn relative(&self) -> &Path {
        &self.relative
    }
    pub fn path(&self) -> &LocalPath {
        &self.path
    }
    pub const fn identity(&self) -> FileIdentity {
        self.identity
    }
    pub const fn size(&self) -> u64 {
        self.size
    }
    /// The entry was a symbolic link; the authorized read still decides
    /// whether its target is inside an authorized root.
    pub const fn via_symlink(&self) -> bool {
        self.via_symlink
    }
}

/// A metadata rejection, not permission to read or invoke the entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateProblem {
    Oversized,
    NotRegularFile,
}

/// A named skill-file slot rejected before reserving any content-read bytes.
/// Retain its source and local relative path so resolution can withhold that
/// invocation name without either promoting a loser or suppressing other names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RejectedCandidate {
    source: SourceId,
    kind: SourceKind,
    relative: LocalPath,
    problem: CandidateProblem,
}

impl RejectedCandidate {
    pub fn source(&self) -> &SourceId {
        &self.source
    }
    pub const fn kind(&self) -> SourceKind {
        self.kind
    }
    /// Local-only; Debug redacts the path just like other local load targets.
    pub fn relative(&self) -> &Path {
        self.relative.as_path()
    }
    pub const fn problem(&self) -> CandidateProblem {
        self.problem
    }
}

/// Diagnostics name the source, never a path, entry name or content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Diagnostic {
    RootMissing(SourceId),
    RootUnreadable(SourceId),
    SourceNotEnumerated(SourceId),
    DirectoryUnreadable(SourceId),
    SymlinkedDirectorySkipped(SourceId),
    DepthLimitReached(SourceId),
    EntryUnreadable(SourceId),
    EntryLimitReached,
    ByteLimitReached,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RootMissing(source) => write!(f, "optional root absent for {}", source.as_str()),
            Self::RootUnreadable(source) => {
                write!(f, "configured root unreadable for {}", source.as_str())
            }
            Self::SourceNotEnumerated(source) => {
                write!(
                    f,
                    "source not enumerated by this build: {}",
                    source.as_str()
                )
            }
            Self::DirectoryUnreadable(source) => {
                write!(f, "directory unreadable under {}", source.as_str())
            }
            Self::SymlinkedDirectorySkipped(source) => {
                write!(
                    f,
                    "symbolic-link directory skipped under {}",
                    source.as_str()
                )
            }
            Self::DepthLimitReached(source) => {
                write!(f, "depth bound reached under {}", source.as_str())
            }
            Self::EntryUnreadable(source) => {
                write!(f, "entry unreadable under {}", source.as_str())
            }
            Self::EntryLimitReached => f.write_str("entry bound reached; enumeration stopped"),
            Self::ByteLimitReached => f.write_str("byte bound reached; enumeration stopped"),
        }
    }
}

/// The result of one bounded pass.
#[derive(Debug, Default)]
pub struct Discovery {
    candidates: Vec<Candidate>,
    rejected: Vec<RejectedCandidate>,
    diagnostics: Vec<Diagnostic>,
    entries_examined: usize,
    bytes_examined: u64,
    partial: bool,
    // Entry/byte ceilings apply to the entire pass, not one root at a time.
    stopped: bool,
}

impl Discovery {
    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }
    /// Known rejected slots. These consume the same entry allowance as other
    /// entries, but are never opened for content or charged to the byte budget.
    pub fn rejected_candidates(&self) -> &[RejectedCandidate] {
        &self.rejected
    }
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }
    pub const fn entries_examined(&self) -> usize {
        self.entries_examined
    }
    /// Declared bytes reserved for candidate reads, excluding metadata-only
    /// rejections. Discovery itself does not read skill contents.
    pub const fn bytes_examined(&self) -> u64 {
        self.bytes_examined
    }
    /// True when any source was skipped, truncated or unreadable. A partial
    /// result cannot support a global "no skill exists" claim.
    pub const fn is_partial(&self) -> bool {
        self.partial
    }

    fn note(&mut self, diagnostic: Diagnostic) {
        self.partial = true;
        self.diagnostics.push(diagnostic);
    }

    fn stop(&mut self, diagnostic: Diagnostic) {
        self.stopped = true;
        self.note(diagnostic);
    }

    fn reject(&mut self, planned: &PlannedRoot, relative: PathBuf, problem: CandidateProblem) {
        self.partial = true;
        self.rejected.push(RejectedCandidate {
            source: planned.spec.source.clone(),
            kind: planned.spec.kind,
            relative: LocalPath::new(relative),
            problem,
        });
    }

    // A failed readdir need not advance its stream. Abandon that directory
    // after one error rather than retrying forever outside the entry budget.
    fn next_entry<T>(&mut self, next: Option<Result<T, Errno>>, source: &SourceId) -> Option<T> {
        match next {
            Some(Ok(entry)) => Some(entry),
            Some(Err(_)) => {
                self.note(Diagnostic::EntryUnreadable(source.clone()));
                None
            }
            None => None,
        }
    }

    fn walk_roots<E>(
        &mut self,
        planned_roots: &[PlannedRoot],
        limits: DiscoveryLimits,
        checkpoint: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<(), E> {
        checkpoint()?;
        // One breadth-first queue for the entire plan. A project's nested
        // reference/script tree must not exhaust the shared budget before a
        // personal or configured root's shallower skill directories are seen.
        // Keep names, not open sibling directories, so descriptor use stays
        // constant beyond the roots the plan already pins.
        let mut queue = VecDeque::new();
        for planned in planned_roots {
            checkpoint()?;
            if let Some(root) = planned.root.as_ref() {
                queue.push_back((planned, root, PathBuf::new(), 0usize));
            } else {
                self.note(Diagnostic::RootUnreadable(planned.spec.source.clone()));
            }
        }
        while let Some((planned, root, relative, depth)) = queue.pop_front() {
            checkpoint()?;
            let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
            // A fresh stream relative to the pinned root avoids shared dup()
            // positions and stale directory indexes across repeated scans.
            let mut directory = openat(root.as_fd(), ".", flags, Mode::empty());
            checkpoint()?;
            for component in relative.components() {
                checkpoint()?;
                directory = directory.and_then(|parent| {
                    openat(&parent, component.as_os_str(), flags, Mode::empty())
                });
                checkpoint()?;
            }
            let directory = match directory {
                Ok(directory) => directory,
                Err(Errno::ELOOP | Errno::ENOTDIR) => {
                    self.note(Diagnostic::SymlinkedDirectorySkipped(
                        planned.spec.source.clone(),
                    ));
                    continue;
                }
                Err(_) => {
                    self.note(Diagnostic::DirectoryUnreadable(planned.spec.source.clone()));
                    continue;
                }
            };
            // Keep this directory pinned for candidate metadata as well as
            // enumeration. Re-resolving its path could follow a replaced parent.
            checkpoint()?;
            let listing = directory
                .as_fd()
                .try_clone_to_owned()
                .ok()
                .and_then(|fd| Dir::from_fd(fd).ok());
            checkpoint()?;
            let Some(mut listing) = listing else {
                self.note(Diagnostic::DirectoryUnreadable(planned.spec.source.clone()));
                continue;
            };
            let mut iterator = listing.iter();
            loop {
                checkpoint()?;
                let entry = self.next_entry(iterator.next(), &planned.spec.source);
                checkpoint()?;
                let Some(entry) = entry else {
                    break;
                };
                let name = OsStr::from_bytes(entry.file_name().to_bytes());
                if name == OsStr::new(".") || name == OsStr::new("..") {
                    continue;
                }
                // At most one overflow sentinel is examined for the whole
                // pass; the sentinel is never admitted as a candidate.
                self.entries_examined = self.entries_examined.saturating_add(1);
                if limits.entries.check(self.entries_examined).is_err() {
                    self.stop(Diagnostic::EntryLimitReached);
                    return Ok(());
                }
                let kind = match entry.file_type() {
                    Some(kind) => kind,
                    // Some filesystems omit d_type; ask the pinned directory.
                    None => {
                        checkpoint()?;
                        let stat = fstatat(&directory, name, AtFlags::AT_SYMLINK_NOFOLLOW);
                        checkpoint()?;
                        match stat {
                            Ok(stat) => sflag_to_type(stat.st_mode),
                            Err(_) => {
                                self.note(Diagnostic::EntryUnreadable(planned.spec.source.clone()));
                                continue;
                            }
                        }
                    }
                };
                checkpoint()?;
                match kind {
                    // Inspect every named skill-file slot, including FIFOs,
                    // sockets and directories. Silently ignoring an invalid
                    // higher-priority slot could promote a shadowed loser.
                    // A root-level directory called SKILL.md is a skill name,
                    // however: its own SKILL.md is the file slot to inspect.
                    _ if name == planned.spec.skill_file
                        && !(relative.as_os_str().is_empty() && matches!(kind, Type::Directory)) =>
                    {
                        self.push_candidate(
                            planned,
                            directory.as_fd(),
                            &relative,
                            name,
                            matches!(kind, Type::Symlink),
                            limits,
                        );
                    }
                    Type::Directory => {
                        if depth + 1 > limits.depth {
                            self.note(Diagnostic::DepthLimitReached(planned.spec.source.clone()));
                            continue;
                        }
                        queue.push_back((planned, root, relative.join(name), depth + 1));
                    }
                    Type::Symlink => {
                        self.note(Diagnostic::SymlinkedDirectorySkipped(
                            planned.spec.source.clone(),
                        ));
                    }
                    _ => {}
                }
                checkpoint()?;
                if self.stopped {
                    return Ok(());
                }
            }
        }
        checkpoint()?;
        Ok(())
    }

    // `dev_t`/`ino_t` widths differ by platform: the casts below are identity
    // on Linux and widening on macOS.
    #[allow(clippy::unnecessary_cast)]
    fn push_candidate(
        &mut self,
        planned: &PlannedRoot,
        directory: BorrowedFd<'_>,
        relative: &Path,
        name: &OsStr,
        via_symlink: bool,
        limits: DiscoveryLimits,
    ) {
        let Some(root) = planned.root.as_ref() else {
            return;
        };
        // Follow a symlinked candidate only far enough to learn size and
        // identity; the authorized read repeats every containment check.
        let flags = if via_symlink {
            AtFlags::empty()
        } else {
            AtFlags::AT_SYMLINK_NOFOLLOW
        };
        let full = relative.join(name);
        let Ok(stat) = fstatat(directory, name, flags) else {
            self.note(Diagnostic::EntryUnreadable(planned.spec.source.clone()));
            return;
        };
        let kind = SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT;
        if kind != SFlag::S_IFREG {
            if via_symlink && relative.as_os_str().is_empty() && kind == SFlag::S_IFDIR {
                // A root-level SKILL.md link may hide a directory bearing that
                // callable name, not merely an unsupported root-level file.
                self.note(Diagnostic::SymlinkedDirectorySkipped(
                    planned.spec.source.clone(),
                ));
            }
            self.reject(planned, full, CandidateProblem::NotRegularFile);
            return;
        }
        let size = stat.st_size.max(0) as u64;
        if size > SKILL_FILE_BYTES.max() as u64 {
            // No content read could admit this snapshot. Retain the rejected
            // name, not a reservation that can exhaust every other skill's
            // cumulative allowance. A later repair needs a fresh capture.
            self.reject(planned, full, CandidateProblem::Oversized);
            return;
        }
        let next = self.bytes_examined.saturating_add(size);
        if usize::try_from(next).map_or(true, |total| limits.bytes.check(total).is_err()) {
            self.stop(Diagnostic::ByteLimitReached);
            return;
        }
        self.bytes_examined = next;
        self.candidates.push(Candidate {
            source: planned.spec.source.clone(),
            kind: planned.spec.kind,
            priority: planned.spec.priority,
            visibility: planned.spec.visibility.clone(),
            path: LocalPath::new(root.absolute_path().join(&full)),
            relative: full,
            identity: FileIdentity::new(stat.st_dev as u64, stat.st_ino as u64),
            size,
            via_symlink,
        });
    }
}

fn sflag_to_type(mode: nix::sys::stat::mode_t) -> Type {
    let bits = SFlag::from_bits_truncate(mode) & SFlag::S_IFMT;
    match bits {
        SFlag::S_IFDIR => Type::Directory,
        SFlag::S_IFLNK => Type::Symlink,
        SFlag::S_IFIFO => Type::Fifo,
        SFlag::S_IFSOCK => Type::Socket,
        SFlag::S_IFCHR => Type::CharacterDevice,
        SFlag::S_IFBLK => Type::BlockDevice,
        _ => Type::File,
    }
}

/// Build the documented Claude Code plan: the project and user skill roots and
/// nothing else. Plugin and managed sources are declared unenumerated, so the
/// result stays partial rather than implying a complete inventory. Ancestor
/// directories are never walked, and no other harness's roots are added.
pub fn claude_code_plan(
    workspace: &Path,
    user_home: Option<&Path>,
    visibility: Visibility,
) -> Result<DiscoveryPlan, DiscoveryError> {
    let harness =
        HarnessId::new(crate::adapter::CLAUDE_CODE_ID).map_err(DiscoveryError::Identity)?;
    let mut plan = DiscoveryPlan::new(harness);
    let declared = [
        (
            CLAUDE_PROJECT_SOURCE,
            SourceKind::Project,
            CLAUDE_PROJECT_PRIORITY,
            Some(workspace.join(".claude").join("skills")),
        ),
        (
            CLAUDE_USER_SOURCE,
            SourceKind::User,
            CLAUDE_USER_PRIORITY,
            user_home.map(|home| home.join(".claude").join("skills")),
        ),
    ];
    for (id, kind, priority, path) in declared {
        let source = SourceId::new(id).map_err(DiscoveryError::Identity)?;
        let Some(path) = path else {
            plan.note_missing(source);
            continue;
        };
        let spec = RootSpec::new(
            source.clone(),
            kind,
            priority,
            visibility.clone(),
            CLAUDE_SKILL_FILE,
        )?;
        match PlannedRoot::open(spec, &path)? {
            Some(root) => plan.push_root(root),
            None => plan.note_missing(source),
        }
    }
    for id in CLAUDE_UNENUMERATED_SOURCES {
        plan.note_unenumerated(SourceId::new(*id).map_err(DiscoveryError::Identity)?);
    }
    Ok(plan)
}

/// Add effective configured skill roots to local Claude-layout inspection.
/// Configuration grants read access, not a verified harness load/precedence
/// contract. Inspection passes `Visibility::Unverified`. Rank passes its
/// provisional label, so configured skills can be suggested but are reported
/// as unverified. Configured roots rank below every declared root.
/// Project-relative paths are opened beneath the workspace descriptor,
/// including symlinks.
pub fn claude_code_plan_with_roots(
    workspace: &Path,
    user_home: Option<&Path>,
    visibility: Visibility,
    configured: &[crate::privacy::SkillRoot],
) -> Result<DiscoveryPlan, DiscoveryError> {
    use crate::privacy::SkillRoot;
    // At most 32 roots in each of the trusted-user and project layers.
    if configured.len() > 64 {
        return Err(DiscoveryError::InvalidRootPath);
    }
    let mut plan = claude_code_plan(workspace, user_home, visibility.clone())?;
    if configured.is_empty() {
        return Ok(plan);
    }
    let workspace_root =
        AuthorizedRoot::open_absolute(workspace).map_err(|_| DiscoveryError::InvalidRootPath)?;
    let mut ordered: Vec<_> = configured.iter().collect();
    ordered.sort();
    ordered.dedup();
    for configured_root in ordered {
        let (declared, kind, opened) = match configured_root {
            SkillRoot::WorkspaceRelative(relative) => (
                workspace.join(relative.as_path()),
                SourceKind::Project,
                workspace_root.open_workspace_relative(relative),
            ),
            SkillRoot::TrustedAbsolute(absolute) => (
                absolute.as_path().to_path_buf(),
                SourceKind::User,
                AuthorizedRoot::open_trusted(absolute),
            ),
        };
        if let Ok(root) = &opened
            && plan
                .roots
                .iter()
                .filter_map(|r| r.root())
                .any(|r| r.identity() == root.identity())
        {
            continue;
        }
        let source = SourceId::new(format!(
            "configured.{}",
            blake3::hash(declared.as_os_str().as_bytes()).to_hex()
        ))
        .map_err(DiscoveryError::Identity)?;
        let spec = RootSpec::new(
            source.clone(),
            kind,
            0,
            visibility.clone(),
            CLAUDE_SKILL_FILE,
        )?;
        match opened {
            Ok(root) => plan.push_root(PlannedRoot {
                spec,
                root: Some(root),
                declared,
            }),
            Err(ReadError::NotFound) => plan.note_missing(source),
            Err(_) => plan.push_root(PlannedRoot {
                spec,
                root: None,
                declared,
            }),
        }
    }
    Ok(plan)
}

#[cfg(test)]
mod tests;
