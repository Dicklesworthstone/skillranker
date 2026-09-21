//! Local identity, collision and invocation resolution over authorized skill bytes.
//! Inputs carrying harness visibility must come from a trusted adapter, never YAML.

mod discovery_scope;

use super::discovery::{Diagnostic, DiscoveryPlan, SourceKind};
use super::{
    DisplayName, InvocationKind, InvocationName, InvocationRestrictions, LoadTarget, SkillAlias,
    SkillRecord, Visibility, parse_skill_metadata,
};
use crate::authorized_read::{AuthorizedRoots, BoundedRead, FileIdentity};
use crate::identity::{LogicalSkillKey, OptionId, SkillId, SourceId};
use crate::limits::{
    DISCOVERY_FILES, DISCOVERY_PARSED_BYTES, LimitUnit, ResourceLimit, SKILL_FILE_BYTES,
};
use crate::runtime::EntryClock;
use asupersync::Cx;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolutionError {
    InvalidBinding,
    DuplicateId,
    ChangedFile,
    UnsupportedLayout,
    Read,
    /// The file exceeded the per-skill read bound. Distinct from `Read`: the
    /// entry is present and readable, just too large to parse, and saying
    /// "unreadable" sends the owner looking for a permissions fault.
    Oversized,
    Metadata,
    Limit,
    Cancelled,
    Deadline,
    IneligibleOption,
    DuplicateOption,
    UnknownOption,
}
impl fmt::Display for ResolutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "roster resolution: {self:?}")
    }
}
impl std::error::Error for ResolutionError {}

/// Adapter-owned assertions. `Some(priority)` is comparable only within the
/// same verified contract. Unknown precedence must use `None`.
#[derive(Clone)]
pub struct BindingSpec {
    pub source: SourceId,
    pub logical_key: LogicalSkillKey,
    pub invocation: InvocationName,
    pub priority: Option<i32>,
    pub visibility: Visibility,
    pub restrictions: InvocationRestrictions,
}

#[derive(Clone, Debug)]
pub struct Binding {
    pub id: SkillId,
    pub source: SourceId,
    pub invocation: InvocationName,
    pub visibility: Visibility,
    pub restrictions: InvocationRestrictions,
    pub priority: Option<i32>,
}

