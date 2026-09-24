//! An unfinished turn is not a failure, not a delivery, not a fast turn, and not a
//! cache hit (sr-roadmap-l1i.6.13).
//!
//! `sr rank` writes an event row *before* its first provider send, so that an
//! invocation which is killed while waiting cannot be mistaken for one that never
//! ran. That row carries `decision = 'unavailable'`, `exposure_state = 'generated'`,
//! `reason = IN_FLIGHT_REASON` and `elapsed_ms = 0`, and the completion path
//! overwrites it in place. A row that still carries that shape is therefore an
//! invocation that never came back: either still running, or killed.
//!
//! Every statistic derived from ranking events has to decide what to do with those
//! rows, and the tempting answers are all wrong in the same direction — they make
//! the product look worse at reliability and better at speed and cache reuse than
//! it is. These cases pin the honest answer for each one:
//!
//! - a row still in flight is not an operational failure,
//! - its `elapsed_ms = 0` is not a real duration and must stay out of the summary,
//! - its absent provider attempts are not evidence of a cache hit,
//! - and once the invocation finishes, its row is counted exactly once.

use asupersync::Cx;
use skillranker::runtime::ProcessInvocation;
use skillranker::storage::ledger::*;
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const BASE_MS: u64 = 1_700_000_000_000;

