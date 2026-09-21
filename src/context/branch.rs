//! Active branch resolution, compaction, task boundaries, and worktree identity.
//!
//! Native events form a branch-aware directed acyclic graph (DAG), not a flat
//! chronological list. Following parent links and compaction/resume markers
//! determines the active branch lineage. Timestamp ordering alone cannot identify
//! a fork and must not merge sibling branches.
//!
//! When the active branch cannot be unambiguously resolved, session-specific
//! filtering and advice are withheld rather than confusing sibling histories.
//! Loaded-state filtering suppresses only reusable references proven present in
//! the current context epoch with matching version; workflows and unknown usage
//! kinds remain eligible for re-invocation.

use crate::context::{EventKind, NormalizedEvent};
use crate::identity::{
    AgentId, BranchId, ContentHash, ContextEpoch, EventId, SkillId, TurnId, WorkspaceId,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

mod lineage;

/// Usage kind for a skill candidate or loaded evidence.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillUsageKind {
    /// Reusable static reference or documentation. Eligible for suppression only
    /// if proven present in current epoch with matching version.
    Reference,
    /// Multi-step procedure, workflow, or script. Always eligible for repeat invocation.
    Workflow,
    /// Unspecified usage kind. Always eligible; never inferred to be a reusable reference.
    #[default]
    Unknown,
}

/// A verified observation or record of a loaded skill within the context history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedSkillRecord {
    pub skill_id: SkillId,
    pub event_id: Option<EventId>,
    pub turn_id: Option<TurnId>,
    pub usage_kind: SkillUsageKind,
    pub epoch: ContextEpoch,
    pub source_content: Option<ContentHash>,
    pub rendered_content: Option<ContentHash>,
    pub has_dynamic_arguments: bool,
    pub turn_scoped: bool,
}

/// Target parameters for active branch resolution.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BranchResolutionTarget {
    /// Explicit target event ID (e.g. current request event or explicit leaf).
    pub target_event_id: Option<EventId>,
    /// Explicit target branch identifier.
    pub target_branch_id: Option<BranchId>,
    /// Explicit target agent identifier.
    pub target_agent_id: Option<AgentId>,
}

/// An unambiguously resolved active branch lineage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveBranch {
    pub branch_id: Option<BranchId>,
    pub leaf_event_id: Option<EventId>,
    /// Chronological sequence of events along the active branch (root to leaf).
    /// All sibling events from other forks are strictly excluded.
    pub events: Vec<NormalizedEvent>,
    pub current_epoch: ContextEpoch,
    pub compaction_count: usize,
    pub task_boundary_count: usize,
    pub ancestor_chain_truncated: bool,
}

impl ActiveBranch {
    /// Checks whether an event belongs to this active branch.
    pub fn contains_event(&self, event_id: &EventId) -> bool {
        self.events
            .iter()
            .any(|e| e.event_id.as_ref() == Some(event_id))
    }

    /// Set of event IDs on the active branch.
    pub fn event_id_set(&self) -> BTreeSet<EventId> {
        self.events
            .iter()
            .filter_map(|e| e.event_id.clone())
            .collect()
    }
}

/// Reason why active branch resolution could not establish an unambiguous lineage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UnresolvedBranchReason {
    EmptyHistory,
    /// No event/leaf satisfies the requested agent and branch scope.
    NoMatchingEvents,
    /// The supplied event or parent ID has multiple possible definitions.
    AmbiguousEventIdentity {
        event: EventId,
    },
    /// Same qualified identity, different content: neither definition is trusted.
    ConflictingEventDefinitions {
        event: EventId,
    },
    AmbiguousSiblingForks {
        candidate_leaves: Vec<EventId>,
    },
    TargetEventNotFound {
        target: EventId,
    },
    CycleDetected {
        at_event: EventId,
    },
    ConflictingBranchIdentities {
        expected: BranchId,
        observed: BranchId,
    },
}

