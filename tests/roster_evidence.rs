use skillranker::authorized_read::{AuthorizedRoot, AuthorizedRoots};
use skillranker::identity::{LogicalSkillKey, SkillId, SourceId};
use skillranker::limits::{DurationMillis, SKILL_FILE_BYTES};
use skillranker::roster::discovery::claude_code_plan;
use skillranker::roster::evidence::{
    Hint, MAX_DETAILS, PolicyView, Reason, RetrievalView, Stage, StageOutcome, snapshot_id,
    summarize, trace,
};
use skillranker::roster::resolution::{
    BindingSpec, ResolvedRoster, SkillEntry, resolve_claude_plan,
};
use skillranker::roster::retrieval::{
    QueryInput, RetrievalBudget, RetrievalError, RetrievalMethod, retrieve,
};
use skillranker::roster::{InvocationName, InvocationRestrictions, Visibility};
use skillranker::runtime::{EntryClock, ProcessInvocation};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use StageOutcome::{Excluded, NotEvaluated, Passed};

fn runtime() -> (EntryClock, ProcessInvocation, asupersync::Cx) {
    let clock = EntryClock::capture_with(
        DurationMillis::new("test", 30_000, 30_000).unwrap(),
        DurationMillis::new("cleanup", 500, 30_000).unwrap(),
    )
    .unwrap();
    let runtime = ProcessInvocation::from_clock(clock).unwrap();
    let cx = runtime.request_cx().unwrap();
    (clock, runtime, cx)
}

fn tree() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "sr-evidence-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&path).unwrap();
    path // Retained for inspection.
}

const SOURCE: &str = "fixture";

struct Spec {
    key: String,
    name: String,
    description: String,
    priority: Option<i32>,
    verified: bool,
    agent: bool,
    user: bool,
}

fn spec(key: &str, name: &str, description: &str) -> Spec {
    Spec {
        key: key.to_owned(),
        name: name.to_owned(),
        description: description.to_owned(),
        priority: Some(1),
        verified: true,
        agent: true,
        user: true,
    }
}

fn id(key: &str) -> SkillId {
    SkillId::from_source(
        &SourceId::new(SOURCE).unwrap(),
        &LogicalSkillKey::new(key).unwrap(),
    )
}

fn entry(root: &Path, spec: &Spec) -> SkillEntry {
    let file = format!("{}.md", spec.key);
    fs::write(
        root.join(&file),
        format!(
            "---\nname: {}\ndescription: {}\n---\nbody\n",
            spec.name, spec.description
        ),
    )
    .unwrap();
    let roots = AuthorizedRoots::single(AuthorizedRoot::open_absolute(root).unwrap());
    SkillEntry::from_read(
        BindingSpec {
            source: SourceId::new(SOURCE).unwrap(),
            logical_key: LogicalSkillKey::new(spec.key.as_str()).unwrap(),
            invocation: InvocationName::new(spec.name.as_str()).unwrap(),
            priority: spec.priority,
            visibility: if spec.verified {
                Visibility::Verified {
                    contract_version: "fixture-v1".into(),
                }
            } else {
                Visibility::Unverified
            },
            restrictions: InvocationRestrictions {
                agent_invocable: spec.agent,
                user_invocable: spec.user,
            },
        },
        roots
            .read_bounded(0, Path::new(&file), SKILL_FILE_BYTES)
            .unwrap(),
    )
    .unwrap()
}

