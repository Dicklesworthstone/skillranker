//! Contract and verification tests for `sr stats` value report,
//! operational metrics, observation attribution, provider attempt accounting,
//! and honest cohort disclosure.
//!
//! Required by sr-roadmap-l1i.6.13:
//! - Evaluated turns, emitted suggestions, valid abstentions, muted/suppressed output, operational failures.
//! - Latency summary: mean, median, p95, min, max.
//! - Distinct denominators across channels (cli, shadow, advisory, tui).
//! - Observations: observed loads, attempted loads, censored observations, attributed loads, unattributed loads.
//! - Suggestion adoption rate and observation coverage with honest caveats (adoption is not task success).
//! - Judged cohort: useful, harmful, neutral, distinct judged events, label coverage rate, useful ratio.
//! - Provider metrics: total attempts, completed/failed/unknown attempts, known tokens, unknown usage attempts.
//! - Cache-served events and cache hit rate.
//! - Cost per useful suggestion across judged cohort tokens/attempts; reports "not estimable" when useful == 0.
//! - Per-skill breakdown (--by-skill): top-1 recommendations, shortlist appearances, loads, judgments.
//! - Time window filtering (--since 7d, 24h, 30m, ISO timestamp).
//! - CLI output: JSON and human-readable table outputs.

use asupersync::Cx;
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
        "sr-stats-{}-{}-{}",
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
        total_candidates: 3,
        eligible_candidates: 3,
        membership_coverage: MembershipCoverage::Complete,
        members_json: serde_json::json!([
            {
                "skill_id": "review", "invocation_name": "review",
                "content_hash": "hash-1", "source": "workspace",
                "eligible": true, "exclusion_reason": null
            },
            {
                "skill_id": "test_runner", "invocation_name": "test_runner",
                "content_hash": "hash-2", "source": "workspace",
                "eligible": true, "exclusion_reason": null
            },
            {
                "skill_id": "deploy", "invocation_name": "deploy",
                "content_hash": "hash-3", "source": "workspace",
                "eligible": true, "exclusion_reason": null
            }
        ])
        .to_string(),
        created_at_unix_ms,
    }
}

fn event_fixture(
    event_id: &str,
    channel: &str,
    decision: DecisionKind,
    exposure: ExposureState,
    elapsed_ms: u64,
    created_at_unix_ms: u64,
) -> NewRankingEvent {
    NewRankingEvent {
        event_id: event_id.into(),
        verified_delivery_key: Some(format!("deliv-{}", event_id)),
        workspace_root: "/data/workspace".into(),
        session_id: "sess-stats".into(),
        agent_branch: "main".into(),
        mode_channel: channel.into(),
        policy_version: "v1".into(),
        schema_version: 1,
        decision,
        reason: "eval".into(),
        exposure_state: exposure,
        elapsed_ms,
        created_at_unix_ms,
        input_tokens: Some(100),
        output_tokens: Some(50),
        snapshot_id: Some("snap-1".into()),
    }
}

fn candidate_fixture(
    event_id: &str,
    skill_id: &str,
    stage: CandidateStage,
    rank_pos: Option<u32>,
) -> NewRankingCandidate {
    NewRankingCandidate {
        event_id: event_id.into(),
        stage,
        skill_id: skill_id.into(),
        skill_version: "1.0.0".into(),
        raw_probability: Some(0.85),
        normalized_probability: Some(0.85),
        fit_score: Some(0.9),
        rank_score: Some(0.88),
        rank_position: rank_pos,
        excluded: false,
        exclusion_reason: None,
    }
}

fn attempt_fixture(
    attempt_id: &str,
    event_id: &str,
    in_tokens: Option<u64>,
    out_tokens: Option<u64>,
    status: AttemptStatus,
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
        status,
        input_tokens: in_tokens,
        output_tokens: out_tokens,
        http_status: Some(200),
        error_kind: None,
    }
}

fn observation_fixture(
    obs_id: &str,
    skill_id: &str,
    evidence: EvidenceState,
    attr_event: Option<&str>,
    observed_at_unix_ms: u64,
) -> NewObservation {
    NewObservation {
        observation_id: obs_id.into(),
        source_event_key: format!("src-{}", obs_id),
        workspace_root: "/data/workspace".into(),
        session_id: "sess-stats".into(),
        agent_branch: "main".into(),
        attributed_event_id: attr_event.map(String::from),
        skill_id: skill_id.into(),
        evidence_state: evidence,
        observed_at_unix_ms,
    }
}

