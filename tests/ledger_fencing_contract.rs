//! Integration tests for store incarnation and schema/data generation fencing.
//!
//! Required by sr-roadmap-l1i.6.2:
//! - Multi-handle / cross-process clear and migrate coordination
//! - Every mutation checks store incarnation and schema/data generation inside its transaction
//! - Clear advances data generation so pre-clear in-memory writers are rejected with StaleGeneration
//! - Cleared history is never resurrected or repopulated by stale writers
//! - Clear does not disable future recording for fresh generation writers
//! - Live database file is not unlinked or replaced underneath open connections
//! - Schema generation mismatch fails with IncompatibleSchema
//! - Incarnation mismatch fails with StoreReplaced

use asupersync::Cx;
use rusqlite::Connection;
use skillranker::runtime::ProcessInvocation;
use skillranker::storage::StoreError;
use skillranker::storage::ledger::*;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_ledger_dir(test_name: &str) -> PathBuf {
    // RCH's TMPDIR can have ancestors owned by a different user. Exercise
    // storage under Linux's root-owned sticky /tmp with private 0700 permissions.
    let dir = PathBuf::from("/tmp").join(format!(
        "sr-ledger-fence-{}-{}-{}",
        test_name,
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

fn test_invocation() -> (ProcessInvocation, Cx) {
    let invocation = ProcessInvocation::enter().expect("process invocation");
    let cx = invocation.request_cx().expect("request_cx");
    (invocation, cx)
}

fn snapshot_fixture(id: &str) -> NewRosterSnapshot {
    NewRosterSnapshot {
        snapshot_id: id.into(),
        workspace_root: "/data/workspace".into(),
        adapter: "claude_code".into(),
        total_candidates: 1,
        eligible_candidates: 1,
        membership_coverage: MembershipCoverage::Complete,
        members_json: serde_json::json!([{
            "skill_id": "review", "invocation_name": "review",
            "content_hash": "hash-1", "source": "workspace",
            "eligible": true, "exclusion_reason": null
        }])
        .to_string(),
        created_at_unix_ms: 1000,
    }
}

fn event_fixture(event_id: &str, snapshot_id: &str) -> NewRankingEvent {
    NewRankingEvent {
        event_id: event_id.into(),
        verified_delivery_key: Some(format!("deliv-{}", event_id)),
        workspace_root: "/data/workspace".into(),
        session_id: "sess-fence".into(),
        agent_branch: "main".into(),
        mode_channel: "cli".into(),
        policy_version: "v1".into(),
        schema_version: 1,
        decision: DecisionKind::Ranked,
        reason: "eligible".into(),
        exposure_state: ExposureState::Emitted,
        elapsed_ms: 42,
        created_at_unix_ms: 1000,
        input_tokens: Some(100),
        output_tokens: Some(20),
        snapshot_id: Some(snapshot_id.into()),
    }
}

fn candidate_fixture(event_id: &str) -> NewRankingCandidate {
    NewRankingCandidate {
        event_id: event_id.into(),
        stage: CandidateStage::Wide,
        skill_id: "review".into(),
        skill_version: "v1".into(),
        raw_probability: Some(0.9),
        normalized_probability: Some(0.9),
        fit_score: Some(0.85),
        rank_score: Some(1.5),
        rank_position: Some(1),
        excluded: false,
        exclusion_reason: None,
    }
}

fn open_ready(inv: &ProcessInvocation, cx: &Cx, dir: &Path, init: bool) -> LedgerStore {
    let access = if init {
        LedgerAccess::Initialize
    } else {
        LedgerAccess::ExistingOnly
    };
    match open_ledger(
        inv,
        cx,
        access,
        LedgerLocation::Directory(dir.to_path_buf()),
    )
    .unwrap()
    {
        LedgerOpen::Ready(store) => *store,
        LedgerOpen::Disabled => panic!("expected Ready store"),
    }
}

#[test]
fn two_process_clear_fences_stale_rank_writer_and_prevents_history_resurrection() {
    let (inv_a, cx_a) = test_invocation();
    let (inv_b, cx_b) = test_invocation();
    let dir = temp_ledger_dir("stale-writer");

    // Process A initializes and opens the store
    let mut store_a = open_ready(&inv_a, &cx_a, &dir, true);
    let stamp_a = store_a.stamp();
    assert_eq!(stamp_a.data_generation, 1);
    assert_eq!(stamp_a.schema_generation, 1);

    // Process B opens the existing store
    let mut store_b = open_ready(&inv_b, &cx_b, &dir, false);
    let stamp_b = store_b.stamp();
    assert_eq!(stamp_a, stamp_b);

    // Step 1: Process A records initial history
    let snap_1 = snapshot_fixture("snap-initial");
    let evt_1 = event_fixture("evt-initial", &snap_1.snapshot_id);
    let cand_1 = candidate_fixture(&evt_1.event_id);
    store_a
        .record_ranking_event(
            inv_a.clock(),
            &cx_a,
            &evt_1,
            &[cand_1],
            Some(&snap_1),
            stamp_a,
        )
        .expect("initial ranking event record succeeds");

    // Verify record exists in database
    let db_path = store_a.database_path();
    let direct_conn = Connection::open(&db_path).unwrap();
    let event_count: i64 = direct_conn
        .query_row("SELECT count(*) FROM ranking_events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(event_count, 1);

    let meta_before = fs::metadata(&db_path).unwrap();
    let ino_before = meta_before.ino();
    let dev_before = meta_before.dev();

    // Step 2: Process B executes clear()
    let cleared_stamp = store_b
        .clear(inv_b.clock(), &cx_b, stamp_b)
        .expect("clear succeeds");
    assert_eq!(cleared_stamp.data_generation, 2);
    assert_eq!(cleared_stamp.schema_generation, 1);
    assert_eq!(cleared_stamp.incarnation, stamp_b.incarnation);

    // Verify DB file was not unlinked or replaced (same inode and device)
    let meta_after = fs::metadata(&db_path).unwrap();
    assert_eq!(
        meta_after.ino(),
        ino_before,
        "database inode must be unchanged"
    );
    assert_eq!(
        meta_after.dev(),
        dev_before,
        "database device must be unchanged"
    );

    // Verify all data tables were purged by clear()
    let event_count_cleared: i64 = direct_conn
        .query_row("SELECT count(*) FROM ranking_events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(event_count_cleared, 0, "clear must purge ranking events");
    let snap_count_cleared: i64 = direct_conn
        .query_row("SELECT count(*) FROM roster_snapshots", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(snap_count_cleared, 0, "clear must purge roster snapshots");

    // Step 3: Process A (holding pre-clear stamp_a with data_generation=1) attempts mutations.
    // Every single mutation MUST fail with StoreError::StaleGeneration and NEVER resurrect records!

    // 3a. Stale ranking event
    let snap_stale = snapshot_fixture("snap-stale-attempt");
    let evt_stale = event_fixture("evt-stale-attempt", &snap_stale.snapshot_id);
    let cand_stale = candidate_fixture(&evt_stale.event_id);
    let res_evt = store_a.record_ranking_event(
        inv_a.clock(),
        &cx_a,
        &evt_stale,
        &[cand_stale],
        Some(&snap_stale),
        stamp_a,
    );
    assert_eq!(
        res_evt.unwrap_err(),
        StoreError::StaleGeneration,
        "stale ranking event write must fail with StaleGeneration"
    );

    // 3b. Stale roster snapshot
    let res_snap = store_a.record_roster_snapshot(inv_a.clock(), &cx_a, &snap_stale, stamp_a);
    assert_eq!(
        res_snap.unwrap_err(),
        StoreError::StaleGeneration,
        "stale roster snapshot write must fail with StaleGeneration"
    );

    // 3c. Stale observation
    let obs_stale = NewObservation {
        observation_id: "obs-stale".into(),
        source_event_key: "key-stale".into(),
        workspace_root: "/data/workspace".into(),
        session_id: "sess-fence".into(),
        agent_branch: "main".into(),
        attributed_event_id: None,
        skill_id: "review".into(),
        evidence_state: EvidenceState::Loaded,
        observed_at_unix_ms: 2000,
    };
    let res_obs = store_a.record_observation(inv_a.clock(), &cx_a, &obs_stale, stamp_a);
    assert_eq!(
        res_obs.unwrap_err(),
        StoreError::StaleGeneration,
        "stale observation write must fail with StaleGeneration"
    );

    // 3d. Stale feedback proposal
    let prop_stale = NewFeedbackProposal {
        proposal_id: "prop-stale".into(),
        workspace_root: "/data/workspace".into(),
        session_id: "sess-fence".into(),
        suggested_skill_reference: "new_tool".into(),
        status: ProposalStatus::Unresolved,
        notes: None,
        created_at_unix_ms: 2000,
    };
    let res_prop = store_a.record_feedback_proposal(inv_a.clock(), &cx_a, &prop_stale, stamp_a);
    assert_eq!(
        res_prop.unwrap_err(),
        StoreError::StaleGeneration,
        "stale feedback proposal write must fail with StaleGeneration"
    );

    // 3e. Stale session cursor
    let cursor_stale = SessionCursor {
        workspace_root: "/data/workspace".into(),
        session_id: "sess-fence".into(),
        agent_branch: "main".into(),
        cursor_kind: CursorKind::Ranking,
        transcript_generation: 1,
        last_complete_event_id: "evt-stale".into(),
        last_offset_bytes: 50,
        updated_at_unix_ms: 2000,
    };
    let res_cur = store_a.update_session_cursor(inv_a.clock(), &cx_a, &cursor_stale, stamp_a);
    assert_eq!(
        res_cur.unwrap_err(),
        StoreError::StaleGeneration,
        "stale session cursor write must fail with StaleGeneration"
    );

    // 3f. Stale provider attempt
    let attempt_stale = NewProviderAttempt {
        attempt_id: "att-stale".into(),
        owner_event_id: "evt-stale".into(),
        stage: CandidateStage::Wide,
        request_fingerprint: "req-fp".into(),
        admitted_at_unix_ms: 2000,
        sent_at_unix_ms: Some(2010),
        completed_at_unix_ms: Some(2050),
        status: AttemptStatus::Completed,
        input_tokens: Some(10),
        output_tokens: Some(5),
        http_status: Some(200),
        error_kind: None,
    };
    let res_att = store_a.record_provider_attempt(inv_a.clock(), &cx_a, &attempt_stale, stamp_a);
    assert_eq!(
        res_att.unwrap_err(),
        StoreError::StaleGeneration,
        "stale provider attempt write must fail with StaleGeneration"
    );

    // 3g. Stale judgment
    let judgment_stale = NewJudgment {
        judgment_id: "judg-stale".into(),
        attributed_event_id: "evt-stale".into(),
        skill_id: "review".into(),
        label: JudgmentLabel::Useful,
        label_version: 1,
        provenance: "user_accepted".into(),
        created_at_unix_ms: 2000,
    };
    let res_judg = store_a.record_judgment(inv_a.clock(), &cx_a, &judgment_stale, stamp_a);
    assert_eq!(
        res_judg.unwrap_err(),
        StoreError::StaleGeneration,
        "stale judgment write must fail with StaleGeneration"
    );

    // 3h. Stale calibration
    let cal_stale = NewCalibration {
        calibration_id: "cal-stale".into(),
        dataset_fingerprint: "fp-1".into(),
        split: DatasetSplit::Train,
        objective: "log_loss".into(),
        coefficients_json: "{}".into(),
        evaluation_report_id: "rep-1".into(),
        created_at_unix_ms: 2000,
    };
    let res_cal = store_a.record_calibration(inv_a.clock(), &cx_a, &cal_stale, stamp_a);
    assert_eq!(
        res_cal.unwrap_err(),
        StoreError::StaleGeneration,
        "stale calibration write must fail with StaleGeneration"
    );

    // Verify that NO records were resurrected in the database!
    let final_event_count: i64 = direct_conn
        .query_row("SELECT count(*) FROM ranking_events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(final_event_count, 0, "no ranking events resurrected");
    let final_snap_count: i64 = direct_conn
        .query_row("SELECT count(*) FROM roster_snapshots", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(final_snap_count, 0, "no snapshots resurrected");
    let final_obs_count: i64 = direct_conn
        .query_row("SELECT count(*) FROM observations", [], |row| row.get(0))
        .unwrap();
    assert_eq!(final_obs_count, 0, "no observations resurrected");
    let final_prop_count: i64 = direct_conn
        .query_row("SELECT count(*) FROM feedback_proposals", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(final_prop_count, 0, "no proposals resurrected");

    // Step 4: Process C opens the ledger at generation 2 and writes successfully.
    // Clear must NOT disable future recording!
    let (inv_c, cx_c) = test_invocation();
    let mut store_c = open_ready(&inv_c, &cx_c, &dir, false);
    let stamp_c = store_c.stamp();
    assert_eq!(stamp_c.data_generation, 2);

    let snap_new = snapshot_fixture("snap-gen2");
    let evt_new = event_fixture("evt-gen2", &snap_new.snapshot_id);
    let cand_new = candidate_fixture(&evt_new.event_id);
    store_c
        .record_ranking_event(
            inv_c.clock(),
            &cx_c,
            &evt_new,
            &[cand_new],
            Some(&snap_new),
            stamp_c,
        )
        .expect("generation 2 ranking event write succeeds");

    let gen2_event_count: i64 = direct_conn
        .query_row("SELECT count(*) FROM ranking_events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(gen2_event_count, 1, "generation 2 write persisted");

    assert!(inv_a.shutdown());
    assert!(inv_b.shutdown());
    assert!(inv_c.shutdown());
}

#[test]
fn two_process_schema_generation_fencing_refuses_mutations() {
    let (inv, cx) = test_invocation();
    let dir = temp_ledger_dir("schema-fence");

    let mut store = open_ready(&inv, &cx, &dir, true);
    let initial_stamp = store.stamp();
    assert_eq!(initial_stamp.schema_generation, 1);

    // Simulate another process performing a schema migration (schema_generation -> 2)
    let direct_conn = Connection::open(store.database_path()).unwrap();
    direct_conn
        .execute(
            "UPDATE store_meta SET schema_generation = 2 WHERE singleton = 1",
            [],
        )
        .unwrap();

    // Store holding expected schema_generation=1 attempts mutation
    let snap = snapshot_fixture("snap-schema-stale");
    let err = store.record_roster_snapshot(inv.clock(), &cx, &snap, initial_stamp);
    assert_eq!(
        err.unwrap_err(),
        StoreError::IncompatibleSchema,
        "mismatched schema_generation must fail with IncompatibleSchema"
    );

    assert!(inv.shutdown());
}

#[test]
fn two_process_store_incarnation_fencing_refuses_mutations() {
    let (inv, cx) = test_invocation();
    let dir = temp_ledger_dir("incarnation-fence");

    let mut store = open_ready(&inv, &cx, &dir, true);
    let initial_stamp = store.stamp();

    // Simulate store replacement / new incarnation by another process
    let direct_conn = Connection::open(store.database_path()).unwrap();
    direct_conn
        .execute(
            "UPDATE store_meta SET incarnation = randomblob(16) WHERE singleton = 1",
            [],
        )
        .unwrap();

    // Store holding original incarnation attempts mutation
    let snap = snapshot_fixture("snap-inc-stale");
    let err = store.record_roster_snapshot(inv.clock(), &cx, &snap, initial_stamp);
    assert_eq!(
        err.unwrap_err(),
        StoreError::StoreReplaced,
        "mismatched store incarnation must fail with StoreReplaced"
    );

    assert!(inv.shutdown());
}

#[test]
fn concurrent_threads_clear_and_writer_race_safety() {
    let dir = temp_ledger_dir("threaded-race");
    let (inv_init, cx_init) = test_invocation();
    let init_store = open_ready(&inv_init, &cx_init, &dir, true);
    let stamp_init = init_store.stamp();
    assert_eq!(stamp_init.data_generation, 1);
    assert!(inv_init.shutdown());

    let (tx_writer_ready, rx_writer_ready) = std::sync::mpsc::channel();
    let (tx_cleared, rx_cleared) = std::sync::mpsc::channel();

    let dir_clone1 = dir.clone();
    let writer_handle = std::thread::spawn(move || {
        let (inv, cx) = test_invocation();
        let mut store = open_ready(&inv, &cx, &dir_clone1, false);

        // Step 1: Writer writes initial snapshot before clear
        let snap1 = snapshot_fixture("snap-pre-clear");
        store
            .record_roster_snapshot(inv.clock(), &cx, &snap1, stamp_init)
            .expect("pre-clear write must succeed");

        // Notify clearer that writer has performed pre-clear work
        tx_writer_ready.send(()).unwrap();

        // Wait until clear() has committed
        rx_cleared.recv().unwrap();

        // Step 2: Writer attempts another write using the pre-clear stamp_init
        let snap2 = snapshot_fixture("snap-post-clear-stale");
        let err = store.record_roster_snapshot(inv.clock(), &cx, &snap2, stamp_init);

        assert!(inv.shutdown());
        err
    });

    let dir_clone2 = dir.clone();
    let clearer_handle = std::thread::spawn(move || {
        // Wait for writer to establish initial record
        rx_writer_ready.recv().unwrap();

        let (inv, cx) = test_invocation();
        let mut store = open_ready(&inv, &cx, &dir_clone2, false);
        let stamp = store.stamp();
        assert_eq!(stamp.data_generation, 1);

        let cleared_stamp = store.clear(inv.clock(), &cx, stamp);
        assert!(inv.shutdown());

        // Notify writer that clear has completed
        tx_cleared.send(()).unwrap();

        cleared_stamp
    });

    let clear_result = clearer_handle.join().expect("clear thread joined");
    let cleared_stamp = clear_result.expect("clear must succeed");
    assert_eq!(cleared_stamp.data_generation, 2);

    let writer_result = writer_handle.join().expect("writer thread joined");
    assert_eq!(
        writer_result.unwrap_err(),
        StoreError::StaleGeneration,
        "writer using pre-clear stamp must be rejected with StaleGeneration after clear"
    );

    // Verify fresh handle succeeds with the new generation
    let (inv_final, cx_final) = test_invocation();
    let mut final_store = open_ready(&inv_final, &cx_final, &dir, false);
    assert_eq!(final_store.stamp().data_generation, 2);
    let fresh_snap = snapshot_fixture("snap-post-race");
    final_store
        .record_roster_snapshot(
            inv_final.clock(),
            &cx_final,
            &fresh_snap,
            final_store.stamp(),
        )
        .expect("fresh handle at gen 2 succeeds after race");
    assert!(inv_final.shutdown());
}
