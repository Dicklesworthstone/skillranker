#![cfg(unix)]
//! Contract and verification tests for deduplicated full historical membership
//! snapshots and paired correction labels (sr-roadmap-l1i.6.6 / Invariant I15).
//!
//! Required behavior:
//! - Extend explicit feedback with `--instead SKILL_ID` and optional bounded reason code.
//! - Resolve both candidates against recorded historical roster snapshot in SQLite ledger (never today's disk bytes).
//! - Snapshot deduplication: multiple events referencing identical membership snapshots share 1 row in `roster_snapshots`.
//! - An ineligible alternative (e.g. shadowed, manual-only, unverified, or excluded) aborts the transaction with ZERO judgments committed (`IneligibleAlternative`).
//! - An absent alternative records as a prospective proposal in `feedback_proposals` with ZERO judgments committed (`ProspectiveProposal`).
//! - A valid eligible alternative atomically commits paired judgments (`Harmful` for original, `Useful` for alternative) with shared group provenance and advances store `data_generation`.
//! - Outside-shortlist alternative: an eligible skill in the full snapshot but outside top-K candidates can be selected as alternative.
//! - Missing snapshot fails closed (`MissingSnapshot`) with zero judgments committed.
//! - Stale revision conflict aborts transaction with zero judgments committed (`RevisionConflict`).
//! - CLI subcommand `sr feedback` behaves correctly with JSON and human-readable output.

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
        "sr-test-paired-{}-{}-{}",
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

fn make_snapshot(id: &str, members: &[SnapshotMember], coverage: MembershipCoverage) -> NewRosterSnapshot {
    let members_json = serde_json::to_string(members).expect("serialize members");
    let eligible_count = members.iter().filter(|m| m.eligible).count() as u64;
    NewRosterSnapshot {
        snapshot_id: id.into(),
        workspace_root: "/data/workspace".into(),
        adapter: "claude_code".into(),
        total_candidates: members.len() as u64,
        eligible_candidates: eligible_count,
        membership_coverage: coverage,
        members_json,
        created_at_unix_ms: 1_700_000_000,
    }
}

fn make_event(event_id: &str, snapshot_id: Option<&str>) -> NewRankingEvent {
    NewRankingEvent {
        event_id: event_id.into(),
        verified_delivery_key: Some(format!("deliv-{}", event_id)),
        workspace_root: "/data/workspace".into(),
        session_id: "session-p5-test".into(),
        agent_branch: "main".into(),
        mode_channel: "cli".into(),
        policy_version: "v1".into(),
        schema_version: 1,
        decision: DecisionKind::Ranked,
        reason: "eligible".into(),
        exposure_state: ExposureState::Emitted,
        elapsed_ms: 25,
        created_at_unix_ms: 1_700_000_000,
        input_tokens: Some(150),
        output_tokens: Some(40),
        snapshot_id: snapshot_id.map(Into::into),
    }
}

fn make_candidate(event_id: &str, skill_id: &str, rank: u32, excluded: bool, reason: Option<&str>) -> NewRankingCandidate {
    NewRankingCandidate {
        event_id: event_id.into(),
        stage: CandidateStage::Rerank,
        skill_id: skill_id.into(),
        skill_version: "1.0.0".into(),
        raw_probability: Some(0.8),
        normalized_probability: Some(0.8),
        fit_score: Some(0.85),
        rank_score: Some(0.85),
        rank_position: Some(rank),
        excluded,
        exclusion_reason: reason.map(Into::into),
    }
}

#[test]
fn unchanged_roster_snapshot_deduplication() {
    let dir = temp_private_dir("snap-dedup");
    let (inv, cx) = test_invocation();

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

    let members = vec![
        SnapshotMember {
            skill_id: "git-commit".into(),
            invocation_name: Some("git-commit".into()),
            content_hash: Some("hash-git-123".into()),
            source: "workspace".into(),
            eligible: true,
            exclusion_reason: None,
        },
        SnapshotMember {
            skill_id: "code-review".into(),
            invocation_name: Some("code-review".into()),
            content_hash: Some("hash-cr-456".into()),
            source: "workspace".into(),
            eligible: true,
            exclusion_reason: None,
        },
    ];
    let snapshot = make_snapshot("snap-shared-v1", &members, MembershipCoverage::Complete);

    let event1 = make_event("ev-dedup-1", Some("snap-shared-v1"));
    let cand1 = make_candidate("ev-dedup-1", "git-commit", 1, false, None);

    let stamp1 = store.stamp();
    store
        .record_ranking_event(inv.clock(), &cx, &event1, &[cand1], Some(&snapshot), stamp1)
        .expect("record event 1");

    let event2 = make_event("ev-dedup-2", Some("snap-shared-v1"));
    let cand2 = make_candidate("ev-dedup-2", "code-review", 1, false, None);

    let stamp2 = store.stamp();
    // Record event 2 with the identical snapshot
    store
        .record_ranking_event(inv.clock(), &cx, &event2, &[cand2], Some(&snapshot), stamp2)
        .expect("record event 2");

    // Inspect SQLite database: verify exactly 1 row in roster_snapshots
    let db_path = dir.join(LEDGER_FILE);
    let conn = Connection::open(&db_path).expect("open db");
    let snap_count: i64 = conn
        .query_row("SELECT count(*) FROM roster_snapshots", [], |r| r.get(0))
        .expect("count snapshots");
    assert_eq!(snap_count, 1, "identical snapshots must be deduplicated to 1 row");

    let event_count: i64 = conn
        .query_row("SELECT count(*) FROM ranking_events", [], |r| r.get(0))
        .expect("count events");
    assert_eq!(event_count, 2, "both events must be recorded");

    // Both events point to the same snapshot_id
    let mut stmt = conn
        .prepare("SELECT event_id, snapshot_id FROM ranking_events ORDER BY event_id")
        .expect("prepare stmt");
    let rows: Vec<(String, Option<String>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query map")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect rows");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], ("ev-dedup-1".into(), Some("snap-shared-v1".into())));
    assert_eq!(rows[1], ("ev-dedup-2".into(), Some("snap-shared-v1".into())));
}