fn judgment_fixture(
    judgment_id: &str,
    event_id: &str,
    skill_id: &str,
    label: JudgmentLabel,
    created_at_unix_ms: u64,
) -> NewJudgment {
    NewJudgment {
        judgment_id: judgment_id.into(),
        attributed_event_id: event_id.into(),
        skill_id: skill_id.into(),
        label,
        label_version: 1,
        provenance: "contract_test".into(),
        created_at_unix_ms,
    }
}

#[test]
fn empty_ledger_stats_reports_clean_zeros_and_not_estimable() {
    let dir = temp_private_dir("empty");
    let (inv, cx) = test_invocation();
    let location = LedgerLocation::Directory(dir.clone());

    let _init = init_ledger(&inv, &cx, location.clone()).expect("init ledger");
    let report = ledger_stats(&inv, &cx, location, 0, false).expect("ledger_stats");

    assert_eq!(report.turns.total_evaluated, 0);
    assert_eq!(report.turns.emitted_suggestions, 0);
    assert_eq!(report.turns.valid_abstentions, 0);
    assert_eq!(report.turns.muted_or_suppressed, 0);
    assert_eq!(report.turns.operational_failures, 0);
    assert_eq!(report.turns.explicit_requirements, 0);
    assert!(report.turns.by_channel.is_empty());

    assert_eq!(report.latency.mean_ms, 0);
    assert_eq!(report.latency.median_ms, 0);
    assert_eq!(report.latency.p95_ms, 0);
    assert_eq!(report.latency.min_ms, 0);
    assert_eq!(report.latency.max_ms, 0);

    assert_eq!(report.observations.total_observations, 0);
    assert_eq!(report.observations.observed_loads, 0);
    assert_eq!(report.observations.attempted_loads, 0);
    assert_eq!(report.observations.censored_observations, 0);
    assert_eq!(report.observations.attributed_loads, 0);
    assert_eq!(report.observations.unattributed_loads, 0);
    assert!(report.observations.observation_coverage.is_none());
    assert!(report.observations.suggestion_adoption_rate.is_none());
    assert!(!report.observations.caveat.is_empty());

    assert_eq!(report.judgments.total_judgments, 0);
    assert_eq!(report.judgments.useful, 0);
    assert_eq!(report.judgments.harmful, 0);
    assert_eq!(report.judgments.neutral, 0);
    assert_eq!(report.judgments.distinct_judged_events, 0);
    assert!(report.judgments.label_coverage_rate.is_none());
    assert!(report.judgments.useful_ratio_in_judged.is_none());

    assert_eq!(report.provider.total_attempts, 0);
    assert_eq!(report.provider.completed_attempts, 0);
    assert_eq!(report.provider.failed_attempts, 0);
    assert_eq!(report.provider.known_total_tokens, 0);
    assert_eq!(report.provider.unknown_usage_attempts, 0);
    assert_eq!(report.provider.cache_served_events, 0);
    assert!(report.provider.cache_hit_rate.is_none());
    assert_eq!(
        report.provider.cost_per_useful_suggestion,
        "not estimable (0 useful labels in judged cohort)"
    );
    assert!(report.provider.tokens_per_useful_suggestion.is_none());
    assert!(report.by_skill.is_none());
}