/// A roster with every exclusion reason plus 255 fillers, so retrieval must
/// use the lexical filter rather than the small-roster bypass.
fn fixture(root: &Path) -> Vec<SkillEntry> {
    let mut specs = vec![
        spec("target", "target", "telescope"),
        spec("excluded", "excluded", "telescope"),
        spec("loaded", "loaded", "telescope"),
        Spec {
            priority: Some(2),
            ..spec("dup-winner", "dup", "telescope")
        },
        spec("dup-loser", "dup", "telescope"),
        Spec {
            priority: None,
            ..spec("amb-a", "amb", "telescope")
        },
        Spec {
            priority: None,
            ..spec("amb-b", "amb", "telescope")
        },
        Spec {
            verified: false,
            ..spec("unverified", "unverified", "telescope")
        },
        Spec {
            agent: false,
            ..spec("manual", "manual", "telescope")
        },
        Spec {
            agent: false,
            user: false,
            ..spec("forbidden", "forbidden", "telescope")
        },
    ];
    for i in 0..255 {
        let name = format!("fill{i:03}");
        specs.push(spec(&name, &name, "sharedterm"));
    }
    specs.iter().map(|s| entry(root, s)).collect()
}

fn roster(root: &Path) -> ResolvedRoster {
    let (clock, _runtime, cx) = runtime();
    ResolvedRoster::resolve(fixture(root), false, &cx, &clock).unwrap()
}

fn query(text: &str) -> QueryInput<'_> {
    QueryInput {
        latest_request: text,
        active_task: "",
        recent_errors: "",
    }
}

fn stages(outcomes: [StageOutcome; 5]) -> [(Stage, StageOutcome); 5] {
    let mut out = Stage::ALL.map(|stage| (stage, NotEvaluated));
    for (slot, outcome) in out.iter_mut().zip(outcomes) {
        slot.1 = outcome;
    }
    out
}

#[test]
fn each_reason_is_traced_with_a_success_counterpart() {
    let root = tree();
    let roster = roster(&root);
    let (clock, runtime, cx) = runtime();
    let excluded_id = id("excluded");
    let loaded_id = id("loaded");
    let excluded = BTreeSet::from([&excluded_id]);
    let loaded = BTreeSet::from([&loaded_id]);
    let policy = PolicyView {
        excluded: &excluded,
        already_loaded: &loaded,
    };
    let selection = runtime
        .runtime()
        .block_on(retrieve(
            &roster,
            &excluded_id_set(&excluded_id),
            query("telescope"),
            RetrievalBudget::default(),
            &cx,
            &clock,
        ))
        .unwrap();
    assert_eq!(
        selection.diagnostics.method,
        Some(RetrievalMethod::QuillBm25)
    );
    let view = RetrievalView::from_selection(&selection);
    let cases = [
        ("target", [Passed, Passed, Passed, Passed, Passed]),
        (
            "excluded",
            [
                Passed,
                Passed,
                Passed,
                Excluded(Reason::Excluded),
                NotEvaluated,
            ],
        ),
        // A loaded reference still reports the admission retrieval gave it.
        (
            "loaded",
            [
                Passed,
                Passed,
                Passed,
                Excluded(Reason::AlreadyLoaded),
                Passed,
            ],
        ),
        (
            "dup-loser",
            [
                Passed,
                Excluded(Reason::Shadowed),
                NotEvaluated,
                NotEvaluated,
                NotEvaluated,
            ],
        ),
        ("dup-winner", [Passed, Passed, Passed, Passed, Passed]),
        (
            "amb-a",
            [
                Passed,
                Excluded(Reason::Ambiguous),
                NotEvaluated,
                NotEvaluated,
                NotEvaluated,
            ],
        ),
        (
            "unverified",
            [
                Passed,
                Excluded(Reason::Unverified),
                NotEvaluated,
                NotEvaluated,
                NotEvaluated,
            ],
        ),
        (
            "manual",
            [
                Passed,
                Passed,
                Excluded(Reason::ManualOnly),
                NotEvaluated,
                NotEvaluated,
            ],
        ),
        (
            "forbidden",
            [
                Passed,
                Passed,
                Excluded(Reason::Forbidden),
                NotEvaluated,
                NotEvaluated,
            ],
        ),
        (
            "fill000",
            [
                Passed,
                Passed,
                Passed,
                Passed,
                Excluded(Reason::NotRetrieved),
            ],
        ),
    ];
    for (key, expected) in cases {
        let traced = trace(&roster, Some(&policy), &view, &id(key));
        assert_eq!(traced.stages, stages(expected), "{key}");
    }
    let unknown = trace(&roster, Some(&policy), &view, &id("never-discovered"));
    assert_eq!(
        unknown.stages,
        stages([
            Excluded(Reason::NotInSnapshot),
            NotEvaluated,
            NotEvaluated,
            NotEvaluated,
            NotEvaluated
        ])
    );
    assert_eq!(
        trace(&roster, Some(&policy), &view, &id("target")).decisive(),
        None
    );
    assert!(runtime.shutdown());
}

