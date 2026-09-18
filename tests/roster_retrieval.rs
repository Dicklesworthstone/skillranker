use skillranker::authorized_read::{AuthorizedRoot, AuthorizedRoots};
use skillranker::identity::{LogicalSkillKey, SkillId, SourceId};
use skillranker::limits::{DurationMillis, SKILL_FILE_BYTES};
use skillranker::roster::resolution::{BindingSpec, OptionMap, ResolvedRoster, SkillEntry};
use skillranker::roster::retrieval::*;
use skillranker::roster::{InvocationName, InvocationRestrictions, Visibility};
use skillranker::runtime::{EntryClock, ProcessInvocation};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

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
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "sr-retrieval-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    fs::create_dir_all(&path).unwrap();
    path
}
fn entry(root: &Path, file: &str, name: &str, agent: bool) -> SkillEntry {
    let roots = AuthorizedRoots::single(AuthorizedRoot::open_absolute(root).unwrap());
    SkillEntry::from_read(
        BindingSpec {
            source: SourceId::new("fixture").unwrap(),
            logical_key: LogicalSkillKey::new(format!("{file}:{name}")).unwrap(),
            invocation: InvocationName::new(name).unwrap(),
            priority: Some(1),
            visibility: Visibility::Verified {
                contract_version: "fixture-v1".into(),
            },
            restrictions: InvocationRestrictions {
                agent_invocable: agent,
                user_invocable: true,
            },
        },
        roots
            .read_bounded(0, Path::new(file), SKILL_FILE_BYTES)
            .unwrap(),
    )
    .unwrap()
}
fn entries(root: &Path, n: usize) -> Vec<SkillEntry> {
    (0..n)
        .map(|i| {
            let file = format!("{i:05}.md");
            fs::write(
                root.join(&file),
                "---\nname: Common\ndescription: sharedterm neutral\n---\nbody",
            )
            .unwrap();
            entry(root, &file, &format!("skill{i:05}"), true)
        })
        .collect()
}
fn query(text: &str) -> QueryInput<'_> {
    QueryInput {
        latest_request: text,
        active_task: "",
        recent_errors: "",
    }
}
fn report(case: &str, diag: &RetrievalDiagnostics) {
    eprintln!(
        "{}",
        serde_json::json!({"schema_version":1,"case_id":case,"stage":"retrieve","policy":diag.policy_version,"engine":diag.engine_version,"method":format!("{:?}",diag.method),"roster_count":diag.roster_count,"eligible_count":diag.eligible_count,"admitted_count":diag.admitted_count,"build_commit_us":diag.build_commit_us,"search_us":diag.search_us,"truncated":diag.truncated})
    );
}
#[test]
fn zero_one_and_254_bypass_the_lexical_filter() {
    let root = tree();
    let (clock, runtime, cx) = runtime();
    for n in [0, 1, 254] {
        let roster = ResolvedRoster::resolve(entries(&root, n), false, &cx, &clock).unwrap();
        let result = runtime.runtime().block_on(retrieve(
            &roster,
            &BTreeSet::new(),
            query("!!!"),
            RetrievalBudget::default(),
            &cx,
            &clock,
        ));
        if n == 0 {
            assert_eq!(
                result.unwrap_err().kind,
                RetrievalError::NoEligibleCandidates
            );
            continue;
        }
        let result = result.unwrap();
        assert_eq!(result.candidates.len(), n);
        assert_eq!(result.diagnostics.method, Some(RetrievalMethod::FullRoster));
        assert!(result.diagnostics.query.is_none());
        assert!(result.diagnostics.build_commit_us.is_none());
        assert!(result.diagnostics.search_us.is_none());
        assert!(!result.diagnostics.truncated);
        report("bypass", &result.diagnostics);
    }
    assert!(runtime.shutdown());
}
#[test]
fn overflow_and_thousand_rosters_preserve_cutoff_ties_and_input_order_independence() {
    let root = tree();
    let (clock, runtime, cx) = runtime();
    for n in [255, 1001] {
        let original = entries(&root, n);
        let mut reversed = original.clone();
        reversed.reverse();
        let roster = ResolvedRoster::resolve(original, false, &cx, &clock).unwrap();
        let reverse = ResolvedRoster::resolve(reversed, false, &cx, &clock).unwrap();
        let run = |r: &ResolvedRoster| {
            let result = runtime
                .runtime()
                .block_on(retrieve(
                    r,
                    &BTreeSet::new(),
                    query("sharedterm"),
                    RetrievalBudget::default(),
                    &cx,
                    &clock,
                ))
                .unwrap();
            assert_eq!(result.diagnostics.method, Some(RetrievalMethod::QuillBm25));
            assert_eq!(result.candidates.len(), 254);
            assert!(result.diagnostics.hit_limit_reached);
            assert!(result.diagnostics.truncated);
            assert!(result.diagnostics.build_commit_us.is_some());
            assert!(result.diagnostics.search_us.is_some());
            report("overflow_tie", &result.diagnostics);
            result
                .candidates
                .iter()
                .map(|s| s.record.id.clone())
                .collect::<Vec<_>>()
        };
        let expected = roster
            .skills()
            .iter()
            .take(254)
            .map(|s| s.record().id.clone())
            .collect::<Vec<_>>();
        assert_eq!(run(&roster), expected);
        assert_eq!(run(&reverse), expected);
    }
    assert!(runtime.shutdown());
}
#[test]
fn titles_aliases_and_tags_are_searchable_but_manual_only_aliases_are_not() {
    let root = tree();
    let (clock, runtime, cx) = runtime();
    let mut data = entries(&root, 255);
    fs::write(
        root.join("target.md"),
        "---\nname: telescope\ndescription: neutral\ntags: [quasar]\n---\nbody",
    )
    .unwrap();
    data.push(entry(&root, "target.md", "constellation", true));
    data.push(entry(&root, "target.md", "nebula", true));
    data.push(entry(&root, "target.md", "forbiddenalias", false));
    let roster = ResolvedRoster::resolve(data, false, &cx, &clock).unwrap();
    for text in ["quasar", "telescope", "constellation", "nebula"] {
        let result = runtime
            .runtime()
            .block_on(retrieve(
                &roster,
                &BTreeSet::new(),
                query(text),
                RetrievalBudget::default(),
                &cx,
                &clock,
            ))
            .unwrap();
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(
            result.candidates[0].record.display_name.as_str(),
            "telescope"
        );
        let ids = result
            .candidates
            .iter()
            .map(|s| s.binding.id.clone())
            .collect::<Vec<_>>();
        assert_eq!(OptionMap::new(&roster, &ids).unwrap().entries().len(), 1);
        report("searchable_fields", &result.diagnostics);
    }
    for text in ["forbiddenalias", "unmatchednonsense", "!!!"] {
        let error = runtime
            .runtime()
            .block_on(retrieve(
                &roster,
                &BTreeSet::new(),
                query(text),
                RetrievalBudget::default(),
                &cx,
                &clock,
            ))
            .unwrap_err();
        assert_eq!(error.kind, RetrievalError::RetrievalEmpty);
        assert_eq!(error.diagnostics.admitted_count, 0);
    }
    assert!(runtime.shutdown());
}
#[test]
fn exclusions_and_restrictions_are_applied_before_overflow_and_cannot_be_bypassed_by_aliases() {
    let root = tree();
    let (clock, runtime, cx) = runtime();
    let mut data = entries(&root, 254);
    fs::write(
        root.join("manual.md"),
        "---\ndisable-model-invocation: true\n---\nsharedterm",
    )
    .unwrap();
    data.push(entry(&root, "manual.md", "manual", true));
    fs::write(root.join("alias.md"), "# Alias\n\nsharedterm").unwrap();
    data.push(entry(&root, "alias.md", "allowed", true));
    data.push(entry(&root, "alias.md", "other", true));
    let roster = ResolvedRoster::resolve(data, false, &cx, &clock).unwrap();
    let excluded: BTreeSet<SkillId> = roster
        .skills()
        .iter()
        .flat_map(|s| s.bindings())
        .filter(|b| b.invocation.as_str() == "other")
        .map(|b| b.id.clone())
        .collect();
    let result = runtime
        .runtime()
        .block_on(retrieve(
            &roster,
            &excluded,
            query("unmatched"),
            RetrievalBudget::default(),
            &cx,
            &clock,
        ))
        .unwrap();
    assert_eq!(result.diagnostics.roster_count, 256);
    assert_eq!(result.diagnostics.eligible_count, Some(254));
    assert_eq!(result.diagnostics.method, Some(RetrievalMethod::FullRoster));
    assert_eq!(result.candidates.len(), 254);
    assert!(
        result
            .candidates
            .iter()
            .all(|s| s.binding.invocation.as_str() != "manual"
                && s.binding.invocation.as_str() != "allowed")
    );
    assert!(runtime.shutdown());
}
#[test]
fn operational_failures_do_not_masquerade_as_empty_matches() {
    let root = tree();
    let (clock, runtime, cx) = runtime();
    let roster = ResolvedRoster::resolve(entries(&root, 255), false, &cx, &clock).unwrap();
    for (budget, expected) in [
        (
            RetrievalBudget {
                query_fuel: 0,
                ..RetrievalBudget::default()
            },
            RetrievalError::InvalidBudget,
        ),
        (
            RetrievalBudget {
                query_fuel: 1,
                ..RetrievalBudget::default()
            },
            RetrievalError::QueryFuel,
        ),
        (
            RetrievalBudget {
                document_bytes: 1,
                ..RetrievalBudget::default()
            },
            RetrievalError::InputLimit,
        ),
    ] {
        let error = runtime
            .runtime()
            .block_on(retrieve(
                &roster,
                &BTreeSet::new(),
                query("sharedterm"),
                budget,
                &cx,
                &clock,
            ))
            .unwrap_err();
        assert_eq!(error.kind, expected);
        assert_eq!(error.diagnostics.admitted_count, 0);
    }
    assert!(
        runtime
            .runtime()
            .block_on(retrieve(
                &roster,
                &BTreeSet::new(),
                query("sharedterm"),
                RetrievalBudget::default(),
                &cx,
                &clock
            ))
            .is_ok()
    );
    runtime.cancel_user(&cx);
    assert_eq!(
        runtime
            .runtime()
            .block_on(retrieve(
                &roster,
                &BTreeSet::new(),
                query("sharedterm"),
                RetrievalBudget::default(),
                &cx,
                &clock
            ))
            .unwrap_err()
            .kind,
        RetrievalError::Cancelled
    );
    assert!(runtime.shutdown());
}
#[test]
fn active_task_supplies_terse_request_evidence_and_diagnostics_do_not_echo_private_text() {
    let root = tree();
    let (clock, runtime, cx) = runtime();
    let roster = ResolvedRoster::resolve(entries(&root, 255), true, &cx, &clock).unwrap();
    let result = runtime
        .runtime()
        .block_on(retrieve(
            &roster,
            &BTreeSet::new(),
            QueryInput {
                latest_request: "PrivateCanary",
                active_task: "sharedterm",
                recent_errors: "",
            },
            RetrievalBudget::default(),
            &cx,
            &clock,
        ))
        .unwrap();
    assert_eq!(result.candidates.len(), 254);
    assert!(result.diagnostics.partial_roster);
    assert!(!format!("{:?}", result.diagnostics).contains("PrivateCanary"));
    let error = runtime
        .runtime()
        .block_on(retrieve(
            &roster,
            &BTreeSet::new(),
            query("PrivateCanary"),
            RetrievalBudget::default(),
            &cx,
            &clock,
        ))
        .unwrap_err();
    assert!(!format!("{error:?} {error}").contains("PrivateCanary"));
    assert!(runtime.shutdown());
}

