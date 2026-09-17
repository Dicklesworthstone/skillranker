//! Source selection before readers perform effects. A selection never falls back
//! to another conversation. Paths and identities here are local, not provider data.
//!
//! Discovery adapters supply a bounded, complete inventory for a independently
//! resolved workspace. They remain responsible for filesystem authorization,
//! native parsing and cass capability checks; selection grants none of those.

use crate::adapter::{SelectedSource, SourceRequest, select_source};
use crate::identity::{HarnessId, SessionIdentity, SourceProvenance, WorkspaceId};
use crate::limits::DISCOVERY_FILES;
use crate::output::ErrorKind;
use crate::privacy::MAX_ROOT_BYTES;
use crate::roster::LocalPath;
use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;

/// These are source-effect restrictions, not permission to contact Jev.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SourcePolicy {
    pub offline: bool,
    pub dry_run: bool,
    pub local_only: bool,
    pub allow_network: bool,
}

impl SourcePolicy {
    pub const fn cass_allowed(self) -> bool {
        !self.offline && !self.dry_run && !self.local_only
    }
}

/// Raw source options. Neither terminal status nor stdin contents infer a mode.
/// `context = "-"` and `claude_hook` are the only stdin forms.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SourceOptions {
    pub claude_hook: bool,
    pub context: Option<LocalPath>,
    pub transcript: Option<LocalPath>,
    pub harness: Option<HarnessId>,
    pub cass_session: Option<LocalPath>,
    pub latest: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceTarget {
    ClaudeHookStdin,
    NormalizedStdin,
    NormalizedFile(LocalPath),
    ClaudeTranscript(LocalPath),
    CassSession(LocalPath),
}

impl SourceTarget {
    pub const fn reads_stdin(&self) -> bool {
        matches!(self, Self::ClaudeHookStdin | Self::NormalizedStdin)
    }

    fn is_cass(&self) -> bool {
        matches!(self, Self::CassSession(_))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionReason {
    Explicit,
    UniqueInWorkspace,
    /// Only an explicit --latest can produce this; it does not prove liveness.
    LatestRequested,
    InteractiveChoice,
}

/// Immutable binding handed to exactly one reader. The reader must validate
/// discovered identity against the opened source before using its contents.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceSelection {
    target: SourceTarget,
    workspace: WorkspaceId,
    expected_identity: Option<SessionIdentity>,
    reason: SelectionReason,
    candidate_count: usize,
}

impl SourceSelection {
    pub fn target(&self) -> &SourceTarget {
        &self.target
    }
    pub fn workspace(&self) -> &WorkspaceId {
        &self.workspace
    }
    pub fn expected_identity(&self) -> Option<&SessionIdentity> {
        self.expected_identity.as_ref()
    }
    pub const fn reason(&self) -> SelectionReason {
        self.reason
    }
    pub const fn candidate_count(&self) -> usize {
        self.candidate_count
    }

    /// A reader failure is returned unchanged, with no discovery/retry branch.
    /// This consumes the selection so a caller must deliberately select again.
    pub fn read<T, E>(self, reader: impl FnOnce(&Self) -> Result<T, E>) -> Result<T, E> {
        reader(&self)
    }
}

/// Adapter-produced inventory entry. Source, session, agent and branch stay
/// distinct even if two entries share a path or session identifier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionCandidate {
    pub target: SourceTarget,
    pub identity: SessionIdentity,
    pub last_activity_unix_ms: Option<i64>,
    pub remote: bool,
}

/// `complete=false` includes paging, truncation and adapter discovery failures.
/// A partial inventory cannot establish uniqueness or newest-session selection.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionInventory {
    pub candidates: Vec<SessionCandidate>,
    pub complete: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SelectionOutcome {
    Selected(SourceSelection),
    NeedsChoice(SessionChoices),
}

/// Only a TTY discovery with multiple candidates creates this explicit prompt.
/// UI code renders it on the controlling terminal, never consumes source stdin.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionChoices {
    workspace: WorkspaceId,
    candidates: Vec<SessionCandidate>,
}

impl SessionChoices {
    pub fn candidates(&self) -> &[SessionCandidate] {
        &self.candidates
    }

    pub fn choose(self, index: usize) -> Result<SourceSelection, SourceError> {
        let count = self.candidates.len();
        let candidate = self
            .candidates
            .into_iter()
            .nth(index)
            .ok_or(SourceError::InvalidChoice)?;
        Ok(bind(candidate, self.workspace, SelectionReason::InteractiveChoice, count))
    }
}