#[test]
fn valid_paired_correction_atomic_commit() {
    let dir = temp_private_dir("paired-valid");
    let (inv, cx) = test_invocation();

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

    let members = vec![
        SnapshotMember {
            skill_id: "inferior-skill".into(),
            invocation_name: Some("inferior-skill".into()),
            content_hash: Some("hash-inf".into()),
            source: "workspace".into(),
            eligible: true,
            exclusion_reason: None,
        },
        SnapshotMember {
            skill_id: "superior-skill".into(),
            invocation_name: Some("superior-skill".into()),
            content_hash: Some("hash-sup".into()),
            source: "workspace".into(),
            eligible: true,
            exclusion_reason: None,
        },
    ];
    let snapshot = make_snapshot("snap-paired-v1", &members, MembershipCoverage::Complete);
    let event = make_event("ev-paired-1", Some("snap-paired-v1"));
    let cand = make_candidate("ev-paired-1", "inferior-skill", 1, false, None);

    let stamp0 = store.stamp();
    let initial_gen = stamp0.data_generation;
    store
        .record_ranking_event(inv.clock(), &cx, &event, &[cand], Some(&snapshot), stamp0)
        .expect("record ranking event");

    let stamp1 = store.stamp();
    let req = PairedCorrectionRequest {
        event_id: "ev-paired-1".into(),
        original_skill_id: "inferior-skill".into(),
        alternative_skill_id: "superior-skill".into(),
        reason_code: Some("better_context_handling".into()),
        provenance: Some("evaluator-alice".into()),
        expected_version: None,
    };

    let (outcome, stamp2) = store
        .record_paired_correction(inv.clock(), &cx, &req, stamp1)
        .expect("paired correction succeeds");

    match outcome {
        FeedbackOutcome::PairedCorrection {
            event_id,
            original_skill_id,
            alternative_skill_id,
            group_id,
            original_judgment_id,
            alternative_judgment_id,
            data_generation,
        } => {
            assert_eq!(event_id, "ev-paired-1");
            assert_eq!(original_skill_id, "inferior-skill");
            assert_eq!(alternative_skill_id, "superior-skill");
            assert!(group_id.starts_with("grp-"));
            assert!(original_judgment_id.starts_with("jdg-"));
            assert!(alternative_judgment_id.starts_with("jdg-"));
            assert_eq!(data_generation, initial_gen + 1);
            assert_eq!(stamp2.data_generation, data_generation);
        }
        other => panic!("expected PairedCorrection, got {other:?}"),
    }

    // Direct database validation
    let db_path = dir.join(LEDGER_FILE);
    let conn = Connection::open(&db_path).expect("open db");

    let jdg_rows: Vec<(String, String, String, i64, String)> = conn
        .prepare("SELECT judgment_id, skill_id, label, label_version, provenance FROM judgments ORDER BY skill_id")
        .expect("prep")
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
        .expect("query map")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect rows");

    assert_eq!(jdg_rows.len(), 2, "exactly 2 paired judgments committed");
    // "inferior-skill" -> Harmful
    assert_eq!(jdg_rows[0].1, "inferior-skill");
    assert_eq!(jdg_rows[0].2, "harmful");
    assert_eq!(jdg_rows[0].3, 1);
    assert!(jdg_rows[0].4.starts_with("paired:grp-"));
    assert!(jdg_rows[0].4.ends_with(":evaluator-alice"));

    // "superior-skill" -> Useful
    assert_eq!(jdg_rows[1].1, "superior-skill");
    assert_eq!(jdg_rows[1].2, "useful");
    assert_eq!(jdg_rows[1].3, 1);
    assert_eq!(jdg_rows[1].4, jdg_rows[0].4, "shared group provenance");

    // Repeat with expected_version = Some(1) to test revision increment
    let req_update = PairedCorrectionRequest {
        event_id: "ev-paired-1".into(),
        original_skill_id: "inferior-skill".into(),
        alternative_skill_id: "superior-skill".into(),
        reason_code: Some("still_better".into()),
        provenance: Some("evaluator-alice-v2".into()),
        expected_version: Some(1),
    };
    let (outcome2, stamp3) = store
        .record_paired_correction(inv.clock(), &cx, &req_update, stamp2)
        .expect("paired update succeeds");

    match outcome2 {
        FeedbackOutcome::PairedCorrection {
            data_generation, ..
        } => {
            assert_eq!(data_generation, initial_gen + 2);
            assert_eq!(stamp3.data_generation, data_generation);
        }
        other => panic!("expected PairedCorrection, got {other:?}"),
    }

    let jdg_rows2: Vec<(String, i64)> = conn
        .prepare("SELECT skill_id, label_version FROM judgments ORDER BY skill_id")
        .expect("prep")
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect");
    assert_eq!(jdg_rows2[0].1, 2, "version incremented to 2");
    assert_eq!(jdg_rows2[1].1, 2, "version incremented to 2");
}

