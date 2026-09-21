//! Contract and verification tests for the observation ledger schema and repositories.
//!
//! Required by sr-roadmap-l1i.6.1:
//! - Real SQLite schema, foreign keys, uniqueness, and readback
//! - Store incarnation and data generation fencing
//! - Distinct ranking and observation watermarks for session cursors
//! - Privacy invariants: no skill bodies or secrets in roster snapshots
//! - Headroom and store replacement protection

use asupersync::Cx;
use rusqlite::Connection;
use skillranker::runtime::ProcessInvocation;
use skillranker::storage::StoreError;
use skillranker::storage::ledger::*;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn snapshot_fixture() -> NewRosterSnapshot {
    NewRosterSnapshot {
        snapshot_id: "snapshot-1".into(),
        workspace_root: "/data/workspace".into(),
        adapter: "claude_code".into(),
        total_candidates: 1,
        eligible_candidates: 1,
        membership_coverage: MembershipCoverage::Complete,
        members_json: serde_json::json!([{
            "skill_id": "review", "invocation_name": "review",
            "content_hash": "revision-1", "source": "workspace",
            "eligible": true, "exclusion_reason": null
        }])
        .to_string(),
        created_at_unix_ms: 1,
    }
}

fn event_fixture(id: &str) -> NewRankingEvent {
    NewRankingEvent {
        event_id: id.into(),
        verified_delivery_key: None,
        workspace_root: "/data/workspace".into(),
        session_id: "session".into(),
        agent_branch: "main".into(),
        mode_channel: "cli".into(),
        policy_version: "v1".into(),
        schema_version: 1,
        decision: DecisionKind::Ranked,
        reason: "eligible".into(),
        exposure_state: ExposureState::Generated,
        elapsed_ms: 1,
        created_at_unix_ms: 1,
        input_tokens: None,
        output_tokens: None,
        snapshot_id: Some("snapshot-1".into()),
    }
}

fn open_test_store(inv: &ProcessInvocation, cx: &Cx, name: &str) -> LedgerStore {
    match open_ledger(
        inv,
        cx,
        LedgerAccess::Initialize,
        LedgerLocation::Directory(temp_ledger_dir(name)),
    )
    .unwrap()
    {
        LedgerOpen::Ready(store) => *store,
        other => panic!("expected Ready store, got {other:?}"),
    }
}