/// Constructed only by parsing the very bytes whose identity/hash were read.
#[derive(Clone, Debug)]
pub struct SkillEntry {
    record: SkillRecord,
    binding: Binding,
    identity: FileIdentity,
    bytes: usize,
}
impl SkillEntry {
    pub fn from_read(spec: BindingSpec, read: BoundedRead) -> Result<Self, ResolutionError> {
        match &spec.visibility {
            Visibility::Verified { contract_version }
                if !contract_version.is_empty() && contract_version.len() <= 512 => {}
            Visibility::Unverified => {}
            _ => return Err(ResolutionError::InvalidBinding),
        }
        let metadata = parse_skill_metadata(read.bytes()).map_err(|_| ResolutionError::Metadata)?;
        let id = SkillId::from_source(&spec.source, &spec.logical_key);
        let restrictions = InvocationRestrictions {
            agent_invocable: spec.restrictions.agent_invocable && metadata.agent_invocable,
            user_invocable: spec.restrictions.user_invocable && metadata.user_invocable,
        };
        let binding = Binding {
            id: id.clone(),
            source: spec.source.clone(),
            invocation: spec.invocation.clone(),
            visibility: spec.visibility.clone(),
            restrictions,
            priority: spec.priority,
        };
        let record = SkillRecord {
            id,
            source: spec.source,
            source_priority: spec.priority.unwrap_or(0),
            display_name: DisplayName::from_text(
                metadata.name.as_deref().unwrap_or(spec.invocation.as_str()),
            ),
            invocation_name: spec.invocation,
            target: LoadTarget::File(read.path().clone()),
            source_content: read.content_hash().clone(),
            rendered_content: None,
            visibility: spec.visibility,
            restrictions,
            usage_kind: metadata.usage_kind,
            forked_context: metadata.forked_context,
            dynamic_content: metadata.dynamic_content,
            aliases: Vec::new(),
            description_full: metadata.description_full,
            description_short: metadata.description_short,
            body_excerpt: metadata.body_excerpt,
            body_window: metadata.body_window,
            tags: metadata.tags,
            phases: metadata.phases,
            parse_warnings: metadata.parse_warnings,
        };
        Ok(Self {
            record,
            binding,
            identity: read.identity(),
            bytes: read.len(),
        })
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedSkill {
    record: SkillRecord,
    bindings: Vec<Binding>,
}
impl ResolvedSkill {
    /// Canonical representative; use a binding for invocation/visibility decisions.
    pub fn record(&self) -> &SkillRecord {
        &self.record
    }
    pub fn bindings(&self) -> &[Binding] {
        &self.bindings
    }
}
#[derive(Clone, Copy, Debug)]
pub struct AdvisorySkill<'a> {
    pub record: &'a SkillRecord,
    pub binding: &'a Binding,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExactResolution<'a> {
    Resolved {
        id: &'a SkillId,
        invocation: &'a InvocationName,
        kind: InvocationKind,
    },
    Missing,
    Ambiguous,
    Shadowed,
    Unverified,
    Forbidden,
}

#[derive(Clone, Default)]
pub struct ResolvedRoster {
    skills: Vec<ResolvedSkill>,
    ids: BTreeMap<SkillId, (usize, usize)>,
    names: BTreeMap<String, Vec<(usize, usize)>>,
    partial: bool,
    diagnostics: Vec<(usize, ResolutionError)>,
    sources: Vec<Diagnostic>,
}
impl fmt::Debug for ResolvedRoster {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedRoster")
            .field("skills", &self.skills.len())
            .field("partial", &self.partial)
            .field("diagnostics", &self.diagnostics)
            .field("sources", &self.sources.len())
            .finish()
    }
}
fn budget(cx: &Cx, clock: &EntryClock) -> Result<(), ResolutionError> {
    if cx.is_cancel_requested() {
        return Err(ResolutionError::Cancelled);
    }
    clock
        .admit_new_work()
        .map(|_| ())
        .map_err(|_| ResolutionError::Deadline)
}
fn eligible(binding: &Binding) -> bool {
    matches!(binding.visibility, Visibility::Verified { .. })
        && binding.restrictions.agent_invocable
}
impl ResolvedRoster {
    pub fn resolve(
        mut entries: Vec<SkillEntry>,
        partial: bool,
        cx: &Cx,
        clock: &EntryClock,
    ) -> Result<Self, ResolutionError> {
        budget(cx, clock)?;
        DISCOVERY_FILES
            .check(entries.len())
            .map_err(|_| ResolutionError::Limit)?;
        let total = entries
            .iter()
            .try_fold(0usize, |sum, entry| sum.checked_add(entry.bytes))
            .ok_or(ResolutionError::Limit)?;
        DISCOVERY_PARSED_BYTES
            .check(total)
            .map_err(|_| ResolutionError::Limit)?;
        entries.sort_by(|a, b| a.binding.id.cmp(&b.binding.id));
        let mut roster = Self {
            partial,
            ..Self::default()
        };
        let mut physical = BTreeMap::<FileIdentity, usize>::new();
        for entry in entries {
            budget(cx, clock)?;
            if roster.ids.contains_key(&entry.binding.id) {
                return Err(ResolutionError::DuplicateId);
            }
            let s = match physical.get(&entry.identity) {
                Some(&s) => {
                    if roster.skills[s].record.source_content != entry.record.source_content {
                        return Err(ResolutionError::ChangedFile);
                    }
                    roster.skills[s].record.aliases.push(SkillAlias {
                        source: entry.binding.source.clone(),
                        invocation: entry.binding.invocation.clone(),
                    });
                    s
                }
                None => {
                    let s = roster.skills.len();
                    physical.insert(entry.identity, s);
                    roster.skills.push(ResolvedSkill {
                        record: entry.record,
                        bindings: Vec::new(),
                    });
                    s
                }
            };
            let b = roster.skills[s].bindings.len();
            roster.ids.insert(entry.binding.id.clone(), (s, b));
            roster
                .names
                .entry(entry.binding.invocation.as_str().to_owned())
                .or_default()
                .push((s, b));
            roster.skills[s].bindings.push(entry.binding);
        }
        for positions in roster.names.values() {
            budget(cx, clock)?;
            resolve_collision(&mut roster.skills, positions);
        }
        for skill in &mut roster.skills {
            skill.record.visibility = skill.bindings[0].visibility.clone();
            skill.record.restrictions = skill.bindings[0].restrictions;
        }
        budget(cx, clock)?;
        Ok(roster)
    }
    pub fn skills(&self) -> &[ResolvedSkill] {
        &self.skills
    }
    pub fn is_partial(&self) -> bool {
        self.partial
    }
    pub fn diagnostics(&self) -> &[(usize, ResolutionError)] {
        &self.diagnostics
    }
    /// Source-level discovery outcomes (missing, unreadable or unenumerated
    /// roots and stopped walks). Empty for rosters not built from a plan.
    pub fn source_diagnostics(&self) -> &[Diagnostic] {
        &self.sources
    }
    pub fn advisory(&self) -> impl Iterator<Item = AdvisorySkill<'_>> {
        self.skills.iter().filter_map(|skill| {
            skill
                .bindings
                .iter()
                .find(|b| eligible(b))
                .map(|binding| AdvisorySkill {
                    record: &skill.record,
                    binding,
                })
        })
    }
    pub fn exact_id(&self, id: &SkillId) -> ExactResolution<'_> {
        self.ids
            .get(id)
            .map_or(ExactResolution::Missing, |&(s, b)| {
                exact(&self.skills[s].bindings[b])
            })
    }
    pub fn exact_name(&self, name: &str) -> ExactResolution<'_> {
        let Some(positions) = self.names.get(name) else {
            return ExactResolution::Missing;
        };
        // Collision resolution ensures any verified result refers to one file.
        positions
            .iter()
            .map(|&(s, b)| &self.skills[s].bindings[b])
            .find(|b| matches!(b.visibility, Visibility::Verified { .. }))
            .map_or_else(
                || exact(&self.skills[positions[0].0].bindings[positions[0].1]),
                exact,
            )
    }
}
fn exact(binding: &Binding) -> ExactResolution<'_> {
    match binding.visibility {
        Visibility::Ambiguous => ExactResolution::Ambiguous,
        Visibility::Shadowed { .. } => ExactResolution::Shadowed,
        Visibility::Unverified => ExactResolution::Unverified,
        Visibility::Verified { .. } => match binding.restrictions.explicit_kind() {
            InvocationKind::Forbidden => ExactResolution::Forbidden,
            kind => ExactResolution::Resolved {
                id: &binding.id,
                invocation: &binding.invocation,
                kind,
            },
        },
    }
}
fn resolve_collision(skills: &mut [ResolvedSkill], positions: &[(usize, usize)]) {
    let files: BTreeSet<_> = positions.iter().map(|p| p.0).collect();
    if files.len() == 1 {
        // Same callable target reached twice cannot weaken an effective override.
        let restrictions = positions.iter().fold(
            InvocationRestrictions {
                agent_invocable: true,
                user_invocable: true,
            },
            |mut r, &(s, b)| {
                r.agent_invocable &= skills[s].bindings[b].restrictions.agent_invocable;
                r.user_invocable &= skills[s].bindings[b].restrictions.user_invocable;
                r
            },
        );
        for &(s, b) in positions {
            skills[s].bindings[b].restrictions = restrictions;
        }
        return;
    }
    let first = &skills[positions[0].0].bindings[positions[0].1];
    let comparable = matches!(first.visibility, Visibility::Verified { .. })
        && positions.iter().all(|&(s, b)| {
            let binding = &skills[s].bindings[b];
            binding.visibility == first.visibility && binding.priority.is_some()
        });
    let highest = positions
        .iter()
        .filter_map(|&(s, b)| skills[s].bindings[b].priority)
        .max();
    let winners: BTreeSet<_> = positions
        .iter()
        .filter(|&&(s, b)| skills[s].bindings[b].priority == highest)
        .map(|p| p.0)
        .collect();
    let winner = if comparable && winners.len() == 1 {
        winners.first().copied()
    } else {
        None
    };
    let winner_id = winner.map(|s| skills[s].record.id.clone());
    let restrictions = positions
        .iter()
        .filter(|&&(s, b)| Some(s) == winner && skills[s].bindings[b].priority == highest)
        .fold(
            InvocationRestrictions {
                agent_invocable: true,
                user_invocable: true,
            },
            |mut r, &(s, b)| {
                r.agent_invocable &= skills[s].bindings[b].restrictions.agent_invocable;
                r.user_invocable &= skills[s].bindings[b].restrictions.user_invocable;
                r
            },
        );
    for &(s, b) in positions {
        let binding = &mut skills[s].bindings[b];
        binding.visibility = match &winner_id {
            Some(id) if Some(s) != winner || binding.priority != highest => {
                Visibility::Shadowed { winner: id.clone() }
            }
            Some(_) => {
                binding.restrictions = restrictions;
                binding.visibility.clone()
            }
            None => Visibility::Ambiguous,
        };
    }
}