impl fmt::Display for UnresolvedBranchReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyHistory => f.write_str("event history is empty"),
            Self::NoMatchingEvents => f.write_str("no event leaf matches the requested scope"),
            Self::AmbiguousEventIdentity { .. } => {
                f.write_str("event identity is ambiguous within the requested scope")
            }
            Self::ConflictingEventDefinitions { .. } => {
                f.write_str("event identity has conflicting definitions")
            }
            Self::AmbiguousSiblingForks { candidate_leaves } => {
                write!(
                    f,
                    "ambiguous sibling forks detected ({} candidate leaves)",
                    candidate_leaves.len()
                )
            }
            Self::TargetEventNotFound { target } => {
                write!(f, "target event {} not found in snapshot", target.as_str())
            }
            Self::CycleDetected { at_event } => {
                write!(
                    f,
                    "cycle detected in parent links at event {}",
                    at_event.as_str()
                )
            }
            Self::ConflictingBranchIdentities { expected, observed } => {
                write!(
                    f,
                    "conflicting branch identities: expected {}, observed {}",
                    expected.as_str(),
                    observed.as_str()
                )
            }
        }
    }
}

impl std::error::Error for UnresolvedBranchReason {}

/// Result of active branch resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BranchResolution {
    Resolved(ActiveBranch),
    Unresolved(UnresolvedBranchReason),
}

impl BranchResolution {
    pub fn is_resolved(&self) -> bool {
        matches!(self, Self::Resolved(_))
    }

    pub fn active_branch(&self) -> Option<&ActiveBranch> {
        match self {
            Self::Resolved(branch) => Some(branch),
            Self::Unresolved(_) => None,
        }
    }

    pub fn unresolved_reason(&self) -> Option<&UnresolvedBranchReason> {
        match self {
            Self::Resolved(_) => None,
            Self::Unresolved(reason) => Some(reason),
        }
    }
}

/// Advisory wrapper that withholds session-specific advice when the branch is unresolved.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BranchAdvice<T> {
    Available(T),
    Withheld { reason: UnresolvedBranchReason },
}

impl<T> BranchAdvice<T> {
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available(_))
    }

    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Available(v) => Some(v),
            Self::Withheld { .. } => None,
        }
    }

    pub fn withheld_reason(&self) -> Option<&UnresolvedBranchReason> {
        match self {
            Self::Available(_) => None,
            Self::Withheld { reason } => Some(reason),
        }
    }
}

/// Resolve the active branch from a set of normalized events and target options.
///
/// Follows parent links backwards from the target leaf to root. Sibling events
/// from alternative forks or subagents are strictly excluded. If the active
/// branch is ambiguous, returns `BranchResolution::Unresolved`.
pub fn resolve_active_branch(
    events: &[NormalizedEvent],
    target: &BranchResolutionTarget,
) -> BranchResolution {
    lineage::resolve(events, target)
}

fn epoch_name(index: u64) -> ContextEpoch {
    ContextEpoch::new(format!("epoch-{index}"))
        .unwrap_or_else(|_| ContextEpoch::new("epoch-0").expect("valid epoch"))
}

/// The epoch of each identified event on the branch. A compaction starts a new
/// epoch, so evidence from before it never carries the branch's current epoch.
pub(crate) fn event_epochs(branch: &ActiveBranch) -> BTreeMap<EventId, ContextEpoch> {
    let mut index = 0u64;
    let mut epochs = BTreeMap::new();
    for event in &branch.events {
        if matches!(event.kind, EventKind::Compaction) {
            index += 1;
        }
        if let Some(id) = &event.event_id {
            epochs.insert(id.clone(), epoch_name(index));
        }
    }
    epochs
}

/// Verdict on whether a candidate skill should be suppressed by prior loaded state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SkillSuppressionVerdict {
    Suppressed { skill_id: SkillId, reason: String },
    Eligible { skill_id: SkillId, reason: String },
}

impl SkillSuppressionVerdict {
    pub fn is_eligible(&self) -> bool {
        matches!(self, Self::Eligible { .. })
    }

    pub fn is_suppressed(&self) -> bool {
        matches!(self, Self::Suppressed { .. })
    }
}