#[test]
fn segment_sealing_does_not_change_equal_score_cutoff() {
    let root = tree();
    let (clock, runtime, cx) = runtime();
    let roster = ResolvedRoster::resolve(entries(&root, 1001), false, &cx, &clock).unwrap();
    let result = runtime
        .runtime()
        .block_on(retrieve(
            &roster,
            &BTreeSet::new(),
            query("sharedterm"),
            RetrievalBudget {
                scribe_bytes: 4096,
                delta_bytes: 1024,
                ..RetrievalBudget::default()
            },
            &cx,
            &clock,
        ))
        .unwrap();
    let actual = result
        .candidates
        .iter()
        .map(|s| &s.record.id)
        .collect::<Vec<_>>();
    let expected = roster
        .skills()
        .iter()
        .take(254)
        .map(|s| &s.record().id)
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
    report("sealed_segments_tie", &result.diagnostics);
    assert!(runtime.shutdown());
}

#[test]
fn field_bounds_preserve_primary_invocation_and_tags_and_report_truncation() {
    let root = tree();
    let (clock, runtime, cx) = runtime();
    let mut data = entries(&root, 255);
    let title = "label ".repeat(600);
    let description = "ordinary ".repeat(1100);
    fs::write(
        root.join("large.md"),
        format!("---\nname: {title}\ndescription: {description}\ntags: [boundarytag]\n---\nbody"),
    )
    .unwrap();
    data.push(entry(&root, "large.md", "primaryinvoke", true));
    let roster = ResolvedRoster::resolve(data, false, &cx, &clock).unwrap();
    for term in ["primaryinvoke", "boundarytag"] {
        let result = runtime
            .runtime()
            .block_on(retrieve(
                &roster,
                &BTreeSet::new(),
                query(term),
                RetrievalBudget::default(),
                &cx,
                &clock,
            ))
            .unwrap();
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(result.diagnostics.truncated_documents, 1);
        report("bounded_fields", &result.diagnostics);
    }
    assert!(runtime.shutdown());
}