/// Claude's supported direct layout, `<name>/SKILL.md` in a project or personal
/// root, yielding the callable name. Discovery and explicit import share it so
/// an imported record cannot claim a layout or name discovery would not produce.
pub(crate) fn claude_invocation(kind: SourceKind, relative: &Path) -> Option<InvocationName> {
    let components: Vec<_> = relative.components().collect();
    let supported = matches!(kind, SourceKind::Project | SourceKind::User)
        && components.len() == 2
        && components[1].as_os_str() == "SKILL.md";
    components
        .first()
        .and_then(|part| part.as_os_str().to_str())
        .filter(|n| !n.eq_ignore_ascii_case("synced") && !n.contains([':', '\\']))
        .and_then(|n| InvocationName::new(n).ok())
        .filter(|_| supported)
}

/// The same declared path always yields the same logical key, so discovery and
/// explicit import assign equal stable IDs to equal records.
pub(crate) fn path_logical_key(path: &Path) -> Result<LogicalSkillKey, ResolutionError> {
    LogicalSkillKey::new(
        blake3::hash(path.as_os_str().as_bytes())
            .to_hex()
            .to_string(),
    )
    .map_err(|_| ResolutionError::InvalidBinding)
}

/// Resolve direct Claude project/personal skill directories. Caller supplies
/// trusted effective settings as restrict-only overrides keyed by callable name.
/// Unsupported layouts are excluded with partial coverage, never guessed.
pub fn resolve_claude_plan(
    plan: &DiscoveryPlan,
    overrides: &BTreeMap<String, InvocationRestrictions>,
    cx: &Cx,
    clock: &EntryClock,
) -> Result<ResolvedRoster, ResolutionError> {
    budget(cx, clock)?;
    if plan.harness().as_str() != "claude_code" {
        return Err(ResolutionError::InvalidBinding);
    }
    let discovery = plan.discover();
    budget(cx, clock)?;
    let roots = AuthorizedRoots::new(
        plan.roots()
            .iter()
            .filter_map(|r| r.root())
            .map(|r| r.try_clone())
            .collect::<Result<_, _>>()
            .map_err(|_| ResolutionError::Read)?,
    );
    let mut entries = Vec::new();
    let mut diagnostics = Vec::new();
    let mut total = 0usize;
    let mut withheld_names = BTreeSet::new();
    for (index, candidate) in discovery.candidates().iter().enumerate() {
        budget(cx, clock)?;
        let Some(invocation) = claude_invocation(candidate.kind(), candidate.relative()) else {
            diagnostics.push((index, ResolutionError::UnsupportedLayout));
            // The verified direct-layout contract gives this path no callable
            // name. It cannot shadow a supported binding. Keep the exclusion
            // visible without revoking authority from unrelated skills.
            continue;
        };
        let remaining = DISCOVERY_PARSED_BYTES.max().saturating_sub(total);
        if remaining == 0 {
            diagnostics.push((index, ResolutionError::Limit));
            withheld_names.insert(invocation.as_str().to_owned());
            continue;
        }
        let limit = ResourceLimit::try_new(
            "roster_read",
            LimitUnit::Bytes,
            remaining.min(SKILL_FILE_BYTES.max()),
        )
        .map_err(|_| ResolutionError::Limit)?;
        let read = match roots.read_absolute(candidate.path().as_path(), limit) {
            Ok(read) => read,
            Err(error) => {
                diagnostics.push((
                    index,
                    match error {
                        crate::authorized_read::ReadError::TooLarge { .. } => {
                            ResolutionError::Oversized
                        }
                        _ => ResolutionError::Read,
                    },
                ));
                withheld_names.insert(invocation.as_str().to_owned());
                continue;
            }
        };
        total += read.len();
        budget(cx, clock)?;
        if read.identity() != candidate.identity() {
            diagnostics.push((index, ResolutionError::ChangedFile));
            withheld_names.insert(invocation.as_str().to_owned());
            continue;
        }
        let logical_key = path_logical_key(candidate.path().as_path())?;
        let restrictions =
            overrides
                .get(invocation.as_str())
                .copied()
                .unwrap_or(InvocationRestrictions {
                    agent_invocable: true,
                    user_invocable: true,
                });
        let spec = BindingSpec {
            source: candidate.source().clone(),
            logical_key,
            invocation: invocation.clone(),
            priority: Some(candidate.priority()),
            visibility: candidate.visibility().clone(),
            restrictions,
        };
        match SkillEntry::from_read(spec, read) {
            Ok(entry) => entries.push(entry),
            Err(error) => {
                diagnostics.push((index, error));
                withheld_names.insert(invocation.as_str().to_owned());
            }
        }
    }
    // Partial discovery is not proof of a complete roster, but neither is it
    // proof that every observed name has an unknown competitor. Verify each
    // name's exact direct-layout slot in every supported root before retaining
    // its authority. This never admits a skipped file or follows directory links.
    let discovery_needs_proof = discovery.diagnostics().iter().any(|d| {
        !matches!(
            d,
            Diagnostic::RootMissing(_) | Diagnostic::SourceNotEnumerated(_)
        )
    });
    if discovery_needs_proof {
        let proof = discovery_scope::prove_names(plan, &entries, &withheld_names, cx, clock)?;
        if !proof.limited.is_empty() {
            for (index, candidate) in discovery.candidates().iter().enumerate() {
                budget(cx, clock)?;
                if claude_invocation(candidate.kind(), candidate.relative())
                    .is_some_and(|name| proof.limited.contains(name.as_str()))
                {
                    diagnostics.push((index, ResolutionError::Limit));
                }
            }
        }
        withheld_names.extend(proof.withheld);
    }
    for entry in &mut entries {
        if withheld_names.contains(entry.binding.invocation.as_str()) {
            entry.binding.visibility = Visibility::Unverified;
            entry.record.visibility = Visibility::Unverified;
        }
    }
    let mut roster = ResolvedRoster::resolve(
        entries,
        discovery.is_partial() || !diagnostics.is_empty(),
        cx,
        clock,
    )?;
    roster.diagnostics = diagnostics;
    roster.sources = discovery.diagnostics().to_vec();
    Ok(roster)
}