#[test]
fn mixed_cohorts_and_channels_preserve_distinct_denominators() {
    let dir = temp_private_dir("channels");
    let (inv, cx) = test_invocation();
    let location = LedgerLocation::Directory(dir.clone());
    let _init = init_ledger(&inv, &cx, location.clone()).expect("init ledger");

    let open = open_ledger(&inv, &cx, LedgerAccess::ExistingOnly, location.clone())
        .expect("open ledger");
    let store = match open {
        LedgerOpen::Ready(s) => s,
        _ => panic!("expected ready store"),
    };

    let base_time = 1_000_000u64;
    store.record_roster_snapshot(&snapshot_fixture("snap-1", base_time)).expect("snap");

    // 1. CLI emitted suggestion
    let ev1 = event_fixture("ev-cli-1", "cli", DecisionKind::Ranked, ExposureState::Emitted, 50, base_time + 100);
    store.record_ranking_event_with_attempts(&ev1, &[], &[]).expect("ev1");

    // 2. CLI valid abstention
    let ev2 = event_fixture("ev-cli-2", "cli", DecisionKind::Abstain, ExposureState::Muted, 20, base_time + 200);
    store.record_ranking_event_with_attempts(&ev2, &[], &[]).expect("ev2");

    // 3. Shadow evaluated turn (muted)
    let ev3 = event_fixture("ev-sh-1", "shadow", DecisionKind::Ranked, ExposureState::Muted, 40, base_time + 300);
    store.record_ranking_event_with_attempts(&ev3, &[], &[]).expect("ev3");

    // 4. Advisory hook operational failure (unavailable)
    let ev4 = event_fixture("ev-adv-1", "advisory", DecisionKind::Unavailable, ExposureState::Muted, 10, base_time + 400);
    store.record_ranking_event_with_attempts(&ev4, &[], &[]).expect("ev4");

    // 5. CLI explicit requirement
    let ev5 = event_fixture("ev-cli-3", "cli", DecisionKind::Explicit, ExposureState::Emitted, 5, base_time + 500);
    store.record_ranking_event_with_attempts(&ev5, &[], &[]).expect("ev5");

    drop(store);

    let report = ledger_stats(&inv, &cx, location, base_time as i64, false).expect("ledger_stats");

    assert_eq!(report.turns.total_evaluated, 5);
    assert_eq!(report.turns.emitted_suggestions, 2); // ev1 and ev5
    assert_eq!(report.turns.valid_abstentions, 1);   // ev2
    assert_eq!(report.turns.muted_or_suppressed, 1); // ev3 (shadow)
    assert_eq!(report.turns.operational_failures, 1);// ev4 (unavailable)
    assert_eq!(report.turns.explicit_requirements, 1);// ev5

    // Distinct channel denominators
    assert_eq!(report.turns.by_channel.len(), 3); // cli, shadow, advisory
    let cli = report.turns.by_channel.iter().find(|c| c.channel == "cli").expect("cli channel");
    assert_eq!(cli.evaluated_turns, 3);
    assert_eq!(cli.emitted, 2);
    assert_eq!(cli.abstain, 1);
    assert_eq!(cli.muted, 0);

    let shadow = report.turns.by_channel.iter().find(|c| c.channel == "shadow").expect("shadow channel");
    assert_eq!(shadow.evaluated_turns, 1);
    assert_eq!(shadow.emitted, 0);
    assert_eq!(shadow.muted, 1);

    let advisory = report.turns.by_channel.iter().find(|c| c.channel == "advisory").expect("advisory channel");
    assert_eq!(advisory.evaluated_turns, 1);
    assert_eq!(advisory.unavailable, 1);
}

#[test]
fn latency_summary_computes_accurate_percentiles() {
    let dir = temp_private_dir("latency");
    let (inv, cx) = test_invocation();
    let location = LedgerLocation::Directory(dir.clone());
    let _init = init_ledger(&inv, &cx, location.clone()).expect("init ledger");

    let open = open_ledger(&inv, &cx, LedgerAccess::ExistingOnly, location.clone())
        .expect("open ledger");
    let store = match open {
        LedgerOpen::Ready(s) => s,
        _ => panic!("expected ready store"),
    };

    let base_time = 1_000_000u64;
    store.record_roster_snapshot(&snapshot_fixture("snap-1", base_time)).expect("snap");

    // Insert 5 events with latencies: 10, 20, 30, 40, 100
    let latencies = [10, 20, 30, 40, 100];
    for (i, &lat) in latencies.iter().enumerate() {
        let ev = event_fixture(
            &format!("ev-lat-{}", i),
            "cli",
            DecisionKind::Ranked,
            ExposureState::Emitted,
            lat,
            base_time + (i as u64 * 100),
        );
        store.record_ranking_event_with_attempts(&ev, &[], &[]).expect("ev");
    }

    drop(store);

    let report = ledger_stats(&inv, &cx, location, base_time as i64, false).expect("ledger_stats");
    assert_eq!(report.latency.min_ms, 10);
    assert_eq!(report.latency.max_ms, 100);
    assert_eq!(report.latency.median_ms, 30);
    assert_eq!(report.latency.mean_ms, 40); // (10+20+30+40+100) / 5 = 40
    assert_eq!(report.latency.p95_ms, 100);
}

