use skillranker::context::tool::{SimpleSkillResolver, SkillMatch, extract_loaded_skill_records};
use skillranker::context::{
    BranchResolutionTarget, EventKind, LoadedSkillRecord, NormalizedEvent, PrivateText, Role,
    SkillUsageKind, ToolEvent, ToolStatus, resolve_active_branch,
};
use skillranker::eligibility::{
    AbstainReason, Estimate, Exclusion, LoadedState, Route, UnavailableReason, Verdict, admit,
    after_rerank, route,
};
use skillranker::identity::{BranchId, ContentHash, EventId, SkillId, ToolCallId, TurnId};
use skillranker::limits::DurationMillis;
use skillranker::roster::discovery::claude_code_plan;
use skillranker::roster::explicit::{ExplicitResolutionRequest, resolve_explicit_requirements};
use skillranker::roster::resolution::{AdvisorySkill, ResolvedRoster, resolve_claude_plan};
use skillranker::roster::{InvocationKind, Visibility};
use skillranker::runtime::{EntryClock, ProcessInvocation};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

const THRESHOLD: f64 = 0.30;

fn tree() -> PathBuf {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "sr-eligibility-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&path).unwrap();
    path // Retained for inspection.
}

/// A real Claude roster resolved from files, and the bytes of each skill.
struct Roster {
    roster: ResolvedRoster,
    bytes: BTreeMap<&'static str, String>,
}

const SKILLS: &[(&str, &str)] = &[
    ("alpha", ""),
    ("beta", ""),
    ("docs", "usage: reference\n"),
    ("flow", "usage: workflow\n"),
    ("forky", "usage: reference\ncontext: fork\n"),
    ("dyn", "usage: reference\n"),
    (
        "manual",
        "usage: reference\ndisable-model-invocation: true\n",
    ),
];

impl Roster {
    fn new() -> Self {
        let workspace = tree();
        let mut bytes = BTreeMap::new();
        for (name, extra) in SKILLS {
            let body = if *name == "dyn" {
                "Run with $ARGUMENTS."
            } else {
                "Static guidance."
            };
            let text = format!(
                "---\nname: {name}\ndescription: {name} skill.\n{extra}---\n# {name}\n\n{body}\n"
            );
            let path = workspace.join(".claude/skills").join(name).join("SKILL.md");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, &text).unwrap();
            bytes.insert(*name, text);
        }
        let visibility = Visibility::Verified {
            contract_version: "test-adapter-v1".into(),
        };
        let plan = claude_code_plan(&workspace, None, visibility).unwrap();
        let clock = EntryClock::capture_with(
            DurationMillis::new("test_total", 30_000, 30_000).unwrap(),
            DurationMillis::new("test_cleanup", 200, 30_000).unwrap(),
        )
        .unwrap();
        let runtime = ProcessInvocation::from_clock(clock).unwrap();
        let cx = runtime.request_cx().unwrap();
        let roster = resolve_claude_plan(&plan, &BTreeMap::new(), &cx, &clock).unwrap();
        Self { roster, bytes }
    }
    fn skill(&self, name: &str) -> AdvisorySkill<'_> {
        self.roster
            .advisory()
            .find(|s| s.binding.invocation.as_str() == name)
            .unwrap_or_else(|| panic!("{name} is not advisory"))
    }
    fn id(&self, name: &str) -> SkillId {
        self.skill(name).binding.id.clone()
    }
    fn hash(&self, name: &str) -> ContentHash {
        ContentHash::from_bytes(self.bytes[name].as_bytes())
    }
    fn pick(&self, names: &[&str]) -> Vec<AdvisorySkill<'_>> {
        names.iter().map(|name| self.skill(name)).collect()
    }
}

fn estimates(roster: &Roster, values: &[(&str, f64, f64)]) -> BTreeMap<SkillId, Estimate> {
    values
        .iter()
        .map(|&(name, fit, rerank)| (roster.id(name), Estimate { fit, rerank }))
        .collect()
}

