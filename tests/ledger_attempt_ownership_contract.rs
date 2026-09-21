#![cfg(unix)]
//! Provider-attempt ownership and cost in the ledger (sr-roadmap-l1i.6.7).
//!
//! The schema contract already covers the happy path of writing an attempt and
//! updating its outcome. These cases pin the rules that make a recorded attempt
//! trustworthy as cost evidence:
//!
//! - an attempt id owns exactly one row, so a replay or a retry cannot silently
//!   overwrite what an earlier attempt cost;
//! - an attempt cannot exist without the ranking event that owns it, so the
//!   ledger cannot hold cost attributed to nothing;
//! - missing usage stays missing. A completed-or-unknown attempt whose tokens
//!   never arrived keeps NULL rather than 0, and a later outcome update cannot
//!   quietly zero tokens that were already known;
//! - `unknown` is a first-class status: an attempt that reached the wire and
//!   never answered is neither a success nor a clean failure.
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

// Intentionally retained: repository policy forbids automatic tree deletion.
fn temp_ledger_dir(test_name: &str) -> PathBuf {
    // As in the schema contract: RCH's TMPDIR can have ancestors owned by
    // another user, so use Linux's root-owned sticky /tmp rather than relaxing
    // the store's own ancestor checks.
    let dir = PathBuf::from("/tmp").join(format!(
        "sr-ledger-attempt-{}-{}-{}",
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
        elapsed_ms: 50,
        created_at_unix_ms: 1_700_000_000,
        input_tokens: None,
        output_tokens: None,
        snapshot_id: None,
    }
}

fn attempt_fixture(id: &str, owner: &str) -> NewProviderAttempt {
    NewProviderAttempt {
        attempt_id: id.into(),
        owner_event_id: owner.into(),
        stage: CandidateStage::Wide,
        request_fingerprint: "req-blake3-abc".into(),
        admitted_at_unix_ms: 1_700_000_001,
        sent_at_unix_ms: Some(1_700_000_002),
        completed_at_unix_ms: None,
        status: AttemptStatus::Sent,
        input_tokens: None,
        output_tokens: None,
        http_status: None,
        error_kind: None,
    }
}

struct Fixture {
    store: LedgerStore,
}

impl Fixture {
    fn new(name: &str, inv: &ProcessInvocation, cx: &Cx) -> Self {
        let dir = temp_ledger_dir(name);
        let store = match open_ledger(
            inv,
            cx,
            LedgerAccess::Initialize,
            LedgerLocation::Directory(dir),
        )
        .expect("open_ledger succeeds")
        {
            LedgerOpen::Ready(store) => *store,
            other => panic!("expected Ready, got {other:?}"),
        };
        Self { store }
    }

    /// Writes the owning event so attempts have something to belong to.
    fn with_event(mut self, inv: &ProcessInvocation, cx: &Cx, id: &str) -> Self {
        let stamp = self.store.stamp();
        self.store
            .record_ranking_event(inv.clock(), cx, &event_fixture(id), &[], None, stamp)
            .expect("event recorded");
        self
    }

    fn tokens(&self, attempt_id: &str) -> (Option<i64>, Option<i64>, String) {
        let conn = Connection::open(self.store.database_path()).expect("open raw sqlite");
        conn.query_row(
            "SELECT input_tokens, output_tokens, status FROM provider_attempts WHERE attempt_id = ?1",
            [attempt_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("attempt row")
    }

    fn attempt_count(&self, owner: &str) -> i64 {
        let conn = Connection::open(self.store.database_path()).expect("open raw sqlite");
        conn.query_row(
            "SELECT count(*) FROM provider_attempts WHERE owner_event_id = ?1",
            [owner],
            |row| row.get(0),
        )
        .expect("count")
    }
}

#[test]
fn an_attempt_id_owns_exactly_one_row() {
    let (inv, cx) = test_invocation();
    let mut f = Fixture::new("dup", &inv, &cx).with_event(&inv, &cx, "evt-1");
    let stamp = f.store.stamp();
    f.store
        .record_provider_attempt(inv.clock(), &cx, &attempt_fixture("att-1", "evt-1"), stamp)
        .expect("first write");

    // A second write under the same id must be refused, not silently applied:
    // it would otherwise replace what the first attempt recorded as cost.
    let mut second = attempt_fixture("att-1", "evt-1");
    second.input_tokens = Some(999_999);
    let stamp = f.store.stamp();
    let result = f
        .store
        .record_provider_attempt(inv.clock(), &cx, &second, stamp);
    assert!(result.is_err(), "duplicate attempt id must be refused");
    assert_eq!(f.attempt_count("evt-1"), 1);
    let (input, _, _) = f.tokens("att-1");
    assert_eq!(input, None, "the first attempt's record is intact");
    assert!(inv.shutdown());
}

#[test]
fn an_attempt_cannot_be_attributed_to_an_absent_event() {
    let (inv, cx) = test_invocation();
    let mut f = Fixture::new("orphan", &inv, &cx);
    // No event written: cost attributed to nothing is not recordable.
    let stamp = f.store.stamp();
    let result = f.store.record_provider_attempt(
        inv.clock(),
        &cx,
        &attempt_fixture("att-orphan", "evt-missing"),
        stamp,
    );
    assert!(
        result.is_err(),
        "an attempt requires the event that owns it"
    );
    assert_eq!(f.attempt_count("evt-missing"), 0);
    assert!(inv.shutdown());
}

#[test]
fn missing_usage_stays_missing_and_known_usage_survives_a_later_update() {
    let (inv, cx) = test_invocation();
    let mut f = Fixture::new("unknown", &inv, &cx).with_event(&inv, &cx, "evt-2");
    let stamp = f.store.stamp();
    f.store
        .record_provider_attempt(inv.clock(), &cx, &attempt_fixture("att-2", "evt-2"), stamp)
        .expect("write");
    // Sent, no response yet: absent is not zero.
    let (input, output, status) = f.tokens("att-2");
    assert_eq!((input, output), (None, None));
    assert_eq!(status, "sent");

    // The response arrives with exact tokens.
    let stamp = f.store.stamp();
    f.store
        .update_provider_attempt_outcome(
            inv.clock(),
            &cx,
            "att-2",
            &ProviderAttemptOutcome {
                status: AttemptStatus::Completed,
                tokens: Some((1_200, 350)),
                http_status: Some(200),
                error_kind: None,
                completed_at_unix_ms: 1_700_000_020,
            },
            stamp,
        )
        .expect("outcome recorded");
    assert_eq!(
        f.tokens("att-2"),
        (Some(1_200), Some(350), "completed".into())
    );

    // A later update that carries no tokens must not zero what is known. An
    // unknown-usage outcome arriving after a known one would otherwise erase
    // real spend.
    let stamp = f.store.stamp();
    f.store
        .update_provider_attempt_outcome(
            inv.clock(),
            &cx,
            "att-2",
            &ProviderAttemptOutcome {
                status: AttemptStatus::Unknown,
                tokens: None,
                http_status: None,
                error_kind: Some("deadline"),
                completed_at_unix_ms: 1_700_000_030,
            },
            stamp,
        )
        .expect("second outcome recorded");
    let (input, output, status) = f.tokens("att-2");
    assert_eq!(
        (input, output),
        (Some(1_200), Some(350)),
        "known tokens survive"
    );
    assert_eq!(status, "unknown", "the status still tells the truth");
    assert!(inv.shutdown());
}

#[test]
fn updating_an_attempt_that_was_never_recorded_is_refused() {
    let (inv, cx) = test_invocation();
    let mut f = Fixture::new("absent", &inv, &cx).with_event(&inv, &cx, "evt-3");
    let stamp = f.store.stamp();
    let result = f.store.update_provider_attempt_outcome(
        inv.clock(),
        &cx,
        "att-never",
        &ProviderAttemptOutcome {
            status: AttemptStatus::Completed,
            tokens: Some((1, 1)),
            http_status: Some(200),
            error_kind: None,
            completed_at_unix_ms: 1_700_000_040,
        },
        stamp,
    );
    assert!(
        matches!(result, Err(StoreError::InvalidRecord | StoreError::Missing)),
        "an outcome for an unrecorded attempt is refused: {result:?}"
    );
    assert!(inv.shutdown());
}

#[test]
fn an_event_with_no_attempts_records_none() {
    // A cache-served invocation, or a single-flight follower reusing an owner's
    // response, admits no attempt. Its event must carry zero attempt rows
    // rather than an inferred one.
    let (inv, cx) = test_invocation();
    let f = Fixture::new("follower", &inv, &cx).with_event(&inv, &cx, "evt-follower");
    assert_eq!(f.attempt_count("evt-follower"), 0);
    assert!(inv.shutdown());
}

#[test]
fn an_invocation_may_advance_its_own_attempt_through_its_lifecycle() {
    // The companion to `an_attempt_id_owns_exactly_one_row`, and the reason that case has
    // to be about *strangers* rather than about second writes as such. One attempt is
    // deliberately recorded more than once — admitted before anything is sent, sent when
    // its request reaches the wire, settled when it returns — so that a process killed
    // between those points still leaves behind what was true at the time. Those writes
    // have to land on the one row.
    //
    // What must never happen is the opposite mistake: a later write erasing a fact an
    // earlier one recorded. So the cost observed at settlement is kept, and a settlement
    // that observed nothing may not blank out what was already known.
    let (inv, cx) = test_invocation();
    let mut f = Fixture::new("advance", &inv, &cx).with_event(&inv, &cx, "evt-1");

    let mut admitted = attempt_fixture("att-1", "evt-1");
    admitted.status = AttemptStatus::Admitted;
    admitted.sent_at_unix_ms = None;
    let stamp = f.store.stamp();
    f.store
        .record_provider_attempt(inv.clock(), &cx, &admitted, stamp)
        .expect("the admission write is the first record of this attempt");

    // Reaching the wire advances the same row and does not date its completion.
    let stamp = f.store.stamp();
    f.store
        .mark_provider_attempt_sent(inv.clock(), &cx, "att-1", 1_700_000_002, stamp)
        .expect("an admitted attempt can be marked sent");
    let (_, _, status) = f.tokens("att-1");
    assert_eq!(status, "sent");
    assert_eq!(f.attempt_count("evt-1"), 1, "still one row, not two");

    // Settling it records what it cost, on that same row.
    let mut settled = attempt_fixture("att-1", "evt-1");
    settled.status = AttemptStatus::Completed;
    settled.completed_at_unix_ms = Some(1_700_000_003);
    settled.input_tokens = Some(120);
    settled.output_tokens = Some(35);
    let stamp = f.store.stamp();
    f.store
        .record_ranking_event_with_attempts(
            inv.clock(),
            &cx,
            &event_fixture("evt-1"),
            &[],
            None,
            std::slice::from_ref(&settled),
            stamp,
        )
        .expect("this invocation settles its own attempt");

    assert_eq!(
        f.attempt_count("evt-1"),
        1,
        "three lifecycle writes left more than one row for one attempt"
    );
    let (input, output, status) = f.tokens("att-1");
    assert_eq!(status, "completed");
    assert_eq!(input, Some(120), "the settled cost was not recorded");
    assert_eq!(output, Some(35));

    // A further write that knows nothing about usage must not erase it.
    let mut blank = attempt_fixture("att-1", "evt-1");
    blank.status = AttemptStatus::Completed;
    blank.input_tokens = None;
    blank.output_tokens = None;
    let stamp = f.store.stamp();
    f.store
        .record_ranking_event_with_attempts(
            inv.clock(),
            &cx,
            &event_fixture("evt-1"),
            &[],
            None,
            std::slice::from_ref(&blank),
            stamp,
        )
        .expect("recording again is permitted");
    let (input, output, _) = f.tokens("att-1");
    assert_eq!(
        input,
        Some(120),
        "recorded spend was erased by a later write"
    );
    assert_eq!(
        output,
        Some(35),
        "recorded spend was erased by a later write"
    );

    assert!(inv.shutdown());
}

#[test]
fn attempt_advancement_refuses_identity_collisions_atomically() {
    let (inv, cx) = test_invocation();
    let mut f = Fixture::new("identity-collision", &inv, &cx);
    let original = attempt_fixture("owned-attempt", "original-event");
    let stamp = f.store.stamp();
    f.store
        .record_ranking_event_with_attempts(
            inv.clock(),
            &cx,
            &event_fixture("original-event"),
            &[],
            None,
            std::slice::from_ref(&original),
            stamp,
        )
        .expect("original event and attempt recorded together");

    for field in ["owner", "stage", "fingerprint", "admission"] {
        let mut conflicting = original.clone();
        let mut event = event_fixture("original-event");
        event.reason = "must-not-commit".into();
        conflicting.status = AttemptStatus::Completed;
        conflicting.input_tokens = Some(999);
        conflicting.completed_at_unix_ms = Some(1_700_000_020);
        match field {
            "owner" => {
                event.event_id = "foreign-event".into();
                conflicting.owner_event_id = event.event_id.clone();
            }
            "stage" => conflicting.stage = CandidateStage::Rerank,
            "fingerprint" => conflicting.request_fingerprint = "different-request".into(),
            _ => conflicting.admitted_at_unix_ms += 1,
        }
        let stamp = f.store.stamp();
        let result = f.store.record_ranking_event_with_attempts(
            inv.clock(),
            &cx,
            &event,
            &[],
            None,
            &[conflicting],
            stamp,
        );
        assert!(
            matches!(result, Err(StoreError::RecordConflict)),
            "conflicting {field} must be refused: {result:?}"
        );
        assert_eq!(
            f.tokens("owned-attempt"),
            (None, None, "sent".into()),
            "conflicting {field} changed original cost/status"
        );
        assert_eq!(f.attempt_count("original-event"), 1);
        let conn = Connection::open(f.store.database_path()).unwrap();
        let events: Vec<(String, String)> = conn
            .prepare("SELECT event_id, reason FROM ranking_events ORDER BY event_id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            events,
            vec![("original-event".into(), "eligible".into())],
            "conflicting {field} partially committed its event"
        );
    }
    assert!(inv.shutdown());
}