#[test]
fn provider_metrics_tracks_tokens_and_cache_served_events() {
    let dir = temp_private_dir("provider");
    let (inv, cx) = test_invocation();
    let location = LedgerLocation::Directory(dir.clone());
    let _init = init_ledger(&inv, &cx, location.clone()).expect("init ledger");

    let open = open_ledger(&inv, &cx, LedgerAccess::ExistingOnly, location.clone())
        .expect("open ledger");
    let store = match open {
        LedgerOpen::Ready(s) => s,
        _ => panic!("expected ready store"),
    };

    let base_time = 1_000_000u64;
    store.record_roster_snapshot(&snapshot_fixture("snap-1", base_time)).expect("snap");

    // Event 1: has 2 attempts (one completed with tokens, one failed)
    let ev1 = event_fixture("ev-p1", "cli", DecisionKind::Ranked, ExposureState::Emitted, 50, base_time + 100);
    let att1 = attempt_fixture("att-1", "ev-p1", Some(120), Some(80), AttemptStatus::Completed, base_time + 100);
    let att2 = attempt_fixture("att-2", "ev-p1", None, None, AttemptStatus::Failed, base_time + 110);
    store.record_ranking_event_with_attempts(&ev1, &[], &[att1, att2]).expect("ev1");

    // Event 2: has 1 attempt with unknown usage (status completed, but tokens None)
    let ev2 = event_fixture("ev-p2", "cli", DecisionKind::Ranked, ExposureState::Emitted, 30, base_time + 200);
    let att3 = attempt_fixture("att-3", "ev-p2", None, None, AttemptStatus::Unknown, base_time + 200);
    store.record_ranking_event_with_attempts(&ev2, &[], &[att3]).expect("ev2");

    // Event 3: cache-served event (0 attempts)
    let ev3 = event_fixture("ev-p3", "cli", DecisionKind::Ranked, ExposureState::Emitted, 2, base_time + 300);
    store.record_ranking_event_with_attempts(&ev3, &[], &[]).expect("ev3");

    drop(store);

    let report = ledger_stats(&inv, &cx, location, base_time as i64, false).expect("ledger_stats");

    assert_eq!(report.provider.total_attempts, 3);
    assert_eq!(report.provider.completed_attempts, 1);
    assert_eq!(report.provider.failed_attempts, 1);
    assert_eq!(report.provider.unknown_attempts, 1);
    assert_eq!(report.provider.known_input_tokens, 120);
    assert_eq!(report.provider.known_output_tokens, 80);
    assert_eq!(report.provider.known_total_tokens, 200);
    assert_eq!(report.provider.unknown_usage_attempts, 2); // att2 and att3 have NULL tokens
    assert_eq!(report.provider.cache_served_events, 1);    // ev3
    // Cache hit rate = 1 cache-served / 3 total evaluated = 0.333...
    let hit_rate = report.provider.cache_hit_rate.expect("cache_hit_rate");
    assert!((hit_rate - 1.0 / 3.0).abs() < 0.001);
}

