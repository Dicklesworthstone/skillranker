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

#[test]
fn feedback_outcome_matches_durable_identity_for_name_and_id_updates() {
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let mut store = fixture(&invocation, &cx, true);
    let mut previous_judgment_id = None;
    for (index, reference) in ["review", "stable-alpha", "review"].into_iter().enumerate() {
        let stamp = store.stamp();
        let (outcome, updated_stamp) = store
            .record_single_feedback(invocation.clock(), &cx, &feedback(reference), stamp)
            .unwrap();
        let FeedbackOutcome::SingleJudgment {
            event_id,
            skill_id,
            judgment_id,
            data_generation,
            ..
        } = outcome
        else {
            panic!("single feedback must return a single judgment")
        };
        let conn = Connection::open(store.database_path()).unwrap();
        let rows: Vec<(String, String, String, u32)> = conn
            .prepare(
                "SELECT judgment_id, attributed_event_id, skill_id, label_version FROM judgments",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rows.len(), 1, "updates must not split the skill identity");
        assert_eq!(rows[0].2, "stable-alpha");
        assert_eq!(
            skill_id, rows[0].2,
            "response disagrees with durable identity for {reference}"
        );
        assert_eq!(event_id, rows[0].1);
        assert_eq!(judgment_id, rows[0].0);
        assert_eq!(rows[0].3, (index + 1) as u32);
        if let Some(previous) = &previous_judgment_id {
            assert_eq!(&judgment_id, previous);
        }
        previous_judgment_id = Some(judgment_id);
        assert_eq!(data_generation, updated_stamp.data_generation);
        assert_eq!(data_generation, stamp.data_generation + 1);
    }
    assert!(invocation.shutdown());
}
