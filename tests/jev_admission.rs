//! Per-attempt provenance for ledger ownership (sr-roadmap-l1i.6.7).
//!
//! `provider_attempts` records one row per attempt, keyed by a unique attempt id
//! and owned by a ranking event, with admitted/sent/settled times, an outcome
//! and the tokens that attempt actually cost. `CostReceipt` only carries
//! invocation-wide counters, so it cannot answer "which attempt incurred this
//! spend" — the question the ledger exists to answer. These cases pin what the
//! admission seam now remembers per attempt, before any of it is written down.
//!
//! The rules that matter, and that a counter cannot express:
//! - absent usage is not zero usage: a sent attempt whose response never
//!   arrived keeps `usage: None` rather than claiming it cost nothing;
//! - an invocation served from cache admits nothing, so a single-flight
//!   follower has no attempt of its own and cannot appear to have paid;
//! - a discard before the wire is distinct from a failure on the wire.
use skillranker::jev::{
    AttemptAdmission, AttemptBudget, AttemptFailure, AttemptOutcome, CanonicalOrigin, RankingStage,
    Usage,
};
use skillranker::limits::DurationMillis;
use skillranker::runtime::EntryClock;

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

#[test]
fn a_completed_attempt_records_its_stage_ownership_and_exact_cost() {
    let mut admission = admission("inv-completed");
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

    let attempts: Vec<_> = admission.attempts().collect();
    assert_eq!(attempts.len(), 1);
    let attempt = attempts[0];
    assert_eq!(attempt.attempt_id, attempt_id);
    assert_eq!(attempt.stage, RankingStage::Wide);
    assert_eq!(attempt.outcome, AttemptOutcome::Completed);
    assert_eq!(attempt.outcome.as_str(), "completed");
    assert_eq!(
        attempt.usage,
        Some(Usage {
            input_tokens: 1_200,
            output_tokens: 340,
        })
    );
    // Times move forward through the attempt's own lifetime.
    let sent_at = attempt.sent_at.expect("a sent attempt has a send time");
    let settled_at = attempt.settled_at.expect("a completed attempt settles");
    assert!(attempt.admitted_at <= sent_at, "{attempt:?}");
    assert!(sent_at <= settled_at, "{attempt:?}");
}

#[test]
fn a_sent_attempt_that_never_answered_keeps_unknown_cost() {
    // Absent usage is not zero usage. Recording 0 tokens here would understate
    // real spend, since the request was already on the wire.
    let mut admission = admission("inv-unknown");
    let origin = endpoint();
    let permit = admission.admit(RankingStage::Wide, &origin).unwrap();
    let sent = permit.mark_sent().unwrap();
    admission.record_sent(&sent).unwrap();
    admission
        .record_terminal_failure(&sent, AttemptFailure::Indeterminate("transient-io"))
        .unwrap();

    let attempts: Vec<_> = admission.attempts().collect();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].outcome, AttemptOutcome::Failed);
    assert_eq!(attempts[0].outcome.as_str(), "failed");
    assert_eq!(attempts[0].usage, None, "{:?}", attempts[0]);
    assert!(attempts[0].sent_at.is_some());
    assert!(attempts[0].settled_at.is_some());
    // The invocation-wide receipt agrees that cost is partly unknown.
    assert!(admission.receipt().has_unknown_usage);
    assert_eq!(admission.receipt().unknown_usage_attempts, 1);
}

#[test]
fn a_discard_before_the_wire_is_not_a_failure_on_the_wire() {
    let mut admission = admission("inv-discarded");
    let origin = endpoint();
    let permit = admission.admit(RankingStage::Wide, &origin).unwrap();
    let discarded = permit.discard_before_send("policy withdrew consent");
    admission.record_discard(&discarded).unwrap();

    let attempts: Vec<_> = admission.attempts().collect();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].outcome, AttemptOutcome::Discarded);
    assert_eq!(attempts[0].outcome.as_str(), "discarded");
    assert_eq!(attempts[0].sent_at, None, "nothing reached the wire");
    assert_eq!(attempts[0].usage, None);
    assert_eq!(admission.receipt().sent_attempts, 0);
}

#[test]
fn an_admitted_attempt_still_in_flight_reports_admitted() {
    let mut admission = admission("inv-inflight");
    let origin = endpoint();
    let _permit = admission.admit(RankingStage::Wide, &origin).unwrap();
    let attempts: Vec<_> = admission.attempts().collect();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].outcome, AttemptOutcome::Admitted);
    assert_eq!(attempts[0].sent_at, None);
    assert_eq!(attempts[0].settled_at, None);
}

#[test]
fn both_stages_are_recorded_separately_with_unique_ids() {
    let mut admission = admission("inv-two-stages");
    let origin = endpoint();
    for stage in [RankingStage::Wide, RankingStage::Rerank] {
        let permit = admission.admit(stage, &origin).unwrap();
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
    let attempts: Vec<_> = admission.attempts().collect();
    assert_eq!(attempts.len(), 2);
    let stages: Vec<RankingStage> = attempts.iter().map(|a| a.stage).collect();
    assert!(stages.contains(&RankingStage::Wide) && stages.contains(&RankingStage::Rerank));
    assert_ne!(
        attempts[0].attempt_id, attempts[1].attempt_id,
        "an attempt id owns exactly one row"
    );
    assert!(
        attempts
            .iter()
            .all(|a| a.outcome == AttemptOutcome::Completed)
    );
}

#[test]
fn a_cache_served_invocation_owns_no_attempt() {
    // A single-flight follower reuses the owner's response. It must reference
    // that ownership and add no cost of its own, so it has no attempt to record.
    let mut admission = admission("inv-follower");
    admission.record_cache_hit();
    assert_eq!(admission.attempts().count(), 0);
    assert_eq!(admission.receipt().admitted_attempts, 0);
    assert_eq!(admission.receipt().sent_attempts, 0);
    assert!(admission.receipt().cache_served);
}
