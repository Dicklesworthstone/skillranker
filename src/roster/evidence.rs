//! Typed discovery and exclusion evidence (P2). Stable reason codes explain
//! where a candidate left the roster pipeline: discovery, visibility,
//! invocation restrictions, local policy or Quill admission. Evidence is read
//! from outputs the pipeline already produced. It opens no file, runs no
//! discovery and changes no candidate set, so collecting it cannot broaden
//! the roster or alter a ranking.
//!
//! An unknown ID is `not-in-snapshot`. A stage that did not run for a
//! candidate is `not-evaluated`, never a zero score or an invented reason.
//! Rendering and later stages (wide, fit/none, ordering, publication) belong
//! to the ranking boundary.

use super::discovery::Diagnostic;
use super::resolution::{ExactResolution, ResolutionError, ResolvedRoster};
use super::retrieval::{RetrievalError, RetrievalFailure, RetrievalMethod, RetrievalSelection};
use super::{InvocationKind, Visibility};
use crate::identity::{ContentHash, SkillId};
use std::collections::{BTreeMap, BTreeSet};

pub const EVIDENCE_VERSION: &str = "roster-evidence-v1";
/// Detailed records kept per summary; the rest are counted as omitted.
pub const MAX_DETAILS: usize = 32;

/// Roster stages, in pipeline order.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Stage {
    Discovery,
    Visibility,
    Restrictions,
    LocalPolicy,
    Retrieval,
}

impl Stage {
    pub const ALL: [Self; 5] = [
        Self::Discovery,
        Self::Visibility,
        Self::Restrictions,
        Self::LocalPolicy,
        Self::Retrieval,
    ];
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discovery => "discovery",
            Self::Visibility => "visibility",
            Self::Restrictions => "restrictions",
            Self::LocalPolicy => "local-policy",
            Self::Retrieval => "retrieval",
        }
    }
}

/// Why a candidate left the pipeline at a stage.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Reason {
    NotInSnapshot,
    Shadowed,
    Ambiguous,
    Unverified,
    ManualOnly,
    Forbidden,
    Excluded,
    AlreadyLoaded,
    NotRetrieved,
}

/// Allowlisted recovery hints. They are identifiers for the renderer, never
/// commands, and none enables networking, lifts an exclusion or lowers a gate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Hint {
    InspectRoster,
    InspectPrecedence,
    VerifyAdapterVisibility,
    RequestExplicitly,
    CheckInvocationRestrictions,
    ReviewExclusions,
    RefineRequest,
}

impl Hint {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InspectRoster => "inspect-roster",
            Self::InspectPrecedence => "inspect-precedence",
            Self::VerifyAdapterVisibility => "verify-adapter-visibility",
            Self::RequestExplicitly => "request-explicitly",
            Self::CheckInvocationRestrictions => "check-invocation-restrictions",
            Self::ReviewExclusions => "review-exclusions",
            Self::RefineRequest => "refine-request",
        }
    }
}

impl Reason {
    pub const fn stage(self) -> Stage {
        match self {
            Self::NotInSnapshot => Stage::Discovery,
            Self::Shadowed | Self::Ambiguous | Self::Unverified => Stage::Visibility,
            Self::ManualOnly | Self::Forbidden => Stage::Restrictions,
            Self::Excluded | Self::AlreadyLoaded => Stage::LocalPolicy,
            Self::NotRetrieved => Stage::Retrieval,
        }
    }
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotInSnapshot => "not-in-snapshot",
            Self::Shadowed => "shadowed",
            Self::Ambiguous => "ambiguous",
            Self::Unverified => "unverified",
            Self::ManualOnly => "manual-only",
            Self::Forbidden => "forbidden",
            Self::Excluded => "excluded",
            Self::AlreadyLoaded => "already-loaded",
            Self::NotRetrieved => "not-retrieved",
        }
    }
    /// `None` when no user action applies: a loaded reference needs none.
    pub const fn hint(self) -> Option<Hint> {
        match self {
            Self::NotInSnapshot => Some(Hint::InspectRoster),
            Self::Shadowed | Self::Ambiguous => Some(Hint::InspectPrecedence),
            Self::Unverified => Some(Hint::VerifyAdapterVisibility),
            Self::ManualOnly => Some(Hint::RequestExplicitly),
            Self::Forbidden => Some(Hint::CheckInvocationRestrictions),
            Self::Excluded => Some(Hint::ReviewExclusions),
            Self::AlreadyLoaded => None,
            Self::NotRetrieved => Some(Hint::RefineRequest),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StageOutcome {
    Passed,
    Excluded(Reason),
    NotEvaluated,
}

impl StageOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Excluded(reason) => reason.as_str(),
            Self::NotEvaluated => "not-evaluated",
        }
    }
}

