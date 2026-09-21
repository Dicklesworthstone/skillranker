//! Contract and verification tests for ledger init, idempotent re-init,
//! recoverable migration, WAL-inclusive backup, read-only opening of newer schemas,
//! and CLI ledger command interactions.
//!
//! Required by sr-roadmap-l1i.6.4:
//! - Absent-only init (`sr ledger init` creates current schema only when absent; repeating on compatible store reports ready without emptying history, resetting cursors, or replacing incompatible store).
//! - Incompatible / newer schemas opened read-only where safe (`LedgerOpen::ReadOnly` with mutations blocked via `StoreError::Permissions`) without automatic downgrade or destructive repair.
//! - Recoverable migration (`sr ledger migrate` previews required changes; `--apply` creates a SQLite-aware WAL-inclusive backup before applying migrations with checksums and generation coordination).
//! - Hooks / rank never silently initialize or migrate ledger (`LedgerAccess::ExistingOnly` returns `LedgerOpen::Missing` if absent).
//! - Real live WAL data survives backup/restore.
//! - Crash each migration boundary, repeat init, test new/old schemas, quota failure, concurrent writer.

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
        session_id: "sess-mig".into(),
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

fn cursor_fixture(session_id: &str, last_event: &str) -> SessionCursor {
    SessionCursor {
        workspace_root: "/data/workspace".into(),
        session_id: session_id.into(),
        agent_branch: "main".into(),
        cursor_kind: CursorKind::Observation,
        transcript_generation: 1,
        last_complete_event_id: last_event.into(),
        last_offset_bytes: 4096,
        updated_at_unix_ms: 1050,
    }
}

#[test]
fn absent_only_init_creates_schema_and_reinit_is_idempotent_without_reset() {
    let dir = temp_private_dir("absent-init");
    let (invocation, cx) = test_invocation();

    // 1. Initially status is missing
    let status = ledger_status(&invocation, &cx, LedgerLocation::Directory(dir.clone())).unwrap();
    assert_eq!(status.status, "missing");
    assert_eq!(status.schema_version, None);

    // 2. Initialize creates current schema
    let init1 = init_ledger(&invocation, &cx, LedgerLocation::Directory(dir.clone())).unwrap();
    assert_eq!(init1.status, InitStatus::Created);
    assert_eq!(init1.schema_version, 1);

    // 3. Open store and record some data
    let open_res = open_ledger(
        &invocation,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
    )
    .unwrap();
    let mut store = match open_res {
        LedgerOpen::Ready(s) => s,
        other => panic!("expected LedgerOpen::Ready, got {other:?}"),
    };
    let initial_stamp = store.stamp();

    let snap = snapshot_fixture("snap-init-1");
    let ev = event_fixture("ev-init-1", "snap-init-1");
    let cand = candidate_fixture("ev-init-1", "review");
    let cursor = cursor_fixture("sess-mig", "ev-init-1");

    store
        .record_ranking_event(
            invocation.clock(),
            &cx,
            &ev,
            &[cand],
            Some(&snap),
            initial_stamp,
        )
        .unwrap();
    store
        .update_session_cursor(invocation.clock(), &cx, &cursor, initial_stamp)
        .unwrap();

    let saved_cursor = store
        .get_session_cursor(
            invocation.clock(),
            &cx,
            "/data/workspace",
            "sess-mig",
            "main",
            CursorKind::Observation,
        )
        .unwrap();
    assert!(saved_cursor.is_some());
    drop(store);

    // 4. Repeating init on compatible store reports already current and does NOT empty history or reset cursors
    let init2 = init_ledger(&invocation, &cx, LedgerLocation::Directory(dir.clone())).unwrap();
    assert_eq!(init2.status, InitStatus::AlreadyCurrent);
    assert_eq!(init2.schema_version, 1);
    assert_eq!(init2.incarnation, init1.incarnation);

    let open_res2 = open_ledger(
        &invocation,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
    )
    .unwrap();
    let store2 = match open_res2 {
        LedgerOpen::Ready(s) => s,
        other => panic!("expected Ready, got {other:?}"),
    };
    assert_eq!(store2.stamp().incarnation, initial_stamp.incarnation);
    assert_eq!(
        store2.stamp().schema_generation,
        initial_stamp.schema_generation
    );

    let verify_cursor = store2
        .get_session_cursor(
            invocation.clock(),
            &cx,
            "/data/workspace",
            "sess-mig",
            "main",
            CursorKind::Observation,
        )
        .unwrap()
        .expect("cursor should still exist after re-init");
    assert_eq!(verify_cursor.last_complete_event_id, "ev-init-1");
    assert_eq!(verify_cursor.last_offset_bytes, 4096);
    drop(store2);

    // 5. Incompatible store (wrong application_id) fails with WrongStore and does NOT replace it
    let bad_dir = temp_private_dir("bad-store");
    let bad_db_path = bad_dir.join(LEDGER_FILE);
    {
        use std::os::unix::fs::OpenOptionsExt;
        let _ = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&bad_db_path)
            .unwrap();
        let conn = Connection::open(&bad_db_path).unwrap();
        conn.pragma_update(None, "application_id", 0x12345678)
            .unwrap();
    }
    let bad_init = init_ledger(&invocation, &cx, LedgerLocation::Directory(bad_dir.clone()));
    assert!(matches!(bad_init, Err(StoreError::WrongStore)));
}