/// All diagnostics are fixed text: no paths, unknown harnesses or parser errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceError {
    ConflictingFlags,
    HarnessRequired,
    UnsupportedHarness,
    InvalidPath,
    CassUnavailableInMode,
    MissingSession,
    AmbiguousSession,
    IncompleteInventory,
    InventoryLimit,
    InvalidInventory,
    RemoteSource,
    InvalidChoice,
}

impl SourceError {
    pub const fn kind(self) -> ErrorKind {
        match self {
            Self::ConflictingFlags | Self::HarnessRequired | Self::InvalidPath | Self::InvalidChoice => ErrorKind::InvalidUsage,
            Self::UnsupportedHarness | Self::RemoteSource => ErrorKind::UnsupportedInput,
            Self::CassUnavailableInMode => ErrorKind::UnsupportedSourceMode,
            Self::MissingSession => ErrorKind::MissingSession,
            Self::AmbiguousSession => ErrorKind::AmbiguousSession,
            Self::IncompleteInventory => ErrorKind::InsufficientContext,
            Self::InventoryLimit => ErrorKind::OversizedInput,
            Self::InvalidInventory => ErrorKind::MalformedInput,
        }
    }

    pub const fn hint(self) -> &'static str {
        match self {
            Self::CassUnavailableInMode => "Choose a direct transcript or normalized context in this mode.",
            Self::AmbiguousSession | Self::InvalidChoice => "Select an exact source; use --latest only to request recency selection.",
            Self::IncompleteInventory | Self::InventoryLimit => "Finish bounded discovery pagination or select an exact source.",
            _ => "Use one supported explicit source with its required source options.",
        }
    }
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ConflictingFlags => "conflicting source or network flags",
            Self::HarnessRequired => "native transcript requires a harness",
            Self::UnsupportedHarness => "unsupported native transcript harness",
            Self::InvalidPath => "invalid source path",
            Self::CassUnavailableInMode => "cass is unavailable in this source mode",
            Self::MissingSession => "no session in the exact workspace",
            Self::AmbiguousSession => "session selection is ambiguous",
            Self::IncompleteInventory => "session discovery is incomplete",
            Self::InventoryLimit => "session inventory exceeds its bound",
            Self::InvalidInventory => "invalid or duplicate session inventory entry",
            Self::RemoteSource => "remote session sources are not accepted",
            Self::InvalidChoice => "session choice is out of range",
        })
    }
}

impl std::error::Error for SourceError {}

fn validate_path(path: &LocalPath) -> Result<(), SourceError> {
    let bytes = path.as_path().as_os_str().as_encoded_bytes();
    if bytes.is_empty() || bytes.len() > MAX_ROOT_BYTES || bytes.contains(&0) {
        Err(SourceError::InvalidPath)
    } else {
        Ok(())
    }
}

impl SourceOptions {
    /// Discovery is lazy and receives the child-effect restriction before it can
    /// run. Explicit source errors never invoke it. `interactive` means a usable
    /// controlling terminal, not merely that stdin is (or is not) a TTY.
    pub fn resolve(
        &self,
        workspace: WorkspaceId,
        policy: SourcePolicy,
        interactive: bool,
        discover: impl FnOnce(&WorkspaceId, SourcePolicy) -> Result<SessionInventory, SourceError>,
    ) -> Result<SelectionOutcome, SourceError> {
        let mode = select_source(SourceRequest {
            claude_hook: self.claude_hook,
            context_file: self.context.is_some(),
            native_transcript: self.transcript.is_some(),
            cass_session: self.cass_session.is_some(),
            // No probe or speculative read of stdin: only flags select it.
            stdin_present: false,
            stdin_mode_explicit: self.context.as_ref().is_some_and(|p| p.as_path() == Path::new("-")),
        }).map_err(|_| SourceError::ConflictingFlags)?;
        if (self.latest && mode != SelectedSource::Discovery)
            || (self.harness.is_some() && self.transcript.is_none())
            || (policy.allow_network && (policy.offline || policy.dry_run))
        {
            return Err(SourceError::ConflictingFlags);
        }
        for path in [&self.context, &self.transcript, &self.cass_session].into_iter().flatten() {
            validate_path(path)?;
        }
        let target = match mode {
            SelectedSource::ClaudeHook => SourceTarget::ClaudeHookStdin,
            SelectedSource::NormalizedContext { stdin: true } => SourceTarget::NormalizedStdin,
            SelectedSource::NormalizedContext { stdin: false } => SourceTarget::NormalizedFile(self.context.clone().ok_or(SourceError::InvalidPath)?),
            SelectedSource::NativeTranscript => {
                let harness = self.harness.as_ref().ok_or(SourceError::HarnessRequired)?;
                if harness.as_str() != "claude_code" {
                    return Err(SourceError::UnsupportedHarness);
                }
                SourceTarget::ClaudeTranscript(self.transcript.clone().ok_or(SourceError::InvalidPath)?)
            }
            SelectedSource::CassSession => {
                if !policy.cass_allowed() {
                    return Err(SourceError::CassUnavailableInMode);
                }
                SourceTarget::CassSession(self.cass_session.clone().ok_or(SourceError::InvalidPath)?)
            }
            SelectedSource::Discovery => {
                let inventory = discover(&workspace, policy)?;
                return select_inventory(workspace, policy, interactive, self.latest, inventory);
            }
        };
        if matches!(&target, SourceTarget::ClaudeTranscript(p) | SourceTarget::CassSession(p) if p.as_path() == Path::new("-")) {
            return Err(SourceError::InvalidPath);
        }
        Ok(SelectionOutcome::Selected(SourceSelection {
            target, workspace, expected_identity: None,
            reason: SelectionReason::Explicit, candidate_count: 0,
        }))
    }
}

