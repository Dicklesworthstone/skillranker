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

    let open =
        open_ledger(&inv, &cx, LedgerAccess::ExistingOnly, location.clone()).expect("open ledger");
    let mut store = match open {
        LedgerOpen::Ready(s) => s,
        _ => panic!("expected ready store"),
    };

    let stamp = store.stamp();
    let base_time = 1_000_000u64;
    store
        .record_roster_snapshot(
            inv.clock(),
            &cx,
            &snapshot_fixture("snap-1", base_time),
            stamp,
        )
        .expect("snap");

    // 1. CLI emitted suggestion
    let ev1 = event_fixture(
        "ev-cli-1",
        "cli",
        DecisionKind::Ranked,
        ExposureState::Emitted,
        50,
        base_time + 100,
    );
    store
        .record_ranking_event_with_attempts(inv.clock(), &cx, &ev1, &[], None, &[], stamp)
        .expect("ev1");

    // 2. CLI valid abstention
    let ev2 = event_fixture(
        "ev-cli-2",
        "cli",
        DecisionKind::Abstain,
        ExposureState::Generated,
        20,
        base_time + 200,
    );
    store
        .record_ranking_event_with_attempts(inv.clock(), &cx, &ev2, &[], None, &[], stamp)
        .expect("ev2");

    // 3. Shadow evaluated turn (muted)
    let ev3 = event_fixture(
        "ev-sh-1",
        "shadow",
        DecisionKind::Ranked,
        ExposureState::Generated,
        40,
        base_time + 300,
    );
    store
        .record_ranking_event_with_attempts(inv.clock(), &cx, &ev3, &[], None, &[], stamp)
        .expect("ev3");

    // 4. Advisory hook operational failure (unavailable)
    let ev4 = event_fixture(
        "ev-adv-1",
        "advisory",
        DecisionKind::Unavailable,
        ExposureState::Generated,
        10,
        base_time + 400,
    );
    store
        .record_ranking_event_with_attempts(inv.clock(), &cx, &ev4, &[], None, &[], stamp)
        .expect("ev4");

    // 5. CLI explicit requirement
    let ev5 = event_fixture(
        "ev-cli-3",
        "cli",
        DecisionKind::Explicit,
        ExposureState::Emitted,
        5,
        base_time + 500,
    );
    store
        .record_ranking_event_with_attempts(inv.clock(), &cx, &ev5, &[], None, &[], stamp)
        .expect("ev5");

    drop(store);

    let report = ledger_stats(&inv, &cx, location, base_time as i64, false).expect("ledger_stats");

    assert_eq!(report.turns.total_evaluated, 5);
    assert_eq!(report.turns.emitted_suggestions, 2); // ev1 and ev5
    assert_eq!(report.turns.valid_abstentions, 1); // ev2
    assert_eq!(report.turns.muted_or_suppressed, 1); // ev3 (shadow)
    assert_eq!(report.turns.operational_failures, 1); // ev4 (unavailable)
    assert_eq!(report.turns.explicit_requirements, 1); // ev5

    // Distinct channel denominators
    assert_eq!(report.turns.by_channel.len(), 3); // cli, shadow, advisory
    let cli = report
        .turns
        .by_channel
        .iter()
        .find(|c| c.channel == "cli")
        .expect("cli channel");
    assert_eq!(cli.evaluated_turns, 3);
    assert_eq!(cli.emitted, 2);
    assert_eq!(cli.abstain, 1);
    assert_eq!(cli.muted, 0);

    let shadow = report
        .turns
        .by_channel
        .iter()
        .find(|c| c.channel == "shadow")
        .expect("shadow channel");
    assert_eq!(shadow.evaluated_turns, 1);
    assert_eq!(shadow.emitted, 0);
    assert_eq!(shadow.muted, 1);

    let advisory = report
        .turns
        .by_channel
        .iter()
        .find(|c| c.channel == "advisory")
        .expect("advisory channel");
    assert_eq!(advisory.evaluated_turns, 1);
    assert_eq!(advisory.unavailable, 1);
}