/// Local policy inputs the ranking actually applied, by skill ID.
#[derive(Clone, Copy, Debug)]
pub struct PolicyView<'a> {
    pub excluded: &'a BTreeSet<&'a SkillId>,
    pub already_loaded: &'a BTreeSet<&'a SkillId>,
}

/// What Quill admission did for this request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RetrievalView<'a> {
    NotEvaluated,
    Admitted {
        method: RetrievalMethod,
        ids: BTreeSet<&'a SkillId>,
    },
    /// Retrieval ran and admitted nothing.
    Empty,
    /// An operational failure is not a lexical miss: nothing was evaluated.
    Failed,
}

impl<'a> RetrievalView<'a> {
    pub fn from_selection(selection: &'a RetrievalSelection<'a>) -> Self {
        Self::Admitted {
            method: selection
                .diagnostics
                .method
                .unwrap_or(RetrievalMethod::FullRoster),
            ids: selection
                .candidates
                .iter()
                .map(|skill| &skill.binding.id)
                .collect(),
        }
    }
    pub fn from_failure(failure: &RetrievalFailure) -> Self {
        match failure.kind {
            RetrievalError::RetrievalEmpty => Self::Empty,
            _ => Self::Failed,
        }
    }
}

/// Every roster stage for one requested skill ID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Trace {
    pub stages: [(Stage, StageOutcome); 5],
}

impl Trace {
    /// The first stage that removed the candidate, if any.
    pub fn decisive(&self) -> Option<Reason> {
        self.stages.iter().find_map(|(_, outcome)| match outcome {
            StageOutcome::Excluded(reason) => Some(*reason),
            _ => None,
        })
    }
    pub fn outcome(&self, stage: Stage) -> StageOutcome {
        self.stages[stage as usize].1
    }
}

fn visibility_reason(resolution: ExactResolution<'_>) -> Result<Option<Reason>, Reason> {
    match resolution {
        ExactResolution::Missing => Err(Reason::NotInSnapshot),
        ExactResolution::Shadowed => Err(Reason::Shadowed),
        ExactResolution::Ambiguous => Err(Reason::Ambiguous),
        ExactResolution::Unverified => Err(Reason::Unverified),
        ExactResolution::Forbidden => Ok(Some(Reason::Forbidden)),
        ExactResolution::Resolved { kind, .. } => Ok(match kind {
            InvocationKind::Agent => None,
            InvocationKind::ManualOnly => Some(Reason::ManualOnly),
            InvocationKind::Forbidden => Some(Reason::Forbidden),
        }),
    }
}

fn sibling_ids<'r>(roster: &'r ResolvedRoster, target: &SkillId) -> Vec<&'r SkillId> {
    roster
        .skills()
        .iter()
        .find(|skill| skill.bindings().iter().any(|b| &b.id == target))
        .map(|skill| skill.bindings().iter().map(|b| &b.id).collect())
        .unwrap_or_default()
}

