#![cfg(unix)]
//! Attempt rows built from real admission provenance (sr-roadmap-l1i.6.7).
//!
//! `tests/jev_admission.rs` pins what the admission seam remembers per attempt,
//! and `tests/ledger_attempt_ownership_contract.rs` pins what the table refuses.
//! Between them sat the part that actually turns one into the other, which is
//! where cost evidence can quietly become wrong: a monotonic offset written as a
//! unix time, an absent token count written as zero, or an attempt whose answer
//! never arrived written down as a clean failure.
//!
//! Every case here drives a real `AttemptAdmission` through its real transitions
//! and inserts the mapped row into a real SQLite ledger, so the schema's own
//! CHECK constraints are part of the evidence rather than something these
//! assertions merely hope about.
use asupersync::Cx;
use rusqlite::Connection;
use skillranker::jev::client::{TransportError, TransportErrorKind};
use skillranker::jev::retry::RetryAfter;
use skillranker::jev::{
    AttemptAdmission, AttemptBudget, AttemptFailure, AttemptOutcome, AttemptProvenance,
    CanonicalOrigin, RankingStage, Usage,
};
use skillranker::limits::{DurationMillis, MonotonicMillis};
use skillranker::runtime::{EntryClock, ProcessInvocation};
use skillranker::storage::StoreError;
use skillranker::storage::ledger::*;
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// A wall-clock base far from zero, so an unconverted monotonic offset cannot
/// pass for a converted one.
const ENTRY_WALL_MS: u64 = 1_789_941_701_834;

/// Stands in for one invocation. Two deliveries of the same event are two
/// invocations with two tokens, which is what keeps their rows apart.
const TOKEN: &str = "inv-token-1";

fn endpoint() -> CanonicalOrigin {
    CanonicalOrigin::parse("https://api.typesafe.ai").expect("valid production origin")
}

fn clock() -> EntryClock {
    EntryClock::capture_with(
        DurationMillis::new("total", 30_000, 60_000).unwrap(),
        DurationMillis::new("cleanup", 500, 60_000).unwrap(),
    )
    .unwrap()
}

fn admission(invocation: &str) -> AttemptAdmission {
    AttemptAdmission::new(AttemptBudget::new(4, 8).unwrap(), clock(), invocation).unwrap()
}

fn test_invocation() -> (ProcessInvocation, Cx) {
    let inv = ProcessInvocation::enter().expect("process invocation");
    let cx = inv.request_cx().expect("request cx");
    (inv, cx)
}

