#![cfg(unix)]
//! A replayed case must reproduce what it recorded, or say it cannot
//! (sr-roadmap-l1i.6.14).
//!
//! Replay exists to turn a surprising suggestion into a reproducible example, so
//! a difference between the historical and recomputed decision has to mean
//! something changed. Three ways that promise was broken, each fixed here and
//! pinned by a case below:
//!
//! - `choice_confidence` came from the provider's stated confidence live, but
//!   from the chosen option's probability on replay. Two different numbers under
//!   one name, so every comparison showed a regression nobody caused. Measured
//!   on a real captured case: 0.87 historical against 0.94 recomputed.
//! - `visibility` was dropped, so a replayed recommendation lost the caveat that
//!   the harness's precedence is unverified.
//! - `path` was fabricated as `.claude/skills/<name>/SKILL.md`, a filesystem
//!   location the case never recorded and which is wrong for a project skill or
//!   a configured root.
//!
//! A case captured before the provider confidence was recorded cannot reproduce
//! a ranked decision's confidence, and the output contract requires a ranked
//! decision to carry one. Such a case reports `partial` with no recomputation
//! rather than inventing the number.
use serde_json::{Value, json};
use skillranker::output::SCHEMA_VERSION;
use skillranker::replay::{
    CandidateFitItem, CapturedCandidate, CapturedLocalEvidence, CapturedRequest,
    CapturedScoringProfile, ChoiceDistributionItem, RecordedRerankChoice, RecordedResponses,
    RecordedWideChoice, ReplayCase, ReplayManifest,
};
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

// Intentionally retained: repository policy forbids automatic tree deletion.
fn fixture_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "sr-replay-completeness-{}-{}-{}",
        name,
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    ));
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&root)
        .unwrap();
    for dir in ["home", "config", "workspace"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    root
}

