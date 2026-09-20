//! Integration tests for `sr replay` CLI subcommand (sr-roadmap-l1i.6.15, sr-roadmap-l1i.6.16).
//!
//! Validates:
//! 1. `sr replay --help` exits 0 and advertises the command.
//! 2. Missing arguments exit 2 with `invalid-usage`.
//! 3. Replay evaluates cases offline with zero network, child processes, or state writes.
//! 4. JSON and table formats are supported.
//! 5. Policy overrides and comparisons execute without ambient configuration.
//! 6. Owner-only file permissions (0600/0400) are enforced.
//! 7. Incompatible policies (such as uncaptured priors) are rejected.

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
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn temp_workspace(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "sr-replay-cli-{}-{}-{}",
        name,
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    ));
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&root)
        .unwrap();
    fs::create_dir_all(root.join("home")).unwrap();
    fs::create_dir_all(root.join("config")).unwrap();
    fs::create_dir_all(root.join("workspace")).unwrap();
    root
}

fn run_sr(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sr"))
        .env_clear()
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .current_dir(root.join("workspace"))
        .args(args)
        .output()
        .unwrap()
}

fn ranked_decision_fixture() -> Value {
    let mut v: Value =
        serde_json::from_str(include_str!("fixtures/output-ranked.v1.json")).unwrap();
    v["skills"][0]["skill_id"] = json!("s_triage");
    v["skills"][0]["name"] = json!("rust-test-triage");
    v["skills"][0]["invocation_name"] = json!("rust-test-triage");
    v["skills"][1]["skill_id"] = json!("s_review");
    v["skills"][1]["name"] = json!("rust-code-review");
    v["skills"][1]["invocation_name"] = json!("rust-code-review");
    v
}

fn sample_ranked_case() -> ReplayCase {
    ReplayCase {
        schema_version: SCHEMA_VERSION,
        case_id: "case-cli-test-001".into(),
        created_at_unix_ms: 1726700000000,
        manifest: ReplayManifest {
            evidence_origin: "recorded".into(),
            adapter: "claude_code".into(),
            model: Some("claude-3-7-sonnet".into()),
            stages_recorded: vec!["wide".into(), "rerank".into()],
            prompt_summary: Some("Triage failing rust tests".into()),
        },
        captured_request: CapturedRequest {
            context_text: Some("We have an issue in our tests".into()),
            current_constraints: vec!["no release work".into()],
            candidate_options: vec![
                CapturedCandidate {
                    skill_id: "s_triage".into(),
                    invocation_name: "rust-test-triage".into(),
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
                choices_probability: 0.85,
                gate_score: Some(0.85),
                distribution: vec![
                    ChoiceDistributionItem {
                        option_id: "s_triage".into(),
                        probability: 0.85,
                    },
                    ChoiceDistributionItem {
                        option_id: "s_review".into(),
                        probability: 0.10,
                    },
                    ChoiceDistributionItem {
                        option_id: "__none__".into(),
                        probability: 0.05,
                    },
                ],
            }),
            rerank: Some(RecordedRerankChoice {
                choice: "s_triage".into(),
                choices_probability: 0.80,
                fits: vec![
                    CandidateFitItem {
                        skill_id: "s_triage".into(),
                        fit: 0.90,
                    },
                    CandidateFitItem {
                        skill_id: "s_review".into(),
                        fit: 0.40,
                    },
                ],
                distribution: vec![
                    ChoiceDistributionItem {
                        option_id: "s_triage".into(),
                        probability: 0.80,
                    },
                    ChoiceDistributionItem {
                        option_id: "s_review".into(),
                        probability: 0.15,
                    },
                    ChoiceDistributionItem {
                        option_id: "__none__".into(),
                        probability: 0.05,
                    },
                ],
            }),
        },
        local_evidence: CapturedLocalEvidence {
            as_of_unix_ms: 1726700000000,
            active_snoozes: Vec::new(),
            loaded_references: Vec::new(),
            scoring_profile: CapturedScoringProfile {
                gate_threshold: 0.30,
                fit_threshold: 0.30,
                w_fit: 1.0,
                w_prior: 0.0,
                w_phase: 0.0,
                top_k: 5,
            },
        },
        historical_decision: ranked_decision_fixture(),
    }
}

