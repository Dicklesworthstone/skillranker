#![cfg(unix)]
//! Contract and verification tests for exposure lifecycle and acknowledgment
//! (sr-roadmap-l1i.6.8 / Invariant I17).
//!
//! Required behavior:
//! - Record generated, prepared, emitted and acknowledged separately.
//! - Database commit and stdout cannot form one atomic transaction; crash ambiguity remains unknown.
//! - Before stdout output, best-effort commit a prepared event; after successful bounded stdout write, transition to emitted.
//! - A crash or write failure between these leaves delivery unknown (persisted as prepared).
//! - Empty shadow output must never qualify as an emitted recommendation (0 bytes written in shadow mode returns false, remains prepared).
//! - Acknowledgment requires verified harness evidence (`record_acknowledgment` with `verified_delivery_key`).
//! - Duplicate delivery keys across different events must be rejected (`StoreError::RecordConflict`).
//! - Re-acknowledging the same event with the same delivery key is idempotent.
//! - Record mode and channel explicitly (`shadow`, `advisory-hook`, `cli`, or `tui`).
//! - Disabled and missing ledger states degrade gracefully without panicking or corrupting state.

use asupersync::Cx;
use rusqlite::Connection;
use skillranker::runtime::ProcessInvocation;
use skillranker::storage::StoreError;
use skillranker::storage::ledger::*;
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn test_invocation() -> (ProcessInvocation, Cx) {
    let inv = ProcessInvocation::enter().expect("process invocation");
    let cx = inv.request_cx().expect("request cx");
    (inv, cx)
}

