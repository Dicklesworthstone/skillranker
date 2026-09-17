//! Optional local project signals. No manifest bodies, ambient PATH, or tool probes.
//!
//! The caller supplies independently authorized workspace and trusted PATH roots;
//! normalized input declarations do not grant that authority. Dirty paths are
//! local private text, not a provider payload: redaction still applies before send.
//! As with the subprocess boundary, local filesystem/spawn syscalls cannot provide
//! a hard deadline under uninterruptible kernel I/O. No network is performed here.
use super::PrivateText;
use crate::limits::DurationMillis;
use crate::runtime::EntryClock;
use crate::subprocess::{self, ChildRequest, SubprocessError, TrustedExecutable};
use asupersync::Cx;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub const STAGE_MILLIS: u64 = 250;
pub const STATUS_BYTES: usize = 64 * 1024;
pub const DIRTY_PATH_LIMIT: usize = 100;
const MARKERS: &[&str] = &[
    "Cargo.toml",
    "go.mod",
    "package.json",
    "pyproject.toml",
    "Makefile",
    "CMakeLists.txt",
    "lakefile.lean",
    "lakefile.toml",
    "Gemfile",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
];
const TOOLS: &[&str] = &[
    "git", "cargo", "rustc", "go", "node", "npm", "python3", "make", "cmake", "lake", "ruby",
    "java", "rch",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Omission {
    InvalidAuthority,
    UnsupportedPlatform,
    Unavailable,
    UnsupportedGit,
    Budget,
    Cancelled,
    MalformedStatus,
    OutputLimit,
}

#[derive(Debug, Default)]
pub struct DirtyPaths {
    pub paths: Vec<PrivateText>,
    pub omitted_non_utf8: usize,
    pub omitted_unsafe: usize,
    pub truncated: bool,
}

/// Not serializable: consumers must construct a separate redacted provider view.
#[derive(Debug, Default)]
pub struct ProjectSignals {
    pub filenames: Vec<&'static str>,
    pub tools_on_path: Vec<&'static str>,
    pub dirty_paths: Option<DirtyPaths>,
    pub git_omission: Option<Omission>,
    /// Partial inventories cannot establish absence of a tool or framework.
    pub inventory_partial: bool,
}

/// Parses only porcelain v1 -z without renames/untracked entries. Never decodes
/// lossy UTF-8, follows a path, or treats a partial final record as complete.
pub fn parse_dirty_paths(bytes: &[u8]) -> Result<DirtyPaths, Omission> {
    if bytes.len() > STATUS_BYTES {
        return Err(Omission::OutputLimit);
    }
    if !bytes.is_empty() && bytes.last() != Some(&0) {
        return Err(Omission::MalformedStatus);
    }
    if bytes.is_empty() {
        return Ok(DirtyPaths::default());
    }
    let mut result = DirtyPaths::default();
    let mut seen = BTreeSet::new();
    for record in bytes[..bytes.len() - 1].split(|b| *b == 0) {
        if record.len() < 4
            || record[2] != b' '
            || record[..2] == *b"  "
            || record[..2].iter().any(|b| !b" MADUT".contains(b))
        {
            return Err(Omission::MalformedStatus);
        }
        let Ok(path) = std::str::from_utf8(&record[3..]) else {
            result.omitted_non_utf8 += 1;
            continue;
        };
        if path.starts_with('/') || path.contains('\\') || path.chars().any(|c| c.is_control() || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
            || path.split('/').any(|part| part.is_empty() || matches!(part, "." | ".."))
            || path.split('/').next().is_some_and(|first| first.contains(':')) {
            result.omitted_unsafe += 1;
            continue;
        }
        if !seen.insert(path) {
            return Err(Omission::MalformedStatus);
        }
        if result.paths.len() == DIRTY_PATH_LIMIT {
            result.truncated = true;
        } else {
            result.paths.push(PrivateText::new(path));
        }
    }
    Ok(result)
}

fn modern_git(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let Some(version) = text.trim_end().strip_prefix("git version ") else {
        return false;
    };
    let Some(number) = version.split_whitespace().next() else {
        return false;
    };
    let mut components = number.split('.');
    let (Some(major), Some(minor), Some(patch)) =
        (components.next(), components.next(), components.next())
    else {
        return false;
    };
    let (Ok(major), Ok(minor), Ok(_patch)) = (
        major.parse::<u32>(),
        minor.parse::<u32>(),
        patch.parse::<u32>(),
    ) else {
        return false;
    };
    // A future major's semantics have not been verified. Vendor suffixes after
    // the numeric patch (e.g. Apple Git) do not change the required Git 2 flags.
    major == 2 && minor >= 36
}

fn live(cx: &Cx, parent: &EntryClock, stage: &EntryClock) -> Result<(), Omission> {
    if parent.admit_new_work().is_err() || stage.admit_new_work().is_err() {
        return Err(Omission::Budget);
    }
    cx.checkpoint().map_err(|_| Omission::Cancelled)
}
fn omitted(error: SubprocessError) -> Omission {
    match error {
        SubprocessError::DeadlineExceeded => Omission::Budget,
        SubprocessError::Cancelled => Omission::Cancelled,
        SubprocessError::OutputLimit => Omission::OutputLimit,
        SubprocessError::UnsupportedPlatform => Omission::UnsupportedPlatform,
        _ => Omission::Unavailable,
    }
}

#[cfg(unix)]
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}
#[cfg(not(unix))]
fn executable(_path: &Path) -> bool {
    false
}

/// Fixed bounded filename metadata and trusted executable presence. PATH roots
/// are ordered and explicitly supplied by the caller; the process environment
/// and repository cannot expand them. Canonical targets inside the workspace
/// are never trusted merely because a PATH symlink points there.
pub async fn collect(
    cx: &Cx,
    parent: &EntryClock,
    workspace: &Path,
    trusted_path: &[PathBuf],
) -> ProjectSignals {
    let mut result = ProjectSignals::default();
    let remaining = parent
        .remaining_before_cleanup()
        .as_millis()
        .min(STAGE_MILLIS);
    if remaining < 2 {
        result.inventory_partial = true;
        result.git_omission = Some(Omission::Budget);
        return result;
    }
    let stage = EntryClock::capture_with(
        DurationMillis::new("project_signals", remaining, STAGE_MILLIS).expect("bounded stage"),
        DurationMillis::new("project_signals_reserve", 1, STAGE_MILLIS).expect("positive reserve"),
    )
    .expect("stage is longer than reserve");
    let gathered = gather(cx, parent, &stage, workspace, trusted_path, &mut result).await;
    if let Err(error) = gathered {
        result.git_omission = Some(error);
    }
    result
}

async fn gather(
    cx: &Cx,
    parent: &EntryClock,
    stage: &EntryClock,
    workspace: &Path,
    trusted_path: &[PathBuf],
    result: &mut ProjectSignals,
) -> Result<(), Omission> {
    result.inventory_partial = true;
    live(cx, parent, stage)?;
    if !workspace.is_absolute()
        || trusted_path.len() > 32
        || trusted_path
            .iter()
            .any(|p| !p.is_absolute() || p.as_os_str().len() > 4096)
    {
        return Err(Omission::InvalidAuthority);
    }
    let workspace = workspace
        .canonicalize()
        .map_err(|_| Omission::InvalidAuthority)?;
    if !workspace.is_dir() {
        return Err(Omission::InvalidAuthority);
    }
    let mut roots = Vec::new();
    for root in trusted_path {
        live(cx, parent, stage)?;
        let canonical = root
            .canonicalize()
            .map_err(|_| Omission::InvalidAuthority)?;
        if !canonical.is_dir() || canonical.starts_with(&workspace) {
            return Err(Omission::InvalidAuthority);
        }
        if !roots.contains(&canonical) {
            roots.push(canonical);
        }
    }
    for &marker in MARKERS {
        live(cx, parent, stage)?;
        // Do not follow repository symlinks, even to read metadata outside it.
        match std::fs::symlink_metadata(workspace.join(marker)) {
            Ok(meta) if meta.is_file() => result.filenames.push(marker),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(Omission::Unavailable),
        }
    }
    let mut git = None;
    for &tool in TOOLS {
        for root in &roots {
            live(cx, parent, stage)?;
            let candidate = root.join(tool);
            let Ok(canonical) = candidate.canonicalize() else {
                continue;
            };
            if canonical.starts_with(&workspace) || !executable(&canonical) {
                continue;
            }
            let Ok(trusted) = TrustedExecutable::resolve(&canonical, &roots) else {
                continue;
            };
            result.tools_on_path.push(tool);
            if tool == "git" {
                git = Some(trusted);
            }
            break;
        }
    }
    result.inventory_partial = false;
    let git = git.ok_or(Omission::Unavailable)?;
    live(cx, parent, stage)?;
    let request = |args: &[&str], directory: PathBuf, stdout_limit| ChildRequest {
        executable: git.clone(),
        args: args.iter().map(Into::into).collect(),
        directory,
        environment: vec![("LC_ALL".into(), "C".into())],
        stdin: Vec::new(),
        stdout_limit,
        stderr_limit: 4096,
    };
    // Version detection does not inspect the repository or consult shell PATH.
    let version = subprocess::run(cx, stage, request(&["--version"], PathBuf::from("/"), 1024))
        .await
        .map_err(omitted)?;
    if !version.status.success() || !modern_git(&version.stdout) {
        return Err(Omission::UnsupportedGit);
    }
    live(cx, parent, stage)?;
    let status = subprocess::run(
        cx,
        stage,
        request(
            &[
                "--no-optional-locks",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.untrackedCache=false",
                "status",
                "--no-renames",
                "--untracked-files=no",
                "--ignore-submodules=all",
                "--porcelain=v1",
                "-z",
            ],
            workspace,
            STATUS_BYTES,
        ),
    )
    .await
    .map_err(omitted)?;
    live(cx, parent, stage)?;
    if !status.status.success() {
        return Err(Omission::Unavailable);
    }
    result.dirty_paths = Some(parse_dirty_paths(&status.stdout)?);
    Ok(())
}