/// Trace one skill ID through the roster stages. Each stage is reported only
/// when it actually ran for that candidate; a later stage after a decisive
/// exclusion is `not-evaluated` unless it evaluated the candidate anyway.
pub fn trace(
    roster: &ResolvedRoster,
    policy: Option<&PolicyView<'_>>,
    retrieval: &RetrievalView<'_>,
    target: &SkillId,
) -> Trace {
    use StageOutcome::{Excluded, NotEvaluated, Passed};
    let mut stages = Stage::ALL.map(|stage| (stage, NotEvaluated));
    let restriction = match visibility_reason(roster.exact_id(target)) {
        Err(Reason::NotInSnapshot) => {
            stages[0].1 = Excluded(Reason::NotInSnapshot);
            return Trace { stages };
        }
        Err(reason) => {
            stages[0].1 = Passed;
            stages[1].1 = Excluded(reason);
            return Trace { stages };
        }
        Ok(restriction) => restriction,
    };
    stages[0].1 = Passed;
    stages[1].1 = Passed;
    if let Some(reason) = restriction {
        stages[2].1 = Excluded(reason);
        return Trace { stages };
    }
    stages[2].1 = Passed;
    // Policy and retrieval act per skill, so any binding of the target's file
    // counts. Retrieval never sees explicitly excluded skills, but it does see
    // loaded references, so a loaded candidate still reports its admission.
    let siblings = sibling_ids(roster, target);
    let any = |set: &BTreeSet<&SkillId>| siblings.iter().any(|id| set.contains(id));
    let explicitly_excluded = policy.is_some_and(|p| any(p.excluded));
    if let Some(policy) = policy {
        stages[3].1 = if explicitly_excluded {
            Excluded(Reason::Excluded)
        } else if any(policy.already_loaded) {
            Excluded(Reason::AlreadyLoaded)
        } else {
            Passed
        };
    }
    if !explicitly_excluded {
        stages[4].1 = match retrieval {
            RetrievalView::Admitted { ids, .. } if any(ids) => Passed,
            RetrievalView::Admitted { .. } | RetrievalView::Empty => Excluded(Reason::NotRetrieved),
            RetrievalView::NotEvaluated | RetrievalView::Failed => NotEvaluated,
        };
    }
    Trace { stages }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Counts {
    pub skills: usize,
    pub bindings: usize,
    pub verified: usize,
    pub shadowed: usize,
    pub ambiguous: usize,
    pub unverified: usize,
    pub manual_only: usize,
    pub forbidden: usize,
    /// Skills with an agent-invocable verified binding.
    pub advisory: usize,
    /// `None` when local policy or retrieval was not evaluated.
    pub policy_excluded: Option<usize>,
    pub already_loaded: Option<usize>,
    pub retrieved: Option<usize>,
}

/// Roster-level evidence bound to one snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RosterEvidence {
    pub version: &'static str,
    pub snapshot: ContentHash,
    pub partial: bool,
    pub counts: Counts,
    /// Discovery outcome counts by stable code, including normal missing roots.
    pub source_causes: BTreeMap<&'static str, usize>,
    /// Records excluded while resolving discovered files, by stable code.
    pub record_causes: BTreeMap<&'static str, usize>,
    /// At most [`MAX_DETAILS`] exclusions in stable ID order.
    pub details: Vec<(SkillId, Reason)>,
    pub omitted_details: usize,
}

pub(crate) const fn source_code(diagnostic: &Diagnostic) -> &'static str {
    match diagnostic {
        Diagnostic::RootMissing(_) => "root-missing",
        Diagnostic::RootUnreadable(_) => "root-unreadable",
        Diagnostic::SourceNotEnumerated(_) => "source-not-enumerated",
        Diagnostic::DirectoryUnreadable(_) => "directory-unreadable",
        Diagnostic::SymlinkedDirectorySkipped(_) => "symlinked-directory-skipped",
        Diagnostic::DepthLimitReached(_) => "depth-limit",
        Diagnostic::EntryUnreadable(_) => "entry-unreadable",
        Diagnostic::EntryLimitReached => "entry-limit",
        Diagnostic::ByteLimitReached => "byte-limit",
    }
}

pub(crate) const fn record_code(error: ResolutionError) -> &'static str {
    match error {
        ResolutionError::UnsupportedLayout => "unsupported-layout",
        ResolutionError::Metadata => "malformed-metadata",
        ResolutionError::Read => "unreadable",
        ResolutionError::ChangedFile => "changed-during-read",
        ResolutionError::Limit => "limit",
        ResolutionError::InvalidBinding => "invalid-binding",
        ResolutionError::DuplicateId => "duplicate-id",
        ResolutionError::Cancelled => "cancelled",
        ResolutionError::Deadline => "deadline",
        ResolutionError::IneligibleOption
        | ResolutionError::DuplicateOption
        | ResolutionError::UnknownOption => "option-map",
    }
}

fn frame(bytes: &mut Vec<u8>, part: &[u8]) {
    bytes.extend_from_slice(&(part.len() as u64).to_le_bytes());
    bytes.extend_from_slice(part);
}