#[test]
fn observations_and_adoption_rate_tracked_with_caveats() {
    let dir = temp_private_dir("obs");
    let (inv, cx) = test_invocation();
    let location = LedgerLocation::Directory(dir.clone());
    let _init = init_ledger(&inv, &cx, location.clone()).expect("init ledger");

    let open = open_ledger(&inv, &cx, LedgerAccess::ExistingOnly, location.clone())
        .expect("open ledger");
    let store = match open {
        LedgerOpen::Ready(s) => s,
        _ => panic!("expected ready store"),
    };

    let base_time = 1_000_000u64;
    store.record_roster_snapshot(&snapshot_fixture("snap-1", base_time)).expect("snap");

    // 2 emitted events
    let ev1 = event_fixture("ev-o1", "cli", DecisionKind::Ranked, ExposureState::Emitted, 50, base_time + 100);
    let cand1 = candidate_fixture("ev-o1", "review", CandidateStage::Rerank, Some(1));
    store.record_ranking_event_with_attempts(&ev1, &[cand1], &[]).expect("ev1");

    let ev2 = event_fixture("ev-o2", "cli", DecisionKind::Ranked, ExposureState::Emitted, 50, base_time + 200);
    let cand2 = candidate_fixture("ev-o2", "test_runner", CandidateStage::Rerank, Some(1));
    store.record_ranking_event_with_attempts(&ev2, &[cand2], &[]).expect("ev2");

    // Observations:
    // 1 attributed load to ev1
    let o1 = observation_fixture("obs-1", "review", EvidenceState::Loaded, Some("ev-o1"), base_time + 150);
    // 1 unattributed load
    let o2 = observation_fixture("obs-2", "deploy", EvidenceState::Loaded, None, base_time + 160);
    // 1 attempted load
    let o3 = observation_fixture("obs-3", "review", EvidenceState::Attempted, None, base_time + 170);
    // 1 censored observation
    let o4 = observation_fixture("obs-4", "review", EvidenceState::Censored, None, base_time + 180);

    store.record_observations(&[o1, o2, o3, o4]).expect("record observations");

    drop(store);

    let report = ledger_stats(&inv, &cx, location, base_time as i64, false).expect("ledger_stats");

    assert_eq!(report.observations.total_observations, 4);
    assert_eq!(report.observations.observed_loads, 2);
    assert_eq!(report.observations.attempted_loads, 1);
    assert_eq!(report.observations.censored_observations, 1);
    assert_eq!(report.observations.attributed_loads, 1);
    assert_eq!(report.observations.unattributed_loads, 1);

    // Adoption rate = 1 attributed load / 2 emitted suggestions = 0.5
    assert_eq!(report.observations.suggestion_adoption_rate, Some(0.5));
    // Observation coverage = 1 loaded with known attribution / 2 total loaded = 0.5
    assert_eq!(report.observations.observation_coverage, Some(0.5));
    assert!(report.observations.caveat.contains("adoption is not task success"));
}

#[test]
fn judgments_and_cost_per_useful_suggestion_calculated() {
    let dir = temp_private_dir("judgments");
    let (inv, cx) = test_invocation();
    let location = LedgerLocation::Directory(dir.clone());
    let _init = init_ledger(&inv, &cx, location.clone()).expect("init ledger");

    let open = open_ledger(&inv, &cx, LedgerAccess::ExistingOnly, location.clone())
        .expect("open ledger");
    let store = match open {
        LedgerOpen::Ready(s) => s,
        _ => panic!("expected ready store"),
    };

    let base_time = 1_000_000u64;
    store.record_roster_snapshot(&snapshot_fixture("snap-1", base_time)).expect("snap");

    // Event 1: 1 attempt with 500 input, 500 output tokens (= 1000 tokens)
    let ev1 = event_fixture("ev-j1", "cli", DecisionKind::Ranked, ExposureState::Emitted, 50, base_time + 100);
    let att1 = attempt_fixture("att-j1", "ev-j1", Some(500), Some(500), AttemptStatus::Completed, base_time + 100);
    store.record_ranking_event_with_attempts(&ev1, &[], &[att1]).expect("ev1");

    // Event 2: 1 attempt with 200 input, 200 output tokens (= 400 tokens)
    let ev2 = event_fixture("ev-j2", "cli", DecisionKind::Ranked, ExposureState::Emitted, 50, base_time + 200);
    let att2 = attempt_fixture("att-j2", "ev-j2", Some(200), Some(200), AttemptStatus::Completed, base_time + 200);
    store.record_ranking_event_with_attempts(&ev2, &[], &[att2]).expect("ev2");

    // Event 3 (unlabeled): 1 attempt with 3000 tokens
    let ev3 = event_fixture("ev-j3", "cli", DecisionKind::Ranked, ExposureState::Emitted, 50, base_time + 300);
    let att3 = attempt_fixture("att-j3", "ev-j3", Some(1500), Some(1500), AttemptStatus::Completed, base_time + 300);
    store.record_ranking_event_with_attempts(&ev3, &[], &[att3]).expect("ev3");

    // Judgments:
    // ev1 labeled useful
    let j1 = judgment_fixture("jdg-1", "ev-j1", "review", JudgmentLabel::Useful, base_time + 400);
    // ev2 labeled harmful
    let j2 = judgment_fixture("jdg-2", "ev-j2", "test_runner", JudgmentLabel::Harmful, base_time + 410);

    store.record_judgment(&j1).expect("j1");
    store.record_judgment(&j2).expect("j2");

    drop(store);

    let report = ledger_stats(&inv, &cx, location, base_time as i64, false).expect("ledger_stats");

    assert_eq!(report.judgments.total_judgments, 2);
    assert_eq!(report.judgments.useful, 1);
    assert_eq!(report.judgments.harmful, 1);
    assert_eq!(report.judgments.neutral, 0);
    assert_eq!(report.judgments.distinct_judged_events, 2);
    // Label coverage = 2 distinct judged / 3 emitted = 0.666...
    let cov = report.judgments.label_coverage_rate.expect("label coverage");
    assert!((cov - 2.0 / 3.0).abs() < 0.001);
    // Useful ratio in judged = 1 useful / 2 judgments = 0.5
    assert_eq!(report.judgments.useful_ratio_in_judged, Some(0.5));

    // Judged cohort tokens: ev1 (1000) + ev2 (400) = 1400 tokens (att3 from ev3 must NOT leak into judged cohort!)
    // Judged cohort attempts: 2 attempts
    // Useful: 1
    // Cost per useful suggestion = 2 attempts / 1400 tokens per useful suggestion
    assert!(report.provider.cost_per_useful_suggestion.contains("2 attempts / 1400 tokens per useful suggestion"));
    assert_eq!(report.provider.tokens_per_useful_suggestion, Some(1400.0));
}

