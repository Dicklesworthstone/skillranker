//! Native Claude Code session discovery for one exact workspace.
//!
//! Claude keeps each session as `<projects root>/<encoded workspace>/<session>.jsonl`.
//! The directory encoding is an unverified harness convention, and it changed
//! between Claude versions, so it only narrows where to look. A transcript is a
//! candidate only when its own bounded head records this workspace as its first
//! working directory and carries its file name as a session identity. Other
//! workspaces' directories are never listed, symlinks and special files are
//! never opened, and no candidate is read beyond its head.

use crate::adapter::{CLAUDE_CODE_ID, CONTRACT_VERSION};
use crate::authorized_read::{AuthorizedRoot, ReadError};
use crate::context::source::{SessionCandidate, SessionInventory, SourceTarget};
use crate::identity::{
    AdapterId, AdapterVersion, SessionId, SessionIdentity, SourceProvenance, WorkspaceId,
};
use crate::roster::LocalPath;
use nix::dir::{Dir, Type};
use nix::fcntl::{OFlag, openat};
use nix::sys::stat::{FileStat, Mode, SFlag, fstat};
use serde_json::Value;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Transcripts examined per discovery, across all roots and directory names.
pub const MAX_TRANSCRIPTS: usize = 1_000;
/// Bytes read from the start of a transcript to establish its identity.
pub const HEAD_BYTES: usize = 64 * 1024;

/// Directory names Claude has used for `workspace`: older versions replaced
/// only `/`, newer ones every character that is not ASCII alphanumeric.
pub fn project_directories(workspace: &Path) -> Vec<String> {
    let Some(text) = workspace.to_str() else {
        return Vec::new();
    };
    let mut names = vec![
        text.replace('/', "-"),
        text.replace(['/', '.'], "-"),
        text.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect(),
    ];
    names.sort();
    names.dedup();
    names
}

/// The session a transcript head attributes itself to: `stem` when the first
/// recorded working directory is exactly `workspace` and a record carries
/// `stem` as its session ID. Only complete lines count.
pub fn attribution(head: &[u8], stem: &str, workspace: &str) -> Option<SessionId> {
    checked_attribution(head, stem, workspace).ok().flatten()
}

/// `Ok(None)` proves another workspace; `Err` means attribution is unresolved.
/// Never turn unreadable or ambiguous evidence into an absent candidate.
fn checked_attribution(head: &[u8], stem: &str, workspace: &str) -> Result<Option<SessionId>, ()> {
    if head.len() > HEAD_BYTES {
        return Err(());
    }
    let end = head.iter().rposition(|byte| *byte == b'\n').ok_or(())?;
    let mut in_workspace = false;
    let mut claimed = false;
    for line in head[..end].split(|byte| *byte == b'\n') {
        let Value::Object(record) =
            crate::adapter::decode_json(line, HEAD_BYTES).map_err(|_| ())?
        else {
            return Err(());
        };
        if !in_workspace && let Some(cwd) = record.get("cwd") {
            let cwd = cwd.as_str().ok_or(())?;
            if cwd != workspace {
                return Ok(None);
            }
            in_workspace = true;
        }
        if let Some(session) = record.get("sessionId") {
            if session.as_str() != Some(stem) {
                return Err(());
            }
            claimed = true;
        }
    }
    if in_workspace && claimed {
        SessionId::new(stem).map(Some).map_err(|_| ())
    } else {
        Err(())
    }
}

/// The session of an explicitly selected transcript, when its own records
/// attribute it to `workspace`; otherwise it has no durable identity.
pub fn transcript_session(path: &Path, workspace: &Path) -> Option<SessionId> {
    let (parent, name) = (path.parent()?, path.file_name()?);
    let directory = AuthorizedRoot::open_absolute(parent).ok()?;
    probe(&directory, name, workspace.to_str()?)
        .ok()
        .flatten()
        .map(|(session, _, _)| session)
}