fn excluded_id_set(id: &SkillId) -> BTreeSet<SkillId> {
    BTreeSet::from([id.clone()])
}

#[test]
fn unrun_or_failed_stages_are_not_evaluated_rather_than_misses() {
    let root = tree();
    let roster = roster(&root);
    let target = id("target");
    // No policy and no retrieval: only the stages that ran are reported.
    let traced = trace(&roster, None, &RetrievalView::NotEvaluated, &target);
    assert_eq!(
        traced.stages,
        stages([Passed, Passed, Passed, NotEvaluated, NotEvaluated])
    );
    assert_eq!(traced.decisive(), None);

    let (clock, runtime, cx) = runtime();
    // A genuine empty match is a retrieval miss.
    let empty = runtime
        .runtime()
        .block_on(retrieve(
            &roster,
            &BTreeSet::new(),
            query("unmatchednonsense"),
            RetrievalBudget::default(),
            &cx,
            &clock,
        ))
        .unwrap_err();
    assert_eq!(empty.kind, RetrievalError::RetrievalEmpty);
    let view = RetrievalView::from_failure(&empty);
    assert_eq!(view, RetrievalView::Empty);
    assert_eq!(
        trace(&roster, None, &view, &target).outcome(Stage::Retrieval),
        Excluded(Reason::NotRetrieved)
    );
    // An operational failure evaluated nothing.
    runtime.cancel_user(&cx);
    let cancelled = runtime
        .runtime()
        .block_on(retrieve(
            &roster,
            &BTreeSet::new(),
            query("telescope"),
            RetrievalBudget::default(),
            &cx,
            &clock,
        ))
        .unwrap_err();
    assert_eq!(cancelled.kind, RetrievalError::Cancelled);
    let view = RetrievalView::from_failure(&cancelled);
    assert_eq!(view, RetrievalView::Failed);
    assert_eq!(
        trace(&roster, None, &view, &target).outcome(Stage::Retrieval),
        NotEvaluated
    );
    assert!(runtime.shutdown());
}

#[test]
fn multiple_causes_and_identical_decisions_keep_their_own_reasons() {
    let root = tree();
    let roster = roster(&root);
    let loser = id("dup-loser");
    let excluded_id = id("excluded");
    let excluded = BTreeSet::from([&loser, &excluded_id]);
    let loaded = BTreeSet::new();
    let policy = PolicyView {
        excluded: &excluded,
        already_loaded: &loaded,
    };
    let (clock, runtime, cx) = runtime();
    let selection = runtime
        .runtime()
        .block_on(retrieve(
            &roster,
            &BTreeSet::from([loser.clone(), excluded_id.clone()]),
            query("sharedterm"),
            RetrievalBudget::default(),
            &cx,
            &clock,
        ))
        .unwrap();
    let view = RetrievalView::from_selection(&selection);
    // Shadowed and excluded: the first decisive stage wins, later stages did not run.
    let both = trace(&roster, Some(&policy), &view, &loser);
    assert_eq!(both.decisive(), Some(Reason::Shadowed));
    assert_eq!(both.outcome(Stage::LocalPolicy), NotEvaluated);
    // None of these is in the candidate set, each for a different reason.
    let retrieved: BTreeSet<_> = selection.candidates.iter().map(|s| &s.binding.id).collect();
    let mut causes = BTreeMap::new();
    for key in ["dup-loser", "excluded", "target", "manual"] {
        assert!(!retrieved.contains(&id(key)), "{key}");
        causes.insert(
            key,
            trace(&roster, Some(&policy), &view, &id(key)).decisive(),
        );
    }
    assert_eq!(causes["dup-loser"], Some(Reason::Shadowed));
    assert_eq!(causes["excluded"], Some(Reason::Excluded));
    assert_eq!(causes["target"], Some(Reason::NotRetrieved));
    assert_eq!(causes["manual"], Some(Reason::ManualOnly));
    assert!(runtime.shutdown());
}