const NO_LOADS: LoadedState<'static> = LoadedState {
    branch: None,
    records: &[],
};

fn removed_ids(removed: &[(SkillId, Exclusion)], roster: &Roster) -> Vec<(String, Exclusion)> {
    let names: BTreeMap<SkillId, &str> = SKILLS
        .iter()
        .filter(|(name, _)| *name != "manual")
        .map(|(name, _)| (roster.id(name), *name))
        .collect();
    removed
        .iter()
        .map(|(id, why)| (names[id].to_owned(), *why))
        .collect()
}

#[test]
fn fit_filtering_cannot_leave_a_candidate_that_does_not_beat_none() {
    let r = Roster::new();
    let shortlist = r.pick(&["alpha", "beta"]);
    let outcome = after_rerank(
        &shortlist,
        &estimates(&r, &[("alpha", 0.10, 0.70), ("beta", 0.80, 0.10)]),
        0.20,
        THRESHOLD,
        &BTreeSet::new(),
        NO_LOADS,
    );
    assert!(outcome.eligible.is_empty());
    assert_eq!(
        outcome.verdict,
        Some(Verdict::Abstain(AbstainReason::NoShortlistMatch))
    );
    assert_eq!(
        removed_ids(&outcome.removed, &r),
        vec![
            ("alpha".to_owned(), Exclusion::LowFit),
            ("beta".to_owned(), Exclusion::NotAboveNone),
        ]
    );
}

#[test]
fn each_returned_candidate_beats_none_on_its_own_probability() {
    let r = Roster::new();
    let shortlist = r.pick(&["alpha", "beta"]);
    let outcome = after_rerank(
        &shortlist,
        &estimates(&r, &[("alpha", 0.31, 0.70), ("beta", 0.999, 0.10)]),
        0.20,
        THRESHOLD,
        &BTreeSet::new(),
        NO_LOADS,
    );
    assert_eq!(outcome.verdict, None);
    assert_eq!(outcome.eligible.len(), 1);
    assert_eq!(outcome.eligible[0].skill.binding.id, r.id("alpha"));
    assert_eq!(outcome.eligible[0].fit, 0.31);
    assert_eq!(outcome.eligible[0].rerank, 0.70);
    assert_eq!(
        removed_ids(&outcome.removed, &r),
        vec![("beta".to_owned(), Exclusion::NotAboveNone)]
    );
}

#[test]
fn ties_favor_abstention_and_the_fit_threshold_is_inclusive() {
    let r = Roster::new();
    let shortlist = r.pick(&["alpha"]);
    let run = |fit: f64, rerank: f64| {
        after_rerank(
            &shortlist,
            &estimates(&r, &[("alpha", fit, rerank)]),
            0.20,
            THRESHOLD,
            &BTreeSet::new(),
            NO_LOADS,
        )
        .verdict
    };
    assert_eq!(run(0.30, 0.21), None, "fit equal to the threshold survives");
    assert_eq!(
        run(0.2999, 0.9),
        Some(Verdict::Abstain(AbstainReason::LowFit))
    );
    assert_eq!(
        run(0.9, 0.20),
        Some(Verdict::Abstain(AbstainReason::NoShortlistMatch)),
        "a tie with none abstains"
    );
    assert_eq!(
        run(f64::NAN, 0.9),
        Some(Verdict::Abstain(AbstainReason::LowFit))
    );
    assert_eq!(
        run(0.9, f64::NAN),
        Some(Verdict::Abstain(AbstainReason::NoShortlistMatch))
    );
}

#[test]
fn a_changed_shortlist_is_unavailable() {
    let r = Roster::new();
    let shortlist = r.pick(&["alpha", "beta"]);
    let cases = [
        estimates(&r, &[("alpha", 0.9, 0.9)]),
        estimates(
            &r,
            &[("alpha", 0.9, 0.9), ("beta", 0.9, 0.9), ("docs", 0.9, 0.9)],
        ),
        estimates(&r, &[("alpha", 0.9, 0.9), ("docs", 0.9, 0.9)]),
    ];
    for estimates in cases {
        let outcome = after_rerank(
            &shortlist,
            &estimates,
            0.1,
            THRESHOLD,
            &BTreeSet::new(),
            NO_LOADS,
        );
        assert_eq!(
            outcome.verdict,
            Some(Verdict::Unavailable(UnavailableReason::RosterChanged))
        );
    }
}