fn replay(root: &Path, case: &Path) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_sr"))
        .env_clear()
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .current_dir(root.join("workspace"))
        .args(["replay", case.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    serde_json::from_slice(&output.stdout).unwrap()
}

/// A ranked case whose recorded rerank carries the provider's own confidence.
/// `stated_confidence` is deliberately distinct from `choices_probability` so a
/// substitution of one for the other is visible rather than coincidentally equal.
fn case_with_confidence(stated: Option<f64>, visibility: Option<&str>) -> ReplayCase {
    // The shipped ranked fixture is a valid document; only the skill ids are
    // renamed to match this case's candidates, exactly as the CLI contract test
    // does. Its own `choice_confidence` (0.81) is the value a faithful replay
    // has to reproduce.
    let historical = {
        let mut v: Value =
            serde_json::from_str(include_str!("fixtures/output-ranked.v1.json")).unwrap();
        v["skills"][0]["skill_id"] = json!("s_triage");
        v["skills"][0]["name"] = json!("rust-test-triage");
        v["skills"][0]["invocation_name"] = json!("rust-test-triage");
        v["skills"][1]["skill_id"] = json!("s_review");
        v["skills"][1]["name"] = json!("rust-code-review");
        v["skills"][1]["invocation_name"] = json!("rust-code-review");
        v
    };
    ReplayCase {
        schema_version: SCHEMA_VERSION,
        case_id: "case-confidence".into(),
        created_at_unix_ms: 1_700_000_000,
        manifest: ReplayManifest {
            evidence_origin: "recorded".into(),
            adapter: "claude_code".into(),
            model: Some("jev-latest".into()),
            stages_recorded: vec!["wide".into(), "rerank".into()],
            prompt_summary: Some("Triage failing rust tests".into()),
        },
        captured_request: CapturedRequest {
            context_text: Some("tests are failing".into()),
            current_constraints: Vec::new(),
            candidate_options: vec![
                CapturedCandidate {
                    skill_id: "s_triage".into(),
                    invocation_name: "rust-test-triage".into(),
                    visibility: visibility.map(str::to_owned),
                    content_hash:
                        "0000000000000000000000000000000000000000000000000000000000000001".into(),
                    source: "workspace".into(),
                    usage_kind: "workflow".into(),
                    description: Some("Triage failing rust tests".into()),
                    excerpt: None,
                },
                CapturedCandidate {
                    skill_id: "s_review".into(),
                    invocation_name: "rust-code-review".into(),
                    visibility: visibility.map(str::to_owned),
                    content_hash:
                        "0000000000000000000000000000000000000000000000000000000000000002".into(),
                    source: "workspace".into(),
                    usage_kind: "workflow".into(),
                    description: Some("Review rust code".into()),
                    excerpt: None,
                },
            ],
        },
        recorded_responses: RecordedResponses {
            wide: Some(RecordedWideChoice {
                choice: "s_triage".into(),
                choices_probability: 0.80,
                gate_score: Some(0.80),
                distribution: vec![
                    ChoiceDistributionItem {
                        option_id: "s_triage".into(),
                        probability: 0.80,
                    },
                    ChoiceDistributionItem {
                        option_id: "__none__".into(),
                        probability: 0.20,
                    },
                ],
            }),
            rerank: Some(RecordedRerankChoice {
                choice: "s_triage".into(),
                // Deliberately different from `stated_confidence` below.
                choices_probability: 0.94,
                stated_confidence: stated,
                fits: vec![CandidateFitItem {
                    skill_id: "s_triage".into(),
                    fit: 0.90,
                }],
                distribution: vec![
                    ChoiceDistributionItem {
                        option_id: "s_triage".into(),
                        probability: 0.94,
                    },
                    ChoiceDistributionItem {
                        option_id: "__none__".into(),
                        probability: 0.06,
                    },
                ],
            }),
        },
        local_evidence: CapturedLocalEvidence {
            as_of_unix_ms: 1_700_000_000,
            active_snoozes: Vec::new(),
            loaded_references: Vec::new(),
            scoring_profile: CapturedScoringProfile {
                gate_threshold: 0.3,
                fit_threshold: 0.3,
                w_fit: 1.0,
                w_prior: 0.0,
                w_phase: 0.0,
                top_k: 5,
            },
        },
        historical_decision: historical,
    }
}

fn write_case(root: &Path, case: &ReplayCase) -> PathBuf {
    let path = root.join("case.json");
    fs::write(&path, serde_json::to_vec(case).unwrap()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    path
}

#[test]
fn a_recorded_confidence_is_reproduced_not_replaced_by_a_probability() {
    let root = fixture_root("confidence");
    let case = case_with_confidence(Some(0.81), Some("unverified"));
    let path = write_case(&root, &case);
    let value = replay(&root, &path);
    assert_eq!(value["run_status"], "complete", "{value}");
    let recomputed = &value["recomputed"];
    // The provider's stated confidence, not the 0.94 chosen-option probability.
    assert_eq!(recomputed["choice_confidence"], 0.81, "{value}");
    assert_eq!(
        value["historical"]["choice_confidence"], recomputed["choice_confidence"],
        "an unchanged case must not show a confidence difference: {value}"
    );
}

#[test]
fn a_case_without_a_recorded_confidence_declines_to_recompute() {
    // The output contract requires a ranked decision to carry a confidence, so
    // a case that cannot supply one is partial rather than ranked-with-a-guess.
    let root = fixture_root("legacy");
    let case = case_with_confidence(None, Some("unverified"));
    let path = write_case(&root, &case);
    let value = replay(&root, &path);
    assert_eq!(value["run_status"], "partial", "{value}");
    assert!(value["recomputed"].is_null(), "{value}");
    assert_eq!(
        value["completeness"]["evidence_compatible"], false,
        "evidence that cannot reproduce the decision is not compatible: {value}"
    );
    // The historical decision is still reported verbatim.
    assert_eq!(value["historical"]["choice_confidence"], 0.81, "{value}");
}

#[test]
fn a_replayed_recommendation_keeps_its_visibility_caveat() {
    let root = fixture_root("visibility");
    let case = case_with_confidence(Some(0.81), Some("unverified"));
    let path = write_case(&root, &case);
    let value = replay(&root, &path);
    assert_eq!(
        value["recomputed"]["skills"][0]["visibility"], "unverified",
        "a replayed suggestion must not silently lose that visibility is unverified: {value}"
    );
}

#[test]
fn a_replay_states_no_filesystem_path_it_never_recorded() {
    let root = fixture_root("path");
    let case = case_with_confidence(Some(0.81), Some("unverified"));
    let path = write_case(&root, &case);
    let value = replay(&root, &path);
    let recomputed_path = &value["recomputed"]["skills"][0]["path"];
    assert!(
        recomputed_path.is_null(),
        "a case records no load path, so replay reports none rather than guessing: {value}"
    );
    let text = serde_json::to_string(&value["recomputed"]).unwrap();
    assert!(
        !text.contains(".claude/skills/rust-test-triage/SKILL.md"),
        "no fabricated layout path: {text}"
    );
}

#[test]
fn a_case_without_visibility_omits_it_rather_than_inventing_one() {
    let root = fixture_root("no-visibility");
    let case = case_with_confidence(Some(0.81), None);
    let path = write_case(&root, &case);
    let value = replay(&root, &path);
    let skill = &value["recomputed"]["skills"][0];
    assert!(
        skill.get("visibility").is_none(),
        "an uncaptured visibility is absent, not assumed verified: {value}"
    );
}
