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
    dir: PathBuf,
}

impl Fixture {
    fn new(name: &str, inv: &ProcessInvocation, cx: &Cx) -> Self {
        let dir = temp_ledger_dir(name);
        let store = match open_ledger(
            inv,
            cx,
            LedgerAccess::Initialize,
            LedgerLocation::Directory(dir.clone()),
        )
        .expect("open_ledger succeeds")
        {
            LedgerOpen::Ready(store) => *store,
            other => panic!("expected Ready, got {other:?}"),
        };
        Self { store, dir }
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

/// A judgment must key on the stable skill id even when a person supplies the invocation
/// name they read in the ranking output (sr-oufi).
fn snapshot_with(id: &str, members: &[(&str, &str)]) -> NewRosterSnapshot {
    let json: Vec<_> = members
        .iter()
        .map(|(skill_id, invocation)| {
            serde_json::json!({
                "skill_id": skill_id,
                "invocation_name": invocation,
                "content_hash": "hash-1",
                "source": "claude_code.project",
                "eligible": true,
                "exclusion_reason": null,
            })
        })
        .collect();
    NewRosterSnapshot {
        snapshot_id: id.into(),
        workspace_root: "/data/workspace".into(),
        adapter: "claude_code".into(),
        total_candidates: members.len() as u64,
        eligible_candidates: members.len() as u64,
        membership_coverage: MembershipCoverage::Complete,
        members_json: serde_json::Value::Array(json).to_string(),
        created_at_unix_ms: 1_700_000_000,
    }
}

fn candidate_fixture(
    event_id: &str,
    skill_id: &str,
    stage: CandidateStage,
    rank_position: Option<u32>,
) -> NewRankingCandidate {
    NewRankingCandidate {
        event_id: event_id.into(),
        stage,
        skill_id: skill_id.into(),
        skill_version: "1.0.0".into(),
        raw_probability: Some(0.7),
        normalized_probability: Some(0.7),
        fit_score: Some(0.8),
        rank_score: Some(0.9),
        rank_position,
        excluded: false,
        exclusion_reason: None,
    }
}

/// Builds an event that carries a snapshot and one recorded candidate, which is the shape a
/// real ranking leaves behind.
fn event_with_snapshot(id: &str, snapshot_id: &str) -> NewRankingEvent {
    let mut event = event_fixture(id);
    event.snapshot_id = Some(snapshot_id.into());
    event
}

#[test]
fn a_judgment_supplied_by_invocation_name_is_stored_under_the_stable_skill_id() {
    // The defect this pins: `sr feedback --skill rust-code-review` stored the literal string
    // 'rust-code-review' while the candidates and observations for the same skill used the
    // stable opaque id, so `stats --by-skill` reported one skill as two rows and per-skill
    // usefulness could never be joined to per-skill recommendations.
    let (inv, cx) = test_invocation();
    let mut f = Fixture::new("resolve-name", &inv, &cx);
    let stable = "s_aaaa0000000000000000000000000000000000000000000000000000000000aa";

    let stamp = f.store.stamp();
    f.store
        .record_roster_snapshot(
            inv.clock(),
            &cx,
            &snapshot_with("snap-1", &[(stable, "rust-code-review")]),
            stamp,
        )
        .expect("snapshot recorded");
    let stamp = f.store.stamp();
    f.store
        .record_ranking_event(
            inv.clock(),
            &cx,
            &event_with_snapshot("evt-1", "snap-1"),
            &[],
            None,
            stamp,
        )
        .expect("event recorded");

    let stamp = f.store.stamp();
    f.store
        .record_single_feedback(
            inv.clock(),
            &cx,
            &SingleFeedbackRequest {
                event_id: "evt-1".into(),
                skill_id: "rust-code-review".into(),
                verdict: JudgmentLabel::Useful,
                reason_code: None,
                provenance: Some("contract_test".into()),
                expected_version: None,
            },
            stamp,
        )
        .expect("a judgment supplied by invocation name is accepted");

    let conn = Connection::open(f.store.database_path()).expect("open raw sqlite");
    let stored: String = conn
        .query_row(
            "SELECT skill_id FROM judgments WHERE attributed_event_id = ?1",
            ["evt-1"],
            |row| row.get(0),
        )
        .expect("one judgment row");
    assert_eq!(
        stored, stable,
        "the judgment was stored under the supplied name instead of the stable skill id, so it \
         cannot join the candidates or observations for the same skill"
    );
    assert!(inv.shutdown());
}

#[test]
fn a_stable_skill_id_supplied_directly_still_works_without_a_snapshot() {
    // Resolution must not become a new requirement for callers who already supply the id.
    // An event whose roster membership was only partially observed has no complete snapshot,
    // and feedback on it has to keep working.
    let (inv, cx) = test_invocation();
    let mut f = Fixture::new("resolve-id", &inv, &cx);
    let stable = "s_bbbb0000000000000000000000000000000000000000000000000000000000bb";

    let stamp = f.store.stamp();
    f.store
        .record_ranking_event(
            inv.clock(),
            &cx,
            &event_fixture("evt-2"),
            &[candidate_fixture(
                "evt-2",
                stable,
                CandidateStage::Wide,
                Some(1),
            )],
            None,
            stamp,
        )
        .expect("event with a candidate and no snapshot");

    let stamp = f.store.stamp();
    f.store
        .record_single_feedback(
            inv.clock(),
            &cx,
            &SingleFeedbackRequest {
                event_id: "evt-2".into(),
                skill_id: stable.into(),
                verdict: JudgmentLabel::Useful,
                reason_code: None,
                provenance: Some("contract_test".into()),
                expected_version: None,
            },
            stamp,
        )
        .expect("a candidate's stable id needs no snapshot to resolve");

    let conn = Connection::open(f.store.database_path()).expect("open raw sqlite");
    let stored: String = conn
        .query_row(
            "SELECT skill_id FROM judgments WHERE attributed_event_id = ?1",
            ["evt-2"],
            |row| row.get(0),
        )
        .expect("one judgment row");
    assert_eq!(stored, stable);
    assert!(inv.shutdown());
}

#[test]
fn an_invocation_name_matching_two_skills_is_refused_rather_than_guessed() {
    // Invocation names are not unique across sources and the harness may keep same-name
    // skills distinct, so a name matching two members cannot be resolved. Attaching the label
    // to whichever happened to sort first would put it on the wrong skill.
    let (inv, cx) = test_invocation();
    let mut f = Fixture::new("resolve-ambiguous", &inv, &cx);
    let one = "s_cccc0000000000000000000000000000000000000000000000000000000000cc";
    let two = "s_dddd0000000000000000000000000000000000000000000000000000000000dd";

    let stamp = f.store.stamp();
    f.store
        .record_roster_snapshot(
            inv.clock(),
            &cx,
            &snapshot_with("snap-2", &[(one, "review"), (two, "review")]),
            stamp,
        )
        .expect("snapshot with two same-named skills");
    let stamp = f.store.stamp();
    f.store
        .record_ranking_event(
            inv.clock(),
            &cx,
            &event_with_snapshot("evt-3", "snap-2"),
            &[],
            None,
            stamp,
        )
        .expect("event recorded");

    let stamp = f.store.stamp();
    let result = f.store.record_single_feedback(
        inv.clock(),
        &cx,
        &SingleFeedbackRequest {
            event_id: "evt-3".into(),
            skill_id: "review".into(),
            verdict: JudgmentLabel::Useful,
            reason_code: None,
            provenance: Some("contract_test".into()),
            expected_version: None,
        },
        stamp,
    );
    assert!(
        result.is_err(),
        "an ambiguous invocation name must be refused, not resolved to an arbitrary match"
    );
    let conn = Connection::open(f.store.database_path()).expect("open raw sqlite");
    let count: i64 = conn
        .query_row("SELECT count(*) FROM judgments", [], |row| row.get(0))
        .expect("count");
    assert_eq!(count, 0, "a refused resolution must write no label at all");
    assert!(inv.shutdown());
}

#[test]
fn a_name_with_no_match_and_no_snapshot_is_refused_without_writing() {
    // The remaining path: nothing to resolve against. The refusal must say so rather than
    // storing the unresolved string, which is the behaviour that created the defect.
    let (inv, cx) = test_invocation();
    let mut f = Fixture::new("resolve-nothing", &inv, &cx);
    let stamp = f.store.stamp();
    f.store
        .record_ranking_event(inv.clock(), &cx, &event_fixture("evt-4"), &[], None, stamp)
        .expect("event with neither candidates nor a snapshot");

    let stamp = f.store.stamp();
    let result = f.store.record_single_feedback(
        inv.clock(),
        &cx,
        &SingleFeedbackRequest {
            event_id: "evt-4".into(),
            skill_id: "rust-code-review".into(),
            verdict: JudgmentLabel::Useful,
            reason_code: None,
            provenance: Some("contract_test".into()),
            expected_version: None,
        },
        stamp,
    );
    assert!(result.is_err(), "an unresolvable reference must be refused");
    let conn = Connection::open(f.store.database_path()).expect("open raw sqlite");
    let count: i64 = conn
        .query_row("SELECT count(*) FROM judgments", [], |row| row.get(0))
        .expect("count");
    assert_eq!(count, 0, "no unresolved name may be stored as a skill id");
    assert!(inv.shutdown());
}

#[test]
fn a_skill_recommended_and_judged_by_name_reports_one_by_skill_row() {
    // The user-visible consequence, asserted on the real report rather than on the row: the
    // reality check observed `stats --by-skill` return
    //   [(s_6d9c784e05bfd6ba44, 1, 0, 0), ..., (rust-code-review, 0, 0, 1)]
    // for a single skill that was recommended once and judged useful once. One skill, two rows,
    // and no way for a reader to see they are the same skill. This is the planted negative for
    // the defect: with the resolution removed, `rows` below has length two.
    let (inv, cx) = test_invocation();
    let mut f = Fixture::new("by-skill-one-row", &inv, &cx);
    let stable = "s_bbbb0000000000000000000000000000000000000000000000000000000000bb";

    let stamp = f.store.stamp();
    f.store
        .record_roster_snapshot(
            inv.clock(),
            &cx,
            &snapshot_with("snap-stats", &[(stable, "rust-code-review")]),
            stamp,
        )
        .expect("snapshot recorded");

    // A recommendation a reader would count: ranked, emitted, and top of the rerank stage.
    let mut event = event_with_snapshot("evt-stats", "snap-stats");
    event.exposure_state = ExposureState::Emitted;
    let stamp = f.store.stamp();
    f.store
        .record_ranking_event(
            inv.clock(),
            &cx,
            &event,
            &[candidate_fixture(
                "evt-stats",
                stable,
                CandidateStage::Rerank,
                Some(1),
            )],
            None,
            stamp,
        )
        .expect("event and its top candidate recorded");

    // An observed load under the stable id, which is what the observation path already writes.
    let stamp = f.store.stamp();
    f.store
        .record_observation(
            inv.clock(),
            &cx,
            &NewObservation {
                observation_id: "obs-stats".into(),
                source_event_key: "native:session:tool-1".into(),
                workspace_root: "/data/workspace".into(),
                session_id: "session".into(),
                agent_branch: "main".into(),
                attributed_event_id: Some("evt-stats".into()),
                skill_id: stable.into(),
                evidence_state: EvidenceState::Loaded,
                observed_at_unix_ms: 1_700_000_100,
            },
            stamp,
        )
        .expect("observation recorded");

    // The judgment arrives the way a human supplies it: by the name printed in the output.
    let stamp = f.store.stamp();
    f.store
        .record_single_feedback(
            inv.clock(),
            &cx,
            &SingleFeedbackRequest {
                event_id: "evt-stats".into(),
                skill_id: "rust-code-review".into(),
                verdict: JudgmentLabel::Useful,
                reason_code: None,
                provenance: Some("contract_test".into()),
                expected_version: None,
            },
            stamp,
        )
        .expect("judgment by invocation name accepted");

    let report = ledger_stats(&inv, &cx, LedgerLocation::Directory(f.dir.clone()), 0, true)
        .expect("ledger_stats with --by-skill");
    let rows = report.by_skill.expect("--by-skill populates the cohort");
    let named: Vec<&str> = rows.iter().map(|r| r.skill_id.as_str()).collect();
    assert_eq!(
        rows.len(),
        1,
        "one skill must occupy one row; saw {named:?}, which is the two-row split that makes \
         per-skill usefulness unjoinable to per-skill recommendations"
    );
    let row = rows.first().expect("the single row just asserted above");
    assert_eq!(
        row.skill_id, stable,
        "the row must key on the stable skill id"
    );
    assert_eq!(
        row.top1_recommendations, 1,
        "the recommendation is on this row"
    );
    assert_eq!(
        row.observed_loads, 1,
        "the observed load is on the same row"
    );
    assert_eq!(row.judged_useful, 1, "the judgment is on the same row");
    assert!(inv.shutdown());
}

#[test]
fn a_judgment_already_stored_under_a_name_is_left_untouched() {
    // Migration honesty (sr-oufi refinement 1b, answered by reading the DDL): judgments has no
    // uniqueness constraint or foreign key on skill_id, only `judgment_id` as primary key and
    // `attributed_event_id` referencing the event. Name-keyed rows written before the fix can
    // therefore coexist with id-keyed ones, so no migration is forced and nothing has to be
    // rewritten on a guess. This test pins that: the historical row is still there, unchanged,
    // after a new judgment for the same event and the same skill is written under the stable id.
    let (inv, cx) = test_invocation();
    let mut f = Fixture::new("legacy-row", &inv, &cx);
    let stable = "s_cccc0000000000000000000000000000000000000000000000000000000000cc";

    let stamp = f.store.stamp();
    f.store
        .record_roster_snapshot(
            inv.clock(),
            &cx,
            &snapshot_with("snap-legacy", &[(stable, "rust-code-review")]),
            stamp,
        )
        .expect("snapshot recorded");
    let stamp = f.store.stamp();
    f.store
        .record_ranking_event(
            inv.clock(),
            &cx,
            &event_with_snapshot("evt-legacy", "snap-legacy"),
            &[],
            None,
            stamp,
        )
        .expect("event recorded");

    // Exactly what the defective build left behind: the label keyed by the invocation name.
    {
        let conn = Connection::open(f.store.database_path()).expect("open raw sqlite");
        conn.execute(
            "INSERT INTO judgments (judgment_id, attributed_event_id, skill_id, label, \
             label_version, provenance, created_at_unix_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                "jdg-legacy",
                "evt-legacy",
                "rust-code-review",
                "useful",
                1,
                "pre_fix_build",
                1_700_000_050i64,
            ],
        )
        .expect("historical name-keyed judgment");
    }

    let stamp = f.store.stamp();
    f.store
        .record_single_feedback(
            inv.clock(),
            &cx,
            &SingleFeedbackRequest {
                event_id: "evt-legacy".into(),
                skill_id: "rust-code-review".into(),
                verdict: JudgmentLabel::Harmful,
                reason_code: None,
                provenance: Some("contract_test".into()),
                expected_version: None,
            },
            stamp,
        )
        .expect("a new judgment resolves and writes under the stable id");

    let conn = Connection::open(f.store.database_path()).expect("open raw sqlite");
    let (label, version, provenance): (String, i64, String) = conn
        .query_row(
            "SELECT label, label_version, provenance FROM judgments WHERE judgment_id = ?1",
            ["jdg-legacy"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("the historical row still exists");
    assert_eq!(
        (label.as_str(), version, provenance.as_str()),
        ("useful", 1, "pre_fix_build"),
        "the historical name-keyed label must not be rewritten, revised or relabelled"
    );
    let fresh: String = conn
        .query_row(
            "SELECT skill_id FROM judgments WHERE judgment_id != ?1",
            ["jdg-legacy"],
            |row| row.get(0),
        )
        .expect("the new judgment is a separate row");
    assert_eq!(
        fresh, stable,
        "the new judgment keys on the stable id while the old row keeps its name"
    );
    assert!(inv.shutdown());
}