/// Events for one skill load on the main branch, optionally followed by a
/// compaction, ending in a user message that is the branch leaf.
struct Session {
    events: Vec<NormalizedEvent>,
    resolver: SimpleSkillResolver,
}

fn event(id: &str, parent: Option<&str>, kind: EventKind, branch: Option<&str>) -> NormalizedEvent {
    NormalizedEvent {
        event_id: Some(EventId::new(id).unwrap()),
        parent_id: parent.map(|p| EventId::new(p).unwrap()),
        turn_id: Some(TurnId::new(format!("turn-{id}")).unwrap()),
        agent_id: None,
        branch_id: branch.map(|b| BranchId::new(b).unwrap()),
        role: Role::User,
        kind,
        timestamp_unix_ms: None,
        text: PrivateText::new("text"),
        tool: None,
    }
}

fn load(id: &str, parent: &str, tool: &str, branch: Option<&str>) -> [NormalizedEvent; 2] {
    let tool_event = |status, result: Option<&str>| {
        Some(ToolEvent {
            call_id: Some(ToolCallId::new(format!("call-{id}")).unwrap()),
            name: PrivateText::new(tool),
            status,
            arguments: None,
            result: result.map(PrivateText::new),
        })
    };
    let mut call = event(id, Some(parent), EventKind::ToolInvocation, branch);
    call.role = Role::Assistant;
    call.tool = tool_event(ToolStatus::Attempted, None);
    let result_id = format!("{id}-result");
    let mut result = event(&result_id, Some(id), EventKind::ToolResult, branch);
    result.role = Role::Tool;
    result.tool = tool_event(ToolStatus::Succeeded, Some("loaded"));
    [call, result]
}

impl Session {
    fn new() -> Self {
        Self {
            events: vec![event("root", None, EventKind::Message, None)],
            resolver: SimpleSkillResolver::new(),
        }
    }
    fn last(&self) -> String {
        self.events
            .last()
            .unwrap()
            .event_id
            .as_ref()
            .unwrap()
            .as_str()
            .to_owned()
    }
    /// Record a successful load of `skill` whose observed evidence is `evidence`.
    fn load(mut self, r: &Roster, name: &str, evidence: SkillMatch) -> Self {
        let tool = format!("skill-{name}-{}", self.events.len());
        let SkillMatch { skill_id, .. } = &evidence;
        assert_eq!(skill_id, &r.id(name));
        self.resolver.register_tool(tool.clone(), evidence);
        let parent = self.last();
        let id = format!("load-{}", self.events.len());
        self.events.extend(load(&id, &parent, &tool, None));
        self
    }
    fn compact(mut self) -> Self {
        let parent = self.last();
        let id = format!("compact-{}", self.events.len());
        self.events
            .push(event(&id, Some(&parent), EventKind::Compaction, None));
        self
    }
    /// Resolve the active branch at a fresh leaf and extract its loads through
    /// the production extraction path.
    fn state(mut self) -> (skillranker::context::ActiveBranch, Vec<LoadedSkillRecord>) {
        let parent = self.last();
        self.events
            .push(event("leaf", Some(&parent), EventKind::Message, None));
        let target = BranchResolutionTarget {
            target_event_id: Some(EventId::new("leaf").unwrap()),
            target_branch_id: None,
            target_agent_id: None,
        };
        let branch = resolve_active_branch(&self.events, &target)
            .active_branch()
            .unwrap()
            .clone();
        let records = extract_loaded_skill_records(
            &self.events,
            &self.resolver,
            Some(&branch),
            &branch.current_epoch,
        );
        (branch, records)
    }
}

