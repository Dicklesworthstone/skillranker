//! Publication revalidation (P2). A decision depends on more than the skills it
//! returns: the whole membership and precedence of the scope it searched, the
//! content of every indexed or wide candidate, and the content and
//! restrictions of the entire shortlist, including candidates later removed.
//! A changed wide candidate outside the shortlist, a new overflow match or a
//! new shadowing source can invalidate a decision whose returned skills are
//! unchanged.
//!
//! No adapter here offers a trusted generation covering all of that, so
//! revalidation always re-enumerates: it builds a fresh plan (re-opening roots
//! by path), re-resolves within the invocation deadline, and compares a digest
//! of the dependencies. Size and modification time are never used. A scan is
//! an observation, not a freeze; the harness still validates its later load.

use super::discovery::{DiscoveryPlan, claude_code_plan};
use super::evidence::{record_code, source_code};
use super::resolution::{ResolutionError, ResolvedRoster, resolve_claude_plan};
use super::{InvocationRestrictions, Visibility};
use crate::identity::{ContentHash, SkillId};
use crate::limits::MonotonicMillis;
use crate::output::ErrorKind;
use crate::runtime::EntryClock;
use asupersync::Cx;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub const REVALIDATION_VERSION: &str = "roster-revalidation-v1";

/// Causes that mean part of the scope could not be observed.
const INCOMPLETE_CAUSES: &[&str] = &[
    "root-unreadable",
    "directory-unreadable",
    "entry-unreadable",
    "symlinked-directory-skipped",
    "depth-limit",
    "entry-limit",
    "byte-limit",
    "unreadable",
    "changed-during-read",
    "limit",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RevalidationError {
    /// A dependency differs: `unavailable / roster-changed`.
    Changed,
    /// Part of the scope the decision used could not be re-observed.
    Incomplete,
    /// The invocation deadline ran out; revalidation never extends it.
    Deadline,
    Cancelled,
    /// A declared dependency is not in the captured roster.
    UnknownDependency,
}

impl RevalidationError {
    /// Output-contract kind. Cancellation follows the signal path.
    pub const fn kind(self) -> Option<ErrorKind> {
        match self {
            Self::Changed => Some(ErrorKind::RosterChanged),
            Self::Incomplete => Some(ErrorKind::IncompleteRoster),
            Self::Deadline => Some(ErrorKind::Timeout),
            Self::Cancelled => None,
            Self::UnknownDependency => Some(ErrorKind::UnusableRoster),
        }
    }
}

/// The dependencies of one decision, captured from the roster it used.
#[derive(Clone, Debug)]
pub struct Dependencies {
    digest: ContentHash,
    content_scope: BTreeSet<SkillId>,
    incomplete: BTreeMap<&'static str, usize>,
    captured_at: MonotonicMillis,
}

impl Dependencies {
    pub fn captured_at(&self) -> MonotonicMillis {
        self.captured_at
    }
}

/// The capture and last-validation boundaries of a validated decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Validated {
    pub captured_at: MonotonicMillis,
    pub validated_at: MonotonicMillis,
}

fn frame(bytes: &mut Vec<u8>, part: &[u8]) {
    bytes.extend_from_slice(&(part.len() as u64).to_le_bytes());
    bytes.extend_from_slice(part);
}

fn restriction_bits(r: InvocationRestrictions) -> [u8; 2] {
    [u8::from(r.agent_invocable), u8::from(r.user_invocable)]
}

/// Membership, precedence and restrictions of every binding; content of the
/// files in scope; and the set of records excluded while resolving.
fn digest(roster: &ResolvedRoster, content_scope: &BTreeSet<SkillId>) -> ContentHash {
    let mut bytes = Vec::new();
    frame(&mut bytes, REVALIDATION_VERSION.as_bytes());
    frame(&mut bytes, &[u8::from(roster.is_partial())]);
    let mut skills: Vec<_> = roster.skills().iter().collect();
    skills.sort_by(|a, b| a.record().id.cmp(&b.record().id));
    frame(&mut bytes, &(skills.len() as u64).to_le_bytes());
    for skill in skills {
        let record = skill.record();
        frame(&mut bytes, record.id.as_str().as_bytes());
        let content = if content_scope.contains(&record.id) {
            record.source_content.as_str()
        } else {
            "-"
        };
        frame(&mut bytes, content.as_bytes());
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
            frame(&mut bytes, &restriction_bits(binding.restrictions));
        }
    }
    let mut excluded: Vec<&str> = roster
        .diagnostics()
        .iter()
        .map(|(_, error)| record_code(*error))
        .filter(|code| !INCOMPLETE_CAUSES.contains(code))
        .collect();
    excluded.sort_unstable();
    for code in excluded {
        frame(&mut bytes, code.as_bytes());
    }
    ContentHash::from_bytes(&bytes)
}