fn write_case_file(path: &Path, case: &ReplayCase) {
    let json_bytes = serde_json::to_vec_pretty(case).unwrap();
    fs::write(path, json_bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn write_policy_file(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn replay_help_succeeds_and_documents_command() {
    let root = temp_workspace("help");
    let output = run_sr(&root, &["replay", "--help"]);
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("sr replay"));
    assert!(text.contains("--policy"));
    assert!(text.contains("--compare-policy"));
}

#[test]
fn replay_missing_case_arg_exits_with_invalid_usage() {
    let root = temp_workspace("missing-arg");
    let output = run_sr(&root, &["replay", "--json"]);
    assert_eq!(output.status.code(), Some(2));
    let err: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(err["error"]["kind"], "invalid-usage");
}

#[test]
fn replay_nonexistent_case_file_fails_with_structured_error() {
    let root = temp_workspace("nonexistent");
    let output = run_sr(&root, &["replay", "nonexistent-case.json", "--json"]);
    assert_ne!(output.status.code(), Some(0));
    let err: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(err["decision"], "unavailable");
}

#[test]
fn replay_runs_recorded_case_with_json_envelope() {
    let root = temp_workspace("roundtrip-json");
    let case = sample_ranked_case();
    let case_path = root.join("workspace/case.json");
    write_case_file(&case_path, &case);

    let output = run_sr(&root, &["replay", "case.json", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let val: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(val["schema_version"], 1);
    assert_eq!(val["kind"], "replay");
    assert_eq!(val["actionable"], false);
    assert_eq!(val["run_status"], "complete");
    assert_eq!(val["gate_status"], "passed");
    assert_eq!(val["historical"]["decision"], "ranked");
    assert_eq!(val["recomputed"]["decision"], "ranked");
}

#[test]
fn replay_renders_table_format() {
    let root = temp_workspace("table");
    let case = sample_ranked_case();
    let case_path = root.join("workspace/case.json");
    write_case_file(&case_path, &case);

    let output = run_sr(&root, &["replay", "case.json", "--table"]);
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("ARTIFACT: REPLAY"));
    assert!(text.contains("Run Status:  complete"));
    assert!(text.contains("Gate Status: passed"));
}

#[test]
fn replay_with_policy_override_recomputes_decision() {
    let root = temp_workspace("policy-override");
    let case = sample_ranked_case();
    let case_path = root.join("workspace/case.json");
    write_case_file(&case_path, &case);

    // Stricter fit threshold: 0.95 (which s_triage at 0.90 fails)
    let policy_path = root.join("workspace/strict_policy.json");
    write_policy_file(&policy_path, r#"{"fit_threshold": 0.95}"#);

    let output = run_sr(
        &root,
        &[
            "replay",
            "case.json",
            "--policy",
            "strict_policy.json",
            "--json",
        ],
    );
    assert_eq!(output.status.code(), Some(0));
    let val: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(val["run_status"], "complete");
    assert_eq!(val["historical"]["decision"], "ranked");
    assert_eq!(val["recomputed"]["decision"], "abstain");
}

#[test]
fn replay_with_policy_and_compare_policy() {
    let root = temp_workspace("policy-comparison");
    let case = sample_ranked_case();
    let case_path = root.join("workspace/case.json");
    write_case_file(&case_path, &case);

    let baseline_path = root.join("workspace/baseline.json");
    write_policy_file(&baseline_path, r#"{"fit_threshold": 0.30}"#);

    let candidate_path = root.join("workspace/candidate.json");
    write_policy_file(&candidate_path, r#"{"fit_threshold": 0.95}"#);

    let output = run_sr(
        &root,
        &[
            "replay",
            "case.json",
            "--policy",
            "baseline.json",
            "--compare-policy",
            "candidate.json",
            "--json",
        ],
    );
    assert_eq!(output.status.code(), Some(0));
    let val: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(val["recomputed"]["decision"], "ranked");
    assert_eq!(val["comparison"]["decision"], "abstain");
}

#[test]
fn replay_rejects_incompatible_policy_with_uncaptured_prior() {
    let root = temp_workspace("incompatible-policy");
    let case = sample_ranked_case();
    let case_path = root.join("workspace/case.json");
    write_case_file(&case_path, &case);

    let policy_path = root.join("workspace/prior_policy.json");
    write_policy_file(&policy_path, r#"{"w_prior": 0.5}"#);

    let output = run_sr(
        &root,
        &[
            "replay",
            "case.json",
            "--policy",
            "prior_policy.json",
            "--json",
        ],
    );
    assert_eq!(output.status.code(), Some(2));
    let val: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(val["error"]["kind"], "invalid-configuration");
}

#[test]
fn replay_rejects_non_owner_only_permissions() {
    let root = temp_workspace("permissions");
    let case = sample_ranked_case();
    let case_path = root.join("workspace/case.json");
    let json_bytes = serde_json::to_vec_pretty(&case).unwrap();
    fs::write(&case_path, json_bytes).unwrap();
    // Insecure permissions (0666 world-writable/readable)
    fs::set_permissions(&case_path, fs::Permissions::from_mode(0o666)).unwrap();

    let output = run_sr(&root, &["replay", "case.json", "--json"]);
    assert_ne!(output.status.code(), Some(0));
    let val: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(val["decision"], "unavailable");
}
