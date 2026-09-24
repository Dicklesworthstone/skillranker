#![cfg(unix)]
//! `sr stats` over a deliberately mixed cohort (sr-roadmap-l1i.6.13).
//!
//! The report's whole job is to be honest about what it does and does not know, so
//! these cases are built around the ways an aggregate can lie: a denominator that
//! quietly mixes two channels, an unfinished invocation counted as a failure or as a
//! delivery, a cost ratio computed over traffic nobody labelled, an unknown token
//! count summed as zero, and a measure that was never recorded reported as `0`.
//!
//! The fixture is one ledger holding, on purpose: two channels; a delivered turn with
//! attempts and a useful label; a delivered turn whose response was reused, so it has
//! no attempts; an abstention; a finished operational failure; a turn that died in
//! flight; observations in all three evidence states with one unattributed; and a
//! neutral label. Every assertion below is about that shape.
use asupersync::Cx;
use skillranker::runtime::ProcessInvocation;
use skillranker::storage::ledger::*;
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// A fixed "now" so window arithmetic in the assertions is exact.
const NOW_MS: i64 = 1_789_950_000_000;
const HOUR_MS: i64 = 3_600_000;

fn test_invocation() -> (ProcessInvocation, Cx) {
    let inv = ProcessInvocation::enter().expect("process invocation");
    let cx = inv.request_cx().expect("request cx");
    (inv, cx)
}

