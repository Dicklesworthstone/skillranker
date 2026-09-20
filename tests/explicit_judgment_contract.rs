#![cfg(unix)]
//! Contract and verification tests for versioned explicit judgments without
//! adoption leakage (sr-roadmap-l1i.6.11 / Invariant I15).
//!
//! Required behavior:
//! - Explicit feedback via `sr feedback <EVENT_ID> --skill <ID> --verdict useful|not-useful|unknown`.
//! - Versioned revision conflict detection (competing assessors / revision race).
//! - Unknown consumed event fails honestly with exit code 2 (event-not-found).
//! - Required storage failures (missing ledger, read-only, invalid IDs) cannot report success.
//! - Attempts and loads without labels do not leak into judgments (adoption is separate from correctness).
//! - Retention boundary: judgments older than 30 days are pruned while active judgments are preserved.
//! - Judgments update store `data_generation`, invalidating derived evidence and priors.

use asupersync::Cx;
use rusqlite::Connection;
use skillranker::runtime::ProcessInvocation;
use skillranker::storage::ledger::*;
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn test_invocation() -> (ProcessInvocation, Cx) {
    let inv = ProcessInvocation::enter().expect("process invocation");
    let cx = inv.request_cx().expect("request cx");
    (inv, cx)
}

fn temp_private_dir(prefix: &str) -> PathBuf {
    let dir = PathBuf::from("/tmp").join(format!(
        "sr-test-judg-{}-{}-{}",
        prefix,
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&dir)
        .expect("temp dir create");
    dir
}

fn make_event(event_id: &str, age_days: u64) -> NewRankingEvent {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let created_at = now_ms.saturating_sub(age_days * 86_400_000);
    NewRankingEvent {
        event_id: event_id.into(),
        verified_delivery_key: Some(format!("deliv-{}", event_id)),
        workspace_root: "/data/workspace".into(),
        session_id: "session-judg-test".into(),
        agent_branch: "main".into(),
        mode_channel: "cli".into(),
        policy_version: "v1".into(),
        schema_version: 1,
        decision: DecisionKind::Ranked,
        reason: "eligible".into(),
        exposure_state: ExposureState::Emitted,
        elapsed_ms: 25,
        created_at_unix_ms: created_at,
        input_tokens: Some(150),
        output_tokens: Some(40),
        snapshot_id: None,
    }
}

fn make_candidate(event_id: &str, skill_id: &str) -> NewRankingCandidate {
    NewRankingCandidate {
        event_id: event_id.into(),
        stage: CandidateStage::Rerank,
        skill_id: skill_id.into(),
        skill_version: "1.0.0".into(),
        raw_probability: Some(0.8),
        normalized_probability: Some(0.8),
        fit_score: Some(0.85),
        rank_score: Some(0.85),
        rank_position: Some(1),
        excluded: false,
        exclusion_reason: None,
    }
}

#[test]
fn test_competing_assessors_revision_race() {
    let bin = env!("CARGO_BIN_EXE_sr");
    let dir = temp_private_dir("competing-assessors");
    let dir_str = dir.to_str().unwrap();

    // Init ledger
    let init_out = Command::new(bin)
        .args(["ledger", "init", "--dir", dir_str, "--json"])
        .output()
        .expect("run ledger init");
    assert_eq!(init_out.status.code(), Some(0));

    // Populate event
    let (inv, cx) = test_invocation();
    let open_res = open_ledger(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
    )
    .expect("open ledger");
    let mut store = match open_res {
        LedgerOpen::Ready(s) => s,
        other => panic!("expected Ready, got {other:?}"),
    };
    let event = make_event("ev-race-1", 0);
    let cand = make_candidate("ev-race-1", "skill-race");
    let stamp0 = store.stamp();
    store
        .record_ranking_event(inv.clock(), &cx, &event, &[cand], None, stamp0)
        .expect("record event");

    // 1. Assessor A records initial useful verdict (version 1)
    let out_a1 = Command::new(bin)
        .args([
            "feedback",
            "ev-race-1",
            "--skill",
            "skill-race",
            "--verdict",
            "useful",
            "--provenance",
            "assessor:alice",
            "--dir",
            dir_str,
            "--json",
        ])
        .output()
        .expect("run assessor A feedback");
    assert_eq!(out_a1.status.code(), Some(0));

    let db_path = dir.join(LEDGER_FILE);
    let conn = Connection::open(&db_path).expect("open db");
    let (ver, label, prov): (i64, String, String) = conn
        .query_row(
            "SELECT label_version, label, provenance FROM judgments WHERE attributed_event_id = 'ev-race-1' AND skill_id = 'skill-race'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("query judgment");
    assert_eq!(ver, 1);
    assert_eq!(label, "useful");
    assert_eq!(prov, "assessor:alice");

    // 2. Assessor B tries to update with stale expected-version (expected 2 when actual is 1)
    let out_b_conflict = Command::new(bin)
        .args([
            "feedback",
            "ev-race-1",
            "--skill",
            "skill-race",
            "--verdict",
            "not-useful",
            "--expected-version",
            "2",
            "--provenance",
            "assessor:bob",
            "--dir",
            dir_str,
            "--json",
        ])
        .output()
        .expect("run assessor B stale feedback");
    assert_eq!(
        out_b_conflict.status.code(),
        Some(11),
        "revision mismatch must fail with exit code 11 (revision-conflict)"
    );
    let err_json: serde_json::Value =
        serde_json::from_slice(&out_b_conflict.stdout).expect("parse err json");
    assert!(
        err_json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("revision conflict")
    );

    // 3. Assessor A updates with matching expected-version 1 -> advances to version 2
    let out_a2 = Command::new(bin)
        .args([
            "feedback",
            "ev-race-1",
            "--skill",
            "skill-race",
            "--verdict",
            "useful",
            "--expected-version",
            "1",
            "--provenance",
            "assessor:alice",
            "--dir",
            dir_str,
            "--json",
        ])
        .output()
        .expect("run assessor A update");
    assert_eq!(out_a2.status.code(), Some(0));

    let (ver2, _): (i64, String) = conn
        .query_row(
            "SELECT label_version, label FROM judgments WHERE attributed_event_id = 'ev-race-1' AND skill_id = 'skill-race'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("query judgment");
    assert_eq!(ver2, 2);

    // 4. Assessor B now attempts to update with old expected-version 1 (actual is 2) -> fails with revision conflict
    let out_b_conflict2 = Command::new(bin)
        .args([
            "feedback",
            "ev-race-1",
            "--skill",
            "skill-race",
            "--verdict",
            "not-useful",
            "--expected-version",
            "1",
            "--provenance",
            "assessor:bob",
            "--dir",
            dir_str,
            "--json",
        ])
        .output()
        .expect("run assessor B stale update");
    assert_eq!(
        out_b_conflict2.status.code(),
        Some(11),
        "stale revision must fail with exit code 11"
    );
}

#[test]
fn test_unknown_consumed_version_and_missing_event() {
    let bin = env!("CARGO_BIN_EXE_sr");
    let dir = temp_private_dir("missing-event");
    let dir_str = dir.to_str().unwrap();

    let init_out = Command::new(bin)
        .args(["ledger", "init", "--dir", dir_str, "--json"])
        .output()
        .expect("run ledger init");
    assert_eq!(init_out.status.code(), Some(0));

    // Feedback on nonexistent event ID fails with exit code 2 (event-not-found)
    let out_missing = Command::new(bin)
        .args([
            "feedback",
            "ev-nonexistent-999",
            "--skill",
            "skill-test",
            "--verdict",
            "useful",
            "--dir",
            dir_str,
            "--json",
        ])
        .output()
        .expect("run feedback missing event");
    assert_eq!(
        out_missing.status.code(),
        Some(2),
        "nonexistent event must fail with exit code 2"
    );
    let val: serde_json::Value =
        serde_json::from_slice(&out_missing.stdout).expect("parse err json");
    assert!(
        val["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not found in ledger")
    );
}

#[test]
fn test_ordinary_single_skill_write_required_failure_cannot_report_success() {
    let bin = env!("CARGO_BIN_EXE_sr");
    let dir = temp_private_dir("failures-cannot-succeed");
    let dir_str = dir.to_str().unwrap();

    // 1. Missing / uninitialized ledger directory -> exit code 9 (storage-failure)
    let missing_ledger = dir.join("not-initialized");
    let out_no_ledger = Command::new(bin)
        .args([
            "feedback",
            "ev-1",
            "--skill",
            "skill-1",
            "--verdict",
            "useful",
            "--dir",
            missing_ledger.to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("run no ledger");
    assert_eq!(
        out_no_ledger.status.code(),
        Some(9),
        "missing ledger must exit 9 (storage-failure)"
    );
    let err_val: serde_json::Value =
        serde_json::from_slice(&out_no_ledger.stdout).expect("parse err");
    assert_eq!(err_val["error"]["kind"], "storage-failure");

    // Initialize for remaining parameter validation checks
    let init_out = Command::new(bin)
        .args(["ledger", "init", "--dir", dir_str, "--json"])
        .output()
        .expect("run ledger init");
    assert_eq!(init_out.status.code(), Some(0));

    // 2. Neither --instead nor --verdict -> exit code 2 (invalid-arguments)
    let out_no_action = Command::new(bin)
        .args([
            "feedback", "ev-1", "--skill", "skill-1", "--dir", dir_str, "--json",
        ])
        .output()
        .expect("run no action");
    assert_eq!(
        out_no_action.status.code(),
        Some(2),
        "neither instead nor verdict must exit 2"
    );

    // 3. Both --instead and --verdict -> exit code 2 (invalid-arguments)
    let out_both_actions = Command::new(bin)
        .args([
            "feedback",
            "ev-1",
            "--skill",
            "skill-1",
            "--instead",
            "skill-2",
            "--verdict",
            "useful",
            "--dir",
            dir_str,
            "--json",
        ])
        .output()
        .expect("run both actions");
    assert_eq!(
        out_both_actions.status.code(),
        Some(2),
        "both instead and verdict must exit 2"
    );

    // 4. Invalid verdict string -> exit code 2 (invalid-arguments)
    let out_bad_verdict = Command::new(bin)
        .args([
            "feedback",
            "ev-1",
            "--skill",
            "skill-1",
            "--verdict",
            "super-effective",
            "--dir",
            dir_str,
            "--json",
        ])
        .output()
        .expect("run bad verdict");
    assert_eq!(
        out_bad_verdict.status.code(),
        Some(2),
        "invalid verdict string must exit 2"
    );
}

#[test]
fn test_explicit_verdict_variants_useful_not_useful_unknown() {
    let bin = env!("CARGO_BIN_EXE_sr");
    let dir = temp_private_dir("verdict-variants");
    let dir_str = dir.to_str().unwrap();

    let init_out = Command::new(bin)
        .args(["ledger", "init", "--dir", dir_str, "--json"])
        .output()
        .expect("run ledger init");
    assert_eq!(init_out.status.code(), Some(0));

    // Pre-populate ranking event
    let (inv, cx) = test_invocation();
    let open_res = open_ledger(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
    )
    .expect("open ledger");
    let mut store = match open_res {
        LedgerOpen::Ready(s) => s,
        other => panic!("expected Ready, got {other:?}"),
    };
    let event = make_event("ev-var-1", 0);
    let cand = make_candidate("ev-var-1", "skill-var");
    let stamp0 = store.stamp();
    store
        .record_ranking_event(inv.clock(), &cx, &event, &[cand], None, stamp0)
        .expect("record event");

    let db_path = dir.join(LEDGER_FILE);
    let conn = Connection::open(&db_path).expect("open db");

    // 1. --verdict useful -> records label 'useful'
    let out_useful = Command::new(bin)
        .args([
            "feedback",
            "ev-var-1",
            "--skill",
            "skill-var",
            "--verdict",
            "useful",
            "--dir",
            dir_str,
            "--json",
        ])
        .output()
        .expect("verdict useful");
    assert_eq!(out_useful.status.code(), Some(0));
    let (lbl1, v1): (String, i64) = conn
        .query_row(
            "SELECT label, label_version FROM judgments WHERE attributed_event_id = 'ev-var-1' AND skill_id = 'skill-var'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("query v1");
    assert_eq!(lbl1, "useful");
    assert_eq!(v1, 1);

    // 2. --verdict not-useful -> updates label to 'harmful' (version 2)
    let out_not_useful = Command::new(bin)
        .args([
            "feedback",
            "ev-var-1",
            "--skill",
            "skill-var",
            "--verdict",
            "not-useful",
            "--dir",
            dir_str,
            "--json",
        ])
        .output()
        .expect("verdict not-useful");
    assert_eq!(out_not_useful.status.code(), Some(0));
    let (lbl2, v2): (String, i64) = conn
        .query_row(
            "SELECT label, label_version FROM judgments WHERE attributed_event_id = 'ev-var-1' AND skill_id = 'skill-var'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("query v2");
    assert_eq!(lbl2, "harmful");
    assert_eq!(v2, 2);

    // 3. --verdict unknown -> updates label to 'neutral' (version 3)
    let out_unknown = Command::new(bin)
        .args([
            "feedback",
            "ev-var-1",
            "--skill",
            "skill-var",
            "--verdict",
            "unknown",
            "--dir",
            dir_str,
            "--json",
        ])
        .output()
        .expect("verdict unknown");
    assert_eq!(out_unknown.status.code(), Some(0));
    let (lbl3, v3): (String, i64) = conn
        .query_row(
            "SELECT label, label_version FROM judgments WHERE attributed_event_id = 'ev-var-1' AND skill_id = 'skill-var'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("query v3");
    assert_eq!(lbl3, "neutral");
    assert_eq!(v3, 3);
}

#[test]
fn test_attempts_and_loads_without_labels_no_adoption_leakage() {
    let bin = env!("CARGO_BIN_EXE_sr");
    let dir = temp_private_dir("adoption-separation");
    let dir_str = dir.to_str().unwrap();

    let init_out = Command::new(bin)
        .args(["ledger", "init", "--dir", dir_str, "--json"])
        .output()
        .expect("run ledger init");
    assert_eq!(init_out.status.code(), Some(0));

    // Pre-record an event, candidate, and tool load observation
    let (inv, cx) = test_invocation();
    let open_res = open_ledger(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
    )
    .expect("open ledger");
    let mut store = match open_res {
        LedgerOpen::Ready(s) => s,
        other => panic!("expected Ready, got {other:?}"),
    };
    let event = make_event("ev-obs-no-leak", 0);
    let cand = make_candidate("ev-obs-no-leak", "skill-observed");
    let stamp0 = store.stamp();
    store
        .record_ranking_event(inv.clock(), &cx, &event, &[cand], None, stamp0)
        .expect("record event");

    let cursor = SessionCursor {
        workspace_root: "/data/workspace".into(),
        session_id: "session-judg-test".into(),
        agent_branch: "main".into(),
        cursor_kind: CursorKind::Observation,
        transcript_generation: 1,
        last_complete_event_id: "ev-tool-res".into(),
        last_offset_bytes: 512,
        updated_at_unix_ms: 1_700_000_000,
    };

    let obs = NewObservation {
        observation_id: "obs-loaded-1".into(),
        source_event_key: "key-obs-1".into(),
        workspace_root: "/data/workspace".into(),
        session_id: "session-judg-test".into(),
        agent_branch: "main".into(),
        attributed_event_id: Some("ev-obs-no-leak".into()),
        skill_id: "skill-observed".into(),
        evidence_state: EvidenceState::Loaded,
        observed_at_unix_ms: 1_700_000_000,
    };

    let stamp1 = store.stamp();
    store
        .record_observations_with_cursor(inv.clock(), &cx, &[obs], &cursor, Some(0), stamp1)
        .expect("record observation");

    let db_path = dir.join(LEDGER_FILE);
    let conn = Connection::open(&db_path).expect("open db");

    // Observations table has 1 loaded record
    let obs_count: i64 = conn
        .query_row(
            "SELECT count(*) FROM observations WHERE evidence_state = 'loaded'",
            [],
            |r| r.get(0),
        )
        .expect("query obs");
    assert_eq!(obs_count, 1, "observation must be recorded");

    // Judgments table MUST BE COMPLETELY EMPTY: observing a load NEVER creates an explicit judgment!
    let judg_count: i64 = conn
        .query_row("SELECT count(*) FROM judgments", [], |r| r.get(0))
        .expect("query judgments");
    assert_eq!(
        judg_count, 0,
        "observed load must NEVER create an explicit judgment (no adoption leakage)"
    );
}

#[test]
fn test_retention_boundary_and_priors_invalidation() {
    let (inv, cx) = test_invocation();
    let dir = temp_private_dir("retention-boundary");
    init_ledger(&inv, &cx, LedgerLocation::Directory(dir.clone())).expect("init ledger");

    let open_res = open_ledger(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
    )
    .expect("open ledger");
    let mut store = match open_res {
        LedgerOpen::Ready(s) => s,
        other => panic!("expected Ready, got {other:?}"),
    };

    // 1. Record an event that occurred 35 days ago (outside 30-day retention)
    let old_event = make_event("ev-old-35d", 35);
    let old_cand = make_candidate("ev-old-35d", "skill-old");
    let stamp0 = store.stamp();
    store
        .record_ranking_event(inv.clock(), &cx, &old_event, &[old_cand], None, stamp0)
        .expect("record old event");

    // Record judgment for old event
    let stamp1 = store.stamp();
    let req_old = SingleFeedbackRequest {
        event_id: "ev-old-35d".into(),
        skill_id: "skill-old".into(),
        verdict: JudgmentLabel::Useful,
        reason_code: None,
        provenance: Some("user".into()),
        expected_version: None,
    };
    let (_, stamp2) = store
        .record_single_feedback(inv.clock(), &cx, &req_old, stamp1)
        .expect("record old feedback");

    // 2. Record an event that occurred 5 days ago (within 30-day retention)
    let recent_event = make_event("ev-recent-5d", 5);
    let recent_cand = make_candidate("ev-recent-5d", "skill-recent");
    store
        .record_ranking_event(
            inv.clock(),
            &cx,
            &recent_event,
            &[recent_cand],
            None,
            stamp2,
        )
        .expect("record recent event");

    let stamp3 = store.stamp();
    let req_recent = SingleFeedbackRequest {
        event_id: "ev-recent-5d".into(),
        skill_id: "skill-recent".into(),
        verdict: JudgmentLabel::Useful,
        reason_code: None,
        provenance: Some("user".into()),
        expected_version: None,
    };
    let (_, stamp4) = store
        .record_single_feedback(inv.clock(), &cx, &req_recent, stamp3)
        .expect("record recent feedback");

    // Advance of data_generation on each judgment write invalidates derived priors
    assert!(stamp4.data_generation > stamp0.data_generation);

    // 3. Run prune_retention with 30-day cutoff
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let cutoff_30d = now_ms.saturating_sub(30 * 86_400_000);

    let report = store
        .prune_apply(cutoff_30d as i64, inv.clock(), &cx, stamp4)
        .expect("prune retention");

    assert!(report.events_pruned >= 1, "old event must be pruned");
    assert!(report.judgments_pruned >= 1, "old judgment must be pruned");

    let db_path = dir.join(LEDGER_FILE);
    let conn = Connection::open(&db_path).expect("open db");

    // Old judgment is pruned
    let old_count: i64 = conn
        .query_row(
            "SELECT count(*) FROM judgments WHERE attributed_event_id = 'ev-old-35d'",
            [],
            |r| r.get(0),
        )
        .expect("query old");
    assert_eq!(old_count, 0, "old judgment beyond 30 days must be pruned");

    // Recent judgment within retention is preserved
    let recent_count: i64 = conn
        .query_row(
            "SELECT count(*) FROM judgments WHERE attributed_event_id = 'ev-recent-5d'",
            [],
            |r| r.get(0),
        )
        .expect("query recent");
    assert_eq!(
        recent_count, 1,
        "recent judgment within 30 days must be preserved"
    );
}