/// Evaluate candidate skill eligibility against prior loaded-state records.
///
/// Embedded invariant:
/// - Workflows and unknown usage kinds remain eligible for re-invocation.
/// - Reusable references are suppressed ONLY if:
///   1. Active branch is resolved.
///   2. Loaded by an identified event on the active branch (sibling branch
///      loads and loads without an event ID do NOT count).
///   3. Loaded in the current context epoch (loaded prior to compaction is NOT proven present).
///   4. Both source hashes are known and equal: an unknown version is never a
///      match. Rendered hashes, when both are known, must also match.
///   5. Invocations did not have dynamic arguments and were not turn-scoped.
///
/// Callers must also withhold suppression for skills whose content forks or
/// renders dynamically; this record-level check cannot see that.
pub fn evaluate_loaded_skill_eligibility(
    candidate_skill_id: &SkillId,
    candidate_usage_kind: SkillUsageKind,
    candidate_source_hash: Option<&ContentHash>,
    candidate_rendered_hash: Option<&ContentHash>,
    active_branch: Option<&ActiveBranch>,
    loaded_records: &[LoadedSkillRecord],
) -> SkillSuppressionVerdict {
    // If the active branch is unresolved, session-specific suppression is withheld
    let Some(branch) = active_branch else {
        return SkillSuppressionVerdict::Eligible {
            skill_id: candidate_skill_id.clone(),
            reason: "active branch is unresolved; loaded-state suppression withheld".to_string(),
        };
    };

    // Workflows and unknown usage kinds are always eligible
    if candidate_usage_kind != SkillUsageKind::Reference {
        return SkillSuppressionVerdict::Eligible {
            skill_id: candidate_skill_id.clone(),
            reason: "workflows and unknown usage kinds remain eligible for re-invocation"
                .to_string(),
        };
    }

    let active_event_ids = branch.event_id_set();

    // Check if a matching reusable reference is proven present in current epoch
    for record in loaded_records {
        if record.skill_id != *candidate_skill_id {
            continue;
        }

        // Must be proven on the active branch; an unidentified load proves nothing.
        if !record
            .event_id
            .as_ref()
            .is_some_and(|event_id| active_event_ids.contains(event_id))
        {
            continue; // Loaded on a sibling branch or fork, or unattributed
        }

        // Must be in the current context epoch (compaction invalidates presence assumption)
        if record.epoch != branch.current_epoch {
            continue; // Compaction occurred after this load
        }

        // Must not have dynamic arguments
        if record.has_dynamic_arguments {
            continue; // Dynamic invocation cannot establish reusable reference presence
        }

        // Must not be turn-scoped
        if record.turn_scoped {
            continue; // Turn-scoped permissions/loads do not survive across turns
        }

        // Source content hash must be known on both sides and match
        if candidate_source_hash.is_none()
            || candidate_source_hash != record.source_content.as_ref()
        {
            continue; // Unknown or mismatched version
        }

        // Rendered content hash must match if known
        if let (Some(cand_rend), Some(rec_rend)) =
            (candidate_rendered_hash, record.rendered_content.as_ref())
            && cand_rend != rec_rend
        {
            continue; // Rendered output mismatch
        }

        // All conditions satisfied: proven available reusable reference!
        return SkillSuppressionVerdict::Suppressed {
            skill_id: candidate_skill_id.clone(),
            reason: "reusable reference is proven available in current epoch with matching version"
                .to_string(),
        };
    }

    SkillSuppressionVerdict::Eligible {
        skill_id: candidate_skill_id.clone(),
        reason: "no matching reusable reference proven available in current epoch".to_string(),
    }
}

/// Canonical worktree identity and Git status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedWorktree {
    pub workspace_id: WorkspaceId,
    pub canonical_path: PathBuf,
    pub is_git: bool,
    pub is_linked_worktree: bool,
    pub git_branch: Option<BranchId>,
    pub is_detached_head: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorktreeError {
    PathNotFound(PathBuf),
    Io(String),
    InvalidWorkspaceId(String),
}