#[test]
fn expired_deadline_rejects_even_the_small_roster_fast_path() {
    let root = tree();
    let (clock, runtime, cx) = runtime();
    let roster = ResolvedRoster::resolve(entries(&root, 1), false, &cx, &clock).unwrap();
    let expired = EntryClock::capture_with(
        DurationMillis::new("short", 20, 100).unwrap(),
        DurationMillis::new("reserve", 5, 100).unwrap(),
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(25));
    let error = runtime
        .runtime()
        .block_on(retrieve(
            &roster,
            &BTreeSet::new(),
            query("sharedterm"),
            RetrievalBudget::default(),
            &cx,
            &expired,
        ))
        .unwrap_err();
    assert_eq!(error.kind, RetrievalError::Deadline);
    assert!(error.diagnostics.eligible_count.is_none());
    assert!(runtime.shutdown());
}

#[test]
fn lexical_coverage_is_observed_separately_from_semantic_relevance() {
    let root = tree();
    let (clock, runtime, cx) = runtime();
    let mut data = entries(&root, 255);
    fs::write(
        root.join("multilingual.md"),
        "---\nname: multilingual\ndescription: repair réparation 修复\n---\nbody",
    )
    .unwrap();
    data.push(entry(&root, "multilingual.md", "multilingual", true));
    let roster = ResolvedRoster::resolve(data, false, &cx, &clock).unwrap();
    for term in ["repair", "réparation", "修复"] {
        let result = runtime
            .runtime()
            .block_on(retrieve(
                &roster,
                &BTreeSet::new(),
                query(term),
                RetrievalBudget::default(),
                &cx,
                &clock,
            ))
            .unwrap();
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(
            result.candidates[0].binding.invocation.as_str(),
            "multilingual"
        );
        report("multilingual_exact_term", &result.diagnostics);
    }
    // A related English paraphrase is a real lexical miss, not evidence that
    // the skill is irrelevant and not a reason to silently use another engine.
    let error = runtime
        .runtime()
        .block_on(retrieve(
            &roster,
            &BTreeSet::new(),
            query("mend"),
            RetrievalBudget::default(),
            &cx,
            &clock,
        ))
        .unwrap_err();
    assert_eq!(error.kind, RetrievalError::RetrievalEmpty);
    report("paraphrase_lexical_miss", &error.diagnostics);
    assert!(runtime.shutdown());
}