#[test]
fn by_skill_breakdown_orders_by_recommendations_and_shortlist() {
    let dir = temp_private_dir("by-skill");
    let (inv, cx) = test_invocation();
    let location = LedgerLocation::Directory(dir.clone());
    let _init = init_ledger(&inv, &cx, location.clone()).expect("init ledger");

    let open = open_ledger(&inv, &cx, LedgerAccess::ExistingOnly, location.clone())
        .expect("open ledger");
    let store = match open {
        LedgerOpen::Ready(s) => s,
        _ => panic!("expected ready store"),
    };

    let base_time = 1_000_000u64;
    store.record_roster_snapshot(&snapshot_fixture("snap-1", base_time)).expect("snap");

    // Event 1: skill "review" is Top 1, "test_runner" is Top 2
    let ev1 = event_fixture("ev-bs1", "cli", DecisionKind::Ranked, ExposureState::Emitted, 50, base_time + 100);
    let c1 = candidate_fixture("ev-bs1", "review", CandidateStage::Rerank, Some(1));
    let c2 = candidate_fixture("ev-bs1", "test_runner", CandidateStage::Rerank, Some(2));
    store.record_ranking_event_with_attempts(&ev1, &[c1, c2], &[]).expect("ev1");

    // Event 2: skill "review" is Top 1 again
    let ev2 = event_fixture("ev-bs2", "cli", DecisionKind::Ranked, ExposureState::Emitted, 50, base_time + 200);
    let c3 = candidate_fixture("ev-bs2", "review", CandidateStage::Rerank, Some(1));
    store.record_ranking_event_with_attempts(&ev2, &[c3], &[]).expect("ev2");

    // Observation: "review" loaded
    let o1 = observation_fixture("obs-bs1", "review", EvidenceState::Loaded, Some("ev-bs1"), base_time + 300);
    store.record_observations(&[o1]).expect("obs");

    // Judgment: "review" useful
    let j1 = judgment_fixture("jdg-bs1", "ev-bs1", "review", JudgmentLabel::Useful, base_time + 400);
    store.record_judgment(&j1).expect("j1");

    drop(store);

    let report = ledger_stats(&inv, &cx, location, base_time as i64, true).expect("ledger_stats");

    let skills = report.by_skill.expect("by_skill data");
    assert!(!skills.is_empty());

    // "review" should be first because it has 2 Top-1 recommendations
    let first = &skills[0];
    assert_eq!(first.skill_id, "review");
    assert_eq!(first.top1_recommendations, 2);
    assert_eq!(first.shortlist_appearances, 2);
    assert_eq!(first.observed_loads, 1);
    assert_eq!(first.attributed_loads, 1);
    assert_eq!(first.judged_useful, 1);

    // "test_runner" should be second with 0 Top-1, 1 Shortlist
    let second = &skills[1];
    assert_eq!(second.skill_id, "test_runner");
    assert_eq!(second.top1_recommendations, 0);
    assert_eq!(second.shortlist_appearances, 1);
}

