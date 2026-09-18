//! Local eligibility before scoring (P4). Explicit resolution comes first and
//! bypasses every advisory stage. Advisory candidates then pass, in order:
//! invocation restrictions and explicit exclusions, proven reusable-reference
//! reuse, the minimum fit, and a strict per-candidate check against `__none__`.
//! No stage reads priors, phase or blended scores, so none of those can re-admit
//! a candidate an earlier stage removed. Scoring the survivors is separate.

use crate::context::{
    ActiveBranch, LoadedSkillRecord, SkillUsageKind, evaluate_loaded_skill_eligibility,
};
use crate::identity::SkillId;
use crate::output::ErrorKind;
use crate::roster::UsageKind;
use crate::roster::explicit::{ExplicitResolutionResult, ResolvedExplicitSkill, UnresolvedRecord};
use crate::roster::resolution::AdvisorySkill;
use std::collections::{BTreeMap, BTreeSet};

/// The documented default minimum fit; the effective value comes from config.
pub const DEFAULT_FIT_THRESHOLD: f64 = 0.30;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AbstainReason {
    /// Fit filtering removed the last candidates.
    LowFit,
    /// Every remaining candidate is a proven-present reusable reference.
    AlreadyLoaded,
    /// Restrictions or explicit exclusions removed every candidate.
    Excluded,
    /// No remaining candidate's own Choice probability beat `__none__`.
    NoShortlistMatch,
}

impl AbstainReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LowFit => "low-fit",
            Self::AlreadyLoaded => "already-loaded",
            Self::Excluded => "excluded",
            Self::NoShortlistMatch => "no-shortlist-match",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnavailableReason {
    /// An explicit reference was missing, ambiguous, forbidden or conflicting.
    ExplicitResolution,
    /// The roster offered no advisory candidate at all: an operational failure.
    EmptyRoster,
    /// The evaluated candidates no longer match the shortlist they were asked about.
    RosterChanged,
}

impl UnavailableReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitResolution => "explicit-resolution",
            Self::EmptyRoster => "empty-roster",
            Self::RosterChanged => "roster-changed",
        }
    }
    pub const fn kind(self) -> ErrorKind {
        match self {
            Self::ExplicitResolution => ErrorKind::UnresolvedExplicit,
            Self::EmptyRoster => ErrorKind::EmptyRoster,
            Self::RosterChanged => ErrorKind::RosterChanged,
        }
    }
}

/// A decision reached locally, with no further provider request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verdict {
    Abstain(AbstainReason),
    Unavailable(UnavailableReason),
}

/// The first stage that removed one candidate, for explanations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Exclusion {
    Restricted,
    Excluded,
    AlreadyLoaded,
    LowFit,
    NotAboveNone,
}

/// Explicit resolution is decided before any advisory stage.
#[derive(Debug)]
pub enum Route<'a> {
    /// Every explicit reference resolved: emit them and call no provider.
    /// Manual-only references keep their kind; nothing here grants a load.
    Explicit(&'a [ResolvedExplicitSkill]),
    /// An explicit reference failed: unavailable, and no provider call.
    Unresolved(&'a [UnresolvedRecord]),
    /// No explicit request: continue with advisory eligibility.
    Advisory { excluded: BTreeSet<&'a SkillId> },
}

pub fn route(explicit: &ExplicitResolutionResult) -> Route<'_> {
    match explicit {
        ExplicitResolutionResult::Resolved { skills, .. } => Route::Explicit(skills),
        ExplicitResolutionResult::Unavailable { unresolved } => Route::Unresolved(unresolved),
        ExplicitResolutionResult::NoneSpecified { excluded_skills } => Route::Advisory {
            excluded: excluded_skills.iter().collect(),
        },
    }
}

/// Loaded-state evidence for the resolved active branch. Without a resolved
/// branch nothing is proven present, so nothing is suppressed.
#[derive(Clone, Copy, Debug)]
pub struct LoadedState<'a> {
    pub branch: Option<&'a ActiveBranch>,
    pub records: &'a [LoadedSkillRecord],
}

/// Only a reusable reference whose complete content is proven present in the
/// current epoch at a matching version is suppressed. Workflows, unknown kinds,
/// forked skills and dynamically rendered skills always stay eligible.
fn proven_loaded(skill: &AdvisorySkill<'_>, loaded: LoadedState<'_>) -> bool {
    let record = skill.record;
    if record.usage_kind != UsageKind::Reference || record.forked_context || record.dynamic_content
    {
        return false;
    }
    evaluate_loaded_skill_eligibility(
        &skill.binding.id,
        SkillUsageKind::Reference,
        Some(&record.source_content),
        record.rendered_content.as_ref(),
        loaded.branch,
        loaded.records,
    )
    .is_suppressed()
}

/// Candidates surviving the policy stages, plus what each stage removed.
#[derive(Debug)]
pub struct Admission<'a> {
    pub admitted: Vec<AdvisorySkill<'a>>,
    pub removed: Vec<(SkillId, Exclusion)>,
    /// `Some` exactly when nothing was admitted.
    pub verdict: Option<Verdict>,
}