#[test]
fn latency_summary_computes_accurate_percentiles() {
    let dir = temp_private_dir("latency");
    let (inv, cx) = test_invocation();
    let location = LedgerLocation::Directory(dir.clone());
    let _init = init_ledger(&inv, &cx, location.clone()).expect("init ledger");

    let open =
        open_ledger(&inv, &cx, LedgerAccess::ExistingOnly, location.clone()).expect("open ledger");
    let mut store = match open {
        LedgerOpen::Ready(s) => s,
        _ => panic!("expected ready store"),
    };

    let stamp = store.stamp();
    let base_time = 1_000_000u64;
    store
        .record_roster_snapshot(
            inv.clock(),
            &cx,
            &snapshot_fixture("snap-1", base_time),
            stamp,
        )
        .expect("snap");

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
        store
            .record_ranking_event_with_attempts(inv.clock(), &cx, &ev, &[], None, &[], stamp)
            .expect("ev");
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

    let open =
        open_ledger(&inv, &cx, LedgerAccess::ExistingOnly, location.clone()).expect("open ledger");
    let mut store = match open {
        LedgerOpen::Ready(s) => s,
        _ => panic!("expected ready store"),
    };

    let stamp = store.stamp();
    let base_time = 1_000_000u64;
    store
        .record_roster_snapshot(
            inv.clock(),
            &cx,
            &snapshot_fixture("snap-1", base_time),
            stamp,
        )
        .expect("snap");

    // Event 1: has 2 attempts (one completed with tokens, one failed)
    let ev1 = event_fixture(
        "ev-p1",
        "cli",
        DecisionKind::Ranked,
        ExposureState::Emitted,
        50,
        base_time + 100,
    );
    let att1 = attempt_fixture(
        "att-1",
        "ev-p1",
        Some(120),
        Some(80),
        AttemptStatus::Completed,
        base_time + 100,
    );
    let att2 = attempt_fixture(
        "att-2",
        "ev-p1",
        None,
        None,
        AttemptStatus::Failed,
        base_time + 110,
    );
    store
        .record_ranking_event_with_attempts(inv.clock(), &cx, &ev1, &[], None, &[att1, att2], stamp)
        .expect("ev1");

    // Event 2: has 1 attempt with unknown usage (status completed, but tokens None)
    let ev2 = event_fixture(
        "ev-p2",
        "cli",
        DecisionKind::Ranked,
        ExposureState::Emitted,
        30,
        base_time + 200,
    );
    let att3 = attempt_fixture(
        "att-3",
        "ev-p2",
        None,
        None,
        AttemptStatus::Unknown,
        base_time + 200,
    );
    store
        .record_ranking_event_with_attempts(inv.clock(), &cx, &ev2, &[], None, &[att3], stamp)
        .expect("ev2");

    // Event 3: cache-served event (0 attempts)
    let ev3 = event_fixture(
        "ev-p3",
        "cli",
        DecisionKind::Ranked,
        ExposureState::Emitted,
        2,
        base_time + 300,
    );
    store
        .record_ranking_event_with_attempts(inv.clock(), &cx, &ev3, &[], None, &[], stamp)
        .expect("ev3");

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
    assert_eq!(report.provider.cache_served_events, 1); // ev3
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

    let open =
        open_ledger(&inv, &cx, LedgerAccess::ExistingOnly, location.clone()).expect("open ledger");
    let mut store = match open {
        LedgerOpen::Ready(s) => s,
        _ => panic!("expected ready store"),
    };

    let stamp = store.stamp();
    let base_time = 1_000_000u64;
    store
        .record_roster_snapshot(
            inv.clock(),
            &cx,
            &snapshot_fixture("snap-1", base_time),
            stamp,
        )
        .expect("snap");

    // 2 emitted events
    let ev1 = event_fixture(
        "ev-o1",
        "cli",
        DecisionKind::Ranked,
        ExposureState::Emitted,
        50,
        base_time + 100,
    );
    let cand1 = candidate_fixture("ev-o1", "review", CandidateStage::Rerank, Some(1));
    store
        .record_ranking_event_with_attempts(inv.clock(), &cx, &ev1, &[cand1], None, &[], stamp)
        .expect("ev1");

    let ev2 = event_fixture(
        "ev-o2",
        "cli",
        DecisionKind::Ranked,
        ExposureState::Emitted,
        50,
        base_time + 200,
    );
    let cand2 = candidate_fixture("ev-o2", "test_runner", CandidateStage::Rerank, Some(1));
    store
        .record_ranking_event_with_attempts(inv.clock(), &cx, &ev2, &[cand2], None, &[], stamp)
        .expect("ev2");

    // Observations:
    // 1 attributed load to ev1
    let o1 = observation_fixture(
        "obs-1",
        "review",
        EvidenceState::Loaded,
        Some("ev-o1"),
        base_time + 150,
    );
    // 1 unattributed load
    let o2 = observation_fixture(
        "obs-2",
        "deploy",
        EvidenceState::Loaded,
        None,
        base_time + 160,
    );
    // 1 attempted load
    let o3 = observation_fixture(
        "obs-3",
        "review",
        EvidenceState::Attempted,
        None,
        base_time + 170,
    );
    // 1 censored observation
    let o4 = observation_fixture(
        "obs-4",
        "review",
        EvidenceState::Censored,
        None,
        base_time + 180,
    );

    for observation in [o1, o2, o3, o4] {
        store
            .record_observation(inv.clock(), &cx, &observation, stamp)
            .expect("record observations");
    }

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
    assert!(
        report
            .observations
            .caveat
            .contains("adoption is not task success")
    );
}