// Intentionally retained: repository policy forbids automatic tree deletion.
fn temp_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from("/tmp").join(format!(
        "sr-stats-{}-{}-{}",
        name,
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
        .expect("temp dir");
    dir
}

struct Fixture {
    store: LedgerStore,
}

#[allow(clippy::too_many_arguments)]
fn event(
    id: &str,
    channel: &str,
    decision: DecisionKind,
    exposure: ExposureState,
    elapsed_ms: u64,
    created_at_unix_ms: i64,
    tokens: Option<(u64, u64)>,
) -> NewRankingEvent {
    NewRankingEvent {
        event_id: id.into(),
        verified_delivery_key: None,
        workspace_root: "/data/workspace".into(),
        session_id: "session".into(),
        agent_branch: "main".into(),
        mode_channel: channel.into(),
        policy_version: "ranking-v1".into(),
        schema_version: 1,
        decision,
        reason: "fixture".into(),
        exposure_state: exposure,
        elapsed_ms,
        created_at_unix_ms: u64::try_from(created_at_unix_ms).unwrap(),
        input_tokens: tokens.map(|(i, _)| i),
        output_tokens: tokens.map(|(_, o)| o),
        snapshot_id: None,
    }
}

fn candidate(
    event_id: &str,
    skill: &str,
    rank: Option<u32>,
    excluded: bool,
) -> NewRankingCandidate {
    NewRankingCandidate {
        event_id: event_id.into(),
        stage: CandidateStage::Rerank,
        skill_id: skill.into(),
        skill_version: "v1".into(),
        raw_probability: Some(0.8),
        normalized_probability: Some(0.8),
        fit_score: Some(0.7),
        rank_score: Some(0.9),
        rank_position: rank,
        excluded,
        exclusion_reason: excluded.then(|| "not-eligible".to_string()),
    }
}

#[allow(clippy::too_many_arguments)]
fn attempt(
    id: &str,
    owner: &str,
    status: AttemptStatus,
    tokens: Option<(u64, u64)>,
    stage: CandidateStage,
) -> NewProviderAttempt {
    NewProviderAttempt {
        attempt_id: id.into(),
        owner_event_id: owner.into(),
        stage,
        request_fingerprint: "fp".into(),
        admitted_at_unix_ms: u64::try_from(NOW_MS).unwrap(),
        sent_at_unix_ms: Some(u64::try_from(NOW_MS).unwrap()),
        completed_at_unix_ms: Some(u64::try_from(NOW_MS).unwrap()),
        status,
        input_tokens: tokens.map(|(i, _)| i),
        output_tokens: tokens.map(|(_, o)| o),
        http_status: None,
        error_kind: None,
    }
}

impl Fixture {
    /// The mixed cohort. Everything here is deliberate; see the module comment.
    fn new(name: &str, inv: &ProcessInvocation, cx: &Cx) -> Self {
        let dir = temp_dir(name);
        let mut store = match open_ledger(
            inv,
            cx,
            LedgerAccess::Initialize,
            LedgerLocation::Directory(dir),
        )
        .expect("open_ledger")
        {
            LedgerOpen::Ready(store) => *store,
            other => panic!("expected Ready, got {other:?}"),
        };

        // A delivered CLI turn that paid, with a useful label on its suggestion.
        let stamp = store.stamp();
        store
            .record_ranking_event_with_attempts(
                inv.clock(),
                cx,
                &event(
                    "ev-paid",
                    "cli",
                    DecisionKind::Ranked,
                    ExposureState::Emitted,
                    300,
                    NOW_MS,
                    Some((100, 25)),
                ),
                &[candidate("ev-paid", "skill-a", Some(1), false)],
                None,
                &[
                    attempt(
                        "att-paid-wide",
                        "ev-paid",
                        AttemptStatus::Completed,
                        Some((100, 25)),
                        CandidateStage::Wide,
                    ),
                    attempt(
                        "att-paid-rerank",
                        "ev-paid",
                        AttemptStatus::Completed,
                        Some((120, 30)),
                        CandidateStage::Rerank,
                    ),
                ],
                stamp,
            )
            .expect("paid turn");

        // A delivered hook turn that reused a response: no attempts of its own.
        let stamp = store.stamp();
        store
            .record_ranking_event_with_attempts(
                inv.clock(),
                cx,
                &event(
                    "ev-reused",
                    "hook-claude",
                    DecisionKind::Ranked,
                    ExposureState::Emitted,
                    120,
                    NOW_MS,
                    None,
                ),
                &[candidate("ev-reused", "skill-b", Some(1), false)],
                None,
                &[],
                stamp,
            )
            .expect("reused turn");

        // An abstention, a finished failure, and a turn that died in flight.
        for (id, channel, decision, exposure, elapsed) in [
            (
                "ev-abstain",
                "cli",
                DecisionKind::Abstain,
                ExposureState::Emitted,
                90u64,
            ),
            (
                "ev-failed",
                "cli",
                DecisionKind::Unavailable,
                ExposureState::Prepared,
                210,
            ),
            (
                "ev-killed",
                "hook-claude",
                DecisionKind::Unavailable,
                ExposureState::Generated,
                0,
            ),
        ] {
            let stamp = store.stamp();
            store
                .record_ranking_event_with_attempts(
                    inv.clock(),
                    cx,
                    &event(id, channel, decision, exposure, elapsed, NOW_MS, None),
                    &[],
                    None,
                    &[],
                    stamp,
                )
                .unwrap_or_else(|e| panic!("{id}: {e:?}"));
        }

        // An older turn, outside a one-hour window.
        let stamp = store.stamp();
        store
            .record_ranking_event_with_attempts(
                inv.clock(),
                cx,
                &event(
                    "ev-old",
                    "cli",
                    DecisionKind::Ranked,
                    ExposureState::Emitted,
                    150,
                    NOW_MS - 3 * HOUR_MS,
                    None,
                ),
                &[candidate("ev-old", "skill-a", Some(1), false)],
                None,
                &[],
                stamp,
            )
            .expect("old turn");

        // Observations: one loaded and attributed, one attempted and attributed, one
        // censored with no attribution at all.
        let cursor = SessionCursor {
            workspace_root: "/data/workspace".into(),
            session_id: "session".into(),
            agent_branch: "main".into(),
            cursor_kind: CursorKind::Observation,
            transcript_generation: 1,
            last_complete_event_id: "t-3".into(),
            last_offset_bytes: 10,
            updated_at_unix_ms: u64::try_from(NOW_MS).unwrap(),
        };
        let observations = vec![
            NewObservation {
                observation_id: "obs-loaded".into(),
                source_event_key: "src-1".into(),
                workspace_root: "/data/workspace".into(),
                session_id: "session".into(),
                agent_branch: "main".into(),
                attributed_event_id: Some("ev-paid".into()),
                skill_id: "skill-a".into(),
                evidence_state: EvidenceState::Loaded,
                observed_at_unix_ms: u64::try_from(NOW_MS).unwrap(),
            },
            NewObservation {
                observation_id: "obs-attempted".into(),
                source_event_key: "src-2".into(),
                workspace_root: "/data/workspace".into(),
                session_id: "session".into(),
                agent_branch: "main".into(),
                attributed_event_id: Some("ev-reused".into()),
                skill_id: "skill-b".into(),
                evidence_state: EvidenceState::Attempted,
                observed_at_unix_ms: u64::try_from(NOW_MS).unwrap(),
            },
            // Observed long before any emission exists, so attribution genuinely
            // cannot find a preceding one. Passing `None` is not enough: the store
            // attributes to the latest eligible preceding emission, which is the
            // point of that machinery.
            NewObservation {
                observation_id: "obs-censored".into(),
                source_event_key: "src-3".into(),
                workspace_root: "/data/workspace".into(),
                session_id: "session".into(),
                agent_branch: "main".into(),
                attributed_event_id: None,
                skill_id: "skill-a".into(),
                evidence_state: EvidenceState::Censored,
                observed_at_unix_ms: u64::try_from(NOW_MS - 10 * HOUR_MS).unwrap(),
            },
        ];
        let stamp = store.stamp();
        store
            .record_observations_with_cursor(inv.clock(), cx, &observations, &cursor, None, stamp)
            .expect("observations");

        // One useful label on the paid turn, one neutral label on the reused turn.
        for (id, event_id, skill, label) in [
            ("jdg-useful", "ev-paid", "skill-a", JudgmentLabel::Useful),
            (
                "jdg-neutral",
                "ev-reused",
                "skill-b",
                JudgmentLabel::Neutral,
            ),
        ] {
            let stamp = store.stamp();
            store
                .record_judgment(
                    inv.clock(),
                    cx,
                    &NewJudgment {
                        judgment_id: id.into(),
                        attributed_event_id: event_id.into(),
                        skill_id: skill.into(),
                        label,
                        label_version: 1,
                        provenance: "fixture-assessor".into(),
                        created_at_unix_ms: u64::try_from(NOW_MS).unwrap(),
                    },
                    stamp,
                )
                .unwrap_or_else(|e| panic!("{id}: {e:?}"));
        }
        Self { store }
    }

    fn all(&self) -> StatsSnapshot {
        self.store
            .query_stats(None, NOW_MS + HOUR_MS, true)
            .expect("stats over everything")
    }
}

#[test]
fn channels_keep_separate_denominators() {
    let (inv, cx) = test_invocation();
    let fixture = Fixture::new("channels", &inv, &cx);
    let snapshot = fixture.all();

    let channels: Vec<&str> = snapshot
        .channels
        .iter()
        .map(|c| c.channel.as_str())
        .collect();
    assert_eq!(channels, vec!["cli", "hook-claude"], "{channels:?}");
    let cli = snapshot
        .channels
        .iter()
        .find(|c| c.channel == "cli")
        .unwrap();
    let hook = snapshot
        .channels
        .iter()
        .find(|c| c.channel == "hook-claude")
        .unwrap();
    // Four cli turns (paid, abstain, failed, old) and two hook turns (reused, killed).
    assert_eq!(cli.evaluated_turns, 4, "{cli:#?}");
    assert_eq!(hook.evaluated_turns, 2, "{hook:#?}");
    // Totals are the sum of the parts, never a separate query that could disagree.
    assert_eq!(
        snapshot.totals.evaluated_turns,
        cli.evaluated_turns + hook.evaluated_turns
    );
    assert_eq!(
        snapshot.totals.attempts,
        cli.attempts + hook.attempts,
        "attempts must not be counted outside a channel"
    );
}

#[test]
fn an_unfinished_turn_is_neither_a_failure_nor_a_delivery() {
    let (inv, cx) = test_invocation();
    let fixture = Fixture::new("unfinished", &inv, &cx);
    let snapshot = fixture.all();
    let totals = &snapshot.totals;

    // `ev-killed` is `unavailable` and still `generated`; `ev-failed` is `unavailable`
    // and finished. Only the second is an operational failure.
    assert_eq!(totals.in_flight_or_killed, 1, "{totals:#?}");
    assert_eq!(totals.operational_failures, 1, "{totals:#?}");
    // Three events reached `emitted`: paid, reused, abstain, plus the old one.
    assert_eq!(totals.emitted, 4, "{totals:#?}");
    assert_eq!(totals.prepared_not_emitted, 1, "{totals:#?}");
    // Its zero elapsed is excluded rather than dragging the percentiles down.
    assert_eq!(totals.latency_excluded_unfinished, 1, "{totals:#?}");
    assert_eq!(totals.latency_samples, 5, "{totals:#?}");
    let p50 = totals.latency_p50_ms.expect("finished turns have a p50");
    assert!(
        (90..=300).contains(&p50),
        "p50 {p50} outside the fixture's range"
    );
}

#[test]
fn a_reused_response_is_not_reported_as_a_cache_hit() {
    let (inv, cx) = test_invocation();
    let fixture = Fixture::new("reused", &inv, &cx);
    let snapshot = fixture.all();
    let hook = snapshot
        .channels
        .iter()
        .find(|c| c.channel == "hook-claude")
        .unwrap();

    // The measure is named for what was observed — no attempt rows — rather than for
    // a cause the ledger cannot see. An event recorded before attempts were written
    // down looks identical, and calling either a cache hit would be an invention.
    assert_eq!(hook.events_without_recorded_attempts, 2, "{hook:#?}");
    assert_eq!(hook.attempts, 0, "{hook:#?}");
    assert_eq!(
        snapshot.totals.attempts, 2,
        "only the paid turn sent anything"
    );
}

#[test]
fn only_delivered_suggestions_count_as_delivered() {
    let (inv, cx) = test_invocation();
    let fixture = Fixture::new("delivered", &inv, &cx);
    let snapshot = fixture.all();
    // Three candidates exist with a rank, all on emitted events: paid, reused, old.
    assert_eq!(
        snapshot.totals.emitted_suggestions, 3,
        "{:#?}",
        snapshot.totals
    );
    // The judged cohort is the two labelled events, not all four emitted ones.
    assert_eq!(snapshot.judgments.judged_events, 2);
    assert_eq!(snapshot.judgments.useful, 1);
    assert_eq!(snapshot.judgments.neutral, 1);
    assert_eq!(snapshot.judgments.harmful, 0);
}

#[test]
fn observation_evidence_states_and_attribution_stay_distinct() {
    let (inv, cx) = test_invocation();
    let fixture = Fixture::new("observations", &inv, &cx);
    let snapshot = fixture.all();
    let obs = snapshot.observations;
    assert_eq!(obs.total, 3, "{obs:#?}");
    assert_eq!(obs.loaded, 1);
    assert_eq!(obs.attempted, 1, "an attempted load is not a resolved one");
    assert_eq!(obs.censored, 1);
    assert_eq!(obs.attributed, 2);
    assert_eq!(obs.unattributed, 1, "missing attribution stays visible");
}

#[test]
fn cost_per_useful_uses_only_the_judged_cohorts_own_attempts() {
    let (inv, cx) = test_invocation();
    let fixture = Fixture::new("cohort", &inv, &cx);
    let snapshot = fixture.all();
    let cohort = snapshot.judged_cohort;

    assert_eq!(cohort.useful_labels, 1);
    // Only `ev-paid` carries a useful label, so only its two attempts match. The
    // neutral label's event contributes nothing, and unlabelled traffic never does.
    assert_eq!(cohort.matched_attempts, 2, "{cohort:#?}");
    assert_eq!(cohort.attempts_with_unknown_usage, 0);
    assert_eq!(cohort.known_input_tokens, 220);
    assert_eq!(cohort.known_output_tokens, 55);
}

#[test]
fn an_unknown_usage_attempt_in_the_cohort_prevents_an_exact_ratio() {
    let (inv, cx) = test_invocation();
    let mut fixture = Fixture::new("unknown-usage", &inv, &cx);
    // A third attempt on the labelled event, sent and never settled: cost incurred,
    // amount unknown. The ratio must stop being exact rather than treat it as zero.
    let stamp = fixture.store.stamp();
    fixture
        .store
        .record_provider_attempt(
            inv.clock(),
            &cx,
            &NewProviderAttempt {
                attempt_id: "att-paid-unknown".into(),
                owner_event_id: "ev-paid".into(),
                stage: CandidateStage::Rerank,
                request_fingerprint: "fp".into(),
                admitted_at_unix_ms: u64::try_from(NOW_MS).unwrap(),
                sent_at_unix_ms: Some(u64::try_from(NOW_MS).unwrap()),
                completed_at_unix_ms: None,
                status: AttemptStatus::Unknown,
                input_tokens: None,
                output_tokens: None,
                http_status: None,
                error_kind: Some("transient-io".into()),
            },
            stamp,
        )
        .expect("unknown attempt recorded");

    let cohort = fixture.all().judged_cohort;
    assert_eq!(cohort.matched_attempts, 3, "{cohort:#?}");
    assert_eq!(cohort.attempts_with_unknown_usage, 1, "{cohort:#?}");
    // The known tokens are still reported, so the report degrades to counts rather
    // than to silence.
    assert_eq!(cohort.known_input_tokens, 220);
}

#[test]
fn a_window_excludes_older_records_and_says_which_window_it_used() {
    let (inv, cx) = test_invocation();
    let fixture = Fixture::new("window", &inv, &cx);
    let from = NOW_MS - HOUR_MS;
    let snapshot = fixture
        .store
        .query_stats(Some(from), NOW_MS + HOUR_MS, false)
        .expect("windowed stats");

    assert_eq!(snapshot.window_from_unix_ms, Some(from));
    assert_eq!(snapshot.window_to_unix_ms, NOW_MS + HOUR_MS);
    // `ev-old` is three hours back and drops out; the other five remain.
    assert_eq!(snapshot.totals.evaluated_turns, 5, "{:#?}", snapshot.totals);
    let cli = snapshot
        .channels
        .iter()
        .find(|c| c.channel == "cli")
        .unwrap();
    assert_eq!(cli.evaluated_turns, 3, "{cli:#?}");
    // A narrower window changes the denominator, which is exactly why it is reported.
    assert!(snapshot.window_from_unix_ms.is_some());
    assert_eq!(snapshot.by_skill.len(), 0, "skills were not requested");
}

#[test]
fn per_skill_reports_appearances_and_labels_without_inventing_a_rate() {
    let (inv, cx) = test_invocation();
    let fixture = Fixture::new("by-skill", &inv, &cx);
    let snapshot = fixture.all();
    let a = snapshot
        .by_skill
        .iter()
        .find(|s| s.skill_id == "skill-a")
        .expect("skill-a present");
    // Suggested twice (paid and old), loaded once, censored once, one useful label.
    assert_eq!(a.emitted_suggestions, 2, "{a:#?}");
    assert_eq!(a.observed_loaded, 1);
    assert_eq!(a.observed_censored, 1);
    assert_eq!(a.judged_useful, 1);
    let b = snapshot
        .by_skill
        .iter()
        .find(|s| s.skill_id == "skill-b")
        .expect("skill-b present");
    assert_eq!(b.emitted_suggestions, 1);
    assert_eq!(b.observed_attempted, 1);
    assert_eq!(b.judged_neutral, 1);
}

/// Runs the built binary's `stats` against an explicit ledger directory.
fn run_stats(dir: Option<&std::path::Path>, extra: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sr"));
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .arg("stats");
    if let Some(dir) = dir {
        command.arg("--dir").arg(dir);
    }
    command.args(extra).output().expect("sr stats runs")
}

#[test]
fn a_missing_ledger_is_a_typed_error_not_an_empty_report() {
    // An empty report would read as "nothing happened", which is a different claim
    // from "there is nothing to read". The second is the truth here.
    let dir = temp_dir("no-ledger");
    let out = run_stats(Some(&dir), &["--json"]);
    assert_eq!(
        out.status.code(),
        Some(9),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let combined = format!("{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        combined.contains("ledger") && combined.contains("init"),
        "the error should say what to do: {combined}"
    );
}

#[test]
fn an_unparsable_since_is_a_usage_error_before_any_read() {
    let dir = temp_dir("bad-since");
    let out = run_stats(Some(&dir), &["--json", "--since", "last-tuesday"]);
    // Usage, not storage: the window could not be understood, so nothing was read.
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn the_report_names_what_it_cannot_observe() {
    let (inv, cx) = test_invocation();
    let fixture = Fixture::new("cli-render", &inv, &cx);
    let dir = fixture
        .store
        .database_path()
        .parent()
        .unwrap()
        .to_path_buf();
    drop(fixture);

    let json = run_stats(Some(&dir), &["--json"]);
    assert_eq!(
        json.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&json.stderr)
    );
    let document: serde_json::Value =
        serde_json::from_slice(&json.stdout).expect("stats emits valid json");
    let not_recorded: Vec<&str> = document["not_recorded"]
        .as_array()
        .expect("not_recorded is an array")
        .iter()
        .map(|entry| entry["measure"].as_str().unwrap_or_default())
        .collect();
    // Each of these is absent from the ledger by design. A reader who saw 0 would
    // conclude something false, so they are named instead of counted.
    for measure in [
        "muted_or_suppressed_output",
        "cache_reuse",
        "acknowledged_delivery",
        "estimated_cost",
    ] {
        assert!(
            not_recorded.contains(&measure),
            "{measure} missing: {not_recorded:?}"
        );
    }
    assert_eq!(document["kind"], "stats");
    assert_eq!(document["actionable"], false, "a report is not an advisory");
    // The monetary ratio is separately not estimable, with its reason named.
    assert_eq!(
        document["cost_per_useful_suggestion"]["monetary"]["reason"],
        "no-pricing-configuration"
    );

    let table = run_stats(Some(&dir), &["--table"]);
    let text = String::from_utf8_lossy(&table.stdout);
    assert!(text.contains("Not recorded:"), "{text}");
    assert!(
        text.contains("unfinished"),
        "the table keeps unfinished turns visible: {text}"
    );
}