/// Request-local map. Select eligible *binding* IDs, not display names or paths.
#[derive(Debug)]
pub struct OptionMap<'a> {
    entries: BTreeMap<OptionId, AdvisorySkill<'a>>,
}
#[derive(Debug)]
pub enum ResolvedOption<'a> {
    None,
    Skill(AdvisorySkill<'a>),
}
impl<'a> OptionMap<'a> {
    pub fn new(roster: &'a ResolvedRoster, selected: &[SkillId]) -> Result<Self, ResolutionError> {
        if selected.len() > 254 {
            return Err(ResolutionError::Limit);
        }
        let mut seen = BTreeSet::new();
        let mut candidates = Vec::new();
        for id in selected {
            let &(s, b) = roster
                .ids
                .get(id)
                .ok_or(ResolutionError::IneligibleOption)?;
            let skill = &roster.skills[s];
            let binding = &skill.bindings[b];
            if !eligible(binding) {
                return Err(ResolutionError::IneligibleOption);
            }
            if !seen.insert(s) {
                return Err(ResolutionError::DuplicateOption);
            }
            candidates.push(AdvisorySkill {
                record: &skill.record,
                binding,
            });
        }
        candidates.sort_by(|a, b| a.record.id.cmp(&b.record.id));
        let entries = candidates
            .into_iter()
            .enumerate()
            .map(|(index, skill)| {
                OptionId::new(format!("o{index:03}"))
                    .map(|id| (id, skill))
                    .map_err(|_| ResolutionError::InvalidBinding)
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { entries })
    }
    pub fn entries(&self) -> &BTreeMap<OptionId, AdvisorySkill<'a>> {
        &self.entries
    }
    pub fn resolve(&self, value: &str) -> Result<ResolvedOption<'a>, ResolutionError> {
        if value == "__none__" {
            return Ok(ResolvedOption::None);
        }
        if value.len() > crate::identity::MAX_ID_BYTES {
            return Err(ResolutionError::UnknownOption);
        }
        let id = OptionId::new(value).map_err(|_| ResolutionError::UnknownOption)?;
        self.entries
            .get(&id)
            .copied()
            .map(ResolvedOption::Skill)
            .ok_or(ResolutionError::UnknownOption)
    }
}