fn evidence(r: &Roster, name: &str) -> SkillMatch {
    SkillMatch {
        skill_id: r.id(name),
        usage_kind: SkillUsageKind::Reference,
        source_content: Some(r.hash(name)),
        rendered_content: None,
        has_dynamic_arguments: false,
        turn_scoped: false,
    }
}

fn admitted_names(r: &Roster, names: &[&str], session: Session) -> (Vec<String>, Option<Verdict>) {
    let (branch, records) = session.state();
    let loaded = LoadedState {
        branch: Some(&branch),
        records: &records,
    };
    let admission = admit(&r.pick(names), &BTreeSet::new(), loaded);
    let admitted = admission
        .admitted
        .iter()
        .map(|s| s.binding.invocation.as_str().to_owned())
        .collect();
    (admitted, admission.verdict)
}

#[test]
fn a_proven_present_reference_abstains_before_any_provider_call() {
    let r = Roster::new();
    let loaded = || Session::new().load(&r, "docs", evidence(&r, "docs"));
    assert_eq!(
        admitted_names(&r, &["docs"], loaded()),
        (vec![], Some(Verdict::Abstain(AbstainReason::AlreadyLoaded)))
    );
    assert_eq!(
        admitted_names(&r, &["docs", "alpha"], loaded()),
        (vec!["alpha".to_owned()], None)
    );
    // After rerank the same evidence removes it from the shortlist too.
    let (branch, records) = loaded().state();
    let outcome = after_rerank(
        &r.pick(&["docs", "alpha"]),
        &estimates(&r, &[("docs", 0.9, 0.6), ("alpha", 0.9, 0.3)]),
        0.1,
        THRESHOLD,
        &BTreeSet::new(),
        LoadedState {
            branch: Some(&branch),
            records: &records,
        },
    );
    assert_eq!(outcome.eligible.len(), 1);
    assert_eq!(outcome.eligible[0].skill.binding.id, r.id("alpha"));
}

#[test]
fn compaction_invalidates_reference_reuse_through_real_extraction() {
    let r = Roster::new();
    let before = Session::new()
        .load(&r, "docs", evidence(&r, "docs"))
        .compact();
    assert_eq!(
        admitted_names(&r, &["docs"], before),
        (vec!["docs".to_owned()], None),
        "a load before compaction is not proven present"
    );
    let after = Session::new()
        .compact()
        .load(&r, "docs", evidence(&r, "docs"));
    assert_eq!(
        admitted_names(&r, &["docs"], after),
        (vec![], Some(Verdict::Abstain(AbstainReason::AlreadyLoaded))),
        "a load after the latest compaction is proven present"
    );
}