#[test]
fn summaries_count_bind_to_a_snapshot_and_bound_details() {
    let root = tree();
    let roster = roster(&root);
    let excluded_id = id("excluded");
    let loaded_id = id("loaded");
    let excluded = BTreeSet::from([&excluded_id]);
    let loaded = BTreeSet::from([&loaded_id]);
    let policy = PolicyView {
        excluded: &excluded,
        already_loaded: &loaded,
    };
    let (clock, runtime, cx) = runtime();
    let selection = runtime
        .runtime()
        .block_on(retrieve(
            &roster,
            &excluded_id_set(&excluded_id),
            query("telescope"),
            RetrievalBudget::default(),
            &cx,
            &clock,
        ))
        .unwrap();
    let view = RetrievalView::from_selection(&selection);
    let summary = summarize(&roster, Some(&policy), &view);
    let counts = &summary.counts;
    assert_eq!(counts.skills, 265);
    assert_eq!(counts.bindings, 265);
    assert_eq!(
        (counts.shadowed, counts.ambiguous, counts.unverified),
        (1, 2, 1)
    );
    assert_eq!((counts.manual_only, counts.forbidden), (1, 1));
    assert_eq!(counts.verified, 265 - 4);
    assert_eq!(counts.advisory, 265 - 6);
    assert_eq!(counts.policy_excluded, Some(1));
    assert_eq!(counts.already_loaded, Some(1));
    assert_eq!(counts.retrieved, Some(selection.candidates.len()));
    // Every exclusion is counted; only the first 32 by stable ID are detailed.
    let excluded_total = 265 - selection.candidates.len() + 1; // loaded is retrieved but excluded
    assert_eq!(summary.details.len(), MAX_DETAILS);
    assert_eq!(
        summary.details.len() + summary.omitted_details,
        excluded_total
    );
    let mut sorted = summary.details.clone();
    sorted.sort();
    assert_eq!(summary.details, sorted);
    assert!(!summary.partial);
    assert!(summary.source_causes.is_empty() && summary.record_causes.is_empty());

    // The snapshot identity is stable, and a content change moves it.
    assert_eq!(snapshot_id(&roster_of(&root)), summary.snapshot);
    let single = |description: &str| {
        let dir = tree();
        let (clock, _runtime, cx) = crate::runtime();
        let entries = vec![entry(&dir, &spec("target", "target", description))];
        snapshot_id(&ResolvedRoster::resolve(entries, false, &cx, &clock).unwrap())
    };
    assert_eq!(single("telescope"), single("telescope"));
    assert_ne!(single("telescope"), single("changed"));
    assert!(runtime.shutdown());
}

fn roster_of(root: &Path) -> ResolvedRoster {
    roster(root)
}

#[test]
fn collecting_evidence_changes_no_result_and_rediscovers_nothing() {
    let root = tree();
    let roster = roster(&root);
    let before = snapshot_id(&roster);
    let (clock, runtime, cx) = runtime();
    let run = || {
        runtime
            .runtime()
            .block_on(retrieve(
                &roster,
                &BTreeSet::new(),
                query("telescope sharedterm"),
                RetrievalBudget::default(),
                &cx,
                &clock,
            ))
            .unwrap()
            .candidates
            .iter()
            .map(|s| s.binding.id.clone())
            .collect::<Vec<_>>()
    };
    let without = run();
    let selection = runtime
        .runtime()
        .block_on(retrieve(
            &roster,
            &BTreeSet::new(),
            query("telescope sharedterm"),
            RetrievalBudget::default(),
            &cx,
            &clock,
        ))
        .unwrap();
    let view = RetrievalView::from_selection(&selection);
    let _ = summarize(&roster, None, &view);
    for key in ["target", "dup-loser", "fill100"] {
        let _ = trace(&roster, None, &view, &id(key));
    }
    assert_eq!(run(), without, "tracing must not change the candidate set");
    assert_eq!(snapshot_id(&roster), before);
    // A skill written after the snapshot stays unknown: evidence never rediscovers.
    let late = spec("late", "late", "telescope");
    let _ = entry(&root, &late);
    assert_eq!(
        trace(&roster, None, &view, &id("late")).decisive(),
        Some(Reason::NotInSnapshot)
    );
    assert!(runtime.shutdown());
}

