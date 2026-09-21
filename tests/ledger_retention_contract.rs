//! Contract and verification tests for ledger logical retention, cleanup debt,
//! explicit prune/clear preview and apply, shared snapshot preservation,
//! and label revision invalidating priors.
//!
//! Required by sr-roadmap-l1i.6.5:
//! - Exclude >30day events from ordinary stats/priors by versioned as_of.
//! - Preserve evaluation bundle retention choices and prune dependency graph without dangling labels/snapshots.
//! - Shared referenced snapshots: snapshots referenced by active events survive pruning of older events.
//! - Revised/removed labels advance data generation and invalidate derived priors / fence stale writers.
//! - Explicit prune/clear preview does not mutate; --apply mutates after preflighting headroom.
//! - Doctor reports cleanup debt. No secure-erasure claim.
//! - WAL checkpoint and separate budget/capacity behavior unaffected.

use asupersync::Cx;
use rusqlite::Connection;
use skillranker::runtime::ProcessInvocation;
use skillranker::storage::ledger::*;
use skillranker::storage::*;
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_private_dir(prefix: &str) -> PathBuf {
    let dir = PathBuf::from("/tmp").join(format!(
        "sr-ret-{}-{}-{}",
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

fn test_invocation() -> (ProcessInvocation, Cx) {
    let invocation = ProcessInvocation::enter().expect("process invocation");
    let cx = invocation.request_cx().expect("request_cx");
    (invocation, cx)
}

fn snapshot_fixture(id: &str, created_at_unix_ms: u64) -> NewRosterSnapshot {
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
        created_at_unix_ms,
    }
}

fn event_fixture(event_id: &str, snapshot_id: &str, created_at_unix_ms: u64) -> NewRankingEvent {
    NewRankingEvent {
        event_id: event_id.into(),
        verified_delivery_key: Some(format!("deliv-{}", event_id)),
        workspace_root: "/data/workspace".into(),
        session_id: "sess-ret".into(),
        agent_branch: "main".into(),
        mode_channel: "cli".into(),
        policy_version: "v1".into(),
        schema_version: 1,
        decision: DecisionKind::Ranked,
        reason: "eligible".into(),
        exposure_state: ExposureState::Generated,
        elapsed_ms: 12,
        created_at_unix_ms,
        input_tokens: Some(100),
        output_tokens: Some(50),
        snapshot_id: Some(snapshot_id.into()),
    }
}

fn candidate_fixture(event_id: &str, skill_id: &str) -> NewRankingCandidate {
    NewRankingCandidate {
        event_id: event_id.into(),
        stage: CandidateStage::Rerank,
        skill_id: skill_id.into(),
        skill_version: "1.0.0".into(),
        raw_probability: Some(0.85),
        normalized_probability: Some(0.85),
        fit_score: Some(0.9),
        rank_score: Some(0.88),
        rank_position: Some(1),
        excluded: false,
        exclusion_reason: None,
    }
}

fn judgment_fixture(
    id: &str,
    event_id: &str,
    skill_id: &str,
    label: JudgmentLabel,
    created_at_unix_ms: u64,
) -> NewJudgment {
    NewJudgment {
        judgment_id: id.into(),
        attributed_event_id: event_id.into(),
        skill_id: skill_id.into(),
        label,
        label_version: 1,
        provenance: "test-eval".into(),
        created_at_unix_ms,
    }
}

fn observation_fixture(
    id: &str,
    key: &str,
    event_id: Option<&str>,
    skill_id: &str,
    observed_at_unix_ms: u64,
) -> NewObservation {
    NewObservation {
        observation_id: id.into(),
        source_event_key: key.into(),
        workspace_root: "/data/workspace".into(),
        session_id: "sess-ret".into(),
        agent_branch: "main".into(),
        attributed_event_id: event_id.map(|s| s.into()),
        skill_id: skill_id.into(),
        evidence_state: EvidenceState::Loaded,
        observed_at_unix_ms,
    }
}

fn provider_attempt_fixture(
    attempt_id: &str,
    event_id: &str,
    admitted_at_unix_ms: u64,
) -> NewProviderAttempt {
    NewProviderAttempt {
        attempt_id: attempt_id.into(),
        owner_event_id: event_id.into(),
        stage: CandidateStage::Rerank,
        request_fingerprint: "req-fingerprint-1".into(),
        admitted_at_unix_ms,
        sent_at_unix_ms: None,
        completed_at_unix_ms: None,
        status: AttemptStatus::Admitted,
        input_tokens: None,
        output_tokens: None,
        http_status: None,
        error_kind: None,
    }
}

#[test]
fn boundary_dates_and_clock_policy_filters_expired_events() {
    let dir = temp_private_dir("retention-boundary");
    let (invocation, cx) = test_invocation();

    let init = init_ledger(&invocation, &cx, LedgerLocation::Directory(dir.clone())).unwrap();
    assert_eq!(init.status, InitStatus::Created);

    let open_res = open_ledger(
        &invocation,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
    )
    .unwrap();
    let mut store = match open_res {
        LedgerOpen::Ready(s) => s,
        _ => panic!("expected ready ledger"),
    };
    let stamp = store.stamp();

    // Base time: T = 5,000,000,000 ms (~57 days from epoch 0)
    let t_now: i64 = 5_000_000_000;
    let day_ms = 86_400_000i64;

    // Event 1: T - 35 days (older than 30-day retention cutoff)
    let t_old = (t_now - 35 * day_ms) as u64;
    let snap_old = snapshot_fixture("snap-old", t_old);
    store
        .record_roster_snapshot(invocation.clock(), &cx, &snap_old, stamp)
        .unwrap();
    let ev_old = event_fixture("ev-old", "snap-old", t_old);
    store
        .record_ranking_event(invocation.clock(), &cx, &ev_old, &[], None, stamp)
        .unwrap();
    let j_old = judgment_fixture("j-old", "ev-old", "review", JudgmentLabel::Useful, t_old);
    store
        .record_judgment(invocation.clock(), &cx, &j_old, stamp)
        .unwrap();
    let o_old = observation_fixture("obs-old", "key-old", Some("ev-old"), "review", t_old);
    store
        .record_observation(invocation.clock(), &cx, &o_old, stamp)
        .unwrap();

    // Event 2: T - 15 days (within 30-day retention cutoff)
    let t_active = (t_now - 15 * day_ms) as u64;
    let snap_active = snapshot_fixture("snap-active", t_active);
    store
        .record_roster_snapshot(invocation.clock(), &cx, &snap_active, stamp)
        .unwrap();
    let ev_active = event_fixture("ev-active", "snap-active", t_active);
    store
        .record_ranking_event(invocation.clock(), &cx, &ev_active, &[], None, stamp)
        .unwrap();
    let j_active = judgment_fixture(
        "j-active",
        "ev-active",
        "review",
        JudgmentLabel::Useful,
        t_active,
    );
    store
        .record_judgment(invocation.clock(), &cx, &j_active, stamp)
        .unwrap();
    let o_active = observation_fixture(
        "obs-active",
        "key-active",
        Some("ev-active"),
        "review",
        t_active,
    );
    store
        .record_observation(invocation.clock(), &cx, &o_active, stamp)
        .unwrap();

    // Query retained stats as of T_now
    let stats = store.query_retained_stats(t_now).unwrap();
    assert_eq!(stats.total_events, 2);
    assert_eq!(stats.active_events, 1);
    assert_eq!(stats.expired_events, 1);
    assert_eq!(stats.active_judgments, 1);
    assert_eq!(stats.active_observations, 1);
    assert_eq!(stats.active_snapshots, 1);

    // Query retained stats with versioned as_of = T - 10 days
    // Cutoff is (T - 10 days) - 30 days = T - 40 days.
    // Both ev_old (T - 35 days) and ev_active (T - 15 days) are active!
    let historical_as_of = t_now - 10 * day_ms;
    let hist_stats = store.query_retained_stats(historical_as_of).unwrap();
    assert_eq!(hist_stats.total_events, 2);
    assert_eq!(hist_stats.active_events, 2);
    assert_eq!(hist_stats.expired_events, 0);

    // Check cleanup debt
    let debt = store.cleanup_debt(t_now).unwrap();
    assert!(debt.has_debt);
    assert_eq!(debt.expired_events, 1);
}

#[test]
fn shared_referenced_snapshots_are_preserved_when_older_events_pruned() {
    let dir = temp_private_dir("shared-snapshots");
    let (invocation, cx) = test_invocation();

    init_ledger(&invocation, &cx, LedgerLocation::Directory(dir.clone())).unwrap();
    let open_res = open_ledger(
        &invocation,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
    )
    .unwrap();
    let mut store = match open_res {
        LedgerOpen::Ready(s) => s,
        _ => panic!("expected ready ledger"),
    };
    let stamp = store.stamp();

    let t_now = 5_000_000_000i64;
    let day_ms = 86_400_000i64;
    let t_old = (t_now - 40 * day_ms) as u64;
    let t_new = (t_now - 10 * day_ms) as u64;

    // Snapshot 1: shared between old and new events
    let snap_shared = snapshot_fixture("snap-shared", t_old);
    store
        .record_roster_snapshot(invocation.clock(), &cx, &snap_shared, stamp)
        .unwrap();

    // Snapshot 2: referenced ONLY by old event
    let snap_exclusive = snapshot_fixture("snap-exclusive", t_old);
    store
        .record_roster_snapshot(invocation.clock(), &cx, &snap_exclusive, stamp)
        .unwrap();

    // Event 1: old, references snap-shared
    let ev1 = event_fixture("ev-old-1", "snap-shared", t_old);
    store
        .record_ranking_event(invocation.clock(), &cx, &ev1, &[], None, stamp)
        .unwrap();

    // Event 2: new, references snap-shared (SHARED!)
    let ev2 = event_fixture("ev-new-2", "snap-shared", t_new);
    store
        .record_ranking_event(invocation.clock(), &cx, &ev2, &[], None, stamp)
        .unwrap();

    // Event 3: old, references snap-exclusive
    let ev3 = event_fixture("ev-old-3", "snap-exclusive", t_old);
    store
        .record_ranking_event(invocation.clock(), &cx, &ev3, &[], None, stamp)
        .unwrap();

    // Cutoff: T - 30 days
    let cutoff = t_now - 30 * day_ms;

    // First check preview
    let preview = store.prune_preview(cutoff).unwrap();
    assert_eq!(preview.events_to_prune, 2);
    assert_eq!(preview.snapshots_to_prune, 1); // snap-exclusive to prune
    assert_eq!(preview.shared_snapshots_preserved, 1); // snap-shared preserved

    // Now execute prune_apply
    let report = store
        .prune_apply(cutoff, invocation.clock(), &cx, stamp)
        .unwrap();
    assert_eq!(report.events_pruned, 2);
    assert_eq!(report.snapshots_pruned, 1);
    assert_eq!(report.shared_snapshots_preserved, 1);
    assert_eq!(
        report.stamp_after.data_generation,
        stamp.data_generation + 1
    );

    // Verify database contents
    let conn = Connection::open(dir.join(LEDGER_FILE)).unwrap();
    let remaining_events: i64 = conn
        .query_row("SELECT count(*) FROM ranking_events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(remaining_events, 1);

    let remaining_event_id: String = conn
        .query_row("SELECT event_id FROM ranking_events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(remaining_event_id, "ev-new-2");

    // Verify snap-shared still exists and is referenced by ev-new-2
    let shared_exists: i64 = conn
        .query_row(
            "SELECT count(*) FROM roster_snapshots WHERE snapshot_id = 'snap-shared'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(shared_exists, 1);

    // Verify snap-exclusive was pruned
    let exclusive_exists: i64 = conn
        .query_row(
            "SELECT count(*) FROM roster_snapshots WHERE snapshot_id = 'snap-exclusive'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(exclusive_exists, 0);

    // Check foreign keys
    let fk_violations: Vec<String> = conn
        .prepare("PRAGMA foreign_key_check")
        .unwrap()
        .query_map([], |row| Ok(format!("{:?}", row.get::<_, String>(0))))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert!(
        fk_violations.is_empty(),
        "foreign key violations after prune: {:?}",
        fk_violations
    );
}

#[test]
fn prune_dependency_graph_without_dangling_labels() {
    let dir = temp_private_dir("dependency-graph");
    let (invocation, cx) = test_invocation();

    init_ledger(&invocation, &cx, LedgerLocation::Directory(dir.clone())).unwrap();
    let open_res = open_ledger(
        &invocation,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
    )
    .unwrap();
    let mut store = match open_res {
        LedgerOpen::Ready(s) => s,
        _ => panic!("expected ready ledger"),
    };
    let stamp = store.stamp();

    let t_old = 1_000_000_000u64;
    let snap = snapshot_fixture("snap-1", t_old);
    store
        .record_roster_snapshot(invocation.clock(), &cx, &snap, stamp)
        .unwrap();

    let ev = event_fixture("ev-dep", "snap-1", t_old);
    let cand = candidate_fixture("ev-dep", "review");
    store
        .record_ranking_event(invocation.clock(), &cx, &ev, &[cand], None, stamp)
        .unwrap();

    let prov = provider_attempt_fixture("att-1", "ev-dep", t_old);
    store
        .record_provider_attempt(invocation.clock(), &cx, &prov, stamp)
        .unwrap();

    let obs = observation_fixture("obs-1", "key-dep", Some("ev-dep"), "review", t_old);
    store
        .record_observation(invocation.clock(), &cx, &obs, stamp)
        .unwrap();

    let j = judgment_fixture("j-1", "ev-dep", "review", JudgmentLabel::Useful, t_old);
    store
        .record_judgment(invocation.clock(), &cx, &j, stamp)
        .unwrap();

    // Verify all records present
    let conn = Connection::open(dir.join(LEDGER_FILE)).unwrap();
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM judgments", [], |r| r.get(0))
            .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM observations", [], |r| r.get(0))
            .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM provider_attempts", [], |r| r.get(0))
            .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM ranking_candidates", [], |r| r.get(0))
            .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM ranking_events", [], |r| r.get(0))
            .unwrap(),
        1
    );

    // Prune before t_old + 1000
    let report = store
        .prune_apply((t_old + 1000) as i64, invocation.clock(), &cx, stamp)
        .unwrap();
    assert_eq!(report.events_pruned, 1);
    assert_eq!(report.candidates_pruned, 1);
    assert_eq!(report.observations_pruned, 1);
    assert_eq!(report.judgments_pruned, 1);
    assert_eq!(report.provider_attempts_pruned, 1);
    assert_eq!(report.snapshots_pruned, 1);

    // Verify all tables are empty - no dangling labels or snapshots!
    let conn = Connection::open(dir.join(LEDGER_FILE)).unwrap();
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM judgments", [], |r| r.get(0))
            .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM observations", [], |r| r.get(0))
            .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM provider_attempts", [], |r| r.get(0))
            .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM ranking_candidates", [], |r| r.get(0))
            .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM ranking_events", [], |r| r.get(0))
            .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM roster_snapshots", [], |r| r.get(0))
            .unwrap(),
        0
    );

    let fk_check: i64 = conn
        .query_row("PRAGMA foreign_key_check", [], |r| r.get(0))
        .unwrap_or(0);
    assert_eq!(fk_check, 0);
}

#[test]
fn revised_and_removed_labels_advance_generation_and_fence_stale_writers() {
    let dir = temp_private_dir("label-revision");
    let (invocation, cx) = test_invocation();

    init_ledger(&invocation, &cx, LedgerLocation::Directory(dir.clone())).unwrap();
    let open_res = open_ledger(
        &invocation,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
    )
    .unwrap();
    let mut store = match open_res {
        LedgerOpen::Ready(s) => s,
        _ => panic!("expected ready ledger"),
    };
    let initial_stamp = store.stamp();

    let t = 1_000_000_000u64;
    let snap = snapshot_fixture("snap-label", t);
    store
        .record_roster_snapshot(invocation.clock(), &cx, &snap, initial_stamp)
        .unwrap();
    let ev = event_fixture("ev-label", "snap-label", t);
    store
        .record_ranking_event(invocation.clock(), &cx, &ev, &[], None, initial_stamp)
        .unwrap();

    // 1. Record initial judgment (version 1)
    let j = judgment_fixture("j-rev", "ev-label", "review", JudgmentLabel::Useful, t);
    store
        .record_judgment(invocation.clock(), &cx, &j, initial_stamp)
        .unwrap();

    let conn = Connection::open(dir.join(LEDGER_FILE)).unwrap();
    let (label, ver): (String, i64) = conn
        .query_row(
            "SELECT label, label_version FROM judgments WHERE judgment_id = 'j-rev'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(label, "useful");
    assert_eq!(ver, 1);

    // 2. Revise judgment: label changed to Harmful
    let updated_stamp = store
        .revise_judgment(
            invocation.clock(),
            &cx,
            "j-rev",
            JudgmentLabel::Harmful,
            "revised-by-human",
            t + 5000,
            initial_stamp,
        )
        .unwrap();

    // Data generation MUST have advanced to invalidate derived priors
    assert_eq!(
        updated_stamp.data_generation,
        initial_stamp.data_generation + 1
    );

    let (rev_label, rev_ver): (String, i64) = conn
        .query_row(
            "SELECT label, label_version FROM judgments WHERE judgment_id = 'j-rev'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(rev_label, "harmful");
    assert_eq!(rev_ver, 2);

    // 3. Stale writer using initial_stamp must be fenced!
    let j_stale = judgment_fixture(
        "j-stale",
        "ev-label",
        "review",
        JudgmentLabel::Neutral,
        t + 6000,
    );
    let stale_err = store
        .record_judgment(invocation.clock(), &cx, &j_stale, initial_stamp)
        .unwrap_err();
    assert_eq!(stale_err, StoreError::StaleGeneration);

    // 4. Remove judgment: advances data generation again
    let removed_stamp = store
        .remove_judgment(invocation.clock(), &cx, "j-rev", updated_stamp)
        .unwrap();
    assert_eq!(
        removed_stamp.data_generation,
        updated_stamp.data_generation + 1
    );

    let count: i64 = conn
        .query_row(
            "SELECT count(*) FROM judgments WHERE judgment_id = 'j-rev'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);

    // Writer using updated_stamp is now fenced!
    let stale_err2 = store
        .record_judgment(invocation.clock(), &cx, &j_stale, updated_stamp)
        .unwrap_err();
    assert_eq!(stale_err2, StoreError::StaleGeneration);
}

#[test]
fn prune_and_clear_preview_does_not_mutate_and_apply_mutates_and_fences() {
    let dir = temp_private_dir("preview-apply");
    let (invocation, cx) = test_invocation();

    init_ledger(&invocation, &cx, LedgerLocation::Directory(dir.clone())).unwrap();
    let open_res = open_ledger(
        &invocation,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
    )
    .unwrap();
    let mut store = match open_res {
        LedgerOpen::Ready(s) => s,
        _ => panic!("expected ready ledger"),
    };
    let stamp = store.stamp();

    let t = 1_000_000_000u64;
    let snap = snapshot_fixture("snap-prev", t);
    store
        .record_roster_snapshot(invocation.clock(), &cx, &snap, stamp)
        .unwrap();
    let ev = event_fixture("ev-prev", "snap-prev", t);
    store
        .record_ranking_event(invocation.clock(), &cx, &ev, &[], None, stamp)
        .unwrap();
    let j = judgment_fixture("j-prev", "ev-prev", "review", JudgmentLabel::Useful, t);
    store
        .record_judgment(invocation.clock(), &cx, &j, stamp)
        .unwrap();

    // 1. Prune preview does NOT mutate storage or advance stamp
    let prune_prev = store.prune_preview((t + 1000) as i64).unwrap();
    assert_eq!(prune_prev.events_to_prune, 1);
    assert_eq!(prune_prev.judgments_to_prune, 1);
    assert_eq!(prune_prev.snapshots_to_prune, 1);
    assert!(prune_prev.requires_apply);

    let conn = Connection::open(dir.join(LEDGER_FILE)).unwrap();
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM ranking_events", [], |r| r.get(0))
            .unwrap(),
        1
    );
    assert_eq!(store.stamp().data_generation, stamp.data_generation);

    // 2. Clear preview does NOT mutate storage or advance stamp
    let clear_prev = store.clear_preview().unwrap();
    assert_eq!(clear_prev.events_count, 1);
    assert_eq!(clear_prev.judgments_count, 1);
    assert_eq!(clear_prev.snapshots_count, 1);
    assert_eq!(clear_prev.total_records, 3);
    assert!(clear_prev.requires_apply);

    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM ranking_events", [], |r| r.get(0))
            .unwrap(),
        1
    );
    assert_eq!(store.stamp().data_generation, stamp.data_generation);

    // 3. Clear apply MUTATES and advances stamp
    let clear_rep = store.clear_apply(invocation.clock(), &cx, stamp).unwrap();
    assert_eq!(clear_rep.records_cleared, 3);
    assert_eq!(
        clear_rep.stamp_after.data_generation,
        stamp.data_generation + 1
    );

    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM ranking_events", [], |r| r.get(0))
            .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM judgments", [], |r| r.get(0))
            .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM roster_snapshots", [], |r| r.get(0))
            .unwrap(),
        0
    );

    // Stale write with original stamp is fenced
    let stale_err = store
        .record_roster_snapshot(invocation.clock(), &cx, &snap, stamp)
        .unwrap_err();
    assert_eq!(stale_err, StoreError::StaleGeneration);
}

#[test]
fn insufficient_reserve_preflight_fails_before_mutation() {
    let dir = temp_private_dir("quota-preflight");
    let (invocation, cx) = test_invocation();

    init_ledger(&invocation, &cx, LedgerLocation::Directory(dir.clone())).unwrap();
    let open_res = open_ledger(
        &invocation,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
    )
    .unwrap();
    let store = match open_res {
        LedgerOpen::Ready(s) => s,
        _ => panic!("expected ready ledger"),
    };

    // Preflighting with an excessively large byte requirement exceeding 256 MiB quota fails before mutation
    let err = store
        .preflight_maintenance_with_bytes(MaintenanceKind::Prune, 300 * 1024 * 1024)
        .unwrap_err();
    match err {
        MaintenanceError::QuotaExceeded {
            quota_bytes,
            recovery_step,
            ..
        } => {
            assert_eq!(quota_bytes, LEDGER_QUOTA_BYTES);
            assert!(recovery_step.contains("prune operation exceeds remaining quota headroom"));
        }
        other => panic!("expected QuotaExceeded, got: {:?}", other),
    }

    let err_clear = store
        .preflight_maintenance_with_bytes(MaintenanceKind::Clear, 300 * 1024 * 1024)
        .unwrap_err();
    match err_clear {
        MaintenanceError::QuotaExceeded {
            quota_bytes,
            recovery_step,
            ..
        } => {
            assert_eq!(quota_bytes, LEDGER_QUOTA_BYTES);
            assert!(recovery_step.contains("clear operation exceeds remaining quota headroom"));
        }
        other => panic!("expected QuotaExceeded, got: {:?}", other),
    }
}

#[test]
fn doctor_reports_cleanup_debt_when_ledger_has_expired_records() {
    let dir = temp_private_dir("doctor-debt");
    let (invocation, cx) = test_invocation();

    init_ledger(&invocation, &cx, LedgerLocation::Directory(dir.clone())).unwrap();
    let open_res = open_ledger(
        &invocation,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
    )
    .unwrap();
    let mut store = match open_res {
        LedgerOpen::Ready(s) => s,
        _ => panic!("expected ready ledger"),
    };
    let stamp = store.stamp();

    // Insert an expired event (>30 days relative to now)
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let old_t = (now_ms - 40 * 86_400_000) as u64;
    let snap = snapshot_fixture("snap-debt", old_t);
    store
        .record_roster_snapshot(invocation.clock(), &cx, &snap, stamp)
        .unwrap();
    let ev = event_fixture("ev-debt", "snap-debt", old_t);
    store
        .record_ranking_event(invocation.clock(), &cx, &ev, &[], None, stamp)
        .unwrap();

    // Verify cleanup debt
    let debt = store.cleanup_debt(now_ms).unwrap();
    assert!(debt.has_debt);
    assert_eq!(debt.expired_events, 1);

    // Inspect via ledger_status (before migration: status is ready with upgrade_available, cleanup debt reported)
    let status_pre =
        ledger_status(&invocation, &cx, LedgerLocation::Directory(dir.clone())).unwrap();
    assert_eq!(status_pre.status, "ready");
    assert_eq!(status_pre.upgrade_available, Some(2));
    let reported_debt = status_pre
        .cleanup_debt
        .expect("expected cleanup debt in status");
    assert!(reported_debt.has_debt);
    assert_eq!(reported_debt.expired_events, 1);

    // After migration: status is ready, upgrade_available is None, cleanup debt still reported
    store.migrate_apply(invocation.clock(), &cx).unwrap();
    let status_post =
        ledger_status(&invocation, &cx, LedgerLocation::Directory(dir.clone())).unwrap();
    assert_eq!(status_post.status, "ready");
    assert_eq!(status_post.upgrade_available, None);
    let reported_debt_post = status_post
        .cleanup_debt
        .expect("expected cleanup debt in status");
    assert!(reported_debt_post.has_debt);
    assert_eq!(reported_debt_post.expired_events, 1);

    // Prune expired events
    let cutoff = now_ms - 30 * 86_400_000;
    let report = store
        .prune_apply(cutoff, invocation.clock(), &cx, store.stamp())
        .unwrap();
    assert_eq!(report.events_pruned, 1);

    // Post-prune debt should now be cleared
    let post_debt = store.cleanup_debt(now_ms).unwrap();
    assert!(!post_debt.has_debt);
    assert_eq!(post_debt.expired_events, 0);
}

#[test]
fn cli_ledger_prune_and_clear_e2e() {
    let dir = temp_private_dir("cli-prune-clear");
    let bin = env!("CARGO_BIN_EXE_sr");

    // 1. Init ledger via CLI
    let init_out = Command::new(bin)
        .args(["ledger", "init", "--dir", dir.to_str().unwrap(), "--json"])
        .output()
        .expect("init");
    assert!(init_out.status.success());

    // Populate with 1 event via storage API
    let (invocation, cx) = test_invocation();
    let open_res = open_ledger(
        &invocation,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
    )
    .unwrap();
    let mut store = match open_res {
        LedgerOpen::Ready(s) => s,
        _ => panic!("expected ready ledger"),
    };
    let stamp = store.stamp();
    let t = 1_000_000_000u64;
    let snap = snapshot_fixture("snap-cli", t);
    store
        .record_roster_snapshot(invocation.clock(), &cx, &snap, stamp)
        .unwrap();
    let ev = event_fixture("ev-cli", "snap-cli", t);
    store
        .record_ranking_event(invocation.clock(), &cx, &ev, &[], None, stamp)
        .unwrap();
    drop(store);

    // 2. Run CLI prune preview with --before
    let prev_out = Command::new(bin)
        .args([
            "ledger",
            "prune",
            "--dir",
            dir.to_str().unwrap(),
            "--before",
            "2026-09-01",
            "--json",
        ])
        .output()
        .expect("prune preview");
    assert!(prev_out.status.success());
    let prev_json: serde_json::Value = serde_json::from_slice(&prev_out.stdout).unwrap();
    assert_eq!(prev_json["events_to_prune"], 1);
    assert_eq!(prev_json["requires_apply"], true);

    // 3. Run CLI clear preview
    let clear_prev_out = Command::new(bin)
        .args(["ledger", "clear", "--dir", dir.to_str().unwrap(), "--json"])
        .output()
        .expect("clear preview");
    assert!(clear_prev_out.status.success());
    let clear_json: serde_json::Value = serde_json::from_slice(&clear_prev_out.stdout).unwrap();
    assert_eq!(clear_json["events_count"], 1);
    assert_eq!(clear_json["requires_apply"], true);

    // 4. Run CLI prune --apply
    let apply_out = Command::new(bin)
        .args([
            "ledger",
            "prune",
            "--dir",
            dir.to_str().unwrap(),
            "--before",
            "2026-09-01",
            "--apply",
            "--json",
        ])
        .output()
        .expect("prune apply");
    assert!(apply_out.status.success());
    let rep_json: serde_json::Value = serde_json::from_slice(&apply_out.stdout).unwrap();
    assert_eq!(rep_json["events_pruned"], 1);

    // 5. Run CLI clear --apply
    let clear_apply_out = Command::new(bin)
        .args([
            "ledger",
            "clear",
            "--dir",
            dir.to_str().unwrap(),
            "--apply",
            "--json",
        ])
        .output()
        .expect("clear apply");
    assert!(clear_apply_out.status.success());
    let clear_rep_json: serde_json::Value =
        serde_json::from_slice(&clear_apply_out.stdout).unwrap();
    assert_eq!(clear_rep_json["records_cleared"], 0);
}

#[test]
fn a_relative_prune_cutoff_lands_in_recent_history_not_at_the_epoch() {
    // `cli_ledger_prune_and_clear_e2e` passes `--before 2026-09-01`, an absolute date that
    // `parse_cutoff_to_unix_ms` resolves without consulting the clock — so it never exercised the
    // clock the *relative* forms depend on, and the one input shape that bypassed the bug was the
    // one under test. Both the default cutoff and `--before 30d` derived "now" from the monotonic
    // entry clock, which counts milliseconds since this process started, so every cutoff landed in
    // December 1969. Nothing is older than that, so prune selected nothing and retention was never
    // enforced through this command however often it was run.
    let dir = temp_private_dir("cli-prune-relative");
    let bin = env!("CARGO_BIN_EXE_sr");

    let init_out = Command::new(bin)
        .args(["ledger", "init", "--dir", dir.to_str().unwrap(), "--json"])
        .output()
        .expect("init");
    assert!(init_out.status.success());

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let day_ms = 24 * 60 * 60 * 1000u64;

    // One turn from sixty days ago, one from today.
    let (invocation, cx) = test_invocation();
    let open_res = open_ledger(
        &invocation,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
    )
    .unwrap();
    let mut store = match open_res {
        LedgerOpen::Ready(s) => s,
        _ => panic!("expected ready ledger"),
    };
    for (id, age_days) in [("ev-ancient", 60u64), ("ev-today", 0u64)] {
        let at = now_ms - age_days * day_ms;
        let stamp = store.stamp();
        let snap = snapshot_fixture(&format!("snap-{id}"), at);
        store
            .record_roster_snapshot(invocation.clock(), &cx, &snap, stamp)
            .unwrap();
        let stamp = store.stamp();
        let ev = event_fixture(id, &format!("snap-{id}"), at);
        store
            .record_ranking_event(invocation.clock(), &cx, &ev, &[], None, stamp)
            .unwrap();
    }
    drop(store);

    for args in [
        vec!["ledger", "prune", "--dir", dir.to_str().unwrap(), "--json"],
        vec![
            "ledger",
            "prune",
            "--dir",
            dir.to_str().unwrap(),
            "--before",
            "30d",
            "--json",
        ],
    ] {
        let out = Command::new(bin)
            .args(&args)
            .output()
            .expect("prune preview");
        assert!(out.status.success(), "{args:?} failed: {out:?}");
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let cutoff = json["cutoff_unix_ms"].as_i64().expect("cutoff_unix_ms");

        // A retention cutoff is a point in history, so it must be positive and close to
        // thirty days ago rather than a few milliseconds after the epoch.
        assert!(
            cutoff > 0,
            "{args:?} produced a cutoff at or before the epoch: {} ({})",
            json["cutoff_iso"],
            cutoff
        );
        let expected = (now_ms - 30 * day_ms) as i64;
        assert!(
            (cutoff - expected).abs() < day_ms as i64,
            "{args:?} produced a cutoff of {} ({}), more than a day from the expected {expected}",
            json["cutoff_iso"],
            cutoff
        );

        // And it selects the sixty-day-old turn while leaving today's alone, which is the
        // behaviour a cutoff in 1969 silently withheld.
        assert_eq!(
            json["events_to_prune"], 1,
            "{args:?} selected {} events; retention is not being enforced",
            json["events_to_prune"]
        );
    }
}