/// A label and the change it makes to otherwise-proving load evidence.
type EvidenceChange = (&'static str, fn(&mut SkillMatch));

#[test]
fn unknown_or_changed_versions_arguments_and_turn_scope_prove_nothing() {
    let r = Roster::new();
    let variants: [EvidenceChange; 4] = [
        ("unknown version", |m| m.source_content = None),
        ("older version", |m| {
            m.source_content = Some(ContentHash::from_bytes(b"old"))
        }),
        ("dynamic arguments", |m| m.has_dynamic_arguments = true),
        ("turn scoped", |m| m.turn_scoped = true),
    ];
    for (label, change) in variants {
        let mut observed = evidence(&r, "docs");
        change(&mut observed);
        let session = Session::new().load(&r, "docs", observed);
        assert_eq!(
            admitted_names(&r, &["docs"], session),
            (vec!["docs".to_owned()], None),
            "{label}"
        );
    }
    // A record without an identifying event cannot be placed on the branch.
    let (branch, mut records) = Session::new()
        .load(&r, "docs", evidence(&r, "docs"))
        .state();
    let loaded = |records: &[LoadedSkillRecord]| {
        admit(
            &r.pick(&["docs"]),
            &BTreeSet::new(),
            LoadedState {
                branch: Some(&branch),
                records,
            },
        )
        .verdict
    };
    assert_eq!(
        loaded(&records),
        Some(Verdict::Abstain(AbstainReason::AlreadyLoaded))
    );
    records[0].event_id = None;
    assert_eq!(loaded(&records), None);
}

#[test]
fn forked_dynamic_and_sibling_branch_loads_never_suppress() {
    let r = Roster::new();
    for name in ["forky", "dyn"] {
        let session = Session::new().load(&r, name, evidence(&r, name));
        assert_eq!(
            admitted_names(&r, &[name], session),
            (vec![name.to_owned()], None),
            "{name}"
        );
    }
    // A load recorded on another fork of the conversation is not in this context.
    let mut session = Session::new();
    session
        .resolver
        .register_tool("sibling-docs", evidence(&r, "docs"));
    session
        .events
        .extend(load("side", "root", "sibling-docs", Some("other")));
    // The active leaf descends from root through the main line, not the fork.
    session
        .events
        .push(event("main", Some("root"), EventKind::Message, None));
    assert_eq!(
        admitted_names(&r, &["docs"], session),
        (vec!["docs".to_owned()], None)
    );
}

#[test]
fn workflows_and_unknown_usage_stay_repeatable() {
    let r = Roster::new();
    for name in ["flow", "alpha"] {
        let mut observed = evidence(&r, name);
        observed.usage_kind = if name == "flow" {
            SkillUsageKind::Workflow
        } else {
            SkillUsageKind::Unknown
        };
        let session = Session::new().load(&r, name, observed);
        assert_eq!(
            admitted_names(&r, &[name], session),
            (vec![name.to_owned()], None),
            "{name}"
        );
    }
}

#[test]
fn exclusions_abstain_and_an_empty_roster_is_unavailable() {
    let r = Roster::new();
    let candidates = r.pick(&["alpha", "beta"]);
    let alpha = r.id("alpha");
    let beta = r.id("beta");
    let one = admit(&candidates, &BTreeSet::from([&alpha]), NO_LOADS);
    assert_eq!(one.admitted.len(), 1);
    assert_eq!(one.verdict, None);
    let all = admit(&candidates, &BTreeSet::from([&alpha, &beta]), NO_LOADS);
    assert_eq!(all.verdict, Some(Verdict::Abstain(AbstainReason::Excluded)));
    let none = admit(&[], &BTreeSet::new(), NO_LOADS);
    assert_eq!(
        none.verdict,
        Some(Verdict::Unavailable(UnavailableReason::EmptyRoster))
    );
    assert_eq!(UnavailableReason::EmptyRoster.as_str(), "empty-roster");
    assert_eq!(
        AbstainReason::NoShortlistMatch.as_str(),
        "no-shortlist-match"
    );
}

#[test]
fn explicit_resolution_comes_first_and_manual_only_is_not_bypassed() {
    let r = Roster::new();
    // The manual-only skill is never an advisory candidate.
    assert!(
        r.roster
            .advisory()
            .all(|s| s.binding.invocation.as_str() != "manual")
    );
    let request = |names: &[&str]| ExplicitResolutionRequest {
        cli_required_skills: names.iter().map(|n| n.to_string()).collect(),
        ..ExplicitResolutionRequest::default()
    };
    let resolved = resolve_explicit_requirements(&request(&["manual"]), &r.roster).unwrap();
    match route(&resolved) {
        Route::Explicit(skills) => {
            assert_eq!(skills.len(), 1);
            assert_eq!(skills[0].kind, InvocationKind::ManualOnly);
            assert!(skills[0].manual_only);
        }
        other => panic!("expected explicit route, got {other:?}"),
    }
    let missing = resolve_explicit_requirements(&request(&["ghost"]), &r.roster).unwrap();
    assert!(matches!(route(&missing), Route::Unresolved(records) if records.len() == 1));
    let advisory = resolve_explicit_requirements(&request(&[]), &r.roster).unwrap();
    assert!(matches!(route(&advisory), Route::Advisory { .. }));
    assert_eq!(
        UnavailableReason::ExplicitResolution.kind().as_str(),
        "unresolved-explicit"
    );
}