#[test]
fn partial_sources_and_excluded_records_are_counted_by_cause() {
    let workspace = tree();
    let home = tree();
    // The project root exists but is not a directory, so it is unreadable.
    fs::create_dir_all(workspace.join(".claude")).unwrap();
    fs::write(workspace.join(".claude/skills"), "not a directory").unwrap();
    let user = home.join(".claude/skills");
    for (dir, text) in [
        ("good", "---\nname: good\ndescription: fine\n---\nbody\n"),
        ("broken", "---\nname: [unclosed\n---\nbody\n"),
    ] {
        fs::create_dir_all(user.join(dir)).unwrap();
        fs::write(user.join(dir).join("SKILL.md"), text).unwrap();
    }
    fs::create_dir_all(user.join("nested/deeper")).unwrap();
    fs::write(user.join("nested/deeper/SKILL.md"), "---\nname: n\n---\n").unwrap();
    let visibility = Visibility::Verified {
        contract_version: "test-adapter-v1".into(),
    };
    let plan = claude_code_plan(&workspace, Some(&home), visibility).unwrap();
    let (clock, _runtime, cx) = runtime();
    let roster = resolve_claude_plan(&plan, &BTreeMap::new(), &cx, &clock).unwrap();
    let summary = summarize(&roster, None, &RetrievalView::NotEvaluated);
    assert!(summary.partial);
    assert_eq!(summary.source_causes.get("root-unreadable"), Some(&1));
    assert_eq!(summary.source_causes.get("source-not-enumerated"), Some(&2));
    assert_eq!(summary.record_causes.get("malformed-metadata"), Some(&1));
    assert_eq!(summary.record_causes.get("unsupported-layout"), Some(&1));
    assert_eq!(summary.counts.policy_excluded, None);
    assert_eq!(summary.counts.retrieved, None);
    // A healthy plan without those causes reports none.
    let clean_home = tree();
    fs::create_dir_all(clean_home.join(".claude/skills/good")).unwrap();
    fs::write(
        clean_home.join(".claude/skills/good/SKILL.md"),
        "---\nname: good\ndescription: fine\n---\nbody\n",
    )
    .unwrap();
    let clean_plan = claude_code_plan(
        &tree(),
        Some(&clean_home),
        Visibility::Verified {
            contract_version: "test-adapter-v1".into(),
        },
    )
    .unwrap();
    let clean = resolve_claude_plan(&clean_plan, &BTreeMap::new(), &cx, &clock).unwrap();
    let clean_summary = summarize(&clean, None, &RetrievalView::NotEvaluated);
    assert!(clean_summary.record_causes.is_empty());
    assert_eq!(clean_summary.source_causes.get("root-unreadable"), None);
}