/// This workspace's Claude sessions under `roots` (Claude projects
/// directories). A missing directory contributes nothing. An unreadable one,
/// or reaching a bound, leaves the inventory incomplete, which can neither
/// establish uniqueness nor choose the newest session.
pub fn discover_claude_sessions(
    roots: &[PathBuf],
    workspace: &Path,
    workspace_id: &WorkspaceId,
) -> SessionInventory {
    let mut inventory = SessionInventory {
        candidates: Vec::new(),
        complete: true,
    };
    let Some(cwd) = workspace.to_str() else {
        return inventory;
    };
    let (Ok(adapter), Ok(version)) = (
        AdapterId::new(CLAUDE_CODE_ID),
        AdapterVersion::new(CONTRACT_VERSION.to_string()),
    ) else {
        inventory.complete = false;
        return inventory;
    };
    let mut examined = 0usize;
    let mut files = BTreeSet::new();
    for root in roots {
        for name in project_directories(workspace) {
            let path = root.join(&name);
            let directory = match AuthorizedRoot::open_absolute(&path) {
                Ok(directory) => directory,
                Err(ReadError::NotFound | ReadError::NotADirectory) => continue,
                Err(_) => {
                    inventory.complete = false;
                    continue;
                }
            };
            let listing = directory
                .as_fd()
                .try_clone_to_owned()
                .ok()
                .and_then(|fd| Dir::from_fd(fd).ok());
            let Some(mut listing) = listing else {
                inventory.complete = false;
                continue;
            };
            for entry in listing.iter() {
                let Ok(entry) = entry else {
                    inventory.complete = false;
                    continue;
                };
                let name = OsStr::from_bytes(entry.file_name().to_bytes());
                if !name.as_bytes().ends_with(b".jsonl")
                    || matches!(entry.file_type(), Some(kind) if !matches!(kind, Type::File))
                {
                    continue;
                }
                examined += 1;
                if examined > MAX_TRANSCRIPTS {
                    inventory.complete = false;
                    return inventory;
                }
                let (session, modified, identity) = match probe(&directory, name, cwd) {
                    Ok(Some(candidate)) => candidate,
                    Ok(None) => continue,
                    Err(()) => {
                        inventory.complete = false;
                        continue;
                    }
                };
                // The same file reached through two directory names or roots
                // is one session, not a duplicate.
                if !files.insert(identity) {
                    continue;
                }
                inventory.candidates.push(SessionCandidate {
                    target: SourceTarget::ClaudeTranscript(LocalPath::new(path.join(name))),
                    identity: SessionIdentity {
                        source: SourceProvenance::Native {
                            adapter: adapter.clone(),
                            version: version.clone(),
                        },
                        workspace: Some(workspace_id.clone()),
                        session: Some(session),
                        agent: None,
                        branch: None,
                        epoch: None,
                    },
                    last_activity_unix_ms: Some(modified),
                    remote: false,
                });
            }
        }
    }
    inventory
}

/// Attribution, modification time and file identity of one regular file,
/// opened without following symlinks and read only for its head.
type ProbedSession = (SessionId, i64, (u64, u64));

fn probe(
    directory: &AuthorizedRoot,
    name: &OsStr,
    workspace: &str,
) -> Result<Option<ProbedSession>, ()> {
    let stem =
        std::str::from_utf8(name.as_bytes().strip_suffix(b".jsonl").ok_or(())?).map_err(|_| ())?;
    let flags = OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC;
    let fd = openat(directory.as_fd(), name, flags, Mode::empty()).map_err(|_| ())?;
    let stat = fstat(&fd).map_err(|_| ())?;
    if SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT != SFlag::S_IFREG {
        return Err(());
    }
    let mut head = Vec::with_capacity(HEAD_BYTES);
    std::fs::File::from(fd)
        .take(HEAD_BYTES as u64)
        .read_to_end(&mut head)
        .map_err(|_| ())?;
    let Some(session) = checked_attribution(&head, stem, workspace)? else {
        return Ok(None);
    };
    Ok(Some((
        session,
        modified_unix_ms(&stat),
        file_identity(&stat),
    )))
}

// Stat field widths differ by platform: these casts are identity on Linux and
// widening on macOS.
#[allow(clippy::unnecessary_cast)]
fn modified_unix_ms(stat: &FileStat) -> i64 {
    (stat.st_mtime as i64)
        .saturating_mul(1_000)
        .saturating_add(stat.st_mtime_nsec as i64 / 1_000_000)
}

#[allow(clippy::unnecessary_cast)]
fn file_identity(stat: &FileStat) -> (u64, u64) {
    (stat.st_dev as u64, stat.st_ino as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORKSPACE: &str = "/data/projects/my_app.v2";

    fn line(value: Value) -> String {
        value.to_string() + "\n"
    }

    #[test]
    fn directory_names_cover_both_claude_encodings() {
        let names = project_directories(Path::new(WORKSPACE));
        assert!(names.contains(&"-data-projects-my_app.v2".to_owned()));
        assert!(names.contains(&"-data-projects-my-app-v2".to_owned()));
    }

    #[test]
    fn attribution_needs_this_workspace_first_and_the_file_session() {
        let own = line(serde_json::json!({"type": "summary"}))
            + &line(serde_json::json!({"cwd": WORKSPACE, "sessionId": "s-1"}));
        assert_eq!(
            attribution(own.as_bytes(), "s-1", WORKSPACE),
            Some(SessionId::new("s-1").unwrap())
        );
        // Another workspace, another session, or an incomplete line: none.
        let other = line(serde_json::json!({"cwd": "/data/projects/other", "sessionId": "s-1"}));
        assert_eq!(attribution(other.as_bytes(), "s-1", WORKSPACE), None);
        assert_eq!(attribution(own.as_bytes(), "s-2", WORKSPACE), None);
        let partial = own.trim_end_matches('\n');
        assert_eq!(attribution(partial.as_bytes(), "s-1", WORKSPACE), None);
        // A later change of directory does not disown the session.
        let moved = own + &line(serde_json::json!({"cwd": "/tmp", "sessionId": "s-1"}));
        assert!(attribution(moved.as_bytes(), "s-1", WORKSPACE).is_some());
    }
}