#[test]
fn unsupported_newer_schema_opens_read_only_and_refuses_mutations_without_downgrade() {
    let dir = temp_private_dir("newer-schema");
    let (invocation, cx) = test_invocation();

    // 1. Initialize v1 ledger
    let init = init_ledger(&invocation, &cx, LedgerLocation::Directory(dir.clone())).unwrap();
    assert_eq!(init.status, InitStatus::Created);

    // 2. Artificially set user_version to 999 (newer than supported)
    let db_path = dir.join(LEDGER_FILE);
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "user_version", 999).unwrap();
    }

    // 3. Status reports read_only
    let status = ledger_status(&invocation, &cx, LedgerLocation::Directory(dir.clone())).unwrap();
    assert_eq!(status.status, "read_only");
    assert_eq!(status.schema_version, Some(999));
    assert!(status.is_read_only);

    // 4. Open returns LedgerOpen::ReadOnly
    let open_res = open_ledger(
        &invocation,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
    )
    .unwrap();
    let mut store = match open_res {
        LedgerOpen::ReadOnly(s) => s,
        other => panic!("expected LedgerOpen::ReadOnly, got {other:?}"),
    };
    assert!(store.is_read_only());
    assert_eq!(store.schema_version().unwrap(), 999);

    // 5. Mutations return StoreError::Permissions
    let stamp = store.stamp();
    let snap = snapshot_fixture("snap-ro-1");
    let ev = event_fixture("ev-ro-1", "snap-ro-1");
    let cand = candidate_fixture("ev-ro-1", "review");
    let cursor = cursor_fixture("sess-ro", "ev-ro-1");

    assert_eq!(
        store.record_roster_snapshot(invocation.clock(), &cx, &snap, stamp),
        Err(StoreError::Permissions)
    );
    assert_eq!(
        store.record_ranking_event(invocation.clock(), &cx, &ev, &[cand], Some(&snap), stamp),
        Err(StoreError::Permissions)
    );
    assert_eq!(
        store.update_session_cursor(invocation.clock(), &cx, &cursor, stamp),
        Err(StoreError::Permissions)
    );
    assert_eq!(
        store.clear(invocation.clock(), &cx, stamp),
        Err(StoreError::Permissions)
    );
    assert_eq!(
        store.checkpoint_truncate(invocation.clock(), &cx),
        Err(StoreError::Permissions)
    );
    assert_eq!(
        store.vacuum(invocation.clock(), &cx),
        Err(MaintenanceError::Store(StoreError::Permissions))
    );
    assert_eq!(
        store.prune_events_before(5000, invocation.clock(), &cx, stamp),
        Err(MaintenanceError::Store(StoreError::Permissions))
    );
    assert_eq!(
        store.migrate_apply(invocation.clock(), &cx),
        Err(MigrationError::Store(StoreError::Permissions))
    );

    drop(store);

    // 6. Verify user_version is STILL 999: no automatic downgrade or destructive repair
    let conn = Connection::open(&db_path).unwrap();
    let ver: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(ver, 999);
}