#[test]
fn reason_stage_and_hint_codes_are_stable() {
    let reasons = [
        (
            Reason::NotInSnapshot,
            "not-in-snapshot",
            Stage::Discovery,
            Some(Hint::InspectRoster),
        ),
        (
            Reason::Shadowed,
            "shadowed",
            Stage::Visibility,
            Some(Hint::InspectPrecedence),
        ),
        (
            Reason::Ambiguous,
            "ambiguous",
            Stage::Visibility,
            Some(Hint::InspectPrecedence),
        ),
        (
            Reason::Unverified,
            "unverified",
            Stage::Visibility,
            Some(Hint::VerifyAdapterVisibility),
        ),
        (
            Reason::ManualOnly,
            "manual-only",
            Stage::Restrictions,
            Some(Hint::RequestExplicitly),
        ),
        (
            Reason::Forbidden,
            "forbidden",
            Stage::Restrictions,
            Some(Hint::CheckInvocationRestrictions),
        ),
        (
            Reason::Excluded,
            "excluded",
            Stage::LocalPolicy,
            Some(Hint::ReviewExclusions),
        ),
        (
            Reason::AlreadyLoaded,
            "already-loaded",
            Stage::LocalPolicy,
            None,
        ),
        (
            Reason::NotRetrieved,
            "not-retrieved",
            Stage::Retrieval,
            Some(Hint::RefineRequest),
        ),
    ];
    for (reason, code, stage, hint) in reasons {
        assert_eq!(
            (reason.as_str(), reason.stage(), reason.hint()),
            (code, stage, hint)
        );
    }
    assert_eq!(
        Stage::ALL.map(Stage::as_str),
        [
            "discovery",
            "visibility",
            "restrictions",
            "local-policy",
            "retrieval"
        ]
    );
    assert_eq!(NotEvaluated.as_str(), "not-evaluated");
    assert_eq!(Passed.as_str(), "passed");
}

#[test]
fn aliases_of_one_file_share_policy_and_retrieval_outcomes() {
    let root = tree();
    let (clock, runtime, cx) = runtime();
    // Two callable names bound to the same file resolve to one skill.
    let first = spec("alias-a", "alpha", "telescope");
    let _ = entry(&root, &first);
    let bind = |key: &str, name: &str| {
        let roots = AuthorizedRoots::single(AuthorizedRoot::open_absolute(&root).unwrap());
        SkillEntry::from_read(
            BindingSpec {
                source: SourceId::new(SOURCE).unwrap(),
                logical_key: LogicalSkillKey::new(key).unwrap(),
                invocation: InvocationName::new(name).unwrap(),
                priority: Some(1),
                visibility: Visibility::Verified {
                    contract_version: "fixture-v1".into(),
                },
                restrictions: InvocationRestrictions {
                    agent_invocable: true,
                    user_invocable: true,
                },
            },
            roots
                .read_bounded(0, Path::new("alias-a.md"), SKILL_FILE_BYTES)
                .unwrap(),
        )
        .unwrap()
    };
    let roster = ResolvedRoster::resolve(
        vec![bind("alias-a", "alpha"), bind("alias-b", "beta")],
        false,
        &cx,
        &clock,
    )
    .unwrap();
    assert_eq!(roster.skills().len(), 1);
    assert_eq!(roster.skills()[0].bindings().len(), 2);
    let selection = runtime
        .runtime()
        .block_on(retrieve(
            &roster,
            &BTreeSet::new(),
            query("telescope"),
            RetrievalBudget::default(),
            &cx,
            &clock,
        ))
        .unwrap();
    assert_eq!(selection.candidates.len(), 1);
    let view = RetrievalView::from_selection(&selection);
    // Retrieval admitted the file, so both of its names report it admitted.
    for key in ["alias-a", "alias-b"] {
        assert_eq!(
            trace(&roster, None, &view, &id(key)).outcome(Stage::Retrieval),
            Passed,
            "{key}"
        );
    }
    // Excluding either name excludes the file, for both names.
    let a = id("alias-a");
    let excluded = BTreeSet::from([&a]);
    let loaded = BTreeSet::new();
    let policy = PolicyView {
        excluded: &excluded,
        already_loaded: &loaded,
    };
    for key in ["alias-a", "alias-b"] {
        assert_eq!(
            trace(&roster, Some(&policy), &view, &id(key)).decisive(),
            Some(Reason::Excluded),
            "{key}"
        );
    }
    assert!(runtime.shutdown());
}