#[test]
fn time_window_filtering_respects_since_duration() {
    let dir = temp_private_dir("since");
    let (inv, cx) = test_invocation();
    let location = LedgerLocation::Directory(dir.clone());
    let _init = init_ledger(&inv, &cx, location.clone()).expect("init ledger");

    let open = open_ledger(&inv, &cx, LedgerAccess::ExistingOnly, location.clone())
        .expect("open ledger");
    let store = match open {
        LedgerOpen::Ready(s) => s,
        _ => panic!("expected ready store"),
    };

    let base_time = 1_000_000u64;
    store.record_roster_snapshot(&snapshot_fixture("snap-1", base_time)).expect("snap");

    // Old event: created at 1_000_100
    let ev_old = event_fixture("ev-old", "cli", DecisionKind::Ranked, ExposureState::Emitted, 50, 1_000_100);
    store.record_ranking_event_with_attempts(&ev_old, &[], &[]).expect("old");

    // Newer event: created at 2_000_000
    let ev_new = event_fixture("ev-new", "cli", DecisionKind::Ranked, ExposureState::Emitted, 50, 2_000_000);
    store.record_ranking_event_with_attempts(&ev_new, &[], &[]).expect("new");

    drop(store);

    // Query with since = 1_500_000: only ev-new should be counted
    let report = ledger_stats(&inv, &cx, location, 1_500_000, false).expect("ledger_stats");
    assert_eq!(report.turns.total_evaluated, 1);
    assert_eq!(report.turns.emitted_suggestions, 1);
}

#[test]
fn cli_stats_json_and_table_output_and_missing_store_handled() {
    let dir = temp_private_dir("cli-stats");
    let (inv, cx) = test_invocation();
    let location = LedgerLocation::Directory(dir.clone());

    let sr_bin = env!("CARGO_BIN_EXE_sr");

    // 1. Missing store should fail cleanly with storage-failure exit code
    let output = Command::new(sr_bin)
        .arg("stats")
        .arg("--dir")
        .arg(dir.to_str().unwrap())
        .output()
        .expect("exec sr stats");
    assert_ne!(output.status.code(), Some(0));

    // 2. Init ledger
    let _init = init_ledger(&inv, &cx, location.clone()).expect("init");

    // 3. Stats table output
    let output_table = Command::new(sr_bin)
        .arg("stats")
        .arg("--dir")
        .arg(dir.to_str().unwrap())
        .arg("--table")
        .output()
        .expect("exec sr stats --table");
    assert_eq!(output_table.status.code(), Some(0));
    let table_str = String::from_utf8_lossy(&output_table.stdout);
    assert!(table_str.contains("SkillRanker Value & Operational Report"));
    assert!(table_str.contains("Total turns evaluated:"));

    // 4. Stats JSON output
    let output_json = Command::new(sr_bin)
        .arg("stats")
        .arg("--dir")
        .arg(dir.to_str().unwrap())
        .arg("--json")
        .output()
        .expect("exec sr stats --json");
    assert_eq!(output_json.status.code(), Some(0));
    let json_val: serde_json::Value =
        serde_json::from_slice(&output_json.stdout).expect("parse json");
    assert!(json_val.get("turns").is_some());
    assert!(json_val.get("provider").is_some());
    assert!(json_val.get("observations").is_some());
    assert!(json_val.get("judgments").is_some());

    // 5. Stats with --by-skill and --since
    let output_by_skill = Command::new(sr_bin)
        .arg("stats")
        .arg("--dir")
        .arg(dir.to_str().unwrap())
        .arg("--by-skill")
        .arg("--since")
        .arg("7d")
        .arg("--json")
        .output()
        .expect("exec sr stats --by-skill");
    assert_eq!(output_by_skill.status.code(), Some(0));
    let json_skill: serde_json::Value =
        serde_json::from_slice(&output_by_skill.stdout).expect("parse json skill");
    assert!(json_skill.get("by_skill").is_some());
}