#[test]
fn judgments_and_cost_per_useful_suggestion_calculated() {
    let dir = temp_private_dir("judgments");
    let (inv, cx) = test_invocation();
    let location = LedgerLocation::Directory(dir.clone());
    let _init = init_ledger(&inv, &cx, location.clone()).expect("init ledger");

    let open =
        open_ledger(&inv, &cx, LedgerAccess::ExistingOnly, location.clone()).expect("open ledger");
    let mut store = match open {
        LedgerOpen::Ready(s) => s,
        _ => panic!("expected ready store"),
    };

    let stamp = store.stamp();
    let base_time = 1_000_000u64;
    store
        .record_roster_snapshot(
            inv.clock(),
            &cx,
            &snapshot_fixture("snap-1", base_time),
            stamp,
        )
        .expect("snap");

    // Event 1: 1 attempt with 500 input, 500 output tokens (= 1000 tokens)
    let ev1 = event_fixture(
        "ev-j1",
        "cli",
        DecisionKind::Ranked,
        ExposureState::Emitted,
        50,
        base_time + 100,
    );
    let att1 = attempt_fixture(
        "att-j1",
        "ev-j1",
        Some(500),
        Some(500),
        AttemptStatus::Completed,
        base_time + 100,
    );
    store
        .record_ranking_event_with_attempts(inv.clock(), &cx, &ev1, &[], None, &[att1], stamp)
        .expect("ev1");

    // Event 2: 1 attempt with 200 input, 200 output tokens (= 400 tokens)
    let ev2 = event_fixture(
        "ev-j2",
        "cli",
        DecisionKind::Ranked,
        ExposureState::Emitted,
        50,
        base_time + 200,
    );
    let att2 = attempt_fixture(
        "att-j2",
        "ev-j2",
        Some(200),
        Some(200),
        AttemptStatus::Completed,
        base_time + 200,
    );
    store
        .record_ranking_event_with_attempts(inv.clock(), &cx, &ev2, &[], None, &[att2], stamp)
        .expect("ev2");

    // Event 3 (unlabeled): 1 attempt with 3000 tokens
    let ev3 = event_fixture(
        "ev-j3",
        "cli",
        DecisionKind::Ranked,
        ExposureState::Emitted,
        50,
        base_time + 300,
    );
    let att3 = attempt_fixture(
        "att-j3",
        "ev-j3",
        Some(1500),
        Some(1500),
        AttemptStatus::Completed,
        base_time + 300,
    );
    store
        .record_ranking_event_with_attempts(inv.clock(), &cx, &ev3, &[], None, &[att3], stamp)
        .expect("ev3");

    // Judgments:
    // ev1 labeled useful
    let j1 = judgment_fixture(
        "jdg-1",
        "ev-j1",
        "review",
        JudgmentLabel::Useful,
        base_time + 400,
    );
    // ev2 labeled harmful
    let j2 = judgment_fixture(
        "jdg-2",
        "ev-j2",
        "test_runner",
        JudgmentLabel::Harmful,
        base_time + 410,
    );

    store
        .record_judgment(inv.clock(), &cx, &j1, stamp)
        .expect("j1");
    store
        .record_judgment(inv.clock(), &cx, &j2, stamp)
        .expect("j2");

    drop(store);

    let report = ledger_stats(&inv, &cx, location, base_time as i64, false).expect("ledger_stats");

    assert_eq!(report.judgments.total_judgments, 2);
    assert_eq!(report.judgments.useful, 1);
    assert_eq!(report.judgments.harmful, 1);
    assert_eq!(report.judgments.neutral, 0);
    assert_eq!(report.judgments.distinct_judged_events, 2);
    // Label coverage = 2 distinct judged / 3 emitted = 0.666...
    let cov = report
        .judgments
        .label_coverage_rate
        .expect("label coverage");
    assert!((cov - 2.0 / 3.0).abs() < 0.001);
    // Useful ratio in judged = 1 useful / 2 judgments = 0.5
    assert_eq!(report.judgments.useful_ratio_in_judged, Some(0.5));

    // Judged cohort tokens: ev1 (1000) + ev2 (400) = 1400 tokens (att3 from ev3 must NOT leak into judged cohort!)
    // Judged cohort attempts: 2 attempts
    // Useful: 1
    // Cost per useful suggestion = 2 attempts / 1400 tokens per useful suggestion
    assert!(
        report
            .provider
            .cost_per_useful_suggestion
            .contains("2 attempts / 1400 tokens per useful suggestion")
    );
    assert_eq!(report.provider.tokens_per_useful_suggestion, Some(1400.0));
}