#[test]
fn ineligible_alternative_aborts_transaction_zero_judgments() {
    let dir = temp_private_dir("paired-ineligible");
    let (inv, cx) = test_invocation();

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

    let members = vec![
        SnapshotMember {
            skill_id: "active-skill".into(),
            invocation_name: Some("active-skill".into()),
            content_hash: Some("hash-act".into()),
            source: "workspace".into(),
            eligible: true,
            exclusion_reason: None,
        },
        SnapshotMember {
            skill_id: "shadowed-skill".into(),
            invocation_name: Some("shadowed-skill".into()),
            content_hash: Some("hash-shd".into()),
            source: "workspace".into(),
            eligible: false,
            exclusion_reason: Some("shadowed".into()),
        },
        SnapshotMember {
            skill_id: "manual-only-skill".into(),
            invocation_name: Some("manual-only-skill".into()),
            content_hash: Some("hash-man".into()),
            source: "workspace".into(),
            eligible: false,
            exclusion_reason: Some("manual-only".into()),
        },
        SnapshotMember {
            skill_id: "excluded-skill".into(),
            invocation_name: Some("excluded-skill".into()),
            content_hash: Some("hash-exc".into()),
            source: "workspace".into(),
            eligible: false,
            exclusion_reason: Some("excluded".into()),
        },
    ];
    let snapshot = make_snapshot("snap-inelig-v1", &members, MembershipCoverage::Complete);
    let event = make_event("ev-inelig-1", Some("snap-inelig-v1"));
    let cand = make_candidate("ev-inelig-1", "active-skill", 1, false, None);

    let stamp0 = store.stamp();
    let initial_gen = stamp0.data_generation;
    store
        .record_ranking_event(inv.clock(), &cx, &event, &[cand], Some(&snapshot), stamp0)
        .expect("record ranking event");

    let stamp = store.stamp();

    // 1. Test shadowed alternative
    let req_shadowed = PairedCorrectionRequest {
        event_id: "ev-inelig-1".into(),
        original_skill_id: "active-skill".into(),
        alternative_skill_id: "shadowed-skill".into(),
        reason_code: None,
        provenance: None,
        expected_version: None,
    };
    let err_shadowed = store
        .record_paired_correction(inv.clock(), &cx, &req_shadowed, stamp)
        .expect_err("shadowed alternative must be rejected");
    match err_shadowed {
        FeedbackError::IneligibleAlternative { skill_id, reason } => {
            assert_eq!(skill_id, "shadowed-skill");
            assert_eq!(reason.as_deref(), Some("shadowed"));
        }
        other => panic!("expected IneligibleAlternative, got {other:?}"),
    }

    // 2. Test manual-only alternative
    let req_manual = PairedCorrectionRequest {
        event_id: "ev-inelig-1".into(),
        original_skill_id: "active-skill".into(),
        alternative_skill_id: "manual-only-skill".into(),
        reason_code: None,
        provenance: None,
        expected_version: None,
    };
    let err_manual = store
        .record_paired_correction(inv.clock(), &cx, &req_manual, stamp)
        .expect_err("manual-only alternative must be rejected");
    match err_manual {
        FeedbackError::IneligibleAlternative { skill_id, reason } => {
            assert_eq!(skill_id, "manual-only-skill");
            assert_eq!(reason.as_deref(), Some("manual-only"));
        }
        other => panic!("expected IneligibleAlternative, got {other:?}"),
    }

    // 3. Test excluded alternative
    let req_excluded = PairedCorrectionRequest {
        event_id: "ev-inelig-1".into(),
        original_skill_id: "active-skill".into(),
        alternative_skill_id: "excluded-skill".into(),
        reason_code: None,
        provenance: None,
        expected_version: None,
    };
    let err_excluded = store
        .record_paired_correction(inv.clock(), &cx, &req_excluded, stamp)
        .expect_err("excluded alternative must be rejected");
    match err_excluded {
        FeedbackError::IneligibleAlternative { skill_id, reason } => {
            assert_eq!(skill_id, "excluded-skill");
            assert_eq!(reason.as_deref(), Some("excluded"));
        }
        other => panic!("expected IneligibleAlternative, got {other:?}"),
    }

    // Crucial invariant: ZERO judgments committed to SQLite database, data_generation untouched
    let db_path = dir.join(LEDGER_FILE);
    let conn = Connection::open(&db_path).expect("open db");
    let jdg_count: i64 = conn
        .query_row("SELECT count(*) FROM judgments", [], |r| r.get(0))
        .expect("count judgments");
    assert_eq!(jdg_count, 0, "ZERO judgments must be committed when alternative is ineligible");

    let current_gen: i64 = conn
        .query_row("SELECT data_generation FROM store_meta WHERE singleton = 1", [], |r| r.get(0))
        .expect("query gen");
    assert_eq!(current_gen as u64, initial_gen, "data_generation must not advance on rejected transaction");
}

#[test]
fn absent_alternative_records_prospective_proposal() {
    let dir = temp_private_dir("paired-absent");
    let (inv, cx) = test_invocation();

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

    let members = vec![SnapshotMember {
        skill_id: "existing-skill".into(),
        invocation_name: Some("existing-skill".into()),
        content_hash: Some("hash-ex".into()),
        source: "workspace".into(),
        eligible: true,
        exclusion_reason: None,
    }];
    let snapshot = make_snapshot("snap-absent-v1", &members, MembershipCoverage::Complete);
    let event = make_event("ev-absent-1", Some("snap-absent-v1"));
    let cand = make_candidate("ev-absent-1", "existing-skill", 1, false, None);

    let stamp0 = store.stamp();
    store
        .record_ranking_event(inv.clock(), &cx, &event, &[cand], Some(&snapshot), stamp0)
        .expect("record event");

    let stamp = store.stamp();
    let req = PairedCorrectionRequest {
        event_id: "ev-absent-1".into(),
        original_skill_id: "existing-skill".into(),
        alternative_skill_id: "completely-new-skill-not-in-roster".into(),
        reason_code: Some("novel_technique_needed".into()),
        provenance: Some("user-prompt".into()),
        expected_version: None,
    };

    let (outcome, _stamp_out) = store
        .record_paired_correction(inv.clock(), &cx, &req, stamp)
        .expect("absent alternative records as prospective proposal");

    match outcome {
        FeedbackOutcome::ProspectiveProposal {
            event_id,
            original_skill_id,
            alternative_skill_id,
            proposal_id,
            reason,
        } => {
            assert_eq!(event_id, "ev-absent-1");
            assert_eq!(original_skill_id, "existing-skill");
            assert_eq!(alternative_skill_id, "completely-new-skill-not-in-roster");
            assert!(proposal_id.starts_with("prop-"));
            assert!(reason.contains("not present in the historical roster snapshot"));
        }
        other => panic!("expected ProspectiveProposal, got {other:?}"),
    }

    // Invariant I15: ZERO judgments committed, proposal persisted with status 'historically-absent'
    let db_path = dir.join(LEDGER_FILE);
    let conn = Connection::open(&db_path).expect("open db");

    let jdg_count: i64 = conn
        .query_row("SELECT count(*) FROM judgments", [], |r| r.get(0))
        .expect("count judgments");
    assert_eq!(jdg_count, 0, "ZERO judgments must be committed for absent alternative");

    let prop_rows: Vec<(String, String, String, String, String)> = conn
        .prepare("SELECT proposal_id, workspace_root, session_id, suggested_skill_reference, status FROM feedback_proposals")
        .expect("prep")
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect");

    assert_eq!(prop_rows.len(), 1, "exactly 1 proposal committed");
    assert_eq!(prop_rows[0].3, "completely-new-skill-not-in-roster");
    assert_eq!(prop_rows[0].4, ProposalStatus::HistoricallyAbsent.as_str());
}