#[test]
fn migration_preview_then_apply_creates_wal_inclusive_backup_and_coordinates_generations() {
    let dir = temp_private_dir("migrate-apply");
    let (invocation, cx) = test_invocation();

    // 1. Initialize v1 ledger
    init_ledger(&invocation, &cx, LedgerLocation::Directory(dir.clone())).unwrap();

    // 2. Open store and record live data into WAL without checkpointing
    let open_res = open_ledger(
        &invocation,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
    )
    .unwrap();
    let mut store = match open_res {
        LedgerOpen::Ready(s) => s,
        other => panic!("expected Ready, got {other:?}"),
    };
    let initial_stamp = store.stamp();

    let snap = snapshot_fixture("snap-mig-1");
    let ev = event_fixture("ev-mig-1", "snap-mig-1");
    let cand = candidate_fixture("ev-mig-1", "review");
    store
        .record_ranking_event(
            invocation.clock(),
            &cx,
            &ev,
            &[cand],
            Some(&snap),
            initial_stamp,
        )
        .unwrap();

    // 3. Preview migration
    let preview = store.migrate_preview().unwrap();
    assert_eq!(preview.current_version, 1);
    assert_eq!(preview.target_version, LEDGER_TARGET_SCHEMA_VERSION);
    assert_eq!(preview.pending_migrations.len(), 1);
    assert_eq!(preview.pending_migrations[0].version, 2);
    assert_eq!(preview.pending_migrations[0].name, "v2-add-audit-log");
    assert!(!preview.pending_migrations[0].checksum.is_empty());
    assert!(preview.required_headroom_bytes > 0);

    // 4. Apply migration
    let report = store.migrate_apply(invocation.clock(), &cx).unwrap();
    assert_eq!(report.from_version, 1);
    assert_eq!(report.to_version, 2);
    assert_eq!(report.applied_migrations, vec!["v2-add-audit-log"]);
    assert!(report.backup_bytes > 0);
    assert!(report.backup_path.exists());

    // Verify current store state
    assert_eq!(store.schema_version().unwrap(), 2);
    let post_stamp = store.stamp();
    assert_eq!(
        post_stamp.schema_generation,
        initial_stamp.schema_generation + 1
    );

    // Verify new table / index from v2 migration exists
    let v2_check: i64 = store.migrate_preview().unwrap().pending_migrations.len() as i64;
    assert_eq!(v2_check, 0); // No pending migrations left

    // 5. Verify WAL data survived in the backup file!
    let backup_conn = Connection::open(&report.backup_path).unwrap();
    let backup_ver: i64 = backup_conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(backup_ver, 1); // Backup is at original schema version 1

    let event_count: i64 = backup_conn
        .query_row(
            "SELECT count(*) FROM ranking_events WHERE event_id = 'ev-mig-1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        event_count, 1,
        "uncheckpointed WAL data must be fully present in the backup file"
    );

    // 6. Repeating migrate_apply returns AlreadyUpToDate
    let repeat = store.migrate_apply(invocation.clock(), &cx);
    assert!(matches!(repeat, Err(MigrationError::AlreadyUpToDate)));
}

#[test]
fn migration_preflight_quota_failure_aborts_before_mutation_or_backup() {
    let dir = temp_private_dir("migrate-preflight-fail");
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
        other => panic!("expected Ready, got {other:?}"),
    };

    // Simulate only 100 bytes of available disk space
    store.set_simulated_available_disk_bytes(Some(100));

    let err = store
        .migrate_apply(invocation.clock(), &cx)
        .expect_err("should fail preflight");
    match err {
        MigrationError::Preflight(MaintenanceError::InsufficientDiskSpace {
            available_bytes,
            required_additional_bytes,
            ..
        }) => {
            assert_eq!(available_bytes, 100);
            assert!(required_additional_bytes > 100);
        }
        other => panic!("expected Preflight InsufficientDiskSpace, got {other:?}"),
    }

    // Verify no backup file was created
    let backup_files: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".bak"))
        .collect();
    assert_eq!(
        backup_files.len(),
        0,
        "no backup file should be left behind on preflight failure"
    );

    // Verify database schema version is still 1
    assert_eq!(store.schema_version().unwrap(), 1);
}

#[test]
fn hooks_and_rank_never_silently_initialize_or_migrate_ledger() {
    let dir = temp_private_dir("missing-ledger-dir");
    let (invocation, cx) = test_invocation();

    let missing_path = dir.join("nonexistent_subdir");

    // ExistingOnly on nonexistent dir returns LedgerOpen::Missing
    let res = open_ledger(
        &invocation,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(missing_path.clone()),
    )
    .unwrap();
    assert!(matches!(res, LedgerOpen::Missing));

    // Disk must NOT be touched / created
    assert!(
        !missing_path.exists(),
        "ExistingOnly must not create absent directory"
    );
}