#[test]
fn by_skill_breakdown_orders_by_recommendations_and_shortlist() {
    let dir = temp_private_dir("by-skill");
    let (inv, cx) = test_invocation();
    let location = LedgerLocation::Directory(dir.clone());
    let _init = init_ledger(&inv, &cx, location.clone()).expect("init ledger");

    let open =
        open_ledger(&inv, &cx, LedgerAccess::ExistingOnly, location.clone()).expect("open ledger");
    let mut store = match open {
        LedgerOpen::Ready(s) => s,
        _ => panic!("expected ready store"),
    };

    let stamp = store.stamp();
    let base_time = 1_000_000u64;
    store
        .record_roster_snapshot(
            inv.clock(),
            &cx,
            &snapshot_fixture("snap-1", base_time),
            stamp,
        )
        .expect("snap");

    // Event 1: skill "review" is Top 1, "test_runner" is Top 2
    let ev1 = event_fixture(
        "ev-bs1",
        "cli",
        DecisionKind::Ranked,
        ExposureState::Emitted,
        50,
        base_time + 100,
    );
    let c1 = candidate_fixture("ev-bs1", "review", CandidateStage::Rerank, Some(1));
    let c2 = candidate_fixture("ev-bs1", "test_runner", CandidateStage::Rerank, Some(2));
    store
        .record_ranking_event_with_attempts(inv.clock(), &cx, &ev1, &[c1, c2], None, &[], stamp)
        .expect("ev1");

    // Event 2: skill "review" is Top 1 again
    let ev2 = event_fixture(
        "ev-bs2",
        "cli",
        DecisionKind::Ranked,
        ExposureState::Emitted,
        50,
        base_time + 200,
    );
    let c3 = candidate_fixture("ev-bs2", "review", CandidateStage::Rerank, Some(1));
    store
        .record_ranking_event_with_attempts(inv.clock(), &cx, &ev2, &[c3], None, &[], stamp)
        .expect("ev2");

    // Observation: "review" loaded
    let o1 = observation_fixture(
        "obs-bs1",
        "review",
        EvidenceState::Loaded,
        Some("ev-bs1"),
        base_time + 300,
    );
    store
        .record_observation(inv.clock(), &cx, &o1, stamp)
        .expect("obs");

    // Judgment: "review" useful
    let j1 = judgment_fixture(
        "jdg-bs1",
        "ev-bs1",
        "review",
        JudgmentLabel::Useful,
        base_time + 400,
    );
    store
        .record_judgment(inv.clock(), &cx, &j1, stamp)
        .expect("j1");

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

    let open =
        open_ledger(&inv, &cx, LedgerAccess::ExistingOnly, location.clone()).expect("open ledger");
    let mut store = match open {
        LedgerOpen::Ready(s) => s,
        _ => panic!("expected ready store"),
    };

    let stamp = store.stamp();
    let base_time = 1_000_000u64;
    store
        .record_roster_snapshot(
            inv.clock(),
            &cx,
            &snapshot_fixture("snap-1", base_time),
            stamp,
        )
        .expect("snap");

    // Old event: created at 1_000_100
    let ev_old = event_fixture(
        "ev-old",
        "cli",
        DecisionKind::Ranked,
        ExposureState::Emitted,
        50,
        1_000_100,
    );
    store
        .record_ranking_event_with_attempts(inv.clock(), &cx, &ev_old, &[], None, &[], stamp)
        .expect("old");

    // Newer event: created at 2_000_000
    let ev_new = event_fixture(
        "ev-new",
        "cli",
        DecisionKind::Ranked,
        ExposureState::Emitted,
        50,
        2_000_000,
    );
    store
        .record_ranking_event_with_attempts(inv.clock(), &cx, &ev_new, &[], None, &[], stamp)
        .expect("new");

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

#[test]
fn cli_stats_uses_unix_time_for_recent_events_and_relative_cutoffs() {
    let dir = temp_private_dir("wall-clock");
    let (inv, cx) = test_invocation();
    let location = LedgerLocation::Directory(dir.clone());
    init_ledger(&inv, &cx, location.clone()).unwrap();
    let LedgerOpen::Ready(mut store) =
        open_ledger(&inv, &cx, LedgerAccess::ExistingOnly, location).unwrap()
    else {
        panic!("expected initialized ledger");
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let stamp = store.stamp();
    store
        .record_roster_snapshot(
            inv.clock(),
            &cx,
            &snapshot_fixture("snap-1", now - 120_000),
            stamp,
        )
        .unwrap();
    for (id, time) in [("old", now - 90_000), ("recent", now - 1_000)] {
        let event = event_fixture(
            id,
            "cli",
            DecisionKind::Ranked,
            ExposureState::Emitted,
            10,
            time,
        );
        store
            .record_ranking_event_with_attempts(inv.clock(), &cx, &event, &[], None, &[], stamp)
            .unwrap();
    }
    drop(store);
    let output = Command::new(env!("CARGO_BIN_EXE_sr"))
        .args(["stats", "--json", "--since", "1m", "--dir"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: StatsValueReport = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report.as_of_unix_ms >= now as i64);
    assert!(report.since_unix_ms >= now as i64 - 60_000);
    assert_eq!(report.turns.total_evaluated, 1);
    assert_eq!(report.turns.emitted_suggestions, 1);
}

/// The cohorts sr-roadmap-l1i.6.13's acceptance names but this file did not yet exercise:
/// a follower that reused an owner's response, rows past the retention cutoff, and the TUI
/// and advisory channels.
fn ready_store(prefix: &str, inv: &ProcessInvocation, cx: &Cx) -> (LedgerLocation, LedgerStore) {
    let dir = temp_private_dir(prefix);
    let location = LedgerLocation::Directory(dir);
    let _init = init_ledger(inv, cx, location.clone()).expect("init ledger");
    let open =
        open_ledger(inv, cx, LedgerAccess::ExistingOnly, location.clone()).expect("open ledger");
    match open {
        LedgerOpen::Ready(store) => (location, *store),
        other => panic!("expected a ready store, got {other:?}"),
    }
}

#[test]
fn a_follower_that_reused_a_response_is_a_delivery_with_no_cost_of_its_own() {
    // A follower is a turn that was served from a response another turn paid for. It admits no
    // provider attempt, so it owns no attempt row — which is correct, because the money was
    // spent by the owner and charging it twice would inflate the total the report exists to
    // keep honest.
    //
    // What that leaves is a question about usefulness rather than about spend: if the follower
    // is the turn somebody judged useful, the judged cohort's own attempts are none, and the
    // ratio computed from them describes a suggestion that appears to have cost nothing. This
    // case establishes what the report actually says in that situation.
    let (inv, cx) = test_invocation();
    let (location, mut store) = ready_store("follower", &inv, &cx);
    let base = 1_700_000_000_000u64;

    let stamp = store.stamp();
    store
        .record_roster_snapshot(inv.clock(), &cx, &snapshot_fixture("snap-1", base), stamp)
        .expect("snapshot");

    // The owner paid for its answer.
    let owner = event_fixture(
        "ev-owner",
        "cli",
        DecisionKind::Ranked,
        ExposureState::Emitted,
        480,
        base + 100,
    );
    let paid = attempt_fixture(
        "att-owner",
        "ev-owner",
        Some(80),
        Some(20),
        AttemptStatus::Completed,
        base + 100,
    );
    let stamp = store.stamp();
    store
        .record_ranking_event_with_attempts(
            inv.clock(),
            &cx,
            &owner,
            &[],
            None,
            std::slice::from_ref(&paid),
            stamp,
        )
        .expect("owner");

    // The follower reused it: a real delivery, and no attempt of its own.
    let follower = event_fixture(
        "ev-follower",
        "cli",
        DecisionKind::Ranked,
        ExposureState::Emitted,
        11,
        base + 200,
    );
    let stamp = store.stamp();
    store
        .record_ranking_event_with_attempts(inv.clock(), &cx, &follower, &[], None, &[], stamp)
        .expect("follower");

    // And the follower is the one judged useful.
    let stamp = store.stamp();
    store
        .record_judgment(
            inv.clock(),
            &cx,
            &judgment_fixture(
                "j-1",
                "ev-follower",
                "review",
                JudgmentLabel::Useful,
                base + 300,
            ),
            stamp,
        )
        .expect("judgment");

    let report = ledger_stats(&inv, &cx, location, base as i64 - 1, false).expect("ledger_stats");

    // Both turns are evaluated and both delivered. Only one of them paid.
    assert_eq!(report.turns.total_evaluated, 2, "{:#?}", report.turns);
    assert_eq!(report.turns.emitted_suggestions, 2, "{:#?}", report.turns);
    assert_eq!(report.provider.total_attempts, 1, "{:#?}", report.provider);
    assert_eq!(
        report.provider.known_total_tokens, 100,
        "{:#?}",
        report.provider
    );
    assert_eq!(
        report.provider.cache_served_events, 1,
        "the reused turn was not recognised as served without paying: {:#?}",
        report.provider
    );

    // The follower is not credited with the owner's attempt: that would report the same 100
    // tokens twice across the two turns.
    assert_eq!(report.judgments.useful, 1, "{:#?}", report.judgments);
    assert_eq!(
        report.provider.judged_turns_served_from_cache, 1,
        "the judged cohort's reliance on a response it did not pay for went unreported: {:#?}",
        report.provider
    );

    // And the ratio is declined rather than reported as zero. Zero would say a useful
    // suggestion cost nothing; what is true is that this cohort's spend belongs to a turn
    // outside it, and attributing the owner's tokens here would report them twice once the
    // owner is judged too.
    assert!(
        report.provider.tokens_per_useful_suggestion.is_none(),
        "a cohort that paid nothing itself was given a token ratio: {:?}",
        report.provider.tokens_per_useful_suggestion
    );
    let text = &report.provider.cost_per_useful_suggestion;
    assert!(
        text.starts_with("not estimable"),
        "cost per useful suggestion claimed a figure for a cohort with no attempts: {text}"
    );
    assert!(
        text.contains("reused a response"),
        "the reason a figure was declined is not stated: {text}"
    );
}

#[test]
fn rows_past_the_retention_cutoff_are_counted_and_disclosed_as_expiring() {
    // A window wider than retention includes turns retention already considers expired. They
    // are still recorded, so the window's counts include them — and `retained` says how much of
    // what was just counted will survive the next maintenance pass. The two numbers describe
    // different questions and must not be reconciled by quietly dropping rows from either.
    let (inv, cx) = test_invocation();
    let (location, mut store) = ready_store("expired", &inv, &cx);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let day = 24 * 60 * 60 * 1000u64;

    let stamp = store.stamp();
    store
        .record_roster_snapshot(
            inv.clock(),
            &cx,
            &snapshot_fixture("snap-1", now - 60 * day),
            stamp,
        )
        .expect("snapshot");

    // One turn inside retention, one well past it, each with a paid attempt.
    for (id, age_days) in [("ev-recent", 2u64), ("ev-ancient", 60u64)] {
        let event = event_fixture(
            id,
            "cli",
            DecisionKind::Ranked,
            ExposureState::Emitted,
            400,
            now - age_days * day,
        );
        let attempt = attempt_fixture(
            &format!("att-{id}"),
            id,
            Some(70),
            Some(30),
            AttemptStatus::Completed,
            now - age_days * day,
        );
        let stamp = store.stamp();
        store
            .record_ranking_event_with_attempts(
                inv.clock(),
                &cx,
                &event,
                &[],
                None,
                std::slice::from_ref(&attempt),
                stamp,
            )
            .unwrap_or_else(|error| panic!("recording {id} failed: {error:?}"));
    }

    // A window of ninety days reaches past the thirty-day retention cutoff on purpose.
    let report =
        ledger_stats(&inv, &cx, location, (now - 90 * day) as i64, false).expect("ledger_stats");

    // Nothing is silently omitted: both turns and both paid attempts are counted.
    assert_eq!(report.turns.total_evaluated, 2, "{:#?}", report.turns);
    assert_eq!(report.provider.total_attempts, 2, "{:#?}", report.provider);
    assert_eq!(
        report.provider.known_total_tokens, 200,
        "a paid attempt on an expiring turn was dropped from the totals: {:#?}",
        report.provider
    );

    // And the report says which of them retention no longer protects.
    assert_eq!(report.retained.total_events, 2, "{:#?}", report.retained);
    assert_eq!(
        report.retained.active_events, 1,
        "the turn inside retention was not identified: {:#?}",
        report.retained
    );
    assert_eq!(
        report.retained.expired_events, 1,
        "a turn past the cutoff was not disclosed as expiring: {:#?}",
        report.retained
    );
    assert!(
        report.retained.cutoff_unix_ms < report.retained.as_of_unix_ms,
        "{:#?}",
        report.retained
    );
}

#[test]
fn the_tui_and_advisory_channels_keep_their_own_denominators() {
    // README promises shadow, advisory-hook, CLI and TUI records have separate denominators.
    // Two of the four were already covered; a TUI turn averaged into a CLI one would make a
    // per-channel report worth less than no report, because the reader cannot see it happen.
    let (inv, cx) = test_invocation();
    let (location, mut store) = ready_store("channels4", &inv, &cx);
    let base = 1_700_000_000_000u64;

    let stamp = store.stamp();
    store
        .record_roster_snapshot(inv.clock(), &cx, &snapshot_fixture("snap-1", base), stamp)
        .expect("snapshot");

    // One delivery in each of the four channels, plus an abstention in the TUI only.
    let rows = [
        (
            "ev-cli",
            "cli",
            DecisionKind::Ranked,
            ExposureState::Emitted,
        ),
        (
            "ev-shadow",
            "shadow",
            DecisionKind::Ranked,
            ExposureState::Emitted,
        ),
        (
            "ev-advisory",
            "advisory",
            DecisionKind::Ranked,
            ExposureState::Emitted,
        ),
        (
            "ev-tui",
            "tui",
            DecisionKind::Ranked,
            ExposureState::Emitted,
        ),
        (
            "ev-tui-abstain",
            "tui",
            DecisionKind::Abstain,
            ExposureState::Generated,
        ),
    ];
    for (offset, (id, channel, decision, exposure)) in rows.iter().enumerate() {
        let event = event_fixture(
            id,
            channel,
            *decision,
            *exposure,
            300,
            base + 100 + offset as u64,
        );
        let stamp = store.stamp();
        store
            .record_ranking_event_with_attempts(inv.clock(), &cx, &event, &[], None, &[], stamp)
            .unwrap_or_else(|error| panic!("recording {id} failed: {error:?}"));
    }

    let report = ledger_stats(&inv, &cx, location, base as i64 - 1, false).expect("ledger_stats");

    assert_eq!(report.turns.total_evaluated, 5, "{:#?}", report.turns);
    let by_channel: std::collections::BTreeMap<&str, &ChannelStats> = report
        .turns
        .by_channel
        .iter()
        .map(|channel| (channel.channel.as_str(), channel))
        .collect();
    assert_eq!(
        by_channel.keys().copied().collect::<Vec<_>>(),
        vec!["advisory", "cli", "shadow", "tui"],
        "a channel was merged into another: {:#?}",
        report.turns.by_channel
    );
    for name in ["cli", "shadow", "advisory"] {
        let channel = by_channel[name];
        assert_eq!(channel.evaluated_turns, 1, "{name}: {channel:#?}");
        assert_eq!(channel.emitted, 1, "{name}: {channel:#?}");
        assert_eq!(channel.abstain, 0, "{name}: {channel:#?}");
    }
    // The TUI carries two turns of its own, and its abstention stays in its own denominator.
    let tui = by_channel["tui"];
    assert_eq!(tui.evaluated_turns, 2, "{tui:#?}");
    assert_eq!(tui.emitted, 1, "{tui:#?}");
    assert_eq!(tui.abstain, 1, "{tui:#?}");
    // Totals agree with the sum of the parts, so a total cannot contradict a channel.
    let summed: u64 = report
        .turns
        .by_channel
        .iter()
        .map(|channel| channel.evaluated_turns)
        .sum();
    assert_eq!(summed, report.turns.total_evaluated, "{:#?}", report.turns);
}

#[test]
fn a_judged_attempt_with_unknown_usage_is_not_silently_costed_at_zero() {
    // 6.13's acceptance asks that no paid attempt be silently omitted. An attempt whose usage the
    // provider never reported is still a paid attempt: it contributes nothing to the token sum
    // because nothing is known, not because nothing was spent. The ratio built from that sum is
    // therefore a lower bound, and this case establishes whether the report says so.
    let (inv, cx) = test_invocation();
    let (location, mut store) = ready_store("unknownusage", &inv, &cx);
    let base = 1_700_000_000_000u64;

    let stamp = store.stamp();
    store
        .record_roster_snapshot(inv.clock(), &cx, &snapshot_fixture("snap-1", base), stamp)
        .expect("snapshot");

    let event = event_fixture(
        "ev-1",
        "cli",
        DecisionKind::Ranked,
        ExposureState::Emitted,
        400,
        base + 100,
    );
    // One attempt reported its usage; the other completed without reporting any.
    let known = attempt_fixture(
        "att-known",
        "ev-1",
        Some(90),
        Some(10),
        AttemptStatus::Completed,
        base + 100,
    );
    let silent = attempt_fixture(
        "att-silent",
        "ev-1",
        None,
        None,
        AttemptStatus::Completed,
        base + 110,
    );
    let stamp = store.stamp();
    store
        .record_ranking_event_with_attempts(
            inv.clock(),
            &cx,
            &event,
            &[],
            None,
            &[known, silent],
            stamp,
        )
        .expect("event");
    let stamp = store.stamp();
    store
        .record_judgment(
            inv.clock(),
            &cx,
            &judgment_fixture("j-1", "ev-1", "review", JudgmentLabel::Useful, base + 300),
            stamp,
        )
        .expect("judgment");

    let report = ledger_stats(&inv, &cx, location, base as i64 - 1, false).expect("ledger_stats");

    // Both attempts are counted and one of them reported nothing.
    assert_eq!(report.provider.total_attempts, 2, "{:#?}", report.provider);
    assert_eq!(
        report.provider.known_total_tokens, 100,
        "{:#?}",
        report.provider
    );
    assert_eq!(
        report.provider.unknown_usage_attempts, 1,
        "an attempt that reported no usage was not counted as such: {:#?}",
        report.provider
    );

    // The ratio is computed from 100 known tokens over two attempts, one of whose cost is
    // unknown. Saying "50 tokens per useful suggestion" without that qualification presents a
    // lower bound as a measurement.
    let text = &report.provider.cost_per_useful_suggestion;
    assert!(
        text.contains("unknown") || text.contains("lower bound"),
        "the cost figure omits an attempt whose usage is unknown without saying so: {text}"
    );
}