#[test]
fn outside_shortlist_eligible_alternative_accepted() {
    let dir = temp_private_dir("outside-shortlist");
    let (inv, cx) = test_invocation();

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

    // 5 skills in full snapshot
    let members: Vec<SnapshotMember> = (1..=5)
        .map(|i| SnapshotMember {
            skill_id: format!("skill-{}", i),
            invocation_name: Some(format!("skill-{}", i)),
            content_hash: Some(format!("hash-{}", i)),
            source: "workspace".into(),
            eligible: true,
            exclusion_reason: None,
        })
        .collect();

    let snapshot = make_snapshot("snap-outside-v1", &members, MembershipCoverage::Complete);
    let event = make_event("ev-outside-1", Some("snap-outside-v1"));

    // Shortlist candidates only contained skill-1 and skill-2.
    // skill-5 was outside shortlist, but is present and eligible in snapshot!
    let cands = vec![
        make_candidate("ev-outside-1", "skill-1", 1, false, None),
        make_candidate("ev-outside-1", "skill-2", 2, false, None),
    ];

    let stamp0 = store.stamp();
    store
        .record_ranking_event(inv.clock(), &cx, &event, &cands, Some(&snapshot), stamp0)
        .expect("record ranking event");

    let stamp = store.stamp();
    let req = PairedCorrectionRequest {
        event_id: "ev-outside-1".into(),
        original_skill_id: "skill-1".into(),
        alternative_skill_id: "skill-5".into(), // outside shortlist!
        reason_code: Some("retrieval_miss".into()),
        provenance: Some("maintainer-review".into()),
        expected_version: None,
    };

    let (outcome, _) = store
        .record_paired_correction(inv.clock(), &cx, &req, stamp)
        .expect("outside-shortlist eligible alternative must succeed");

    match outcome {
        FeedbackOutcome::PairedCorrection {
            original_skill_id,
            alternative_skill_id,
            ..
        } => {
            assert_eq!(original_skill_id, "skill-1");
            assert_eq!(alternative_skill_id, "skill-5");
        }
        other => panic!("expected PairedCorrection, got {other:?}"),
    }

    // Verify both judgments committed
    let db_path = dir.join(LEDGER_FILE);
    let conn = Connection::open(&db_path).expect("open db");
    let count: i64 = conn
        .query_row("SELECT count(*) FROM judgments", [], |r| r.get(0))
        .expect("count");
    assert_eq!(count, 2, "both judgments committed for outside-shortlist alternative");
}

#[test]
fn missing_snapshot_fails_closed() {
    let dir = temp_private_dir("missing-snap");
    let (inv, cx) = test_invocation();

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

    // 1. Event without snapshot_id
    let event_no_snap = make_event("ev-no-snap", None);
    let cand1 = make_candidate("ev-no-snap", "skill-x", 1, false, None);
    let stamp0 = store.stamp();
    store
        .record_ranking_event(inv.clock(), &cx, &event_no_snap, &[cand1], None, stamp0)
        .expect("record event without snapshot");

    let stamp1 = store.stamp();
    let req1 = PairedCorrectionRequest {
        event_id: "ev-no-snap".into(),
        original_skill_id: "skill-x".into(),
        alternative_skill_id: "skill-y".into(),
        reason_code: None,
        provenance: None,
        expected_version: None,
    };
    let err1 = store
        .record_paired_correction(inv.clock(), &cx, &req1, stamp1)
        .expect_err("must fail when snapshot is missing");
    assert!(matches!(err1, FeedbackError::MissingSnapshot));

    // 2. Snapshot with partial coverage
    let members = vec![SnapshotMember {
        skill_id: "skill-a".into(),
        invocation_name: Some("skill-a".into()),
        content_hash: Some("hash-a".into()),
        source: "workspace".into(),
        eligible: true,
        exclusion_reason: None,
    }];
    let snap_partial = make_snapshot("snap-partial", &members, MembershipCoverage::Partial);
    let event_partial = make_event("ev-partial-snap", Some("snap-partial"));
    let cand2 = make_candidate("ev-partial-snap", "skill-a", 1, false, None);

    store
        .record_ranking_event(inv.clock(), &cx, &event_partial, &[cand2], Some(&snap_partial), stamp1)
        .expect("record event with partial snapshot");

    let stamp2 = store.stamp();
    let req2 = PairedCorrectionRequest {
        event_id: "ev-partial-snap".into(),
        original_skill_id: "skill-a".into(),
        alternative_skill_id: "skill-b".into(),
        reason_code: None,
        provenance: None,
        expected_version: None,
    };
    let err2 = store
        .record_paired_correction(inv.clock(), &cx, &req2, stamp2)
        .expect_err("must fail when snapshot coverage is partial");
    assert!(matches!(err2, FeedbackError::MissingSnapshot));

    // 3. Event does not exist
    let req3 = PairedCorrectionRequest {
        event_id: "nonexistent-event".into(),
        original_skill_id: "skill-a".into(),
        alternative_skill_id: "skill-b".into(),
        reason_code: None,
        provenance: None,
        expected_version: None,
    };
    let err3 = store
        .record_paired_correction(inv.clock(), &cx, &req3, stamp2)
        .expect_err("must fail when event not found");
    assert!(matches!(err3, FeedbackError::EventNotFound(_)));

    // Verify 0 judgments committed across all failure cases
    let db_path = dir.join(LEDGER_FILE);
    let conn = Connection::open(&db_path).expect("open db");
    let count: i64 = conn
        .query_row("SELECT count(*) FROM judgments", [], |r| r.get(0))
        .expect("count");
    assert_eq!(count, 0, "zero judgments committed on missing snapshot");
}