// Intentionally retained: repository policy forbids automatic tree deletion.
fn temp_ledger_dir(test_name: &str) -> PathBuf {
    // As in the schema contract: RCH's TMPDIR can have ancestors owned by another
    // user, so use Linux's root-owned sticky /tmp rather than relaxing the store's
    // own ancestor checks.
    let dir = PathBuf::from("/tmp").join(format!(
        "sr-attempt-row-{}-{}-{}",
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
        mode_channel: "hook-claude".into(),
        policy_version: "v1".into(),
        schema_version: 1,
        decision: DecisionKind::Ranked,
        reason: "eligible".into(),
        exposure_state: ExposureState::Generated,
        elapsed_ms: 50,
        created_at_unix_ms: ENTRY_WALL_MS,
        input_tokens: None,
        output_tokens: None,
        snapshot_id: None,
    }
}

/// One recorded row, read back through a plain connection rather than the
/// writer's own types, so the values under test are the stored ones.
#[derive(Debug)]
struct StoredAttempt {
    stage: String,
    admitted_at: i64,
    sent_at: Option<i64>,
    completed_at: Option<i64>,
    status: String,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    http_status: Option<i64>,
    error_kind: Option<String>,
}

struct Fixture {
    store: LedgerStore,
}

impl Fixture {
    fn new(name: &str, inv: &ProcessInvocation, cx: &Cx, owner: &str) -> Self {
        let dir = temp_ledger_dir(name);
        let mut store = match open_ledger(
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
        let stamp = store.stamp();
        store
            .record_ranking_event(inv.clock(), cx, &event_fixture(owner), &[], None, stamp)
            .expect("owning event recorded");
        Self { store }
    }

    fn write(&mut self, inv: &ProcessInvocation, cx: &Cx, attempt: &NewProviderAttempt) {
        let stamp = self.store.stamp();
        self.store
            .record_provider_attempt(inv.clock(), cx, attempt, stamp)
            .expect("attempt recorded");
    }

    /// Looks up by the key the row actually carries. The mapping scopes a row key
    /// by its owner because in-process attempt ids repeat across invocations, so
    /// the provenance id alone would find nothing here.
    fn read(&self, attempt_id: &str) -> StoredAttempt {
        let conn = Connection::open(self.store.database_path()).expect("open raw sqlite");
        conn.query_row(
            "SELECT stage, admitted_at_unix_ms, sent_at_unix_ms, completed_at_unix_ms,
                    status, input_tokens, output_tokens, http_status, error_kind
             FROM provider_attempts WHERE attempt_id = ?1",
            [attempt_id],
            |row| {
                Ok(StoredAttempt {
                    stage: row.get(0)?,
                    admitted_at: row.get(1)?,
                    sent_at: row.get(2)?,
                    completed_at: row.get(3)?,
                    status: row.get(4)?,
                    input_tokens: row.get(5)?,
                    output_tokens: row.get(6)?,
                    http_status: row.get(7)?,
                    error_kind: row.get(8)?,
                })
            },
        )
        .expect("attempt row")
    }
}

fn only_row(admission: &AttemptAdmission, owner: &str) -> NewProviderAttempt {
    let provenance: Vec<_> = admission.attempts().collect();
    assert_eq!(provenance.len(), 1, "expected exactly one admitted attempt");
    NewProviderAttempt::from_provenance(owner, TOKEN, "req-fp-hex", ENTRY_WALL_MS, provenance[0])
        .expect("a ranking-stage attempt has a row")
}

fn row_for(admission: &AttemptAdmission, owner: &str, attempt_id: &str) -> NewProviderAttempt {
    let provenance = admission
        .attempts()
        .find(|attempt| attempt.attempt_id == attempt_id)
        .expect("attempt is known to this invocation");
    NewProviderAttempt::from_provenance(owner, TOKEN, "req-fp-hex", ENTRY_WALL_MS, provenance)
        .expect("a ranking-stage attempt has a row")
}

/// The rerank stage is only admissible after a completed wide stage, so a case
/// about rerank has to earn one first rather than assume the seam allows it.
fn complete_wide_stage(admission: &mut AttemptAdmission, origin: &CanonicalOrigin) {
    let permit = admission.admit(RankingStage::Wide, origin).unwrap();
    let sent = permit.mark_sent().unwrap();
    admission.record_sent(&sent).unwrap();
    admission
        .record_response(
            &sent,
            Usage {
                input_tokens: 10,
                output_tokens: 5,
            },
        )
        .unwrap();
}

#[test]
fn a_completed_attempt_records_exact_tokens_and_wall_clock_times() {
    let (inv, cx) = test_invocation();
    let mut admission = admission("inv-complete");
    let origin = endpoint();
    let permit = admission.admit(RankingStage::Wide, &origin).unwrap();
    let attempt_id = permit.attempt_id().as_str().to_owned();
    let sent = permit.mark_sent().unwrap();
    admission.record_sent(&sent).unwrap();
    admission
        .record_response(
            &sent,
            Usage {
                input_tokens: 1_200,
                output_tokens: 340,
            },
        )
        .unwrap();

    let provenance: Vec<_> = admission.attempts().cloned().collect();
    let row = only_row(&admission, "ev-complete");
    // Conversion is exact against the single reading, not an independent sample.
    assert_eq!(
        row.admitted_at_unix_ms,
        ENTRY_WALL_MS + provenance[0].admitted_at.as_millis()
    );
    assert_eq!(
        row.sent_at_unix_ms,
        Some(ENTRY_WALL_MS + provenance[0].sent_at.unwrap().as_millis())
    );

    let mut fixture = Fixture::new("complete", &inv, &cx, "ev-complete");
    fixture.write(&inv, &cx, &row);
    let stored = fixture.read(&row.attempt_id);
    // Owner-scoped, so a second invocation cannot collide with this row.
    assert_eq!(row.attempt_id, format!("ev-complete:{TOKEN}:{attempt_id}"));
    assert_eq!(stored.stage, "wide");
    assert_eq!(stored.status, "completed");
    assert_eq!(stored.input_tokens, Some(1_200));
    assert_eq!(stored.output_tokens, Some(340));
    assert_eq!(stored.error_kind, None);
    assert_eq!(stored.http_status, None);
    // The defect this pins: a row dated from the monotonic clock lands in 1970.
    assert!(
        stored.admitted_at >= 1_700_000_000_000,
        "recorded time {} is not a unix millisecond timestamp",
        stored.admitted_at
    );
    assert!(stored.admitted_at <= stored.sent_at.unwrap());
    assert!(stored.sent_at.unwrap() <= stored.completed_at.unwrap());
}

#[test]
fn an_unanswered_attempt_keeps_null_tokens_and_unknown_status() {
    let (inv, cx) = test_invocation();
    let mut admission = admission("inv-unanswered");
    let origin = endpoint();
    complete_wide_stage(&mut admission, &origin);
    let permit = admission.admit(RankingStage::Rerank, &origin).unwrap();
    let attempt_id = permit.attempt_id().as_str().to_owned();
    let sent = permit.mark_sent().unwrap();
    admission.record_sent(&sent).unwrap();
    admission
        .record_terminal_failure(&sent, AttemptFailure::Indeterminate("transient-io"))
        .unwrap();

    let row = row_for(&admission, "ev-unanswered", &attempt_id);
    let mut fixture = Fixture::new("unanswered", &inv, &cx, "ev-unanswered");
    fixture.write(&inv, &cx, &row);
    let stored = fixture.read(&row.attempt_id);
    assert_eq!(stored.stage, "rerank");
    // Bytes were on the wire and nothing valid came back: neither a success nor a
    // clean failure, and specifically not a zero-cost one.
    assert_eq!(stored.status, "unknown");
    assert_eq!(
        stored.input_tokens, None,
        "absent usage must not become zero"
    );
    assert_eq!(stored.output_tokens, None);
    assert_eq!(stored.error_kind.as_deref(), Some("transient-io"));
    assert_eq!(stored.http_status, None);
    assert!(stored.completed_at.is_some());
}

#[test]
fn a_terminal_provider_status_is_a_failure_carrying_that_status() {
    let (inv, cx) = test_invocation();
    let mut admission = admission("inv-http");
    let origin = endpoint();
    let permit = admission.admit(RankingStage::Wide, &origin).unwrap();
    let attempt_id = permit.attempt_id().as_str().to_owned();
    let sent = permit.mark_sent().unwrap();
    admission.record_sent(&sent).unwrap();
    admission
        .record_terminal_failure(&sent, AttemptFailure::Http(503))
        .unwrap();

    let row = only_row(&admission, "ev-http");
    assert_eq!(row.attempt_id, format!("ev-http:{TOKEN}:{attempt_id}"));
    let mut fixture = Fixture::new("http", &inv, &cx, "ev-http");
    fixture.write(&inv, &cx, &row);
    let stored = fixture.read(&row.attempt_id);
    // The provider answered, so the ending is established.
    assert_eq!(stored.status, "failed");
    assert_eq!(stored.http_status, Some(503));
    assert_eq!(stored.error_kind.as_deref(), Some("http-status"));
    assert_eq!(stored.input_tokens, None);
}

#[test]
fn a_discard_before_send_has_no_sent_time() {
    let (inv, cx) = test_invocation();
    let mut admission = admission("inv-discard");
    let origin = endpoint();
    let permit = admission.admit(RankingStage::Wide, &origin).unwrap();
    let attempt_id = permit.attempt_id().as_str().to_owned();
    admission
        .record_discard(&permit.discard_before_send("local refusal before HTTP"))
        .unwrap();

    let row = only_row(&admission, "ev-discard");
    assert_eq!(row.attempt_id, format!("ev-discard:{TOKEN}:{attempt_id}"));
    assert_eq!(row.sent_at_unix_ms, None);
    let mut fixture = Fixture::new("discard", &inv, &cx, "ev-discard");
    fixture.write(&inv, &cx, &row);
    let stored = fixture.read(&row.attempt_id);
    assert_eq!(stored.sent_at, None, "nothing reached the wire");
    assert_eq!(stored.status, "failed");
    // The permit's own reason is free text from the call site; the recorded kind
    // stays a fixed identifier.
    assert_eq!(stored.error_kind.as_deref(), Some("discarded-before-send"));
    assert_eq!(stored.input_tokens, None);
}

#[test]
fn determinacy_decides_between_failed_and_unknown() {
    // Identical failure kinds, differing only in whether the HTTP client was
    // entered. Recording both as `failed` would assert that the provider did no
    // work on a request that may well have reached it.
    let indeterminate = TransportError {
        kind: TransportErrorKind::TransientIo,
        http_attempt_started: true,
        retry_after: RetryAfter::Absent,
    };
    let determinate = TransportError {
        kind: TransportErrorKind::TransientIo,
        http_attempt_started: false,
        retry_after: RetryAfter::Absent,
    };
    assert_eq!(
        AttemptFailure::from_transport(&indeterminate),
        AttemptFailure::Indeterminate("transient-io")
    );
    assert_eq!(
        AttemptFailure::from_transport(&determinate),
        AttemptFailure::Local("transient-io")
    );

    let row = |failure: AttemptFailure| {
        NewProviderAttempt::from_provenance(
            "ev-determinacy",
            TOKEN,
            "req-fp-hex",
            ENTRY_WALL_MS,
            &AttemptProvenance {
                attempt_id: "att-1".into(),
                stage: RankingStage::Wide,
                admitted_at: MonotonicMillis::from_millis(10),
                sent_at: Some(MonotonicMillis::from_millis(20)),
                settled_at: Some(MonotonicMillis::from_millis(30)),
                outcome: AttemptOutcome::Failed,
                usage: None,
                failure: Some(failure),
            },
        )
        .expect("a ranking-stage attempt has a row")
        .status
    };
    assert_eq!(
        row(AttemptFailure::from_transport(&indeterminate)),
        AttemptStatus::Unknown
    );
    assert_eq!(
        row(AttemptFailure::from_transport(&determinate)),
        AttemptStatus::Failed
    );
    // A status the provider answered with is established regardless of stage.
    assert_eq!(row(AttemptFailure::Http(429)), AttemptStatus::Failed);
}

#[test]
fn a_probe_attempt_has_no_row_in_a_ranking_stage_table() {
    // `provider_attempts.stage` admits 'wide' and 'rerank' only. Filing a breaker
    // probe or an evaluation batch under one of them would attribute its cost to a
    // ranking the user never asked for, so the mapping declines instead.
    for stage in [RankingStage::Probe, RankingStage::Evaluation] {
        let provenance = AttemptProvenance {
            attempt_id: format!("att-{}", stage.as_str()),
            stage,
            admitted_at: MonotonicMillis::from_millis(5),
            sent_at: None,
            settled_at: None,
            outcome: AttemptOutcome::Admitted,
            usage: None,
            failure: None,
        };
        assert!(
            NewProviderAttempt::from_provenance(
                "ev-probe",
                TOKEN,
                "req-fp-hex",
                ENTRY_WALL_MS,
                &provenance
            )
            .is_none(),
            "{} must not be filed as a ranking stage",
            stage.as_str()
        );
    }
}

#[test]
fn a_cache_served_invocation_has_no_attempt_of_its_own() {
    // A single-flight follower reusing an owner's response must not appear to have
    // paid for it: no admission, so no provenance, so no row.
    let mut admission = admission("inv-follower");
    admission.record_cache_hit();
    assert_eq!(admission.attempts().count(), 0);
    assert!(admission.receipt().is_empty());
}

#[test]
fn an_out_of_range_http_status_keeps_the_kind_and_drops_the_number() {
    // The column admits 100..=599. A malformed status must not cost the whole row,
    // which would lose the record of an attempt that really was made.
    let (inv, cx) = test_invocation();
    let row = NewProviderAttempt::from_provenance(
        "ev-range",
        TOKEN,
        "req-fp-hex",
        ENTRY_WALL_MS,
        &AttemptProvenance {
            attempt_id: "att-range".into(),
            stage: RankingStage::Wide,
            admitted_at: MonotonicMillis::from_millis(1),
            sent_at: Some(MonotonicMillis::from_millis(2)),
            settled_at: Some(MonotonicMillis::from_millis(3)),
            outcome: AttemptOutcome::Failed,
            usage: None,
            failure: Some(AttemptFailure::Http(999)),
        },
    )
    .expect("a ranking-stage attempt has a row");
    assert_eq!(row.http_status, None);
    assert_eq!(row.error_kind.as_deref(), Some("http-status"));

    let mut fixture = Fixture::new("range", &inv, &cx, "ev-range");
    fixture.write(&inv, &cx, &row);
    let stored = fixture.read(&row.attempt_id);
    assert_eq!(stored.status, "failed");
    assert_eq!(stored.http_status, None);
    assert_eq!(stored.error_kind.as_deref(), Some("http-status"));
}

#[test]
fn a_second_delivery_of_one_event_keeps_both_deliveries_cost() {
    // sr-qqlk. A duplicate delivery is deliberately the same event, so the second
    // recording conflicts on the event's primary key. Failing there rolled the whole
    // transaction back and took that delivery's attempt rows with it, so a repeat
    // that really paid was recorded nowhere. The event stays as the first delivery
    // wrote it; the attempts accumulate, because both were incurred.
    let (inv, cx) = test_invocation();
    let mut fixture = Fixture::new("duplicate", &inv, &cx, "ev-dup");
    let row_for_invocation = |token: &str| {
        NewProviderAttempt::from_provenance(
            "ev-dup",
            token,
            "req-fp-hex",
            ENTRY_WALL_MS,
            &AttemptProvenance {
                attempt_id: "rank-att-1".into(),
                stage: RankingStage::Wide,
                admitted_at: MonotonicMillis::from_millis(1),
                sent_at: Some(MonotonicMillis::from_millis(2)),
                settled_at: Some(MonotonicMillis::from_millis(3)),
                outcome: AttemptOutcome::Completed,
                usage: Some(Usage {
                    input_tokens: 10,
                    output_tokens: 4,
                }),
                failure: None,
            },
        )
        .expect("a ranking-stage attempt has a row")
    };

    // The first delivery's event already exists from the fixture, so recording the
    // same event again is exactly the duplicate path.
    let first = row_for_invocation("inv-a");
    let second = row_for_invocation("inv-b");
    assert_ne!(
        first.attempt_id, second.attempt_id,
        "two invocations under one event must not share a row key"
    );
    let stamp = fixture.store.stamp();
    fixture
        .store
        .record_ranking_event_with_attempts(
            inv.clock(),
            &cx,
            &event_fixture("ev-dup"),
            &[],
            None,
            std::slice::from_ref(&first),
            stamp,
        )
        .expect("a duplicate delivery is recognised, not refused");
    let stamp = fixture.store.stamp();
    fixture
        .store
        .record_ranking_event_with_attempts(
            inv.clock(),
            &cx,
            &event_fixture("ev-dup"),
            &[],
            None,
            std::slice::from_ref(&second),
            stamp,
        )
        .expect("the second delivery is recognised too");

    let conn = rusqlite::Connection::open(fixture.store.database_path()).unwrap();
    let events: i64 = conn
        .query_row("SELECT count(*) FROM ranking_events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        events, 1,
        "one turn is one event however often it is delivered"
    );
    let attempts: i64 = conn
        .query_row("SELECT count(*) FROM provider_attempts", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(attempts, 2, "both deliveries paid, so both are recorded");
}

#[test]
fn an_event_id_reused_for_another_session_is_refused() {
    // The counterpart: recognising a repeat must not become accepting a collision.
    // An id that arrives with a different session is not the same turn twice.
    let (inv, cx) = test_invocation();
    let mut fixture = Fixture::new("collision", &inv, &cx, "ev-collide");
    let mut foreign = event_fixture("ev-collide");
    foreign.session_id = "a-different-session".into();
    let stamp = fixture.store.stamp();
    assert_eq!(
        fixture
            .store
            .record_ranking_event_with_attempts(inv.clock(), &cx, &foreign, &[], None, &[], stamp,)
            .unwrap_err(),
        StoreError::RecordConflict
    );
}
