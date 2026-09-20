//! Contract and verification tests for ledger quota headroom and bounded maintenance.
//!
//! Required by sr-roadmap-l1i.6.3:
//! - Enforce 256 MiB ledger including WAL/SHM/backups/temp and 64 MiB cache/coordinator.
//! - Stop optional appends before maintenance reserve.
//! - Calculate worst-case backup/WAL/VACUUM space and free disk before mutations.
//! - Expose usable recording capacity separately from the total cap.
//! - Fill recording capacity then successfully migrate/prune within reserve.
//! - Simulate external full disk and reject before mutation with required-space figure and recovery hint.
//! - Preserve recoverable copy and never reset protected charges.

use asupersync::Cx;
use rusqlite::Connection;
use skillranker::runtime::ProcessInvocation;
use skillranker::storage::ledger::*;
use skillranker::storage::*;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_private_dir(prefix: &str) -> PathBuf {
    // RCH's TMPDIR can have ancestors owned by a different user. Exercise
    // storage under Linux's root-owned sticky /tmp with private 0700 permissions.
    let dir = PathBuf::from("/tmp").join(format!(
        "sr-{}-{}-{}",
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
        session_id: "sess-quota".into(),
        agent_branch: "main".into(),
        mode_channel: "cli".into(),
        policy_version: "v1".into(),
        schema_version: 1,
        decision: DecisionKind::Ranked,
        reason: "eligible".into(),
        exposure_state: ExposureState::Generated,
        elapsed_ms: 12,
        created_at_unix_ms: 1000,
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

fn attempt_fixture(attempt_id: &str, owner_event_id: &str) -> NewProviderAttempt {
    NewProviderAttempt {
        attempt_id: attempt_id.into(),
        owner_event_id: owner_event_id.into(),
        stage: CandidateStage::Wide,
        request_fingerprint: "fp-quota-attempt-1".into(),
        admitted_at_unix_ms: 1000,
        sent_at_unix_ms: Some(1010),
        completed_at_unix_ms: Some(1050),
        status: AttemptStatus::Completed,
        input_tokens: Some(500),
        output_tokens: Some(250),
        http_status: Some(200),
        error_kind: None,
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[test]
fn ledger_and_cache_constants_and_capacity_reports_expose_recording_ceiling() {
    let (invocation, cx) = test_invocation();
    let _clock = invocation.clock();

    // 1. Verify constant invariants
    assert_eq!(LEDGER_QUOTA_BYTES, 256 * 1024 * 1024);
    assert_eq!(LEDGER_MAINTENANCE_RESERVE_BYTES, 16 * 1024 * 1024);
    assert_eq!(LEDGER_MUTATION_RESERVE_BYTES, 4 * 1024 * 1024);
    assert_eq!(
        LEDGER_RECORDING_CEILING_BYTES,
        256 * 1024 * 1024 - 16 * 1024 * 1024 - 4 * 1024 * 1024
    );

    assert_eq!(CACHE_QUOTA_BYTES, 64 * 1024 * 1024);
    assert_eq!(MAINTENANCE_RESERVE_BYTES, 4 * 1024 * 1024);
    assert_eq!(MUTATION_RESERVE_BYTES, 1024 * 1024);
    assert_eq!(
        CACHE_RECORDING_CEILING_BYTES,
        64 * 1024 * 1024 - 4 * 1024 * 1024 - 1024 * 1024
    );

    // 2. Ledger Capacity Report
    let ledger_dir = temp_private_dir("ledger-cap-report");
    let open_res = open_ledger(
        &invocation,
        &cx,
        LedgerAccess::Initialize,
        LedgerLocation::Directory(ledger_dir.clone()),
    )
    .expect("open_ledger");
    let LedgerOpen::Ready(ledger_store) = open_res else {
        panic!("expected LedgerOpen::Ready");
    };

    let report = ledger_store.capacity_report().expect("capacity_report");
    assert_eq!(report.total_quota_bytes, LEDGER_QUOTA_BYTES);
    assert_eq!(
        report.maintenance_reserve_bytes,
        LEDGER_MAINTENANCE_RESERVE_BYTES
    );
    assert_eq!(report.mutation_reserve_bytes, LEDGER_MUTATION_RESERVE_BYTES);
    assert_eq!(
        report.recording_ceiling_bytes,
        LEDGER_RECORDING_CEILING_BYTES
    );
    assert!(report.occupied_bytes > 0);
    assert!(report.occupied_bytes < 1024 * 1024); // Fresh schema is small (~100-200 KiB)
    assert_eq!(
        report.usable_recording_bytes,
        LEDGER_RECORDING_CEILING_BYTES - report.occupied_bytes
    );
    assert!(report.available_disk_bytes > 0);
    assert!(report.is_recording_admitted);

    // 3. Cache Capacity Report
    let cache_dir = temp_private_dir("cache-cap-report");
    let cache_res = open_cache(
        &invocation,
        &cx,
        CacheAccess::Initialize,
        CacheLocation::Directory(cache_dir),
    )
    .expect("open_cache");
    let CacheOpen::Ready(cache_store) = cache_res else {
        panic!("expected CacheOpen::Ready");
    };

    let cache_report = cache_store
        .capacity_report()
        .expect("cache capacity_report");
    assert_eq!(cache_report.total_quota_bytes, CACHE_QUOTA_BYTES);
    assert_eq!(
        cache_report.maintenance_reserve_bytes,
        MAINTENANCE_RESERVE_BYTES
    );
    assert_eq!(cache_report.mutation_reserve_bytes, MUTATION_RESERVE_BYTES);
    assert_eq!(
        cache_report.recording_ceiling_bytes,
        CACHE_RECORDING_CEILING_BYTES
    );
    assert!(cache_report.occupied_bytes > 0);
    assert!(cache_report.is_recording_admitted);
}

#[test]
fn filling_recording_capacity_blocks_mutations_and_preserves_history() {
    let (invocation, cx) = test_invocation();
    let clock = invocation.clock();

    let ledger_dir = temp_private_dir("ledger-ceiling-block");
    let open_res = open_ledger(
        &invocation,
        &cx,
        LedgerAccess::Initialize,
        LedgerLocation::Directory(ledger_dir.clone()),
    )
    .expect("open_ledger");
    let LedgerOpen::Ready(mut store) = open_res else {
        panic!("expected LedgerOpen::Ready");
    };
    let stamp = store.stamp();

    // 1. Successfully record baseline snapshot, ranking event, candidate, and attempt
    let snap = snapshot_fixture("snap-baseline");
    store
        .record_roster_snapshot(clock, &cx, &snap, stamp)
        .expect("record snapshot");
    let event = event_fixture("event-baseline", "snap-baseline");
    let cand = candidate_fixture("event-baseline", "review");
    store
        .record_ranking_event(clock, &cx, &event, &[cand], None, stamp)
        .expect("record event");
    let attempt = attempt_fixture("attempt-baseline", "event-baseline");
    store
        .record_provider_attempt(clock, &cx, &attempt, stamp)
        .expect("record attempt");

    // 2. Simulate reaching recording ceiling by creating a dummy file in the private ledger directory
    // Ceiling is 236 MiB. Create a 237 MiB dummy file.
    let dummy_path = ledger_dir.join("ledger.sqlite3.bak_dummy");
    let dummy_file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&dummy_path)
        .expect("open dummy file");
    dummy_file
        .set_len(LEDGER_RECORDING_CEILING_BYTES + 1024 * 1024)
        .expect("set_len dummy file");
    drop(dummy_file);

    // 3. Verify capacity report reflects exhausted recording capacity
    let report = store.capacity_report().expect("capacity_report");
    assert!(!report.is_recording_admitted);
    assert_eq!(report.usable_recording_bytes, 0);
    assert!(report.occupied_bytes > LEDGER_RECORDING_CEILING_BYTES);

    // 4. Mutations must now be REFUSED with StoreError::Quota
    let event2 = event_fixture("event-blocked", "snap-baseline");
    let cand2 = candidate_fixture("event-blocked", "review");
    let err = store
        .record_ranking_event(
            clock,
            &cx,
            &event2,
            std::slice::from_ref(&cand2),
            None,
            stamp,
        )
        .unwrap_err();
    assert_eq!(err, StoreError::Quota);

    let snap2 = snapshot_fixture("snap-blocked");
    let err_snap = store
        .record_roster_snapshot(clock, &cx, &snap2, stamp)
        .unwrap_err();
    assert_eq!(err_snap, StoreError::Quota);

    let attempt2 = attempt_fixture("attempt-blocked", "event-baseline");
    let err_att = store
        .record_provider_attempt(clock, &cx, &attempt2, stamp)
        .unwrap_err();
    assert_eq!(err_att, StoreError::Quota);

    // 5. Existing history must be completely intact and uncorrupted!
    let raw_conn = Connection::open(store.database_path()).expect("open raw conn");
    let event_count: i64 = raw_conn
        .query_row(
            "SELECT count(*) FROM ranking_events WHERE event_id = 'event-baseline'",
            [],
            |r| r.get(0),
        )
        .expect("query event");
    assert_eq!(event_count, 1);

    let attempt_count: i64 = raw_conn
        .query_row(
            "SELECT count(*) FROM provider_attempts WHERE attempt_id = 'attempt-baseline'",
            [],
            |r| r.get(0),
        )
        .expect("query attempt");
    assert_eq!(attempt_count, 1);

    // 6. Cleanup dummy file and verify recording admission is restored
    fs::remove_file(&dummy_path).expect("remove dummy file");
    let restored_report = store.capacity_report().expect("capacity_report restored");
    assert!(restored_report.is_recording_admitted);
    assert!(restored_report.usable_recording_bytes > 0);

    // Now recording succeeds again!
    store
        .record_ranking_event(clock, &cx, &event2, &[cand2], None, stamp)
        .expect("record event after restored capacity");
}

#[test]
fn maintenance_preflight_and_execution_succeed_within_reserve_at_recording_ceiling() {
    let (invocation, cx) = test_invocation();
    let clock = invocation.clock();

    let ledger_dir = temp_private_dir("ledger-reserve-maint");
    let open_res = open_ledger(
        &invocation,
        &cx,
        LedgerAccess::Initialize,
        LedgerLocation::Directory(ledger_dir.clone()),
    )
    .expect("open_ledger");
    let LedgerOpen::Ready(mut store) = open_res else {
        panic!("expected LedgerOpen::Ready");
    };
    let stamp = store.stamp();

    // Populate baseline events
    let snap = snapshot_fixture("snap-1");
    store
        .record_roster_snapshot(clock, &cx, &snap, stamp)
        .expect("record snapshot");
    for i in 1..=5 {
        let ev = event_fixture(&format!("event-{}", i), "snap-1");
        let cand = candidate_fixture(&format!("event-{}", i), "review");
        store
            .record_ranking_event(clock, &cx, &ev, &[cand], None, stamp)
            .expect("record event");
    }

    // Set store at recording ceiling (236 MiB occupied, leaves 20 MiB of total quota headroom,
    // which safely contains the 16 MiB maintenance reserve)
    let dummy_path = ledger_dir.join("ledger.sqlite3.bak_sidecar");
    let dummy_file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&dummy_path)
        .expect("open dummy file");
    dummy_file
        .set_len(LEDGER_RECORDING_CEILING_BYTES)
        .expect("set_len dummy file");
    drop(dummy_file);

    let report = store.capacity_report().expect("capacity report");
    assert!(!report.is_recording_admitted); // Recording ceiling reached

    // Preflight VACUUM: DB is ~150 KiB, worst-case VACUUM space is ~1 MiB
    // Projected total: 236 MiB + 1 MiB = 237 MiB <= 256 MiB
    let preflight_vac = store
        .preflight_maintenance(MaintenanceKind::Vacuum)
        .expect("preflight vacuum");
    assert_eq!(preflight_vac.kind, MaintenanceKind::Vacuum);
    assert!(preflight_vac.projected_total_bytes <= LEDGER_QUOTA_BYTES);

    // Execute VACUUM within the reserved budget
    store.vacuum(clock, &cx).expect("vacuum within reserve");

    // Preflight Prune
    let preflight_prune = store
        .preflight_maintenance(MaintenanceKind::Prune)
        .expect("preflight prune");
    assert_eq!(preflight_prune.kind, MaintenanceKind::Prune);

    // Execute prune_events_before: prune events created before 2000 (which covers events created at 1000)
    let pruned_count = store
        .prune_events_before(2000, clock, &cx, stamp)
        .expect("prune_events_before");
    assert_eq!(pruned_count, 5);

    // Run checkpoint_truncate to reclaim WAL space
    store
        .checkpoint_truncate(clock, &cx)
        .expect("checkpoint_truncate");

    // Remove the sidecar to drop below ceiling
    fs::remove_file(&dummy_path).expect("remove sidecar");
    let report_after = store
        .capacity_report()
        .expect("capacity report after prune");
    assert!(report_after.is_recording_admitted);
}

#[test]
fn simulated_external_full_disk_fails_preflight_before_mutation_with_required_space_and_hint() {
    let (invocation, cx) = test_invocation();
    let clock = invocation.clock();

    let ledger_dir = temp_private_dir("ledger-disk-pressure");
    let open_res = open_ledger(
        &invocation,
        &cx,
        LedgerAccess::Initialize,
        LedgerLocation::Directory(ledger_dir.clone()),
    )
    .expect("open_ledger");
    let LedgerOpen::Ready(mut store) = open_res else {
        panic!("expected LedgerOpen::Ready");
    };
    let stamp = store.stamp();

    // Populate an event and attempt
    let snap = snapshot_fixture("snap-pressure");
    store
        .record_roster_snapshot(clock, &cx, &snap, stamp)
        .expect("record snapshot");
    let ev = event_fixture("event-pressure", "snap-pressure");
    let cand = candidate_fixture("event-pressure", "review");
    store
        .record_ranking_event(clock, &cx, &ev, &[cand], None, stamp)
        .expect("record event");
    let att = attempt_fixture("attempt-pressure", "event-pressure");
    store
        .record_provider_attempt(clock, &cx, &att, stamp)
        .expect("record attempt");

    // 1. Simulate external disk pressure: only 500 bytes available on disk!
    store.set_simulated_available_disk_bytes(Some(500));

    // 2. Preflight VACUUM must FAIL before mutation with required space and recovery hint
    let vac_err = store
        .preflight_maintenance(MaintenanceKind::Vacuum)
        .unwrap_err();
    match vac_err {
        MaintenanceError::InsufficientDiskSpace {
            available_bytes,
            required_additional_bytes,
            recovery_step,
        } => {
            assert_eq!(available_bytes, 500);
            assert!(
                required_additional_bytes >= 1024 * 1024,
                "expected at least 1 MiB worst-case VACUUM space, got {}",
                required_additional_bytes
            );
            assert!(
                recovery_step.contains("free disk space"),
                "expected recovery step to guide freeing disk space, got: {}",
                recovery_step
            );
        }
        other => panic!("expected InsufficientDiskSpace, got: {:?}", other),
    }

    // 3. Calling store.vacuum() must be rejected by preflight without executing VACUUM
    let err_vac = store.vacuum(clock, &cx).unwrap_err();
    assert!(matches!(
        err_vac,
        MaintenanceError::InsufficientDiskSpace { .. }
    ));

    // 4. Calling backup_to() must also be rejected by preflight
    let backup_path = ledger_dir.join("rejected_backup.sqlite3");
    let err_backup = store.backup_to(&backup_path, clock, &cx).unwrap_err();
    assert!(matches!(
        err_backup,
        MaintenanceError::InsufficientDiskSpace { .. }
    ));
    assert!(
        !backup_path.exists(),
        "backup file must NOT be created when preflight fails"
    );

    // 5. Preflight Migration must also report insufficient disk space
    let mig_err = store
        .preflight_maintenance(MaintenanceKind::Migration)
        .unwrap_err();
    match mig_err {
        MaintenanceError::InsufficientDiskSpace {
            available_bytes,
            required_additional_bytes,
            recovery_step,
        } => {
            assert_eq!(available_bytes, 500);
            assert!(
                required_additional_bytes >= 2 * 1024 * 1024,
                "expected at least 2 MiB worst-case migration space"
            );
            assert!(recovery_step.contains("free disk space"));
        }
        other => panic!("expected InsufficientDiskSpace, got: {:?}", other),
    }

    // 6. History and protected charges must be completely untouched!
    let raw_conn = Connection::open(store.database_path()).expect("open raw conn");
    let event_count: i64 = raw_conn
        .query_row(
            "SELECT count(*) FROM ranking_events WHERE event_id = 'event-pressure'",
            [],
            |r| r.get(0),
        )
        .expect("query event");
    assert_eq!(event_count, 1);
    let attempt_count: i64 = raw_conn
        .query_row(
            "SELECT count(*) FROM provider_attempts WHERE attempt_id = 'attempt-pressure'",
            [],
            |r| r.get(0),
        )
        .expect("query attempt");
    assert_eq!(attempt_count, 1);

    // 7. Remove simulated pressure and verify maintenance succeeds
    store.set_simulated_available_disk_bytes(None);
    store
        .vacuum(clock, &cx)
        .expect("vacuum succeeds after disk restored");
}

#[test]
fn maintenance_exceeding_hard_quota_fails_preflight_with_required_space_and_hint() {
    let (invocation, cx) = test_invocation();

    let ledger_dir = temp_private_dir("ledger-quota-exceed");
    let open_res = open_ledger(
        &invocation,
        &cx,
        LedgerAccess::Initialize,
        LedgerLocation::Directory(ledger_dir),
    )
    .expect("open_ledger");
    let LedgerOpen::Ready(store) = open_res else {
        panic!("expected LedgerOpen::Ready");
    };

    // Request maintenance requiring 300 MiB additional space (exceeds 256 MiB quota)
    let err = store
        .preflight_maintenance_with_bytes(MaintenanceKind::Backup, 300 * 1024 * 1024)
        .unwrap_err();

    match err {
        MaintenanceError::QuotaExceeded {
            occupied_bytes,
            required_additional_bytes,
            quota_bytes,
            recovery_step,
        } => {
            assert!(occupied_bytes > 0);
            assert_eq!(required_additional_bytes, 300 * 1024 * 1024);
            assert_eq!(quota_bytes, LEDGER_QUOTA_BYTES);
            assert!(
                recovery_step.contains("backup") || recovery_step.contains("quota"),
                "recovery step: {}",
                recovery_step
            );
        }
        other => panic!("expected QuotaExceeded, got: {:?}", other),
    }
}

#[test]
fn backup_to_creates_atomic_wal_inclusive_backup_and_preserves_charges() {
    let (invocation, cx) = test_invocation();
    let clock = invocation.clock();

    let ledger_dir = temp_private_dir("ledger-backup-atomic");
    let open_res = open_ledger(
        &invocation,
        &cx,
        LedgerAccess::Initialize,
        LedgerLocation::Directory(ledger_dir.clone()),
    )
    .expect("open_ledger");
    let LedgerOpen::Ready(mut store) = open_res else {
        panic!("expected LedgerOpen::Ready");
    };
    let stamp = store.stamp();

    // Populate data with uncheckpointed WAL state
    let snap = snapshot_fixture("snap-backup");
    store
        .record_roster_snapshot(clock, &cx, &snap, stamp)
        .expect("record snapshot");
    let ev = event_fixture("event-backup", "snap-backup");
    let cand = candidate_fixture("event-backup", "review");
    store
        .record_ranking_event(clock, &cx, &ev, &[cand], None, stamp)
        .expect("record event");
    let att = attempt_fixture("attempt-backup", "event-backup");
    store
        .record_provider_attempt(clock, &cx, &att, stamp)
        .expect("record attempt");

    // Perform backup to a separate backup file
    let backup_dir = temp_private_dir("ledger-backup-dest");
    let backup_file = backup_dir.join("ledger_backup.sqlite3");
    let backup_bytes = store
        .backup_to(&backup_file, clock, &cx)
        .expect("backup_to");
    assert!(backup_bytes > 0);
    assert!(backup_file.exists());

    // Inspect the backup database directly with SQLite
    let backup_conn = Connection::open(&backup_file).expect("open backup connection");
    let snap_count: i64 = backup_conn
        .query_row(
            "SELECT count(*) FROM roster_snapshots WHERE snapshot_id = 'snap-backup'",
            [],
            |r| r.get(0),
        )
        .expect("query backup snapshot");
    assert_eq!(snap_count, 1);

    let event_count: i64 = backup_conn
        .query_row(
            "SELECT count(*) FROM ranking_events WHERE event_id = 'event-backup'",
            [],
            |r| r.get(0),
        )
        .expect("query backup event");
    assert_eq!(event_count, 1);

    let attempt_count: i64 = backup_conn
        .query_row(
            "SELECT count(*) FROM provider_attempts WHERE attempt_id = 'attempt-backup'",
            [],
            |r| r.get(0),
        )
        .expect("query backup attempt");
    assert_eq!(attempt_count, 1);

    // Verify protected charges in original store are completely untouched
    let original_conn = Connection::open(store.database_path()).expect("open original connection");
    let orig_attempt_count: i64 = original_conn
        .query_row(
            "SELECT count(*) FROM provider_attempts WHERE attempt_id = 'attempt-backup'",
            [],
            |r| r.get(0),
        )
        .expect("query orig attempt");
    assert_eq!(orig_attempt_count, 1);
}