#[test]
fn revision_conflict_prevents_stale_overwrites() {
    let dir = temp_private_dir("rev-conflict");
    let (inv, cx) = test_invocation();

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

    let members = vec![
        SnapshotMember {
            skill_id: "skill-alpha".into(),
            invocation_name: Some("skill-alpha".into()),
            content_hash: Some("hash-alpha".into()),
            source: "workspace".into(),
            eligible: true,
            exclusion_reason: None,
        },
        SnapshotMember {
            skill_id: "skill-beta".into(),
            invocation_name: Some("skill-beta".into()),
            content_hash: Some("hash-beta".into()),
            source: "workspace".into(),
            eligible: true,
            exclusion_reason: None,
        },
    ];
    let snapshot = make_snapshot("snap-rev-v1", &members, MembershipCoverage::Complete);
    let event = make_event("ev-rev-1", Some("snap-rev-v1"));
    let cand = make_candidate("ev-rev-1", "skill-alpha", 1, false, None);

    let stamp0 = store.stamp();
    store
        .record_ranking_event(inv.clock(), &cx, &event, &[cand], Some(&snapshot), stamp0)
        .expect("record event");

    // Commit initial paired correction -> version is 1
    let stamp1 = store.stamp();
    let req1 = PairedCorrectionRequest {
        event_id: "ev-rev-1".into(),
        original_skill_id: "skill-alpha".into(),
        alternative_skill_id: "skill-beta".into(),
        reason_code: None,
        provenance: Some("agent-1".into()),
        expected_version: None,
    };
    let (_, stamp2) = store
        .record_paired_correction(inv.clock(), &cx, &req1, stamp1)
        .expect("initial commit");

    // Attempt update with expected_version = Some(5) (mismatch: actual is 1)
    let req_stale = PairedCorrectionRequest {
        event_id: "ev-rev-1".into(),
        original_skill_id: "skill-alpha".into(),
        alternative_skill_id: "skill-beta".into(),
        reason_code: None,
        provenance: Some("agent-stale".into()),
        expected_version: Some(5),
    };
    let err = store
        .record_paired_correction(inv.clock(), &cx, &req_stale, stamp2)
        .expect_err("mismatched expected_version must fail with RevisionConflict");

    match err {
        FeedbackError::RevisionConflict { expected, actual } => {
            assert_eq!(expected, 5);
            assert_eq!(actual, 1);
        }
        other => panic!("expected RevisionConflict, got {other:?}"),
    }

    // Verify judgments in DB still have version 1 and original provenance
    let db_path = dir.join(LEDGER_FILE);
    let conn = Connection::open(&db_path).expect("open db");
    let vers: Vec<i64> = conn
        .prepare("SELECT label_version FROM judgments ORDER BY skill_id")
        .expect("prep")
        .query_map([], |r| r.get(0))
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect");
    assert_eq!(vers, vec![1, 1], "version must remain 1 after conflict");
}

#[test]
fn identical_skills_rejected() {
    let dir = temp_private_dir("identical-skills");
    let (inv, cx) = test_invocation();

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

    let stamp = store.stamp();
    let req = PairedCorrectionRequest {
        event_id: "ev-id-1".into(),
        original_skill_id: "same-skill".into(),
        alternative_skill_id: "same-skill".into(),
        reason_code: None,
        provenance: None,
        expected_version: None,
    };

    let err = store
        .record_paired_correction(inv.clock(), &cx, &req, stamp)
        .expect_err("identical skills must be rejected");
    assert!(matches!(err, FeedbackError::IdenticalSkills));
}