impl fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PathNotFound(p) => write!(f, "workspace path not found: {}", p.display()),
            Self::Io(err) => write!(f, "workspace I/O error: {err}"),
            Self::InvalidWorkspaceId(err) => write!(f, "invalid workspace ID: {err}"),
        }
    }
}

impl std::error::Error for WorktreeError {}

/// Resolve canonical worktree identity, distinguishing linked worktrees and detached HEAD.
///
/// The `WorkspaceId` is derived exclusively from the canonical worktree directory path,
/// ensuring that linked worktrees sharing a `.git` common directory receive distinct
/// local identities.
pub fn resolve_worktree(path: &Path) -> Result<ResolvedWorktree, WorktreeError> {
    let canonical_path = path
        .canonicalize()
        .map_err(|_| WorktreeError::PathNotFound(path.to_path_buf()))?;

    let hash = blake3::hash(canonical_path.as_os_str().as_encoded_bytes())
        .to_hex()
        .to_string();
    let workspace_id =
        WorkspaceId::new(hash).map_err(|e| WorktreeError::InvalidWorkspaceId(e.to_string()))?;

    let git_marker = canonical_path.join(".git");
    if !git_marker.exists() {
        return Ok(ResolvedWorktree {
            workspace_id,
            canonical_path,
            is_git: false,
            is_linked_worktree: false,
            git_branch: None,
            is_detached_head: false,
        });
    }

    if git_marker.is_dir() {
        // Standard Git repository
        let head_path = git_marker.join("HEAD");
        let (git_branch, is_detached_head) = parse_git_head(&head_path);
        Ok(ResolvedWorktree {
            workspace_id,
            canonical_path,
            is_git: true,
            is_linked_worktree: false,
            git_branch,
            is_detached_head,
        })
    } else if git_marker.is_file() {
        // Linked worktree (.git is a file pointing to gitdir)
        let (git_branch, is_detached_head) = parse_linked_worktree_head(&git_marker);
        Ok(ResolvedWorktree {
            workspace_id,
            canonical_path,
            is_git: true,
            is_linked_worktree: true,
            git_branch,
            is_detached_head,
        })
    } else {
        Ok(ResolvedWorktree {
            workspace_id,
            canonical_path,
            is_git: false,
            is_linked_worktree: false,
            git_branch: None,
            is_detached_head: false,
        })
    }
}

fn parse_git_head(head_path: &Path) -> (Option<BranchId>, bool) {
    let mut file = match fs::File::open(head_path) {
        Ok(f) => f,
        Err(_) => return (None, false),
    };
    let mut buf = [0u8; 512];
    let n = match file.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return (None, false),
    };
    let content = match std::str::from_utf8(&buf[..n]) {
        Ok(s) => s.trim(),
        Err(_) => return (None, false),
    };

    if let Some(branch_ref) = content.strip_prefix("ref: refs/heads/") {
        let branch_name = branch_ref.trim();
        (BranchId::new(branch_name).ok(), false)
    } else if content.len() == 40 && content.chars().all(|c| c.is_ascii_hexdigit()) {
        // Detached HEAD pointing directly to a commit SHA
        (None, true)
    } else {
        (None, false)
    }
}

fn parse_linked_worktree_head(git_file_path: &Path) -> (Option<BranchId>, bool) {
    let mut file = match fs::File::open(git_file_path) {
        Ok(f) => f,
        Err(_) => return (None, false),
    };
    let mut buf = [0u8; 512];
    let n = match file.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return (None, false),
    };
    let content = match std::str::from_utf8(&buf[..n]) {
        Ok(s) => s.trim(),
        Err(_) => return (None, false),
    };

    let Some(gitdir_str) = content.strip_prefix("gitdir:") else {
        return (None, false);
    };
    let gitdir_path = PathBuf::from(gitdir_str.trim());
    let resolved_gitdir = if gitdir_path.is_absolute() {
        gitdir_path
    } else {
        git_file_path
            .parent()
            .map(|p| p.join(&gitdir_path))
            .unwrap_or(gitdir_path)
    };

    let head_path = resolved_gitdir.join("HEAD");
    parse_git_head(&head_path)
}