/// A digest of the resolved membership: every binding's identity, source,
/// callable name, visibility and restrictions, and each file's content hash.
pub fn snapshot_id(roster: &ResolvedRoster) -> ContentHash {
    let mut skills: Vec<_> = roster.skills().iter().collect();
    skills.sort_by(|a, b| a.record().id.cmp(&b.record().id));
    let mut bytes = Vec::new();
    frame(&mut bytes, b"skillranker.roster-snapshot.v1");
    frame(&mut bytes, &(skills.len() as u64).to_le_bytes());
    for skill in skills {
        frame(&mut bytes, skill.record().id.as_str().as_bytes());
        frame(
            &mut bytes,
            skill.record().source_content.as_str().as_bytes(),
        );
        let mut bindings: Vec<_> = skill.bindings().iter().collect();
        bindings.sort_by(|a, b| a.id.cmp(&b.id));
        frame(&mut bytes, &(bindings.len() as u64).to_le_bytes());
        for binding in bindings {
            frame(&mut bytes, binding.id.as_str().as_bytes());
            frame(&mut bytes, binding.source.as_str().as_bytes());
            frame(&mut bytes, binding.invocation.as_str().as_bytes());
            let visibility = match &binding.visibility {
                Visibility::Verified { contract_version } => format!("verified:{contract_version}"),
                Visibility::Shadowed { winner } => format!("shadowed:{}", winner.as_str()),
                Visibility::Ambiguous => "ambiguous".to_owned(),
                Visibility::Unverified => "unverified".to_owned(),
            };
            frame(&mut bytes, visibility.as_bytes());
            frame(
                &mut bytes,
                &[
                    u8::from(binding.restrictions.agent_invocable),
                    u8::from(binding.restrictions.user_invocable),
                ],
            );
        }
    }
    let partial = [u8::from(roster.is_partial())];
    frame(&mut bytes, &partial);
    ContentHash::from_bytes(&bytes)
}

/// Summarize the whole roster. Per-binding exclusions are listed by stable
/// ID; local policy and retrieval contribute only when they were evaluated.
pub fn summarize(
    roster: &ResolvedRoster,
    policy: Option<&PolicyView<'_>>,
    retrieval: &RetrievalView<'_>,
) -> RosterEvidence {
    let mut counts = Counts {
        skills: roster.skills().len(),
        ..Counts::default()
    };
    let mut exclusions = Vec::new();
    for skill in roster.skills() {
        let mut advisory = false;
        for binding in skill.bindings() {
            counts.bindings += 1;
            let trace = trace(roster, policy, retrieval, &binding.id);
            match trace.outcome(Stage::Visibility) {
                StageOutcome::Excluded(Reason::Shadowed) => counts.shadowed += 1,
                StageOutcome::Excluded(Reason::Ambiguous) => counts.ambiguous += 1,
                StageOutcome::Excluded(Reason::Unverified) => counts.unverified += 1,
                _ => counts.verified += 1,
            }
            match trace.outcome(Stage::Restrictions) {
                StageOutcome::Excluded(Reason::ManualOnly) => counts.manual_only += 1,
                StageOutcome::Excluded(Reason::Forbidden) => counts.forbidden += 1,
                StageOutcome::Passed => advisory = true,
                _ => {}
            }
            if let Some(reason) = trace.decisive() {
                exclusions.push((binding.id.clone(), reason));
            }
        }
        counts.advisory += usize::from(advisory);
    }
    if policy.is_some() {
        let removed = |reason| exclusions.iter().filter(|(_, r)| *r == reason).count();
        counts.policy_excluded = Some(removed(Reason::Excluded));
        counts.already_loaded = Some(removed(Reason::AlreadyLoaded));
    }
    counts.retrieved = match retrieval {
        RetrievalView::Admitted { ids, .. } => Some(ids.len()),
        RetrievalView::Empty => Some(0),
        RetrievalView::NotEvaluated | RetrievalView::Failed => None,
    };
    exclusions.sort();
    let omitted_details = exclusions.len().saturating_sub(MAX_DETAILS);
    exclusions.truncate(MAX_DETAILS);
    let mut source_causes = BTreeMap::new();
    for diagnostic in roster.source_diagnostics() {
        *source_causes.entry(source_code(diagnostic)).or_insert(0) += 1;
    }
    let mut record_causes = BTreeMap::new();
    for (_, error) in roster.diagnostics() {
        *record_causes.entry(record_code(*error)).or_insert(0) += 1;
    }
    RosterEvidence {
        version: EVIDENCE_VERSION,
        snapshot: snapshot_id(roster),
        partial: roster.is_partial(),
        counts,
        source_causes,
        record_causes,
        details: exclusions,
        omitted_details,
    }
}