#[test]
fn ledger_snapshot_rejects_unvalidated_metadata_before_persistence() {
    let (inv, cx) = test_invocation();
    let mut store = open_test_store(&inv, &cx, "snapshot-invalid");
    let stamp = store.stamp();
    let valid = snapshot_fixture();
    let mut forbidden = serde_json::from_str::<serde_json::Value>(&valid.members_json).unwrap();
    forbidden[0]["body"] = "PRIVATE_BODY_SENTINEL".into();
    let duplicate =
        valid
            .members_json
            .replacen("\"skill_id\":", "\"skill_id\":\"other\",\"skill_id\":", 1);
    let mut duplicate_members =
        serde_json::from_str::<Vec<serde_json::Value>>(&valid.members_json).unwrap();
    duplicate_members.push(duplicate_members[0].clone());
    for bad in [
        "not json".into(),
        forbidden.to_string(),
        duplicate,
        serde_json::to_string(&duplicate_members).unwrap(),
        "[[]]".into(),
        " ".repeat(2 * 1024 * 1024 + 1),
    ] {
        let snapshot = NewRosterSnapshot {
            members_json: bad,
            ..valid.clone()
        };
        assert!(
            store
                .record_roster_snapshot(inv.clock(), &cx, &snapshot, stamp)
                .is_err()
        );
    }
    let conn = Connection::open(store.database_path()).unwrap();
    assert_eq!(
        conn.query_row("SELECT count(*) FROM roster_snapshots", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    store
        .record_roster_snapshot(inv.clock(), &cx, &valid, stamp)
        .unwrap();
    assert!(inv.shutdown());
}

#[test]
fn ledger_snapshot_reuse_requires_identical_membership_and_scope() {
    let (inv, cx) = test_invocation();
    let mut store = open_test_store(&inv, &cx, "snapshot-conflict");
    let stamp = store.stamp();
    let snapshot = snapshot_fixture();
    store
        .record_roster_snapshot(inv.clock(), &cx, &snapshot, stamp)
        .unwrap();
    // A later observation of the same snapshot is legitimate deduplication.
    let repeated = NewRosterSnapshot {
        created_at_unix_ms: 2,
        ..snapshot.clone()
    };
    store
        .record_roster_snapshot(inv.clock(), &cx, &repeated, stamp)
        .unwrap();
    for changed in [
        NewRosterSnapshot {
            workspace_root: "/another/workspace".into(),
            ..snapshot.clone()
        },
        NewRosterSnapshot {
            adapter: "another-adapter".into(),
            ..snapshot.clone()
        },
        NewRosterSnapshot {
            members_json: snapshot.members_json.replace("revision-1", "revision-2"),
            ..snapshot.clone()
        },
        NewRosterSnapshot {
            membership_coverage: MembershipCoverage::Partial,
            ..snapshot.clone()
        },
    ] {
        assert!(
            store
                .record_roster_snapshot(inv.clock(), &cx, &changed, stamp)
                .is_err()
        );
        assert!(
            store
                .record_ranking_event(
                    inv.clock(),
                    &cx,
                    &event_fixture("event-conflict"),
                    &[],
                    Some(&changed),
                    stamp
                )
                .is_err()
        );
    }
    let conn = Connection::open(store.database_path()).unwrap();
    assert_eq!(
        conn.query_row("SELECT count(*) FROM ranking_events", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let stored: String = conn
        .query_row("SELECT members_json FROM roster_snapshots", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(stored.contains("revision-1"));
    store
        .record_ranking_event(
            inv.clock(),
            &cx,
            &event_fixture("event-valid"),
            &[],
            Some(&snapshot),
            stamp,
        )
        .unwrap();
    assert!(inv.shutdown());
}

#[test]
fn ledger_snapshot_counts_and_event_scope_must_match_evidence() {
    let (inv, cx) = test_invocation();
    let mut store = open_test_store(&inv, &cx, "snapshot-counts");
    let stamp = store.stamp();
    let valid = snapshot_fixture();
    for bad in [
        NewRosterSnapshot {
            total_candidates: 2,
            ..valid.clone()
        },
        NewRosterSnapshot {
            eligible_candidates: 0,
            ..valid.clone()
        },
        NewRosterSnapshot {
            total_candidates: u64::MAX,
            ..valid.clone()
        },
    ] {
        assert!(
            store
                .record_roster_snapshot(inv.clock(), &cx, &bad, stamp)
                .is_err()
        );
    }
    store
        .record_roster_snapshot(inv.clock(), &cx, &valid, stamp)
        .unwrap();
    let other = NewRankingEvent {
        workspace_root: "/other/workspace".into(),
        ..event_fixture("event-other")
    };
    assert!(
        store
            .record_ranking_event(inv.clock(), &cx, &other, &[], None, stamp)
            .is_err()
    );
    let wrong_id = NewRankingEvent {
        snapshot_id: None,
        ..event_fixture("event-no-snapshot")
    };
    assert!(
        store
            .record_ranking_event(inv.clock(), &cx, &wrong_id, &[], Some(&valid), stamp)
            .is_err()
    );
    let conn = Connection::open(store.database_path()).unwrap();
    assert_eq!(
        conn.query_row("SELECT count(*) FROM ranking_events", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    store
        .record_ranking_event(
            inv.clock(),
            &cx,
            &event_fixture("event-valid"),
            &[],
            None,
            stamp,
        )
        .unwrap();
    assert!(inv.shutdown());
}

#[test]
fn ledger_snapshot_preserves_full_roster_and_rejects_overflow() {
    // Two maximal snapshots (10,000 members each) are written inside this one invocation, so it gets
    // an explicit deadline rather than inheriting the product's and turning contention into a verdict.
    let (inv, cx) = generous_invocation();
    let mut store = open_test_store(&inv, &cx, "snapshot-full");
    let stamp = store.stamp();
    let valid = snapshot_fixture();
    let member = serde_json::from_str::<Vec<serde_json::Value>>(&valid.members_json)
        .unwrap()
        .remove(0);
    let members: Vec<_> = (0..10_000)
        .map(|index| {
            let mut value = member.clone();
            value["skill_id"] = format!("skill-{index}").into();
            value
        })
        .collect();
    let full = NewRosterSnapshot {
        total_candidates: 10_000,
        eligible_candidates: 10_000,
        members_json: serde_json::to_string(&members).unwrap(),
        ..valid
    };
    store
        .record_roster_snapshot(inv.clock(), &cx, &full, stamp)
        .unwrap();
    let conn = Connection::open(store.database_path()).unwrap();
    let recorded: String = conn
        .query_row("SELECT members_json FROM roster_snapshots", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Vec<serde_json::Value>>(&recorded)
            .unwrap()
            .len(),
        10_000
    );
    let overflow = NewRosterSnapshot {
        snapshot_id: "overflow".into(),
        total_candidates: 10_001,
        eligible_candidates: 10_001,
        ..full.clone()
    };
    assert!(
        store
            .record_roster_snapshot(inv.clock(), &cx, &overflow, stamp)
            .is_err()
    );
    // Membership order and JSON formatting do not create different evidence.
    let mut reversed = members;
    reversed.reverse();
    let equivalent = NewRosterSnapshot {
        members_json: serde_json::to_string_pretty(&reversed).unwrap(),
        ..full
    };
    store
        .record_roster_snapshot(inv.clock(), &cx, &equivalent, stamp)
        .unwrap();
    assert!(inv.shutdown());
}

#[test]
fn ledger_event_cannot_attach_candidates_to_another_existing_event() {
    let (inv, cx) = test_invocation();
    let mut store = open_test_store(&inv, &cx, "candidate-owner");
    let stamp = store.stamp();
    let first = NewRankingEvent {
        snapshot_id: None,
        ..event_fixture("first")
    };
    store
        .record_ranking_event(inv.clock(), &cx, &first, &[], None, stamp)
        .unwrap();
    let second = NewRankingEvent {
        snapshot_id: None,
        ..event_fixture("second")
    };
    let mut candidate = NewRankingCandidate {
        event_id: "first".into(),
        stage: CandidateStage::Wide,
        skill_id: "review".into(),
        skill_version: "revision-1".into(),
        raw_probability: Some(0.8),
        normalized_probability: Some(0.8),
        fit_score: None,
        rank_score: None,
        rank_position: None,
        excluded: false,
        exclusion_reason: None,
    };
    assert!(
        store
            .record_ranking_event(inv.clock(), &cx, &second, &[candidate.clone()], None, stamp)
            .is_err()
    );
    let conn = Connection::open(store.database_path()).unwrap();
    assert_eq!(
        conn.query_row("SELECT count(*) FROM ranking_events", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row("SELECT count(*) FROM ranking_candidates", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    candidate.event_id = "second".into();
    store
        .record_ranking_event(inv.clock(), &cx, &second, &[candidate], None, stamp)
        .unwrap();
    assert!(inv.shutdown());
}

#[test]
fn ledger_snapshot_reference_revalidates_legacy_rows_and_preserves_unknown_coverage() {
    let (inv, cx) = test_invocation();
    let mut store = open_test_store(&inv, &cx, "snapshot-reference");
    let stamp = store.stamp();
    let valid = snapshot_fixture();
    store
        .record_roster_snapshot(inv.clock(), &cx, &valid, stamp)
        .unwrap();
    let conn = Connection::open(store.database_path()).unwrap();
    // This represents a row written before the metadata boundary was fixed.
    conn.execute(
        "UPDATE roster_snapshots SET members_json = ?1",
        [r#"[{"body":"PRIVATE_BODY_SENTINEL"}]"#],
    )
    .unwrap();
    assert!(
        store
            .record_ranking_event(inv.clock(), &cx, &event_fixture("legacy"), &[], None, stamp)
            .is_err()
    );
    assert_eq!(
        conn.query_row("SELECT count(*) FROM ranking_events", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    conn.execute(
        "UPDATE roster_snapshots SET members_json = ?1",
        [&valid.members_json],
    )
    .unwrap();
    store
        .record_ranking_event(inv.clock(), &cx, &event_fixture("valid"), &[], None, stamp)
        .unwrap();

    for (id, coverage, total, eligible, members) in [
        ("empty", MembershipCoverage::Complete, 0, 0, "[]".to_owned()),
        (
            "unknown",
            MembershipCoverage::Unknown,
            2,
            1,
            "[]".to_owned(),
        ),
        (
            "partial",
            MembershipCoverage::Partial,
            2,
            1,
            valid.members_json.clone(),
        ),
        (
            "unreadable-member",
            MembershipCoverage::Complete,
            1,
            0,
            serde_json::json!([{
                "skill_id": "unreadable", "invocation_name": null,
                "content_hash": null, "source": "workspace", "eligible": false,
                "exclusion_reason": "unreadable"
            }])
            .to_string(),
        ),
    ] {
        let snapshot = NewRosterSnapshot {
            snapshot_id: id.into(),
            membership_coverage: coverage,
            total_candidates: total,
            eligible_candidates: eligible,
            members_json: members,
            ..valid.clone()
        };
        store
            .record_roster_snapshot(inv.clock(), &cx, &snapshot, stamp)
            .unwrap();
    }
    assert!(inv.shutdown());
}

fn test_invocation() -> (ProcessInvocation, Cx) {
    let invocation = ProcessInvocation::enter().expect("process invocation");
    let cx = invocation.request_cx().expect("request_cx");
    (invocation, cx)
}

/// An invocation with a deliberately generous deadline, for the cases whose work is genuinely
/// expensive.
///
/// The default 3,000 ms deadline is the product's, and using it here means a test also asserts,
/// silently, that its work fits in that budget on whatever machine happens to run it. That is a
/// performance claim nobody wrote down, and it is not what these cases are about: they are about what
/// the store PRESERVES. On a loaded box the full-roster case took 4.11 s and failed on
/// `Err(Cancelled)`, then passed three times at 1.15 s, 1.46 s and 1.44 s minutes later — the verdict
/// was reporting machine load, and it misled two separate reviews (sr-0i7u).
///
/// Sixty seconds is not a performance target either; it is far enough above the real cost (~1.2 s of
/// work) that contention cannot reach it, so the assertions decide the outcome. A case that needs this
/// must still fail when the evidence is wrong, which is why the deadline is the only thing relaxed.
fn generous_invocation() -> (ProcessInvocation, Cx) {
    let clock = skillranker::runtime::EntryClock::capture()
        .expect("entry clock")
        .with_total(
            skillranker::limits::DurationMillis::new("test_deadline_ms", 60_000, 600_000)
                .expect("a bounded test deadline"),
        )
        .expect("a generous deadline");
    let invocation = ProcessInvocation::from_clock(clock).expect("process invocation");
    let cx = invocation.request_cx().expect("request_cx");
    (invocation, cx)
}

#[test]
fn ledger_handle_rejects_same_directory_database_replacement() {
    let (inv, cx) = test_invocation();
    let mut store = open_test_store(&inv, &cx, "database-replacement");
    let stamp = store.stamp();
    let cursor = SessionCursor {
        workspace_root: "/data/workspace".into(),
        session_id: "session".into(),
        agent_branch: "main".into(),
        cursor_kind: CursorKind::Observation,
        transcript_generation: 1,
        last_complete_event_id: "event-1".into(),
        last_offset_bytes: 100,
        updated_at_unix_ms: 1,
    };
    store
        .update_session_cursor(inv.clock(), &cx, &cursor, stamp)
        .unwrap();
    assert_eq!(
        store
            .get_session_cursor(
                inv.clock(),
                &cx,
                &cursor.workspace_root,
                &cursor.session_id,
                &cursor.agent_branch,
                cursor.cursor_kind
            )
            .unwrap(),
        Some(cursor.clone())
    );

    let path = store.database_path();
    fs::rename(&path, path.with_file_name("preserved-original.sqlite3")).unwrap();
    let replacement = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    let read = store.get_session_cursor(
        inv.clock(),
        &cx,
        &cursor.workspace_root,
        &cursor.session_id,
        &cursor.agent_branch,
        cursor.cursor_kind,
    );
    let write = store.update_session_cursor(inv.clock(), &cx, &cursor, stamp);
    assert_eq!(
        replacement.metadata().unwrap().len(),
        0,
        "replacement must remain untouched"
    );
    assert_eq!(
        read,
        Err(StoreError::StoreReplaced),
        "detached history must not be returned"
    );
    assert_eq!(
        write,
        Err(StoreError::StoreReplaced),
        "replacement must invalidate writes"
    );
    assert!(inv.shutdown());
}

#[test]
fn ledger_handle_uses_current_context_after_opener_cancel_or_expiry() {
    for cancel_opener in [true, false] {
        let (opener, old_cx) = test_invocation();
        let mut store = open_test_store(&opener, &old_cx, "reused-context");
        let stamp = store.stamp();
        let old_clock = opener.clock();
        if cancel_opener {
            opener.cancel_user(&old_cx);
        }
        assert!(opener.shutdown());
        if !cancel_opener {
            std::thread::sleep(std::time::Duration::from_millis(
                old_clock.remaining_until_expiry().as_millis() + 1,
            ));
        }
        let (current, cx) = test_invocation();
        for number in 0..4 {
            let snapshot = NewRosterSnapshot {
                snapshot_id: format!("reused-{number}"),
                ..snapshot_fixture()
            };
            store
                .record_roster_snapshot(current.clock(), &cx, &snapshot, stamp)
                .expect("fresh invocation must not inherit opener cancellation or expiry");
        }
        // Refreshing the context must not disable cancellation checks.
        current.cancel_user(&cx);
        assert!(
            store
                .record_roster_snapshot(current.clock(), &cx, &snapshot_fixture(), stamp)
                .is_err()
        );
        assert!(current.shutdown());
    }
}

fn temp_ledger_dir(test_name: &str) -> PathBuf {
    // RCH's TMPDIR can have ancestors owned by a different user. Exercise
    // storage under Linux's root-owned sticky /tmp without relaxing checks.
    let dir = PathBuf::from("/tmp").join(format!(
        "sr-ledger-test-{}-{}-{}",
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

#[test]
fn ledger_initialization_creates_tables_and_metadata() {
    let (inv, cx) = test_invocation();
    let dir = temp_ledger_dir("init");

    let open_res = open_ledger(
        &inv,
        &cx,
        LedgerAccess::Initialize,
        LedgerLocation::Directory(dir.clone()),
    );
    let store = match open_res.expect("open_ledger succeeds") {
        LedgerOpen::Ready(s) => s,
        other => panic!("expected Ready, got {:?}", other),
    };

    let stamp = store.stamp();
    assert_eq!(stamp.schema_generation, 1);
    assert_eq!(stamp.data_generation, 1);
    assert_ne!(stamp.incarnation, [0u8; 16]);

    let db_path = store.database_path();
    assert!(db_path.exists());

    // Inspect database directly to verify raw SQLite invariants
    let conn = Connection::open(&db_path).expect("open raw sqlite");
    let app_id: i64 = conn
        .pragma_query_value(None, "application_id", |row| row.get(0))
        .unwrap();
    assert_eq!(app_id, LEDGER_APPLICATION_ID);

    let user_ver: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(user_ver, LEDGER_SCHEMA_VERSION as i64);

    let table_count: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name NOT GLOB 'sqlite_*'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(table_count, 11, "must have all 11 tables");

    // ExistingOnly access succeeds
    let existing_res = open_ledger(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir),
    );
    let store2 = match existing_res.expect("open existing") {
        LedgerOpen::Ready(s) => s,
        other => panic!("expected Ready, got {:?}", other),
    };
    assert_eq!(store2.stamp(), stamp);

    assert!(inv.shutdown());
}

#[test]
fn ledger_foreign_keys_and_uniqueness_enforced() {
    let (inv, cx) = test_invocation();
    let dir = temp_ledger_dir("fk-unique");

    let mut store = match open_ledger(
        &inv,
        &cx,
        LedgerAccess::Initialize,
        LedgerLocation::Directory(dir),
    )
    .unwrap()
    {
        LedgerOpen::Ready(s) => *s,
        _ => panic!("expected Ready"),
    };
    let stamp = store.stamp();
    let clock = inv.clock();

    let event1 = NewRankingEvent {
        event_id: "evt-001".into(),
        verified_delivery_key: Some("deliv-key-alpha".into()),
        workspace_root: "/data/ws".into(),
        session_id: "sess-1".into(),
        agent_branch: "main".into(),
        mode_channel: "hook".into(),
        policy_version: "v1".into(),
        schema_version: 1,
        decision: DecisionKind::Ranked,
        reason: "eligible".into(),
        exposure_state: ExposureState::Emitted,
        elapsed_ms: 120,
        created_at_unix_ms: 1700000000,
        input_tokens: Some(500),
        output_tokens: Some(150),
        snapshot_id: None,
    };

    let cand1 = NewRankingCandidate {
        event_id: "evt-001".into(),
        stage: CandidateStage::Wide,
        skill_id: "s_rust".into(),
        skill_version: "v1".into(),
        raw_probability: Some(0.85),
        normalized_probability: Some(0.85),
        fit_score: Some(0.9),
        rank_score: Some(1.2),
        rank_position: Some(1),
        excluded: false,
        exclusion_reason: None,
    };

    // Valid ranking event and candidate record succeeds
    store
        .record_ranking_event(clock, &cx, &event1, &[cand1], None, stamp)
        .expect("record ranking event");

    // 1. Uniqueness check on verified_delivery_key:
    // Attempting to record second event with duplicate verified_delivery_key fails
    let event2 = NewRankingEvent {
        event_id: "evt-002".into(),
        verified_delivery_key: Some("deliv-key-alpha".into()), // DUPLICATE KEY
        workspace_root: "/data/ws".into(),
        session_id: "sess-1".into(),
        agent_branch: "main".into(),
        mode_channel: "hook".into(),
        policy_version: "v1".into(),
        schema_version: 1,
        decision: DecisionKind::Ranked,
        reason: "eligible".into(),
        exposure_state: ExposureState::Emitted,
        elapsed_ms: 100,
        created_at_unix_ms: 1700000050,
        input_tokens: Some(400),
        output_tokens: Some(100),
        snapshot_id: None,
    };
    let dup_res = store.record_ranking_event(clock, &cx, &event2, &[], None, stamp);
    assert!(
        dup_res.is_err(),
        "duplicate verified_delivery_key must be rejected"
    );

    // 2. Foreign Key constraint check on ranking_candidates:
    // Attempting to record candidate for non-existent event fails FK check
    let cand_orphan = NewRankingCandidate {
        event_id: "evt-nonexistent".into(),
        stage: CandidateStage::Wide,
        skill_id: "s_python".into(),
        skill_version: "v1".into(),
        raw_probability: Some(0.5),
        normalized_probability: Some(0.5),
        fit_score: Some(0.5),
        rank_score: Some(0.5),
        rank_position: Some(1),
        excluded: false,
        exclusion_reason: None,
    };
    let event_orphan = NewRankingEvent {
        event_id: "evt-003".into(),
        verified_delivery_key: None,
        workspace_root: "/data/ws".into(),
        session_id: "sess-1".into(),
        agent_branch: "main".into(),
        mode_channel: "hook".into(),
        policy_version: "v1".into(),
        schema_version: 1,
        decision: DecisionKind::Ranked,
        reason: "eligible".into(),
        exposure_state: ExposureState::Emitted,
        elapsed_ms: 100,
        created_at_unix_ms: 1700000100,
        input_tokens: None,
        output_tokens: None,
        snapshot_id: None,
    };
    let fk_cand_res =
        store.record_ranking_event(clock, &cx, &event_orphan, &[cand_orphan], None, stamp);
    assert!(
        fk_cand_res.is_err(),
        "orphan candidate must fail FK constraint"
    );

    // 3. Observations and Judgments
    let obs1 = NewObservation {
        observation_id: "obs-001".into(),
        source_event_key: "tx-item-42".into(),
        workspace_root: "/data/ws".into(),
        session_id: "sess-1".into(),
        agent_branch: "main".into(),
        attributed_event_id: Some("evt-001".into()),
        skill_id: "s_rust".into(),
        evidence_state: EvidenceState::Loaded,
        observed_at_unix_ms: 1700000200,
    };
    store
        .record_observation(clock, &cx, &obs1, stamp)
        .expect("record observation");

    // Duplicate source_event_key must fail
    let obs_dup = NewObservation {
        observation_id: "obs-002".into(),
        source_event_key: "tx-item-42".into(), // DUPLICATE KEY
        workspace_root: "/data/ws".into(),
        session_id: "sess-1".into(),
        agent_branch: "main".into(),
        attributed_event_id: Some("evt-001".into()),
        skill_id: "s_rust".into(),
        evidence_state: EvidenceState::Loaded,
        observed_at_unix_ms: 1700000300,
    };
    let dup_obs_res = store.record_observation(clock, &cx, &obs_dup, stamp);
    assert!(
        dup_obs_res.is_err(),
        "duplicate source_event_key must be rejected"
    );

    let jdg1 = NewJudgment {
        judgment_id: "jdg-001".into(),
        attributed_event_id: "evt-001".into(),
        skill_id: "s_rust".into(),
        label: JudgmentLabel::Useful,
        label_version: 1,
        provenance: "user_feedback".into(),
        created_at_unix_ms: 1700000400,
    };
    store
        .record_judgment(clock, &cx, &jdg1, stamp)
        .expect("record judgment");

    assert!(inv.shutdown());
}

#[test]
fn ledger_session_cursors_distinct_watermarks() {
    let (inv, cx) = test_invocation();
    let dir = temp_ledger_dir("cursors");

    let mut store = match open_ledger(
        &inv,
        &cx,
        LedgerAccess::Initialize,
        LedgerLocation::Directory(dir),
    )
    .unwrap()
    {
        LedgerOpen::Ready(s) => *s,
        _ => panic!("expected Ready"),
    };
    let stamp = store.stamp();
    let clock = inv.clock();

    let ws = "/data/projects/skillranker";
    let sess = "session-xyz";
    let branch = "feature/p5";

    // Initial read returns None
    let init_rank = store
        .get_session_cursor(clock, &cx, ws, sess, branch, CursorKind::Ranking)
        .unwrap();
    assert_eq!(init_rank, None);
    let init_obs = store
        .get_session_cursor(clock, &cx, ws, sess, branch, CursorKind::Observation)
        .unwrap();
    assert_eq!(init_obs, None);

    // Record ranking cursor
    let rank_cur = SessionCursor {
        workspace_root: ws.into(),
        session_id: sess.into(),
        agent_branch: branch.into(),
        cursor_kind: CursorKind::Ranking,
        transcript_generation: 1,
        last_complete_event_id: "msg-10".into(),
        last_offset_bytes: 4096,
        updated_at_unix_ms: 1700000000,
    };
    store
        .update_session_cursor(clock, &cx, &rank_cur, stamp)
        .unwrap();

    // Verify ranking cursor is updated, observation cursor remains None
    let read_rank = store
        .get_session_cursor(clock, &cx, ws, sess, branch, CursorKind::Ranking)
        .unwrap();
    assert_eq!(read_rank, Some(rank_cur.clone()));
    let read_obs = store
        .get_session_cursor(clock, &cx, ws, sess, branch, CursorKind::Observation)
        .unwrap();
    assert_eq!(read_obs, None);

    // Record observation cursor with distinct watermark
    let obs_cur = SessionCursor {
        workspace_root: ws.into(),
        session_id: sess.into(),
        agent_branch: branch.into(),
        cursor_kind: CursorKind::Observation,
        transcript_generation: 1,
        last_complete_event_id: "msg-08".into(),
        last_offset_bytes: 3200,
        updated_at_unix_ms: 1700000010,
    };
    store
        .update_session_cursor(clock, &cx, &obs_cur, stamp)
        .unwrap();

    // Verify both exist independently
    let read_rank2 = store
        .get_session_cursor(clock, &cx, ws, sess, branch, CursorKind::Ranking)
        .unwrap()
        .unwrap();
    assert_eq!(read_rank2.last_offset_bytes, 4096);
    let read_obs2 = store
        .get_session_cursor(clock, &cx, ws, sess, branch, CursorKind::Observation)
        .unwrap()
        .unwrap();
    assert_eq!(read_obs2.last_offset_bytes, 3200);

    assert!(inv.shutdown());
}

#[test]
fn ledger_store_incarnation_and_generation_fencing() {
    let (inv, cx) = test_invocation();
    let dir = temp_ledger_dir("fencing");

    let mut store = match open_ledger(
        &inv,
        &cx,
        LedgerAccess::Initialize,
        LedgerLocation::Directory(dir),
    )
    .unwrap()
    {
        LedgerOpen::Ready(s) => *s,
        _ => panic!("expected Ready"),
    };
    let initial_stamp = store.stamp();
    let clock = inv.clock();

    // 1. Mutation with wrong incarnation fails with StoreReplaced
    let mut bad_incarnation = initial_stamp;
    bad_incarnation.incarnation[0] ^= 0xFF;
    let prop1 = NewFeedbackProposal {
        proposal_id: "prop-1".into(),
        workspace_root: "/data/ws".into(),
        session_id: "sess-1".into(),
        suggested_skill_reference: "new_tool".into(),
        status: ProposalStatus::Unresolved,
        notes: Some("suggested by user".into()),
        created_at_unix_ms: 1700000000,
    };
    let err_replaced = store.record_feedback_proposal(clock, &cx, &prop1, bad_incarnation);
    assert_eq!(err_replaced.unwrap_err(), StoreError::StoreReplaced);

    // 2. Mutation with stale data generation fails with StaleGeneration
    let mut stale_gen = initial_stamp;
    stale_gen.data_generation = 999;
    let err_stale = store.record_feedback_proposal(clock, &cx, &prop1, stale_gen);
    assert_eq!(err_stale.unwrap_err(), StoreError::StaleGeneration);

    // 3. Mutation with correct stamp succeeds
    store
        .record_feedback_proposal(clock, &cx, &prop1, initial_stamp)
        .expect("valid stamp succeeds");

    // 4. clear() advances data_generation and deletes records
    let cleared_stamp = store
        .clear(clock, &cx, initial_stamp)
        .expect("clear succeeds");
    assert_eq!(
        cleared_stamp.data_generation,
        initial_stamp.data_generation + 1
    );

    // 5. Subsequent write using the old initial_stamp fails with StaleGeneration
    let prop2 = NewFeedbackProposal {
        proposal_id: "prop-2".into(),
        workspace_root: "/data/ws".into(),
        session_id: "sess-1".into(),
        suggested_skill_reference: "another_tool".into(),
        status: ProposalStatus::Unresolved,
        notes: None,
        created_at_unix_ms: 1700000050,
    };
    let old_writer_res = store.record_feedback_proposal(clock, &cx, &prop2, initial_stamp);
    assert_eq!(old_writer_res.unwrap_err(), StoreError::StaleGeneration);

    // 6. Write with the new cleared_stamp succeeds
    store
        .record_feedback_proposal(clock, &cx, &prop2, cleared_stamp)
        .expect("new stamp succeeds");

    assert!(inv.shutdown());
}

#[test]
fn ledger_provider_attempts_and_calibrations_lifecycle() {
    let (inv, cx) = test_invocation();
    let dir = temp_ledger_dir("attempts-cal");

    let mut store = match open_ledger(
        &inv,
        &cx,
        LedgerAccess::Initialize,
        LedgerLocation::Directory(dir),
    )
    .unwrap()
    {
        LedgerOpen::Ready(s) => *s,
        _ => panic!("expected Ready"),
    };
    let stamp = store.stamp();
    let clock = inv.clock();

    // Need a parent event for provider attempt
    let event = NewRankingEvent {
        event_id: "evt-parent".into(),
        verified_delivery_key: None,
        workspace_root: "/data/ws".into(),
        session_id: "sess-1".into(),
        agent_branch: "main".into(),
        mode_channel: "hook".into(),
        policy_version: "v1".into(),
        schema_version: 1,
        decision: DecisionKind::Ranked,
        reason: "eligible".into(),
        exposure_state: ExposureState::Generated,
        elapsed_ms: 50,
        created_at_unix_ms: 1700000000,
        input_tokens: None,
        output_tokens: None,
        snapshot_id: None,
    };
    store
        .record_ranking_event(clock, &cx, &event, &[], None, stamp)
        .unwrap();

    let attempt = NewProviderAttempt {
        attempt_id: "att-001".into(),
        owner_event_id: "evt-parent".into(),
        stage: CandidateStage::Wide,
        request_fingerprint: "req-blake3-abc".into(),
        admitted_at_unix_ms: 1700000001,
        sent_at_unix_ms: Some(1700000002),
        completed_at_unix_ms: None,
        status: AttemptStatus::Sent,
        input_tokens: None,
        output_tokens: None,
        http_status: None,
        error_kind: None,
    };
    store
        .record_provider_attempt(clock, &cx, &attempt, stamp)
        .unwrap();

    // Update outcome
    let outcome = ProviderAttemptOutcome {
        status: AttemptStatus::Completed,
        tokens: Some((1200, 350)),
        http_status: Some(200),
        error_kind: None,
        completed_at_unix_ms: 1700000020,
    };
    store
        .update_provider_attempt_outcome(clock, &cx, "att-001", &outcome, stamp)
        .unwrap();

    // Record calibration
    let cal = NewCalibration {
        calibration_id: "cal-001".into(),
        dataset_fingerprint: "ds-blake3-123".into(),
        split: DatasetSplit::Train,
        objective: "log_loss".into(),
        coefficients_json: r#"{"w_fit": 1.2, "w_prior": 0.1}"#.into(),
        evaluation_report_id: "rep-001".into(),
        created_at_unix_ms: 1700000100,
    };
    store.record_calibration(clock, &cx, &cal, stamp).unwrap();

    assert!(inv.shutdown());
}

#[test]
fn ledger_roster_snapshot_stores_membership_without_skill_bodies() {
    let (inv, cx) = test_invocation();
    let dir = temp_ledger_dir("roster-privacy");

    let mut store = match open_ledger(
        &inv,
        &cx,
        LedgerAccess::Initialize,
        LedgerLocation::Directory(dir),
    )
    .unwrap()
    {
        LedgerOpen::Ready(s) => *s,
        _ => panic!("expected Ready"),
    };
    let stamp = store.stamp();
    let clock = inv.clock();

    // Snapshot with structured members metadata, but NO skill bodies or full descriptions
    let members_metadata = serde_json::json!([
        {
            "skill_id": "s_code_review",
            "invocation_name": "review",
            "content_hash": "blake3:abc123def456",
            "source": "workspace",
            "eligible": true,
            "exclusion_reason": null
        },
        {
            "skill_id": "s_deploy",
            "invocation_name": "deploy",
            "content_hash": "blake3:789xyz",
            "source": "installed",
            "eligible": false,
            "exclusion_reason": "untrusted-source"
        }
    ]);

    let snapshot = NewRosterSnapshot {
        snapshot_id: "snap-alpha".into(),
        workspace_root: "/data/ws".into(),
        adapter: "claude_code".into(),
        total_candidates: 2,
        eligible_candidates: 1,
        membership_coverage: MembershipCoverage::Complete,
        members_json: serde_json::to_string(&members_metadata).unwrap(),
        created_at_unix_ms: 1700000000,
    };

    store
        .record_roster_snapshot(clock, &cx, &snapshot, stamp)
        .unwrap();

    // Read back raw database row and verify that forbidden text or bodies are not present
    let conn = Connection::open(store.database_path()).unwrap();
    let (stored_json,): (String,) = conn
        .query_row(
            "SELECT members_json FROM roster_snapshots WHERE snapshot_id = 'snap-alpha'",
            [],
            |row| Ok((row.get(0)?,)),
        )
        .unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&stored_json).unwrap();
    let arr = parsed.as_array().expect("array of members");
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["skill_id"], "s_code_review");
    assert_eq!(arr[0]["invocation_name"], "review");
    assert_eq!(arr[0]["eligible"], true);
    // Invariant: no "body" or "content" or "description" field stored in member metadata
    assert!(arr[0].get("body").is_none());
    assert!(arr[0].get("description").is_none());

    assert!(inv.shutdown());
}