fn temp_private_dir(prefix: &str) -> PathBuf {
    let dir = PathBuf::from("/tmp").join(format!(
        "sr-inflight-{}-{}-{}",
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

/// The row `sr rank` writes before it sends anything, reproduced through the same
/// public record path the pipeline uses. `reason` comes from the shared constant
/// rather than a literal, so a producer that renames it breaks this test instead of
/// silently escaping the filters under test.
fn in_flight_event(event_id: &str, channel: &str, created_at_unix_ms: u64) -> NewRankingEvent {
    NewRankingEvent {
        event_id: event_id.into(),
        verified_delivery_key: None,
        workspace_root: "/data/workspace".into(),
        session_id: "sess-inflight".into(),
        agent_branch: "main".into(),
        mode_channel: channel.into(),
        policy_version: "v1".into(),
        schema_version: 1,
        decision: DecisionKind::Unavailable,
        reason: IN_FLIGHT_REASON.into(),
        exposure_state: ExposureState::Generated,
        elapsed_ms: 0,
        created_at_unix_ms,
        input_tokens: None,
        output_tokens: None,
        snapshot_id: None,
    }
}

/// A turn that finished, whatever it decided.
fn finished_event(
    event_id: &str,
    channel: &str,
    decision: DecisionKind,
    exposure: ExposureState,
    reason: &str,
    elapsed_ms: u64,
    created_at_unix_ms: u64,
) -> NewRankingEvent {
    NewRankingEvent {
        event_id: event_id.into(),
        verified_delivery_key: Some(format!("deliv-{event_id}")),
        workspace_root: "/data/workspace".into(),
        session_id: "sess-inflight".into(),
        agent_branch: "main".into(),
        mode_channel: channel.into(),
        policy_version: "v1".into(),
        schema_version: 1,
        decision,
        reason: reason.into(),
        exposure_state: exposure,
        elapsed_ms,
        created_at_unix_ms,
        input_tokens: Some(100),
        output_tokens: Some(50),
        snapshot_id: None,
    }
}

fn completed_attempt(
    attempt_id: &str,
    event_id: &str,
    created_at_unix_ms: u64,
) -> NewProviderAttempt {
    NewProviderAttempt {
        attempt_id: attempt_id.into(),
        owner_event_id: event_id.into(),
        stage: CandidateStage::Rerank,
        request_fingerprint: "req-fp".into(),
        admitted_at_unix_ms: created_at_unix_ms,
        sent_at_unix_ms: Some(created_at_unix_ms + 10),
        completed_at_unix_ms: Some(created_at_unix_ms + 100),
        status: AttemptStatus::Completed,
        input_tokens: Some(80),
        output_tokens: Some(20),
        http_status: Some(200),
        error_kind: None,
    }
}

struct Ledger {
    invocation: ProcessInvocation,
    cx: Cx,
    location: LedgerLocation,
    store: LedgerStore,
}

impl Ledger {
    fn new(prefix: &str) -> Self {
        let dir = temp_private_dir(prefix);
        let (invocation, cx) = test_invocation();
        let location = LedgerLocation::Directory(dir);
        let _init = init_ledger(&invocation, &cx, location.clone()).expect("init ledger");
        let open = open_ledger(
            &invocation,
            &cx,
            LedgerAccess::ExistingOnly,
            location.clone(),
        )
        .expect("open ledger");
        let store = match open {
            LedgerOpen::Ready(store) => *store,
            other => panic!("expected a ready store, got {other:?}"),
        };
        Self {
            invocation,
            cx,
            location,
            store,
        }
    }

    fn record(&mut self, event: &NewRankingEvent, attempts: &[NewProviderAttempt]) {
        let stamp = self.store.stamp();
        self.store
            .record_ranking_event_with_attempts(
                self.invocation.clock(),
                &self.cx,
                event,
                &[],
                None,
                attempts,
                stamp,
            )
            .unwrap_or_else(|error| panic!("recording {} failed: {error:?}", event.event_id));
    }

    fn judge(&mut self, judgment_id: &str, event_id: &str, label: JudgmentLabel) {
        let stamp = self.store.stamp();
        let judgment = NewJudgment {
            judgment_id: judgment_id.into(),
            attributed_event_id: event_id.into(),
            skill_id: "review".into(),
            label,
            label_version: 1,
            provenance: "contract_test".into(),
            created_at_unix_ms: BASE_MS + 500,
        };
        self.store
            .record_judgment(self.invocation.clock(), &self.cx, &judgment, stamp)
            .unwrap_or_else(|error| panic!("recording {judgment_id} failed: {error:?}"));
    }

    fn stats(&self) -> StatsValueReport {
        ledger_stats(
            &self.invocation,
            &self.cx,
            self.location.clone(),
            BASE_MS as i64 - 1,
            false,
        )
        .expect("ledger_stats")
    }
}

#[test]
fn a_turn_still_in_flight_is_not_an_operational_failure() {
    let mut ledger = Ledger::new("failures");
    // One invocation that really did fail after finishing its work, and one that
    // simply has not come back. Only the first is an operational failure; the
    // second has not yet established anything about the provider at all.
    ledger.record(
        &finished_event(
            "ev-failed",
            "cli",
            DecisionKind::Unavailable,
            ExposureState::Generated,
            "provider-unavailable",
            900,
            BASE_MS + 100,
        ),
        &[],
    );
    ledger.record(&in_flight_event("ev-inflight", "cli", BASE_MS + 200), &[]);

    let report = ledger.stats();

    // Both rows are real turns that were evaluated, so the denominator keeps both.
    assert_eq!(report.turns.total_evaluated, 2);
    assert_eq!(
        report.turns.operational_failures, 1,
        "an unfinished invocation was counted as an operational failure: {:#?}",
        report.turns
    );
    // Nor may it be swept into the muted cohort, which is about output that was
    // produced and withheld — this one produced nothing to withhold.
    assert_eq!(report.turns.muted_or_suppressed, 0, "{:#?}", report.turns);
    assert_eq!(report.turns.emitted_suggestions, 0);
    // Excluding it from the failures must not make it vanish: an invocation that may
    // have paid and never reported back is exactly what an operator needs to see.
    assert_eq!(report.turns.in_flight_or_killed, 1, "{:#?}", report.turns);

    let cli = report
        .turns
        .by_channel
        .iter()
        .find(|channel| channel.channel == "cli")
        .expect("a cli channel row");
    assert_eq!(cli.evaluated_turns, 2);
    assert_eq!(
        cli.unavailable, 1,
        "the per-channel failure count made the same mistake: {cli:#?}"
    );
    assert_eq!(cli.in_flight_or_killed, 1, "{cli:#?}");
}

#[test]
fn failure_causes_keep_an_absent_credential_apart_from_a_rejected_key() {
    let mut ledger = Ledger::new("causes");
    for (id, reason, at) in [
        ("ev-absent-1", "credential-absent", 100),
        ("ev-absent-2", "credential-absent", 200),
        ("ev-rejected", "authentication", 300),
    ] {
        ledger.record(
            &finished_event(
                id,
                "hook-shadow",
                DecisionKind::Unavailable,
                ExposureState::Generated,
                reason,
                50,
                BASE_MS + at,
            ),
            &[],
        );
    }
    // An unfinished row is not a failure and must not appear as a cause.
    ledger.record(
        &in_flight_event("ev-inflight", "hook-shadow", BASE_MS + 400),
        &[],
    );

    let report = ledger.stats();
    assert_eq!(report.turns.operational_failures, 3);
    let causes: Vec<(&str, u64)> = report
        .turns
        .failure_causes
        .iter()
        .map(|cause| (cause.reason.as_str(), cause.count))
        .collect();
    assert_eq!(causes, [("credential-absent", 2), ("authentication", 1)]);
    assert_eq!(
        causes.iter().map(|(_, count)| count).sum::<u64>(),
        report.turns.operational_failures,
        "causes partition the operational failures"
    );
}

#[test]
fn an_unfinished_turns_zero_duration_stays_out_of_the_latency_summary() {
    let mut ledger = Ledger::new("latency");
    // Two real durations, and one row whose elapsed_ms is a placeholder rather than
    // a measurement. Admitting the placeholder drags every statistic toward zero and
    // makes the product look faster than it is.
    ledger.record(
        &finished_event(
            "ev-fast",
            "cli",
            DecisionKind::Ranked,
            ExposureState::Emitted,
            "ranked",
            400,
            BASE_MS + 100,
        ),
        &[],
    );
    ledger.record(
        &finished_event(
            "ev-slow",
            "cli",
            DecisionKind::Ranked,
            ExposureState::Emitted,
            "ranked",
            600,
            BASE_MS + 200,
        ),
        &[],
    );
    ledger.record(&in_flight_event("ev-inflight", "cli", BASE_MS + 300), &[]);

    let report = ledger.stats();

    assert_eq!(
        report.latency.min_ms, 400,
        "a placeholder duration became the fastest observed turn: {:#?}",
        report.latency
    );
    assert_eq!(
        report.latency.mean_ms, 500,
        "the mean was computed over a placeholder: {:#?}",
        report.latency
    );
    assert_eq!(report.latency.max_ms, 600, "{:#?}", report.latency);
    // A summary drawn from a subset has to say which subset, or a reader cannot tell a
    // quiet window from a window full of turns that never finished.
    assert_eq!(
        report.latency.excluded_unfinished, 1,
        "the summary did not disclose the turn it left out: {:#?}",
        report.latency
    );
}

#[test]
fn a_turn_that_died_before_sending_is_not_a_cache_hit() {
    let mut ledger = Ledger::new("cache");
    // A turn that paid, a turn that genuinely reused a stored response, and a turn
    // that died before it could admit an attempt. The last two are indistinguishable
    // by attempt count alone, which is exactly why the in-flight marker exists.
    ledger.record(
        &finished_event(
            "ev-paid",
            "cli",
            DecisionKind::Ranked,
            ExposureState::Emitted,
            "ranked",
            500,
            BASE_MS + 100,
        ),
        &[completed_attempt("att-1", "ev-paid", BASE_MS + 100)],
    );
    ledger.record(
        &finished_event(
            "ev-reused",
            "cli",
            DecisionKind::Ranked,
            ExposureState::Emitted,
            "ranked",
            12,
            BASE_MS + 200,
        ),
        &[],
    );
    ledger.record(&in_flight_event("ev-inflight", "cli", BASE_MS + 300), &[]);

    let report = ledger.stats();

    assert_eq!(
        report.provider.cache_served_events, 1,
        "a turn that died before sending was reported as served from cache: {:#?}",
        report.provider
    );
    assert_eq!(report.provider.total_attempts, 1);
    assert_eq!(report.provider.completed_attempts, 1);
    // One of the two turns that could have been served from a stored response was.
    // The unfinished turn belongs in neither half of that ratio: it never reached the
    // point of being served or not served.
    let rate = report
        .provider
        .cache_hit_rate
        .expect("a rate over finished turns");
    assert!(
        (rate - 0.5).abs() < f64::EPSILON,
        "the rate counted an unfinished turn in its denominator: {rate} from {:#?}",
        report.provider
    );
}

#[test]
fn a_finished_invocation_replaces_its_in_flight_row_and_is_counted_once() {
    let mut ledger = Ledger::new("completion");
    // The ordinary path: the same event id is recorded twice, once before sending and
    // once on completion. The second write overwrites the first, so the turn must
    // appear exactly once, as a delivery, with its real duration — and nothing may
    // remain in the unfinished cohort.
    ledger.record(&in_flight_event("ev-one", "cli", BASE_MS + 100), &[]);
    ledger.record(
        &finished_event(
            "ev-one",
            "cli",
            DecisionKind::Ranked,
            ExposureState::Emitted,
            "ranked",
            850,
            BASE_MS + 100,
        ),
        &[completed_attempt("att-1", "ev-one", BASE_MS + 100)],
    );

    let report = ledger.stats();

    assert_eq!(report.turns.total_evaluated, 1, "{:#?}", report.turns);
    assert_eq!(report.turns.emitted_suggestions, 1, "{:#?}", report.turns);
    assert_eq!(report.turns.operational_failures, 0, "{:#?}", report.turns);
    assert_eq!(
        report.latency.min_ms, 850,
        "the completed turn lost its real duration: {:#?}",
        report.latency
    );
    assert_eq!(report.latency.max_ms, 850, "{:#?}", report.latency);
    assert_eq!(
        report.provider.cache_served_events, 0,
        "{:#?}",
        report.provider
    );
    // The marker is gone once the invocation reports back, so nothing lingers in the
    // unfinished cohort and nothing was dropped from the latency sample.
    assert_eq!(report.turns.in_flight_or_killed, 0, "{:#?}", report.turns);
    assert_eq!(
        report.latency.excluded_unfinished, 0,
        "{:#?}",
        report.latency
    );
}

#[test]
fn the_reports_window_ends_at_wall_clock_time_not_at_a_monotonic_counter() {
    // The window's upper bound has to be a Unix timestamp. Taking it from the monotonic
    // entry clock instead puts it a few milliseconds after process start, below every
    // real record, so the whole report comes back as zeros for every ledger and says
    // nothing about why. That failure is invisible from the output alone — an empty
    // report looks exactly like a quiet week — so it is pinned here rather than left to
    // be noticed by someone wondering why their statistics are blank.
    let mut ledger = Ledger::new("window");
    ledger.record(
        &finished_event(
            "ev-recent",
            "cli",
            DecisionKind::Ranked,
            ExposureState::Emitted,
            "ranked",
            300,
            BASE_MS + 100,
        ),
        &[],
    );

    let report = ledger.stats();

    assert!(
        report.as_of_unix_ms >= BASE_MS as i64,
        "the window ended at {}, which is not wall-clock time; a monotonic counter \
         would make every report empty",
        report.as_of_unix_ms
    );
    assert_eq!(
        report.turns.total_evaluated, 1,
        "a recorded turn fell outside the window the report chose for itself"
    );
}

#[test]
fn a_second_label_on_one_turn_does_not_double_its_recorded_cost() {
    // Cost per useful suggestion joins attempts to judgments through the event they
    // share. A turn can carry more than one judgment — two reviewers, or a revised
    // label — and each extra judgment multiplies that turn's attempts through the
    // join, inflating the tokens attributed to the cohort without a single extra
    // token having been spent. The cost of a turn is a property of the turn, not of
    // how many times it was labelled.
    let mut ledger = Ledger::new("cost");
    ledger.record(
        &finished_event(
            "ev-judged",
            "cli",
            DecisionKind::Ranked,
            ExposureState::Emitted,
            "ranked",
            500,
            BASE_MS + 100,
        ),
        &[
            completed_attempt("att-1", "ev-judged", BASE_MS + 100),
            completed_attempt("att-2", "ev-judged", BASE_MS + 110),
        ],
    );
    // Two labels on the one turn, only one of them useful.
    ledger.judge("j-useful", "ev-judged", JudgmentLabel::Useful);
    ledger.judge("j-neutral", "ev-judged", JudgmentLabel::Neutral);

    let report = ledger.stats();

    assert_eq!(report.judgments.useful, 1, "{:#?}", report.judgments);
    assert_eq!(
        report.judgments.total_judgments, 2,
        "{:#?}",
        report.judgments
    );
    assert_eq!(
        report.judgments.distinct_judged_events, 1,
        "{:#?}",
        report.judgments
    );
    assert_eq!(report.provider.total_attempts, 2, "{:#?}", report.provider);

    // Two attempts of 100 known tokens each were spent on the one useful label, so the
    // honest figure is 200. Multiplying by the two labels would report 400.
    let tokens = report
        .provider
        .tokens_per_useful_suggestion
        .expect("a token figure for a cohort with a useful label");
    assert!(
        (tokens - 200.0).abs() < f64::EPSILON,
        "the second label multiplied the cohort's cost: {tokens} from {:#?}",
        report.provider
    );
}

#[test]
fn a_turn_that_never_consulted_the_provider_is_not_a_cache_hit() {
    // `cache_served_events` infers reuse from the absence of attempt rows. A turn that abstained
    // before the provider was ever consulted also has no attempts, so the inference credits the
    // cache for work it never did. The comment in query_value_stats claims "only finished turns
    // are eligible to be served at all", which is the wrong test: eligibility requires having
    // reached the provider stage, not merely having finished.
    let mut ledger = Ledger::new("noprovider");
    ledger.record(
        &finished_event(
            "ev-paid",
            "cli",
            DecisionKind::Ranked,
            ExposureState::Emitted,
            "ranked",
            500,
            BASE_MS + 100,
        ),
        &[completed_attempt("att-1", "ev-paid", BASE_MS + 100)],
    );
    for (id, offset) in [("ev-abstain-1", 200u64), ("ev-abstain-2", 300)] {
        ledger.record(
            &finished_event(
                id,
                "cli",
                DecisionKind::Abstain,
                ExposureState::Generated,
                "abstain",
                40,
                BASE_MS + offset,
            ),
            &[],
        );
    }

    let report = ledger.stats();
    assert_eq!(
        report.provider.cache_served_events, 0,
        "two turns that never consulted the provider were reported as served from cache: {:#?}",
        report.provider
    );
}

#[test]
fn several_labels_on_one_turn_do_not_divide_its_attempts_away() {
    // Three skills judged useful on one emission. The numerator counts attempts once per turn —
    // that was the point of the duplicate-join fix — while the denominator counts labels, so the
    // two halves of the ratio measure different things and one turn's single attempt is divided
    // by three. Rounded, it prints as "0 attempts", next to a nonzero token figure.
    let mut ledger = Ledger::new("manylabels");
    ledger.record(
        &finished_event(
            "ev-1",
            "cli",
            DecisionKind::Ranked,
            ExposureState::Emitted,
            "ranked",
            500,
            BASE_MS + 100,
        ),
        &[completed_attempt("att-1", "ev-1", BASE_MS + 100)],
    );
    for (n, skill) in [(1, "review"), (2, "test_runner"), (3, "deploy")] {
        ledger.judge(&format!("j-{n}"), "ev-1", JudgmentLabel::Useful);
        let _ = skill;
    }

    let report = ledger.stats();
    assert_eq!(report.judgments.useful, 3, "{:#?}", report.judgments);
    assert_eq!(report.provider.total_attempts, 1, "{:#?}", report.provider);
    let text = &report.provider.cost_per_useful_suggestion;
    assert!(
        !text.contains("0 attempts"),
        "one real attempt was rounded away to zero while a token figure stood beside it: {text}"
    );
}

/// A run that failed at its work deadline can still record that it failed (sr-73b6).
///
/// Every ledger write used to be admitted against the work deadline, so a timeout had no time
/// left to record itself and vanished from the availability denominator. The finalization clock
/// gives that one write half of the cleanup reserve. The window here is wide (work ends at
/// 200 ms, finalization at 1,100 ms) so the ordering holds on a loaded host.
#[test]
fn a_failure_past_the_work_deadline_is_still_recorded_within_the_cleanup_reserve() {
    use skillranker::limits::DurationMillis;
    use skillranker::runtime::EntryClock;
    let dir = temp_private_dir("finalization");
    let location = LedgerLocation::Directory(dir);
    {
        let (invocation, cx) = test_invocation();
        init_ledger(&invocation, &cx, location.clone()).expect("init ledger");
        let _ = invocation.shutdown();
    }
    let clock = EntryClock::capture_with(
        DurationMillis::new("total", 2_000, 10_000).unwrap(),
        DurationMillis::new("cleanup", 1_800, 10_000).unwrap(),
    )
    .unwrap();
    let finalization = clock.for_failure_finalization();
    assert_eq!(finalization.deadline().cleanup_reserve().as_millis(), 900);
    assert_eq!(
        finalization.deadline().expires_at(),
        clock.deadline().expires_at(),
        "finalization never extends the invocation"
    );
    let invocation = ProcessInvocation::from_clock(clock).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(
        clock.admit_new_work().is_err(),
        "the work deadline has passed"
    );
    let event = finished_event(
        "ev-timeout",
        "hook-shadow",
        DecisionKind::Unavailable,
        ExposureState::Prepared,
        "timeout",
        300,
        BASE_MS + 100,
    );
    let cleanup = invocation.request_cleanup_cx();
    let refused = record_ranking_with_attempts(
        &invocation,
        clock,
        &cleanup,
        LedgerAccess::ExistingOnly,
        location.clone(),
        &event,
        &[],
        None,
        &[],
    );
    assert!(
        !matches!(refused, Ok(true)),
        "the invocation's own clock refuses work past its deadline: {refused:?}"
    );
    let recorded = record_ranking_with_attempts(
        &invocation,
        finalization,
        &cleanup,
        LedgerAccess::ExistingOnly,
        location.clone(),
        &event,
        &[],
        None,
        &[],
    );
    assert!(
        matches!(recorded, Ok(true)),
        "the finalization clock records the failure: {recorded:?}"
    );
    let _ = invocation.shutdown_within(std::time::Duration::from_secs(1));

    let (reader, cx) = test_invocation();
    let report = ledger_stats(&reader, &cx, location, BASE_MS as i64 - 1, false).unwrap();
    assert_eq!(report.turns.operational_failures, 1, "{:#?}", report.turns);
    assert_eq!(report.turns.failure_causes[0].reason, "timeout");
    let _ = reader.shutdown();
}