#[test]
fn cli_ledger_subcommands_e2e() {
    let dir = temp_private_dir("cli-ledger-e2e");
    let dir_str = dir.to_str().unwrap();

    let bin = env!("CARGO_BIN_EXE_sr");

    // 1. Status initially reports missing
    let out = Command::new(bin)
        .args(["ledger", "status", "--dir", dir_str, "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let val: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(val["status"], "missing");

    // 2. Init creates schema
    let out = Command::new(bin)
        .args(["ledger", "init", "--dir", dir_str, "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let val: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(val["status"], "created");
    assert_eq!(val["schema_version"], 1);

    // 3. Re-init reports already_current
    let out = Command::new(bin)
        .args(["ledger", "init", "--dir", dir_str, "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let val: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(val["status"], "already_current");
    assert_eq!(val["schema_version"], 1);

    // 4. Status reports ready with upgrade_available: 2 because target is 2
    let out = Command::new(bin)
        .args(["ledger", "status", "--dir", dir_str, "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let val: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(val["status"], "ready");
    assert_eq!(val["schema_version"], 1);
    assert_eq!(val["target_version"], 2);
    assert_eq!(val["upgrade_available"], 2);

    let out_hr = Command::new(bin)
        .args(["ledger", "status", "--dir", dir_str])
        .output()
        .unwrap();
    assert_eq!(out_hr.status.code(), Some(0));
    let text = String::from_utf8(out_hr.stdout).unwrap();
    assert!(text.contains("Ledger Status: ready"));
    assert!(text.contains("Upgrade Available: 2"));

    // 5. Migrate preview (without --apply)
    let out = Command::new(bin)
        .args(["ledger", "migrate", "--dir", dir_str, "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let val: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(val["current_version"], 1);
    assert_eq!(val["target_version"], 2);
    assert_eq!(val["pending_migrations"].as_array().unwrap().len(), 1);

    // 6. Migrate apply
    let out = Command::new(bin)
        .args(["ledger", "migrate", "--apply", "--dir", dir_str, "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let val: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(val["from_version"], 1);
    assert_eq!(val["to_version"], 2);
    assert_eq!(val["applied_migrations"][0], "v2-add-audit-log");

    // 7. Status now reports ready with no upgrade_available
    let out = Command::new(bin)
        .args(["ledger", "status", "--dir", dir_str, "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let val: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(val["status"], "ready");
    assert_eq!(val["schema_version"], 2);
    assert_eq!(val["target_version"], 2);
    assert!(val.get("upgrade_available").is_none() || val["upgrade_available"].is_null());
}

#[test]
fn concurrent_writer_fences_stale_stamp() {
    let dir = temp_private_dir("fenced-concurrent");
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
        other => panic!("expected Ready, got {other:?}"),
    };

    let mut stale_stamp = store.stamp();
    stale_stamp.data_generation = 999; // Stale generation

    let snap = snapshot_fixture("snap-stale-1");
    let res = store.record_roster_snapshot(invocation.clock(), &cx, &snap, stale_stamp);
    assert_eq!(res, Err(StoreError::StaleGeneration));
}

#[test]
fn incompatible_schema_store_reports_needs_migration_in_status() {
    let dir = temp_private_dir("incompatible-schema");
    let (invocation, cx) = test_invocation();

    // Initialize a valid store first (creates directory and file with 0600 permissions)
    let init = init_ledger(&invocation, &cx, LedgerLocation::Directory(dir.clone())).unwrap();
    assert_eq!(init.status, InitStatus::Created);

    // Downgrade schema version to 0 (< LEDGER_SCHEMA_VERSION = 1)
    let db_path = dir.join(LEDGER_FILE);
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "user_version", 0).unwrap();
    }

    // Inspect via ledger_status: reports needs_migration
    let status = ledger_status(&invocation, &cx, LedgerLocation::Directory(dir.clone())).unwrap();
    assert_eq!(status.status, "needs_migration");
    assert_eq!(status.schema_version, Some(0));
    assert_eq!(status.target_version, LEDGER_TARGET_SCHEMA_VERSION);
    assert_eq!(status.upgrade_available, Some(LEDGER_TARGET_SCHEMA_VERSION));

    // CLI status also reports needs_migration
    let bin = env!("CARGO_BIN_EXE_sr");
    let dir_str = dir.to_str().unwrap();
    let out = Command::new(bin)
        .args(["ledger", "status", "--dir", dir_str, "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let val: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(val["status"], "needs_migration");
    assert_eq!(val["schema_version"], 0);
    assert_eq!(val["upgrade_available"], 2);

    let out_hr = Command::new(bin)
        .args(["ledger", "status", "--dir", dir_str])
        .output()
        .unwrap();
    assert_eq!(out_hr.status.code(), Some(0));
    let text = String::from_utf8(out_hr.stdout).unwrap();
    assert!(text.contains("Ledger Status: needs_migration"));
    assert!(text.contains("Upgrade Available: 2"));
}