fn incomplete_causes(roster: &ResolvedRoster) -> BTreeMap<&'static str, usize> {
    let mut causes = BTreeMap::new();
    let codes = roster
        .source_diagnostics()
        .iter()
        .map(source_code)
        .chain(roster.diagnostics().iter().map(|(_, e)| record_code(*e)));
    for code in codes.filter(|code| INCOMPLETE_CAUSES.contains(code)) {
        *causes.entry(code).or_insert(0) += 1;
    }
    causes
}

/// Capture a decision's dependencies. `content` names every binding whose
/// file content the decision used: all indexed or wide candidates, the whole
/// shortlist including candidates removed later, and explicit targets.
/// Membership, precedence and restrictions of the whole roster are always
/// dependencies.
pub fn capture<'a>(
    roster: &ResolvedRoster,
    content: impl IntoIterator<Item = &'a SkillId>,
    clock: &EntryClock,
) -> Result<Dependencies, RevalidationError> {
    let mut content_scope = BTreeSet::new();
    for id in content {
        let record = roster
            .skills()
            .iter()
            .find(|skill| skill.bindings().iter().any(|b| &b.id == id))
            .ok_or(RevalidationError::UnknownDependency)?;
        content_scope.insert(record.record().id.clone());
    }
    Ok(Dependencies {
        digest: digest(roster, &content_scope),
        content_scope,
        incomplete: incomplete_causes(roster),
        captured_at: clock.now(),
    })
}

/// Compare a freshly resolved roster with the captured dependencies.
pub fn revalidate(
    captured: &Dependencies,
    fresh: &ResolvedRoster,
    clock: &EntryClock,
) -> Result<Validated, RevalidationError> {
    clock
        .admit_new_work()
        .map_err(|_| RevalidationError::Deadline)?;
    let incomplete = incomplete_causes(fresh);
    if incomplete
        .iter()
        .any(|(code, count)| captured.incomplete.get(code).is_none_or(|was| count > was))
    {
        return Err(RevalidationError::Incomplete);
    }
    if digest(fresh, &captured.content_scope) != captured.digest {
        return Err(RevalidationError::Changed);
    }
    Ok(Validated {
        captured_at: captured.captured_at,
        validated_at: clock.now(),
    })
}

fn resolution_error(error: ResolutionError) -> RevalidationError {
    match error {
        ResolutionError::Deadline => RevalidationError::Deadline,
        ResolutionError::Cancelled => RevalidationError::Cancelled,
        _ => RevalidationError::Incomplete,
    }
}

/// Revalidate against a plan the caller has just built, never the plan the
/// decision used: its open descriptors would miss a replaced root.
pub fn revalidate_plan(
    captured: &Dependencies,
    fresh_plan: &DiscoveryPlan,
    overrides: &BTreeMap<String, InvocationRestrictions>,
    cx: &Cx,
    clock: &EntryClock,
) -> Result<Validated, RevalidationError> {
    let fresh = resolve_claude_plan(fresh_plan, overrides, cx, clock).map_err(resolution_error)?;
    revalidate(captured, &fresh, clock)
}

/// Re-open the documented Claude roots and revalidate against them.
pub fn revalidate_claude(
    captured: &Dependencies,
    workspace: &Path,
    user_home: Option<&Path>,
    visibility: Visibility,
    overrides: &BTreeMap<String, InvocationRestrictions>,
    cx: &Cx,
    clock: &EntryClock,
) -> Result<Validated, RevalidationError> {
    clock
        .admit_new_work()
        .map_err(|_| RevalidationError::Deadline)?;
    let plan = claude_code_plan(workspace, user_home, visibility)
        .map_err(|_| RevalidationError::Incomplete)?;
    revalidate_plan(captured, &plan, overrides, cx, clock)
}
