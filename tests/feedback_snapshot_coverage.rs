#![cfg(unix)]

use asupersync::Cx;
use rusqlite::Connection;
use skillranker::runtime::ProcessInvocation;
use skillranker::storage::ledger::*;
use std::fs::DirBuilder;
use std::os::unix::fs::DirBuilderExt;
use std::time::{SystemTime, UNIX_EPOCH};

fn fixture(invocation: &ProcessInvocation, cx: &Cx, complete: bool) -> LedgerStore {
    // Retained under repository policy; no automatic deletion of test trees.
    let directory = std::path::PathBuf::from("/tmp").join(format!(
        "sr-feedback-coverage-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    DirBuilder::new().mode(0o700).create(&directory).unwrap();
    let LedgerOpen::Ready(mut store) = open_ledger(
        invocation,
        cx,
        LedgerAccess::Initialize,
        LedgerLocation::Directory(directory),
    )
    .unwrap() else {
        panic!("initialized ledger must be writable")
    };
    let snapshot = NewRosterSnapshot {
        snapshot_id: "snapshot".into(),
        workspace_root: "/workspace".into(),
        adapter: "claude_code".into(),
        total_candidates: if complete { 1 } else { 2 },
        eligible_candidates: if complete { 1 } else { 2 },
        membership_coverage: if complete {
            MembershipCoverage::Complete
        } else {
            MembershipCoverage::Partial
        },
        members_json: serde_json::json!([{
            "skill_id":"stable-alpha", "invocation_name":"review",
            "content_hash":"revision", "source":"claude_code.project",
            "eligible":true, "exclusion_reason":null
        }])
        .to_string(),
        created_at_unix_ms: 1000,
    };
    let event = NewRankingEvent {
        event_id: "event".into(),
        verified_delivery_key: None,
        workspace_root: "/workspace".into(),
        session_id: "session".into(),
        agent_branch: "main".into(),
        mode_channel: "cli".into(),
        policy_version: "v1".into(),
        schema_version: 1,
        decision: DecisionKind::Ranked,
        reason: "eligible".into(),
        exposure_state: ExposureState::Emitted,
        elapsed_ms: 1,
        created_at_unix_ms: 1000,
        input_tokens: None,
        output_tokens: None,
        snapshot_id: Some("snapshot".into()),
    };
    let stamp = store.stamp();
    store
        .record_ranking_event(invocation.clock(), cx, &event, &[], Some(&snapshot), stamp)
        .unwrap();
    *store
}

fn feedback(reference: &str) -> SingleFeedbackRequest {
    SingleFeedbackRequest {
        event_id: "event".into(),
        skill_id: reference.into(),
        verdict: JudgmentLabel::Useful,
        reason_code: None,
        provenance: Some("coverage-regression".into()),
        expected_version: None,
    }
}

#[test]
fn partial_membership_cannot_prove_an_invocation_name_unique() {
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let mut store = fixture(&invocation, &cx, false);
    let stamp = store.stamp();
    let result = store.record_single_feedback(invocation.clock(), &cx, &feedback("review"), stamp);
    assert!(
        matches!(result, Err(FeedbackError::MissingSnapshot)),
        "{result:?}"
    );
    assert_eq!(
        store.stamp(),
        stamp,
        "refused lookup advanced the store generation"
    );
    let conn = Connection::open(store.database_path()).unwrap();
    let count: i64 = conn
        .query_row("SELECT count(*) FROM judgments", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0, "uncertain name resolution wrote a judgment");
    assert!(invocation.shutdown());
}

#[test]
fn complete_names_and_exact_partial_member_ids_remain_usable() {
    for (complete, reference) in [(true, "review"), (false, "stable-alpha")] {
        let invocation = ProcessInvocation::enter().unwrap();
        let cx = invocation.request_cx().unwrap();
        let mut store = fixture(&invocation, &cx, complete);
        let stamp = store.stamp();
        store
            .record_single_feedback(invocation.clock(), &cx, &feedback(reference), stamp)
            .unwrap();
        let conn = Connection::open(store.database_path()).unwrap();
        let rows: Vec<String> = conn
            .prepare("SELECT skill_id FROM judgments")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            rows,
            ["stable-alpha"],
            "complete={complete}, reference={reference}"
        );
        assert!(invocation.shutdown());
    }
}