fn bind(candidate: SessionCandidate, workspace: WorkspaceId, reason: SelectionReason, count: usize) -> SourceSelection {
    SourceSelection {
        target: candidate.target, workspace, expected_identity: Some(candidate.identity),
        reason, candidate_count: count,
    }
}

fn select_inventory(
    workspace: WorkspaceId,
    policy: SourcePolicy,
    interactive: bool,
    latest: bool,
    inventory: SessionInventory,
) -> Result<SelectionOutcome, SourceError> {
    if inventory.candidates.len() > DISCOVERY_FILES.max() {
        return Err(SourceError::InventoryLimit);
    }
    if !inventory.complete {
        return Err(SourceError::IncompleteInventory);
    }
    let mut identities = BTreeSet::new();
    let mut candidates = Vec::new();
    for candidate in &inventory.candidates {
        // Exact independently resolved workspace, never prefix or shared Git root.
        if candidate.identity.workspace.as_ref() != Some(&workspace) {
            continue;
        }
        if candidate.remote {
            return Err(SourceError::RemoteSource);
        }
        if candidate.target.is_cass() && !policy.cass_allowed() {
            return Err(SourceError::CassUnavailableInMode);
        }
        let path = match (&candidate.target, &candidate.identity.source) {
            (SourceTarget::ClaudeTranscript(path), SourceProvenance::Native { adapter, .. }) if adapter.as_str() == "claude_code" => path,
            (SourceTarget::CassSession(path), SourceProvenance::Cass { .. }) => path,
            _ => return Err(SourceError::InvalidInventory),
        };
        validate_path(path).map_err(|_| SourceError::InvalidInventory)?;
        if path.as_path() == Path::new("-") || candidate.identity.session.is_none() {
            return Err(SourceError::InvalidInventory);
        }
        let id = &candidate.identity;
        if !identities.insert((&id.source, &id.session, &id.agent, &id.branch, &id.epoch)) {
            return Err(SourceError::InvalidInventory);
        }
        candidates.push(candidate.clone());
    }
    let count = candidates.len();
    if count == 0 {
        return Err(SourceError::MissingSession);
    }
    if latest {
        // Missing timestamps and ties cannot silently use inventory order.
        let mut newest = None;
        let mut tied = false;
        for (index, candidate) in candidates.iter().enumerate() {
            let time = candidate.last_activity_unix_ms.ok_or(SourceError::AmbiguousSession)?;
            match newest {
                None => newest = Some((time, index)),
                Some((max, _)) if time > max => { newest = Some((time, index)); tied = false; }
                Some((max, _)) if time == max => tied = true,
                _ => {}
            }
        }
        if tied {
            return Err(SourceError::AmbiguousSession);
        }
        let (_, index) = newest.ok_or(SourceError::MissingSession)?;
        return Ok(SelectionOutcome::Selected(bind(candidates.swap_remove(index), workspace, SelectionReason::LatestRequested, count)));
    }
    if count == 1 {
        return Ok(SelectionOutcome::Selected(bind(candidates.remove(0), workspace, SelectionReason::UniqueInWorkspace, count)));
    }
    if !interactive {
        return Err(SourceError::AmbiguousSession);
    }
    Ok(SelectionOutcome::NeedsChoice(SessionChoices { workspace, candidates }))
}