/// Apply restrictions, exclusions and reference reuse. Run it before the wide
/// call so that an all-excluded or all-loaded roster abstains with no provider
/// request, and again on the shortlist after rerank.
pub fn admit<'a>(
    candidates: &[AdvisorySkill<'a>],
    excluded: &BTreeSet<&SkillId>,
    loaded: LoadedState<'_>,
) -> Admission<'a> {
    if candidates.is_empty() {
        return Admission {
            admitted: Vec::new(),
            removed: Vec::new(),
            verdict: Some(Verdict::Unavailable(UnavailableReason::EmptyRoster)),
        };
    }
    let mut removed = Vec::new();
    let mut policy = Vec::with_capacity(candidates.len());
    for skill in candidates {
        let id = &skill.binding.id;
        if !skill.binding.restrictions.agent_invocable {
            removed.push((id.clone(), Exclusion::Restricted));
        } else if excluded.contains(id) || excluded.contains(&skill.record.id) {
            removed.push((id.clone(), Exclusion::Excluded));
        } else {
            policy.push(*skill);
        }
    }
    if policy.is_empty() {
        return Admission {
            admitted: policy,
            removed,
            verdict: Some(Verdict::Abstain(AbstainReason::Excluded)),
        };
    }
    let mut admitted = Vec::with_capacity(policy.len());
    for skill in policy {
        if proven_loaded(&skill, loaded) {
            removed.push((skill.binding.id.clone(), Exclusion::AlreadyLoaded));
        } else {
            admitted.push(skill);
        }
    }
    let verdict = admitted
        .is_empty()
        .then_some(Verdict::Abstain(AbstainReason::AlreadyLoaded));
    Admission {
        admitted,
        removed,
        verdict,
    }
}

/// One candidate's raw provider estimates, keyed by skill after the request's
/// option map resolved them. `rerank` is the candidate's own raw Choice value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Estimate {
    pub fit: f64,
    pub rerank: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct Eligible<'a> {
    pub skill: AdvisorySkill<'a>,
    pub fit: f64,
    pub rerank: f64,
}

#[derive(Debug)]
pub struct Evaluation<'a> {
    pub eligible: Vec<Eligible<'a>>,
    pub removed: Vec<(SkillId, Exclusion)>,
    /// `Some` exactly when nothing is eligible.
    pub verdict: Option<Verdict>,
}

/// Decide eligibility after rerank. `estimates` must cover exactly the
/// shortlist; any other membership means the shortlist changed and the result
/// is unavailable. Comparisons fail closed: a non-finite estimate is removed.
pub fn after_rerank<'a>(
    shortlist: &[AdvisorySkill<'a>],
    estimates: &BTreeMap<SkillId, Estimate>,
    none: f64,
    fit_threshold: f64,
    excluded: &BTreeSet<&SkillId>,
    loaded: LoadedState<'_>,
) -> Evaluation<'a> {
    let unavailable = |reason| Evaluation {
        eligible: Vec::new(),
        removed: Vec::new(),
        verdict: Some(Verdict::Unavailable(reason)),
    };
    let members: BTreeSet<&SkillId> = shortlist.iter().map(|s| &s.binding.id).collect();
    if shortlist.is_empty()
        || members.len() != shortlist.len()
        || members.len() != estimates.len()
        || !estimates.keys().all(|id| members.contains(id))
    {
        return unavailable(UnavailableReason::RosterChanged);
    }
    let Admission {
        admitted,
        mut removed,
        verdict,
    } = admit(shortlist, excluded, loaded);
    if verdict.is_some() {
        return Evaluation {
            eligible: Vec::new(),
            removed,
            verdict,
        };
    }
    let mut fitting = Vec::with_capacity(admitted.len());
    for skill in admitted {
        let estimate = estimates[&skill.binding.id];
        if estimate.fit >= fit_threshold {
            fitting.push((skill, estimate));
        } else {
            removed.push((skill.binding.id.clone(), Exclusion::LowFit));
        }
    }
    if fitting.is_empty() {
        return Evaluation {
            eligible: Vec::new(),
            removed,
            verdict: Some(Verdict::Abstain(AbstainReason::LowFit)),
        };
    }
    // Each candidate must beat none on its own raw probability; ties abstain.
    let mut eligible = Vec::with_capacity(fitting.len());
    for (skill, estimate) in fitting {
        if estimate.rerank > none {
            eligible.push(Eligible {
                skill,
                fit: estimate.fit,
                rerank: estimate.rerank,
            });
        } else {
            removed.push((skill.binding.id.clone(), Exclusion::NotAboveNone));
        }
    }
    let verdict = eligible
        .is_empty()
        .then_some(Verdict::Abstain(AbstainReason::NoShortlistMatch));
    Evaluation {
        eligible,
        removed,
        verdict,
    }
}