#[test]
fn single_feedback_useful_and_harmful() {
    let dir = temp_private_dir("single-feedback");
    let (inv, cx) = test_invocation();

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

    let members = vec![SnapshotMember {
        skill_id: "single-test-skill".into(),
        invocation_name: Some("single-test-skill".into()),
        content_hash: Some("hash-single".into()),
        source: "workspace".into(),
        eligible: true,
        exclusion_reason: None,
    }];
    let snapshot = make_snapshot("snap-single-v1", &members, MembershipCoverage::Complete);
    let event = make_event("ev-single-1", Some("snap-single-v1"));
    let cand = make_candidate("ev-single-1", "single-test-skill", 1, false, None);

    let stamp0 = store.stamp();
    let init_gen = stamp0.data_generation;
    store
        .record_ranking_event(inv.clock(), &cx, &event, &[cand], Some(&snapshot), stamp0)
        .expect("record event");

    // 1. Submit useful verdict
    let stamp1 = store.stamp();
    let req_useful = SingleFeedbackRequest {
        event_id: "ev-single-1".into(),
        skill_id: "single-test-skill".into(),
        verdict: JudgmentLabel::Useful,
        reason_code: Some("worked_well".into()),
        provenance: Some("user".into()),
        expected_version: None,
    };
    let (outcome1, stamp2) = store
        .record_single_feedback(inv.clock(), &cx, &req_useful, stamp1)
        .expect("record useful");

    match outcome1 {
        FeedbackOutcome::SingleJudgment {
            skill_id,
            verdict,
            data_generation,
            ..
        } => {
            assert_eq!(skill_id, "single-test-skill");
            assert_eq!(verdict, JudgmentLabel::Useful);
            assert_eq!(data_generation, init_gen + 1);
        }
        other => panic!("expected SingleJudgment, got {other:?}"),
    }

    // 2. Submit harmful update
    let req_harmful = SingleFeedbackRequest {
        event_id: "ev-single-1".into(),
        skill_id: "single-test-skill".into(),
        verdict: JudgmentLabel::Harmful,
        reason_code: Some("side_effects".into()),
        provenance: Some("user".into()),
        expected_version: Some(1),
    };
    let (outcome2, stamp3) = store
        .record_single_feedback(inv.clock(), &cx, &req_harmful, stamp2)
        .expect("record harmful update");

    match outcome2 {
        FeedbackOutcome::SingleJudgment {
            verdict,
            data_generation,
            ..
        } => {
            assert_eq!(verdict, JudgmentLabel::Harmful);
            assert_eq!(data_generation, init_gen + 2);
            assert_eq!(stamp3.data_generation, data_generation);
        }
        other => panic!("expected SingleJudgment, got {other:?}"),
    }

    // DB inspection
    let db_path = dir.join(LEDGER_FILE);
    let conn = Connection::open(&db_path).expect("open db");
    let (label, ver): (String, i64) = conn
        .query_row(
            "SELECT label, label_version FROM judgments WHERE attributed_event_id = 'ev-single-1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("query single");
    assert_eq!(label, "harmful");
    assert_eq!(ver, 2);
}

#[test]
fn cli_feedback_command_e2e() {
    let dir = temp_private_dir("cli-feedback-e2e");
    let dir_str = dir.to_str().unwrap();
    let bin = env!("CARGO_BIN_EXE_sr");

    // Initialize ledger via CLI
    let init_out = Command::new(bin)
        .args(["ledger", "init", "--dir", dir_str, "--json"])
        .output()
        .expect("run ledger init");
    assert_eq!(init_out.status.code(), Some(0));

    // Populate an event and snapshot using Rust storage helper
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

    let members = vec![
        SnapshotMember {
            skill_id: "skill-cli-1".into(),
            invocation_name: Some("skill-cli-1".into()),
            content_hash: Some("hash-c1".into()),
            source: "workspace".into(),
            eligible: true,
            exclusion_reason: None,
        },
        SnapshotMember {
            skill_id: "skill-cli-2".into(),
            invocation_name: Some("skill-cli-2".into()),
            content_hash: Some("hash-c2".into()),
            source: "workspace".into(),
            eligible: true,
            exclusion_reason: None,
        },
        SnapshotMember {
            skill_id: "skill-cli-inelig".into(),
            invocation_name: Some("skill-cli-inelig".into()),
            content_hash: Some("hash-cinel".into()),
            source: "workspace".into(),
            eligible: false,
            exclusion_reason: Some("shadowed".into()),
        },
    ];
    let snapshot = make_snapshot("snap-cli-v1", &members, MembershipCoverage::Complete);
    let event = make_event("ev-cli-1", Some("snap-cli-v1"));
    let cand = make_candidate("ev-cli-1", "skill-cli-1", 1, false, None);
    let stamp = store.stamp();
    store
        .record_ranking_event(inv.clock(), &cx, &event, &[cand], Some(&snapshot), stamp)
        .expect("record event");

    // 1. Successful paired feedback via CLI with --json
    let out_paired = Command::new(bin)
        .args([
            "feedback",
            "ev-cli-1",
            "--skill",
            "skill-cli-1",
            "--instead",
            "skill-cli-2",
            "--reason",
            "better_accuracy",
            "--dir",
            dir_str,
            "--json",
        ])
        .output()
        .expect("run feedback CLI");
    assert_eq!(out_paired.status.code(), Some(0));
    let val: serde_json::Value = serde_json::from_slice(&out_paired.stdout).expect("parse json");
    assert_eq!(val["status"], "paired-correction");
    assert_eq!(val["original_skill_id"], "skill-cli-1");
    assert_eq!(val["alternative_skill_id"], "skill-cli-2");
    assert!(val["group_id"].as_str().unwrap().starts_with("grp-"));

    // 2. Ineligible alternative via CLI -> exit code 2 with failure JSON
    let out_inelig = Command::new(bin)
        .args([
            "feedback",
            "ev-cli-1",
            "--skill",
            "skill-cli-1",
            "--instead",
            "skill-cli-inelig",
            "--dir",
            dir_str,
            "--json",
        ])
        .output()
        .expect("run feedback CLI");
    assert_eq!(out_inelig.status.code(), Some(2));
    let val_err: serde_json::Value = serde_json::from_slice(&out_inelig.stdout).expect("parse err json");
    assert_eq!(val_err["decision"], "unavailable");
    assert!(
        val_err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("ineligible")
    );

    // 3. Historically absent alternative -> exit code 0 with prospective-proposal JSON
    let out_absent = Command::new(bin)
        .args([
            "feedback",
            "ev-cli-1",
            "--skill",
            "skill-cli-1",
            "--instead",
            "nonexistent-skill",
            "--dir",
            dir_str,
            "--json",
        ])
        .output()
        .expect("run feedback CLI");
    assert_eq!(out_absent.status.code(), Some(0));
    let val_prop: serde_json::Value = serde_json::from_slice(&out_absent.stdout).expect("parse prop json");
    assert_eq!(val_prop["status"], "prospective-proposal");
    assert_eq!(val_prop["alternative_skill_id"], "nonexistent-skill");
    assert!(val_prop["proposal_id"].as_str().unwrap().starts_with("prop-"));

    // 4. Single feedback via CLI
    let out_single = Command::new(bin)
        .args([
            "feedback",
            "ev-cli-1",
            "--skill",
            "skill-cli-2",
            "--verdict",
            "useful",
            "--dir",
            dir_str,
            "--json",
        ])
        .output()
        .expect("run feedback CLI");
    assert_eq!(out_single.status.code(), Some(0));
    let val_single: serde_json::Value = serde_json::from_slice(&out_single.stdout).expect("parse single json");
    assert_eq!(val_single["status"], "single-judgment");
    assert_eq!(val_single["skill_id"], "skill-cli-2");
    assert_eq!(val_single["verdict"], "useful");
    // 5. Conflicting flags: both --instead and --verdict specified -> exit code 2
    let out_conflict = Command::new(bin)
        .args([
            "feedback",
            "ev-cli-1",
            "--skill",
            "skill-cli-1",
            "--instead",
            "skill-cli-2",
            "--verdict",
            "useful",
            "--dir",
            dir_str,
            "--json",
        ])
        .output()
        .expect("run feedback CLI");
    assert_eq!(out_conflict.status.code(), Some(2));
    let val_conflict: serde_json::Value =
        serde_json::from_slice(&out_conflict.stdout).expect("parse conflict json");
    assert_eq!(val_conflict["error"]["kind"], "invalid-usage");
    assert!(
        val_conflict["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Cannot specify both --instead and --verdict")
    );

    // 6. Missing both --instead and --verdict -> exit code 2
    let out_neither = Command::new(bin)
        .args([
            "feedback",
            "ev-cli-1",
            "--skill",
            "skill-cli-1",
            "--dir",
            dir_str,
            "--json",
        ])
        .output()
        .expect("run feedback CLI");
    assert_eq!(out_neither.status.code(), Some(2));
    let val_neither: serde_json::Value =
        serde_json::from_slice(&out_neither.stdout).expect("parse neither json");
    assert_eq!(val_neither["error"]["kind"], "invalid-usage");
    assert!(
        val_neither["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Must specify either --instead for paired correction or --verdict for single feedback")
    );
}

#[test]
fn invalid_alternative_leaves_existing_original_untouched() {
    let dir = temp_private_dir("orig-untouched");
    let (inv, cx) = test_invocation();

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

    let members = vec![
        SnapshotMember {
            skill_id: "orig-skill".into(),
            invocation_name: Some("orig-skill".into()),
            content_hash: Some("hash-orig".into()),
            source: "workspace".into(),
            eligible: true,
            exclusion_reason: None,
        },
        SnapshotMember {
            skill_id: "shadowed-alt".into(),
            invocation_name: Some("shadowed-alt".into()),
            content_hash: Some("hash-shd".into()),
            source: "workspace".into(),
            eligible: false,
            exclusion_reason: Some("shadowed".into()),
        },
    ];
    let snapshot = make_snapshot("snap-orig-v1", &members, MembershipCoverage::Complete);
    let event = make_event("ev-orig-1", Some("snap-orig-v1"));
    let cand = make_candidate("ev-orig-1", "orig-skill", 1, false, None);

    let stamp0 = store.stamp();
    store
        .record_ranking_event(inv.clock(), &cx, &event, &[cand], Some(&snapshot), stamp0)
        .expect("record event");

    // Establish an initial single judgment on orig-skill (e.g. Useful, version 1)
    let stamp1 = store.stamp();
    let req_initial = SingleFeedbackRequest {
        event_id: "ev-orig-1".into(),
        skill_id: "orig-skill".into(),
        verdict: JudgmentLabel::Useful,
        reason_code: Some("initial_assessment".into()),
        provenance: Some("initial-assessor".into()),
        expected_version: None,
    };
    let (_, stamp2) = store
        .record_single_feedback(inv.clock(), &cx, &req_initial, stamp1)
        .expect("initial feedback");

    let expected_gen = stamp2.data_generation;

    // Now attempt a paired correction proposing an invalid (shadowed) alternative
    let req_bad = PairedCorrectionRequest {
        event_id: "ev-orig-1".into(),
        original_skill_id: "orig-skill".into(),
        alternative_skill_id: "shadowed-alt".into(),
        reason_code: Some("try_substitute".into()),
        provenance: Some("second-assessor".into()),
        expected_version: None,
    };
    let err = store
        .record_paired_correction(inv.clock(), &cx, &req_bad, stamp2)
        .expect_err("invalid alternative must fail");
    assert!(matches!(err, FeedbackError::IneligibleAlternative { .. }));

    // Verify: in the database, the original judgment is completely untouched
    let db_path = dir.join(LEDGER_FILE);
    let conn = Connection::open(&db_path).expect("open db");
    let (label, ver, prov): (String, i64, String) = conn
        .query_row(
            "SELECT label, label_version, provenance FROM judgments WHERE attributed_event_id = 'ev-orig-1' AND skill_id = 'orig-skill'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("query judgment");
    assert_eq!(label, "useful", "original label must remain untouched");
    assert_eq!(ver, 1, "original version must remain 1");
    assert_eq!(prov, "initial-assessor", "original provenance must remain untouched");

    let total_jdgs: i64 = conn
        .query_row("SELECT count(*) FROM judgments", [], |r| r.get(0))
        .expect("count");
    assert_eq!(total_jdgs, 1, "no new judgments created");

    let current_gen: i64 = conn
        .query_row("SELECT data_generation FROM store_meta WHERE singleton = 1", [], |r| r.get(0))
        .expect("query gen");
    assert_eq!(current_gen as u64, expected_gen, "data_generation must remain untouched on abort");
}

#[test]
fn partial_group_revision_invalidates_derivation() {
    let dir = temp_private_dir("group-inval");
    let (inv, cx) = test_invocation();

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

    let members = vec![
        SnapshotMember {
            skill_id: "paired-orig".into(),
            invocation_name: Some("paired-orig".into()),
            content_hash: Some("hash-po".into()),
            source: "workspace".into(),
            eligible: true,
            exclusion_reason: None,
        },
        SnapshotMember {
            skill_id: "paired-alt".into(),
            invocation_name: Some("paired-alt".into()),
            content_hash: Some("hash-pa".into()),
            source: "workspace".into(),
            eligible: true,
            exclusion_reason: None,
        },
    ];
    let snapshot = make_snapshot("snap-grp-v1", &members, MembershipCoverage::Complete);
    let event = make_event("ev-grp-1", Some("snap-grp-v1"));
    let cand = make_candidate("ev-grp-1", "paired-orig", 1, false, None);

    let stamp0 = store.stamp();
    store
        .record_ranking_event(inv.clock(), &cx, &event, &[cand], Some(&snapshot), stamp0)
        .expect("record event");

    // 1. Commit initial paired correction
    let stamp1 = store.stamp();
    let req_paired = PairedCorrectionRequest {
        event_id: "ev-grp-1".into(),
        original_skill_id: "paired-orig".into(),
        alternative_skill_id: "paired-alt".into(),
        reason_code: Some("prefer_alt".into()),
        provenance: Some("alice".into()),
        expected_version: None,
    };
    let (outcome, stamp2) = store
        .record_paired_correction(inv.clock(), &cx, &req_paired, stamp1)
        .expect("commit paired");
    let grp_id = match &outcome {
        FeedbackOutcome::PairedCorrection { group_id, .. } => group_id.clone(),
        _ => panic!("expected PairedCorrection"),
    };

    let gen_paired = stamp2.data_generation;

    // 2. Now Assessor Bob revises ONE side of the paired group via single feedback
    let req_single = SingleFeedbackRequest {
        event_id: "ev-grp-1".into(),
        skill_id: "paired-orig".into(),
        verdict: JudgmentLabel::Useful, // changes original from harmful to useful!
        reason_code: Some("reconsidered".into()),
        provenance: Some("bob".into()),
        expected_version: Some(1), // matches version 1
    };
    let (_, stamp3) = store
        .record_single_feedback(inv.clock(), &cx, &req_single, stamp2)
        .expect("revise one side");

    // data_generation must advance, invalidating derived priors and group interpretation
    assert_eq!(
        stamp3.data_generation,
        gen_paired + 1,
        "single revision of one side must advance data_generation"
    );

    // Database check:
    let db_path = dir.join(LEDGER_FILE);
    let conn = Connection::open(&db_path).expect("open db");

    let orig_row: (String, i64, String) = conn
        .query_row(
            "SELECT label, label_version, provenance FROM judgments WHERE attributed_event_id = 'ev-grp-1' AND skill_id = 'paired-orig'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("query orig");
    assert_eq!(orig_row.0, "useful");
    assert_eq!(orig_row.1, 2);
    assert_eq!(orig_row.2, "bob", "revised side has new provenance");

    let alt_row: (String, i64, String) = conn
        .query_row(
            "SELECT label, label_version, provenance FROM judgments WHERE attributed_event_id = 'ev-grp-1' AND skill_id = 'paired-alt'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("query alt");
    assert_eq!(alt_row.0, "useful");
    assert_eq!(alt_row.1, 1);
    assert!(
        alt_row.2.starts_with(&format!("paired:{grp_id}:")),
        "unrevised side retains old group provenance"
    );

    // 3. Stale update attempt: another assessor tries to update paired-orig with expected_version = 1
    let req_stale = PairedCorrectionRequest {
        event_id: "ev-grp-1".into(),
        original_skill_id: "paired-orig".into(),
        alternative_skill_id: "paired-alt".into(),
        reason_code: None,
        provenance: Some("carol".into()),
        expected_version: Some(1), // Stale! paired-orig is now version 2
    };
    let err = store
        .record_paired_correction(inv.clock(), &cx, &req_stale, stamp3)
        .expect_err("stale expected_version must fail with RevisionConflict");
    match err {
        FeedbackError::RevisionConflict { expected, actual } => {
            assert_eq!(expected, 1);
            assert_eq!(actual, 2);
        }
        other => panic!("expected RevisionConflict, got {other:?}"),
    }
}

#[test]
fn multiple_acceptable_alternatives_stay_partial() {
    let dir = temp_private_dir("mult-partial");
    let (inv, cx) = test_invocation();

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

    let members = vec![
        SnapshotMember {
            skill_id: "skill-1".into(),
            invocation_name: Some("skill-1".into()),
            content_hash: Some("hash-1".into()),
            source: "workspace".into(),
            eligible: true,
            exclusion_reason: None,
        },
        SnapshotMember {
            skill_id: "skill-2".into(),
            invocation_name: Some("skill-2".into()),
            content_hash: Some("hash-2".into()),
            source: "workspace".into(),
            eligible: true,
            exclusion_reason: None,
        },
        SnapshotMember {
            skill_id: "skill-3".into(),
            invocation_name: Some("skill-3".into()),
            content_hash: Some("hash-3".into()),
            source: "workspace".into(),
            eligible: true,
            exclusion_reason: None,
        },
        SnapshotMember {
            skill_id: "skill-4".into(),
            invocation_name: Some("skill-4".into()),
            content_hash: Some("hash-4".into()),
            source: "workspace".into(),
            eligible: true,
            exclusion_reason: None,
        },
    ];
    let snapshot = make_snapshot("snap-mult-v1", &members, MembershipCoverage::Complete);
    let event = make_event("ev-mult-1", Some("snap-mult-v1"));
    let cands = vec![
        make_candidate("ev-mult-1", "skill-1", 1, false, None),
        make_candidate("ev-mult-1", "skill-2", 2, false, None),
        make_candidate("ev-mult-1", "skill-3", 3, false, None),
        make_candidate("ev-mult-1", "skill-4", 4, false, None),
    ];

    let stamp0 = store.stamp();
    store
        .record_ranking_event(inv.clock(), &cx, &event, &cands, Some(&snapshot), stamp0)
        .expect("record event");

    // Assessor 1: skill-1 was wrong, skill-2 would have worked
    let stamp1 = store.stamp();
    let req1 = PairedCorrectionRequest {
        event_id: "ev-mult-1".into(),
        original_skill_id: "skill-1".into(),
        alternative_skill_id: "skill-2".into(),
        reason_code: Some("alt2_good".into()),
        provenance: Some("assessor-1".into()),
        expected_version: None,
    };
    let (_, stamp2) = store
        .record_paired_correction(inv.clock(), &cx, &req1, stamp1)
        .expect("paired 1");

    // Assessor 2: skill-1 was wrong, skill-3 also acceptable
    let req2 = PairedCorrectionRequest {
        event_id: "ev-mult-1".into(),
        original_skill_id: "skill-1".into(),
        alternative_skill_id: "skill-3".into(),
        reason_code: Some("alt3_also_good".into()),
        provenance: Some("assessor-2".into()),
        expected_version: Some(1), // skill-1 version was 1
    };
    let (_, _stamp3) = store
        .record_paired_correction(inv.clock(), &cx, &req2, stamp2)
        .expect("paired 2");

    // DB inspection:
    // skill-1: Harmful, version 2
    // skill-2: Useful, version 1
    // skill-3: Useful, version 1
    // skill-4: NO judgment! Other skills remain unjudged.
    let db_path = dir.join(LEDGER_FILE);
    let conn = Connection::open(&db_path).expect("open db");

    let rows: Vec<(String, String, i64)> = conn
        .prepare("SELECT skill_id, label, label_version FROM judgments WHERE attributed_event_id = 'ev-mult-1' ORDER BY skill_id")
        .expect("prep")
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect");

    assert_eq!(rows.len(), 3, "exactly 3 skills have judgments");
    assert_eq!(rows[0], ("skill-1".into(), "harmful".into(), 2));
    assert_eq!(rows[1], ("skill-2".into(), "useful".into(), 1));
    assert_eq!(rows[2], ("skill-3".into(), "useful".into(), 1));

    let skill4_count: i64 = conn
        .query_row(
            "SELECT count(*) FROM judgments WHERE attributed_event_id = 'ev-mult-1' AND skill_id = 'skill-4'",
            [],
            |r| r.get(0),
        )
        .expect("count skill-4");
    assert_eq!(skill4_count, 0, "skill-4 remains unjudged; acceptable set stays partial");
}