fn temp_private_dir(prefix: &str) -> PathBuf {
    let dir = PathBuf::from("/tmp").join(format!(
        "sr-test-exposure-{}-{}-{}",
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

fn make_test_event(event_id: &str, mode_channel: &str, state: ExposureState) -> NewRankingEvent {
    NewRankingEvent {
        event_id: event_id.into(),
        verified_delivery_key: None,
        workspace_root: "/data/workspace".into(),
        session_id: "session-exp-test".into(),
        agent_branch: "main".into(),
        mode_channel: mode_channel.into(),
        policy_version: "v1".into(),
        schema_version: 1,
        decision: DecisionKind::Ranked,
        reason: "eligible".into(),
        exposure_state: state,
        elapsed_ms: 15,
        created_at_unix_ms: 1_700_000_000,
        input_tokens: Some(100),
        output_tokens: Some(25),
        snapshot_id: None,
    }
}

fn make_test_candidate(event_id: &str, skill_id: &str) -> NewRankingCandidate {
    NewRankingCandidate {
        event_id: event_id.into(),
        stage: CandidateStage::Rerank,
        skill_id: skill_id.into(),
        skill_version: "1.0.0".into(),
        raw_probability: Some(0.85),
        normalized_probability: Some(0.85),
        fit_score: Some(0.9),
        rank_score: Some(0.9),
        rank_position: Some(1),
        excluded: false,
        exclusion_reason: None,
    }
}

/// Exercise publication through the actual CLI, with every platform path isolated.
struct CliDeliveryFixture {
    root: PathBuf,
    event_id: String,
}

impl CliDeliveryFixture {
    fn new() -> Self {
        let root = temp_private_dir("actual-cli-delivery");
        let workspace = root.join("workspace");
        let skill = workspace.join(".claude/skills/delivery-review");
        fs::create_dir_all(&skill).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        fs::write(skill.join("SKILL.md"), "---\nname: delivery-review\ndescription: Review Rust code\n---\nReview Rust code carefully.\n").unwrap();
        fs::write(
            workspace.join("context.json"),
            serde_json::json!({
                "schema_version": 1, "harness": "claude_code", "producer_id": "delivery-test",
                "workspace_root": workspace, "session_id": "delivery-session", "agent_id": "agent",
                "branch_id": "main", "context_epoch": "epoch-0",
                "current_request": {"event_id": "delivery-turn", "text": "Review the code",
                    "attachments_omitted": false, "essential_attachment_missing": false},
                "events": [], "explicit_skill_references": [], "supplied_loads": []
            })
            .to_string(),
        )
        .unwrap();
        let namespace = serde_json::json!([
            "skillranker.normalized-ranking.v2",
            1,
            workspace,
            "claude_code",
            "delivery-test",
            "agent",
            "delivery-session"
        ]);
        let bytes = serde_json::to_vec(&serde_json::json!([
            namespace,
            "main",
            "epoch-0",
            "delivery-turn",
            null
        ]))
        .unwrap();
        let event_id = format!("ranking-event-v2-{}", blake3::hash(&bytes).to_hex());
        Self { root, event_id }
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        std::process::Command::new(env!("CARGO_BIN_EXE_sr"))
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("home/.config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("TERM", "dumb")
            .current_dir(self.root.join("workspace"))
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    }

    fn rank(&self, bare: bool, flags: &[&str]) -> std::process::Output {
        let mut args = Vec::new();
        if !bare {
            args.push("rank");
        }
        args.extend([
            "--context",
            "context.json",
            "--require-skill",
            "delivery-review",
            "--offline",
            "--no-cache",
        ]);
        args.extend_from_slice(flags);
        self.run(&args)
    }

    fn database(&self) -> Connection {
        Connection::open(self.root.join("data/sr").join(LEDGER_FILE)).unwrap()
    }

    fn initialize(&self) {
        let out = self.run(&["ledger", "init", "--json"]);
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        let out = self.rank(false, &["--json"]);
        assert_explicit_delivery(&out, &self.event_id);
        assert_eq!(self.exposure(), "emitted");
    }

    fn prepare_existing(&self) -> Vec<(String, Vec<Vec<rusqlite::types::Value>>)> {
        let db = self.database();
        assert_eq!(
            db.execute(
                "UPDATE ranking_events SET exposure_state = 'prepared' WHERE event_id = ?1",
                [&self.event_id],
            )
            .unwrap(),
            1
        );
        drop(db);
        self.contents()
    }

    fn exposure(&self) -> String {
        self.database()
            .query_row(
                "SELECT exposure_state FROM ranking_events WHERE event_id = ?1",
                [&self.event_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn contents(&self) -> Vec<(String, Vec<Vec<rusqlite::types::Value>>)> {
        let db = self.database();
        let tables: Vec<String> = db
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        tables
            .into_iter()
            .map(|table| {
                let mut statement = db
                    .prepare(&format!("SELECT * FROM \"{}\"", table.replace('"', "\"\"")))
                    .unwrap();
                let columns = statement.column_count();
                let rows = statement
                    .query_map([], |row| {
                        (0..columns)
                            .map(|column| row.get(column))
                            .collect::<Result<Vec<rusqlite::types::Value>, _>>()
                    })
                    .unwrap()
                    .map(Result::unwrap)
                    .collect();
                (table, rows)
            })
            .collect()
    }
}

fn assert_explicit_delivery(out: &std::process::Output, event_id: &str) -> serde_json::Value {
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["decision"], "explicit");
    assert_eq!(value["event_id"], event_id);
    assert_eq!(value["skills"][0]["invocation_name"], "delivery-review");
    assert_eq!(value["usage"]["http_attempts"], 0);
    value
}

#[test]
fn actual_cli_persistence_opt_outs_and_previews_leave_existing_ledger_unchanged() {
    let fixture = CliDeliveryFixture::new();
    fixture.initialize();
    for bare in [false, true] {
        for flag in ["--no-ledger", "--no-persist"] {
            let before = fixture.prepare_existing();
            let out = fixture.rank(bare, &["--json", flag]);
            assert_eq!(
                assert_explicit_delivery(&out, &fixture.event_id)["persistence"],
                "disabled"
            );
            assert_eq!(fixture.contents(), before, "bare={bare}, {flag}");
        }
        let before = fixture.prepare_existing();
        let out = fixture.rank(bare, &["--json", "--dry-run"]);
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        let preview: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(preview["local_decision"]["decision"], "explicit");
        assert_eq!(preview["local_decision"]["event_id"], fixture.event_id);
        assert_eq!(fixture.contents(), before, "bare={bare}, dry-run");
    }
}

#[test]
fn actual_cli_json_and_table_delivery_record_authorized_emission() {
    let fixture = CliDeliveryFixture::new();
    fixture.initialize();
    for bare in [false, true] {
        for format in ["--json", "--table"] {
            fixture.prepare_existing();
            let out = fixture.rank(bare, &[format]);
            assert_eq!(out.status.code(), Some(0), "{out:?}");
            if format == "--json" {
                assert_eq!(
                    assert_explicit_delivery(&out, &fixture.event_id)["persistence"],
                    "recorded"
                );
            } else {
                assert!(String::from_utf8_lossy(&out.stdout).contains("delivery-review"));
            }
            assert_eq!(fixture.exposure(), "emitted", "bare={bare}, {format}");
        }
    }
}

#[test]
fn actual_cli_opt_outs_and_previews_never_initialize_missing_state() {
    let fixture = CliDeliveryFixture::new();
    for flag in ["--no-ledger", "--no-persist", "--dry-run"] {
        let out = fixture.rank(false, &["--json", flag]);
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let decision = value.get("local_decision").unwrap_or(&value);
        assert_eq!(decision["decision"], "explicit");
        assert_eq!(decision["skills"][0]["invocation_name"], "delivery-review");
        assert!(!fixture.root.join("data").exists(), "{flag}");
        assert!(!fixture.root.join("cache").exists(), "{flag}");
    }
}

/// 1. Prepare before stdout, and transition to emitted after successful bounded stdout write.
#[test]
fn prepare_before_stdout_and_emit_after_successful_write() {
    let dir = temp_private_dir("prep-emit");
    let (inv, cx) = test_invocation();

    init_ledger(&inv, &cx, LedgerLocation::Directory(dir.clone())).expect("init ledger");

    let event_id = "ev-lifecycle-1";
    let event = make_test_event(event_id, "cli", ExposureState::Prepared);
    let cand = make_test_candidate(event_id, "skill-a");

    // 1. Commit prepared event to ledger before stdout write
    let recorded = record_ranking(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        &event,
        &[cand],
        None,
    )
    .expect("record prepared ranking");
    assert!(recorded, "prepared event should be recorded");

    // Verify in database: exposure_state is 'prepared'
    let db_path = dir.join(LEDGER_FILE);
    let conn = Connection::open(&db_path).expect("open db");
    let (state_db, key_db): (String, Option<String>) = conn
        .query_row(
            "SELECT exposure_state, verified_delivery_key FROM ranking_events WHERE event_id = ?1",
            [event_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("query event");
    assert_eq!(state_db, "prepared", "must be prepared before stdout write");
    assert!(key_db.is_none());

    let gen_before: i64 = conn
        .query_row(
            "SELECT data_generation FROM store_meta WHERE singleton = 1",
            [],
            |r| r.get(0),
        )
        .expect("query gen");

    // 2. Successful stdout write of 256 bytes transitions to emitted
    let emitted = record_emission(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        event_id,
        256,
    )
    .expect("record emission");
    assert!(emitted, "emission should transition successfully");

    // Verify in database: exposure_state is now 'emitted', data_generation bumped
    let (state_db2, gen_after): (String, i64) = conn
        .query_row(
            "SELECT exposure_state, (SELECT data_generation FROM store_meta WHERE singleton = 1) FROM ranking_events WHERE event_id = ?1",
            [event_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("query event after emission");
    assert_eq!(
        state_db2, "emitted",
        "must transition to emitted after stdout write"
    );
    assert_eq!(
        gen_after,
        gen_before + 1,
        "data generation must advance on emission"
    );

    // 3. Repeating emission is idempotent
    let emitted_again = record_emission(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        event_id,
        256,
    )
    .expect("record emission idempotent");
    assert!(emitted_again, "repeated emission must succeed idempotently");
}

/// 2. Crash or write failure between prepare and stdout write leaves delivery unknown (persisted as prepared).
#[test]
fn crash_or_failure_between_prepare_and_stdout_leaves_delivery_unknown() {
    let dir = temp_private_dir("crash-sim");
    let (inv, cx) = test_invocation();

    init_ledger(&inv, &cx, LedgerLocation::Directory(dir.clone())).expect("init ledger");

    let event_id = "ev-crash-1";
    let event = make_test_event(event_id, "cli", ExposureState::Prepared);
    let cand = make_test_candidate(event_id, "skill-crash");

    // Prepare is committed
    record_ranking(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        &event,
        &[cand],
        None,
    )
    .expect("record prepared ranking");

    // Simulate process termination / broken pipe before record_emission can be called.
    // Database inspection confirms: state remains 'prepared', not 'emitted', capturing unknown delivery.
    let db_path = dir.join(LEDGER_FILE);
    let conn = Connection::open(&db_path).expect("open db");
    let state_db: String = conn
        .query_row(
            "SELECT exposure_state FROM ranking_events WHERE event_id = ?1",
            [event_id],
            |r| r.get(0),
        )
        .expect("query event");
    assert_eq!(
        state_db, "prepared",
        "unemitted event remains prepared, preserving delivery uncertainty across crashes"
    );
}

/// 3. Empty shadow output must never qualify as an emitted recommendation.
#[test]
fn empty_shadow_output_never_emits() {
    let dir = temp_private_dir("shadow-empty");
    let (inv, cx) = test_invocation();

    init_ledger(&inv, &cx, LedgerLocation::Directory(dir.clone())).expect("init ledger");

    let event_id = "ev-shadow-1";
    let event = make_test_event(event_id, "shadow", ExposureState::Prepared);
    let cand = make_test_candidate(event_id, "skill-shadow");

    record_ranking(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        &event,
        &[cand],
        None,
    )
    .expect("record prepared ranking");

    // In shadow mode with 0 bytes written (e.g. no recommendation shown to user):
    let emitted = record_emission(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        event_id,
        0,
    )
    .expect("record emission on shadow with 0 bytes");
    assert!(!emitted, "0 bytes written in shadow mode must return false");

    // Database check: state MUST remain 'prepared', never 'emitted'
    let db_path = dir.join(LEDGER_FILE);
    let conn = Connection::open(&db_path).expect("open db");
    let state_db: String = conn
        .query_row(
            "SELECT exposure_state FROM ranking_events WHERE event_id = ?1",
            [event_id],
            |r| r.get(0),
        )
        .expect("query event");
    assert_eq!(
        state_db, "prepared",
        "empty shadow output must never transition to emitted"
    );

    // Also test: any channel with 0 bytes written does not emit
    let event_id_cli = "ev-cli-zero";
    let event_cli = make_test_event(event_id_cli, "cli", ExposureState::Prepared);
    let cand_cli = make_test_candidate(event_id_cli, "skill-cli");
    record_ranking(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        &event_cli,
        &[cand_cli],
        None,
    )
    .expect("record prepared ranking");

    let emitted_cli = record_emission(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        event_id_cli,
        0,
    )
    .expect("record emission on cli with 0 bytes");
    assert!(!emitted_cli, "0 bytes written on any channel must not emit");

    let state_cli: String = conn
        .query_row(
            "SELECT exposure_state FROM ranking_events WHERE event_id = ?1",
            [event_id_cli],
            |r| r.get(0),
        )
        .expect("query event");
    assert_eq!(state_cli, "prepared", "0-byte cli write remains prepared");
}

/// 4. Verified acknowledgment lifecycle and idempotency.
#[test]
fn verified_acknowledgment_lifecycle_and_idempotency() {
    let dir = temp_private_dir("ack-lifecycle");
    let (inv, cx) = test_invocation();

    init_ledger(&inv, &cx, LedgerLocation::Directory(dir.clone())).expect("init ledger");

    let event_id = "ev-ack-1";
    let event = make_test_event(event_id, "advisory-hook", ExposureState::Prepared);
    let cand = make_test_candidate(event_id, "skill-hook");

    record_ranking(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        &event,
        &[cand],
        None,
    )
    .expect("record prepared");

    // Transition to emitted
    record_emission(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        event_id,
        512,
    )
    .expect("record emission");

    let db_path = dir.join(LEDGER_FILE);
    let conn = Connection::open(&db_path).expect("open db");
    let gen_before: i64 = conn
        .query_row(
            "SELECT data_generation FROM store_meta WHERE singleton = 1",
            [],
            |r| r.get(0),
        )
        .expect("query gen");

    // Record verified harness acknowledgment
    let delivery_key = "harness-ack-token-998877";
    let acked = record_acknowledgment(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        event_id,
        delivery_key,
    )
    .expect("record acknowledgment");
    assert!(acked, "acknowledgment should succeed");

    // Verify row in database: exposure_state == 'acknowledged', verified_delivery_key matches
    let (state_db, key_db, gen_after): (String, Option<String>, i64) = conn
        .query_row(
            "SELECT exposure_state, verified_delivery_key, (SELECT data_generation FROM store_meta WHERE singleton = 1) FROM ranking_events WHERE event_id = ?1",
            [event_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("query event after ack");
    assert_eq!(state_db, "acknowledged");
    assert_eq!(key_db.as_deref(), Some(delivery_key));
    assert_eq!(
        gen_after,
        gen_before + 1,
        "generation must advance on acknowledgment"
    );

    // Idempotent re-acknowledgment with the same key
    let acked_again = record_acknowledgment(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        event_id,
        delivery_key,
    )
    .expect("record acknowledgment idempotency");
    assert!(acked_again, "re-acknowledgment must be idempotent");
}

/// 5. Duplicate delivery keys across different events must be rejected with StoreError::RecordConflict.
#[test]
fn duplicate_delivery_key_across_different_events_rejected() {
    let dir = temp_private_dir("dup-key-reject");
    let (inv, cx) = test_invocation();

    init_ledger(&inv, &cx, LedgerLocation::Directory(dir.clone())).expect("init ledger");

    let event1 = make_test_event("ev-dup-1", "cli", ExposureState::Emitted);
    let cand1 = make_test_candidate("ev-dup-1", "skill-1");
    record_ranking(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        &event1,
        &[cand1],
        None,
    )
    .expect("record event 1");

    let event2 = make_test_event("ev-dup-2", "cli", ExposureState::Emitted);
    let cand2 = make_test_candidate("ev-dup-2", "skill-2");
    record_ranking(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        &event2,
        &[cand2],
        None,
    )
    .expect("record event 2");

    let shared_delivery_key = "unique-delivery-proof-1001";

    // First event successfully acknowledged with the delivery key
    let ack1 = record_acknowledgment(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        "ev-dup-1",
        shared_delivery_key,
    )
    .expect("ack event 1");
    assert!(ack1);

    // Second event attempting to claim the SAME delivery key must be rejected with RecordConflict
    let ack2_err = record_acknowledgment(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        "ev-dup-2",
        shared_delivery_key,
    )
    .expect_err("second event with duplicate delivery key must fail");

    match ack2_err {
        StoreError::RecordConflict => {} // Expected!
        other => panic!("expected StoreError::RecordConflict, got {other:?}"),
    }

    // Verify event 2 remains in its previous state and delivery key is NOT set
    let db_path = dir.join(LEDGER_FILE);
    let conn = Connection::open(&db_path).expect("open db");
    let (state_db, key_db): (String, Option<String>) = conn
        .query_row(
            "SELECT exposure_state, verified_delivery_key FROM ranking_events WHERE event_id = ?1",
            ["ev-dup-2"],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("query event 2");
    assert_eq!(state_db, "emitted");
    assert!(
        key_db.is_none(),
        "event 2 must not adopt the duplicate delivery key"
    );
}

/// 6. Explicit recording of distinct modes and channels: shadow, advisory-hook, cli, tui.
#[test]
fn distinct_modes_and_channels_recorded() {
    let dir = temp_private_dir("channels");
    let (inv, cx) = test_invocation();

    init_ledger(&inv, &cx, LedgerLocation::Directory(dir.clone())).expect("init ledger");

    let channels = ["shadow", "advisory-hook", "cli", "tui"];

    for (i, channel) in channels.iter().enumerate() {
        let event_id = format!("ev-chan-{}", i);
        let event = make_test_event(&event_id, channel, ExposureState::Prepared);
        let cand = make_test_candidate(&event_id, "skill-x");

        record_ranking(
            &inv,
            &cx,
            LedgerAccess::ExistingOnly,
            LedgerLocation::Directory(dir.clone()),
            &event,
            &[cand],
            None,
        )
        .expect("record channel event");
    }

    let db_path = dir.join(LEDGER_FILE);
    let conn = Connection::open(&db_path).expect("open db");

    for (i, expected_channel) in channels.iter().enumerate() {
        let event_id = format!("ev-chan-{}", i);
        let recorded_channel: String = conn
            .query_row(
                "SELECT mode_channel FROM ranking_events WHERE event_id = ?1",
                [&event_id],
                |r| r.get(0),
            )
            .expect("query channel");
        assert_eq!(
            recorded_channel, *expected_channel,
            "mode_channel must match"
        );
    }
}

/// 7. Disabled or missing ledger degrades gracefully (returns false, no error or panic).
#[test]
fn disabled_or_missing_ledger_degrades_gracefully() {
    let (inv, cx) = test_invocation();

    // Disabled access
    let emit_dis = record_emission(
        &inv,
        &cx,
        LedgerAccess::Disabled,
        LedgerLocation::Platform,
        "any-id",
        100,
    )
    .expect("disabled emission must not error");
    assert!(!emit_dis, "disabled ledger must return false");

    let ack_dis = record_acknowledgment(
        &inv,
        &cx,
        LedgerAccess::Disabled,
        LedgerLocation::Platform,
        "any-id",
        "key",
    )
    .expect("disabled acknowledgment must not error");
    assert!(!ack_dis, "disabled ledger must return false");

    // Missing ledger directory
    let non_existent = PathBuf::from("/tmp/does-not-exist-sr-ledger-xyz");
    let emit_miss = record_emission(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(non_existent.clone()),
        "any-id",
        100,
    )
    .expect("missing emission must not error");
    assert!(!emit_miss, "missing ledger must return false");

    let ack_miss = record_acknowledgment(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(non_existent),
        "any-id",
        "key",
    )
    .expect("missing acknowledgment must not error");
    assert!(!ack_miss, "missing ledger must return false");
}

/// 8. Invalid records: oversized delivery key or non-existent event ID.
#[test]
fn invalid_records_rejected() {
    let dir = temp_private_dir("invalid-records");
    let (inv, cx) = test_invocation();

    init_ledger(&inv, &cx, LedgerLocation::Directory(dir.clone())).expect("init ledger");

    // Oversized delivery key (> 256 bytes)
    let huge_key = "k".repeat(257);
    let ack_err = record_acknowledgment(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        "non-existent",
        &huge_key,
    )
    .expect_err("oversized delivery key must fail");
    assert_eq!(ack_err, StoreError::InvalidRecord);

    // Non-existent event ID for emission
    let emit_err = record_emission(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        "does-not-exist",
        100,
    )
    .expect_err("emission for non-existent event must fail");
    assert_eq!(emit_err, StoreError::InvalidRecord);
}

/// 9. End-to-end lifecycle verification via CLI and storage.
#[test]
fn cli_emission_lifecycle_e2e() {
    let dir = temp_private_dir("cli-lifecycle-e2e");
    let dir_str = dir.to_str().unwrap();
    let bin = env!("CARGO_BIN_EXE_sr");

    // Initialize ledger via CLI
    let init_out = std::process::Command::new(bin)
        .args(["ledger", "init", "--dir", dir_str, "--json"])
        .output()
        .expect("run ledger init");
    assert_eq!(init_out.status.code(), Some(0));

    let (inv, cx) = test_invocation();
    let event_id = "ev-e2e-life";
    let event = make_test_event(event_id, "cli", ExposureState::Prepared);
    let cand = make_test_candidate(event_id, "skill-e2e");

    // 1. Prepared event committed
    record_ranking(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        &event,
        &[cand],
        None,
    )
    .expect("record ranking");

    let db_path = dir.join(LEDGER_FILE);
    let conn = Connection::open(&db_path).expect("open db");
    let state_1: String = conn
        .query_row(
            "SELECT exposure_state FROM ranking_events WHERE event_id = ?1",
            [event_id],
            |r| r.get(0),
        )
        .expect("query state 1");
    assert_eq!(state_1, "prepared");

    // 2. Emission occurs upon successful stdout write
    let emitted = record_emission(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        event_id,
        120,
    )
    .expect("record emission");
    assert!(emitted);

    let state_2: String = conn
        .query_row(
            "SELECT exposure_state FROM ranking_events WHERE event_id = ?1",
            [event_id],
            |r| r.get(0),
        )
        .expect("query state 2");
    assert_eq!(state_2, "emitted");

    // 3. Verified acknowledgment arrives
    let acked = record_acknowledgment(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        event_id,
        "harness-proof-token-12345",
    )
    .expect("record acknowledgment");
    assert!(acked);

    let (state_3, key_3): (String, Option<String>) = conn
        .query_row(
            "SELECT exposure_state, verified_delivery_key FROM ranking_events WHERE event_id = ?1",
            [event_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("query state 3");
    assert_eq!(state_3, "acknowledged");
    assert_eq!(key_3.as_deref(), Some("harness-proof-token-12345"));
}
