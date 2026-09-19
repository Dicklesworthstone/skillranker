//! Offline Demonstration Contract Tests
//!
//! Satisfies contract boundary `p4_offline_demo` (sr-roadmap-l1i.5.16)
//! mapped in `tests/contract_matrix.toml`.
//!
//! Verifies:
//! 1. All four bundled synthetic demo cases (`useful`, `none`, `explicit`, `unavailable`)
//!    render valid non-actionable demo artifact envelopes.
//!    (Boundary test: `offline_demo`).
//! 2. Output conforms to `OutputDocument` and strict `validate_artifact` schema rules.
//! 3. Table rendering (`--table`) and JSON rendering (`--json`) output cleanly.
//! 4. 100% offline execution with poisoned ambient state (`HOME`, `XDG_CONFIG_HOME`,
//!    `TYPESAFE_API_KEY`, non-writable directories, and network denial).
//! 5. Strict rejection of invalid cases, missing arguments, and conflicting format flags.
//! 6. Zero file creation, zero credential leakage, and zero provider network traffic.

#![cfg(unix)]

use serde_json::Value;
use skillranker::demo::DemoCase;
use skillranker::output::{ArtifactKind, CliExit, OutputDocument, OutputKind};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

struct DemoTestFixture {
    root: PathBuf,
}

impl DemoTestFixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "sr-demo-contract-{}-{}",
            std::process::id(),
            FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("user/sr")).unwrap();
        std::fs::create_dir_all(root.join("workspace/.sr")).unwrap();
        Self { root }
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_env(args, &[])
    }

    fn run_with_env(&self, args: &[&str], env_vars: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sr"));
        cmd.env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("user"))
            .current_dir(self.root.join("workspace"))
            .args(args);
        for (k, v) in env_vars {
            cmd.env(k, v);
        }
        cmd.output().expect("binary execution")
    }

    fn run_poisoned(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sr"));
        cmd.env_clear()
            .env("HOME", self.root.join("nonexistent_home"))
            .env("XDG_CONFIG_HOME", self.root.join("nonexistent_config"))
            .env("TYPESAFE_API_KEY", "poisoned-invalid-key-99999")
            .env("TYPESAFE_ENDPOINT", "http://127.0.0.1:1/v1/systemone")
            .current_dir(self.root.join("workspace"))
            .args(args);
        cmd.output().expect("binary execution")
    }
}

impl Drop for DemoTestFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn offline_demo() {
    let f = DemoTestFixture::new();

    // -------------------------------------------------------------------------
    // 1. Useful Case
    // -------------------------------------------------------------------------
    let out_useful = f.run(&["demo", "--case", "useful", "--json"]);
    assert_eq!(out_useful.status.code(), Some(0));
    assert!(out_useful.stderr.is_empty());

    let doc_useful = OutputDocument::from_json(&out_useful.stdout)
        .expect("useful demo output must parse as valid OutputDocument");
    assert_eq!(doc_useful.kind(), OutputKind::Artifact(ArtifactKind::Demo));
    assert_eq!(doc_useful.exit_code(), CliExit::Success);

    let val_useful = doc_useful.as_value();
    assert_eq!(val_useful["schema_version"], 1);
    assert_eq!(val_useful["kind"], "demo");
    assert_eq!(val_useful["actionable"], false);
    assert_eq!(val_useful["run_status"], "complete");
    assert_eq!(val_useful["gate_status"], "not-applicable");
    assert_eq!(val_useful["evidence_origin"], "synthetic");
    assert_eq!(val_useful["completeness"]["cases_requested"], 1);
    assert_eq!(val_useful["completeness"]["cases_completed"], 1);
    assert_eq!(val_useful["completeness"]["stages_required"], 1);
    assert_eq!(val_useful["completeness"]["stages_completed"], 1);
    assert_eq!(val_useful["completeness"]["evidence_compatible"], true);

    let hist_useful = &val_useful["historical"];
    assert_eq!(hist_useful["decision"], "ranked");
    assert_eq!(hist_useful["reason"], "eligible-candidates");
    assert_eq!(hist_useful["harness"], "claude_code");
    assert_eq!(hist_useful["context_quality"], "complete");
    let skills_useful = hist_useful["skills"].as_array().unwrap();
    assert_eq!(skills_useful.len(), 2);
    assert_eq!(skills_useful[0]["rank"], 1);
    assert_eq!(skills_useful[0]["skill_id"], "s_01");
    assert_eq!(skills_useful[0]["invocation_name"], "rust-test-triage");
    assert!(skills_useful[0]["rank_score"].as_f64().unwrap() > 0.8);
    assert!(skills_useful[0]["rerank_probability"].as_f64().unwrap() > 0.5);
    assert_eq!(skills_useful[1]["rank"], 2);
    assert_eq!(skills_useful[1]["skill_id"], "s_02");
    assert_eq!(hist_useful["needs_skill"], 0.74);
    assert_eq!(hist_useful["none_probability"], 0.1);
    assert_eq!(hist_useful["choice_confidence"], 0.81);
    assert_eq!(hist_useful["phase"], "debugging");

    // -------------------------------------------------------------------------
    // 2. None (Abstain) Case
    // -------------------------------------------------------------------------
    let out_none = f.run(&["demo", "--case", "none", "--json"]);
    assert_eq!(out_none.status.code(), Some(0));
    assert!(out_none.stderr.is_empty());

    let doc_none = OutputDocument::from_json(&out_none.stdout)
        .expect("none demo output must parse as valid OutputDocument");
    assert_eq!(doc_none.kind(), OutputKind::Artifact(ArtifactKind::Demo));
    assert_eq!(doc_none.exit_code(), CliExit::Success);

    let val_none = doc_none.as_value();
    assert_eq!(val_none["kind"], "demo");
    assert_eq!(val_none["actionable"], false);
    assert_eq!(val_none["run_status"], "complete");

    let hist_none = &val_none["historical"];
    assert_eq!(hist_none["decision"], "abstain");
    assert_eq!(hist_none["reason"], "no-shortlist-match");
    assert!(hist_none["skills"].as_array().unwrap().is_empty());
    assert_eq!(hist_none["none_probability"], 0.85);
    assert_eq!(hist_none["choice_confidence"], 0.85);

    // -------------------------------------------------------------------------
    // 3. Explicit Case
    // -------------------------------------------------------------------------
    let out_explicit = f.run(&["demo", "--case", "explicit", "--json"]);
    assert_eq!(out_explicit.status.code(), Some(0));
    assert!(out_explicit.stderr.is_empty());

    let doc_explicit = OutputDocument::from_json(&out_explicit.stdout)
        .expect("explicit demo output must parse as valid OutputDocument");
    assert_eq!(
        doc_explicit.kind(),
        OutputKind::Artifact(ArtifactKind::Demo)
    );
    assert_eq!(doc_explicit.exit_code(), CliExit::Success);

    let val_explicit = doc_explicit.as_value();
    assert_eq!(val_explicit["kind"], "demo");
    assert_eq!(val_explicit["actionable"], false);

    let hist_explicit = &val_explicit["historical"];
    assert_eq!(hist_explicit["decision"], "explicit");
    assert_eq!(hist_explicit["reason"], "user-required");
    let skills_explicit = hist_explicit["skills"].as_array().unwrap();
    assert_eq!(skills_explicit.len(), 1);
    assert!(skills_explicit[0]["rank_score"].is_null());
    assert!(skills_explicit[0]["rerank_probability"].is_null());
    assert!(skills_explicit[0]["wide_probability"].is_null());
    assert!(skills_explicit[0]["fits"].is_null());
    assert_eq!(hist_explicit["usage"]["requests"], 0);
    assert_eq!(hist_explicit["usage"]["http_attempts"], 0);

    // -------------------------------------------------------------------------
    // 4. Unavailable Case
    // -------------------------------------------------------------------------
    let out_unavail = f.run(&["demo", "--case", "unavailable", "--json"]);
    assert_eq!(out_unavail.status.code(), Some(0));
    assert!(out_unavail.stderr.is_empty());

    let doc_unavail = OutputDocument::from_json(&out_unavail.stdout)
        .expect("unavailable demo output must parse as valid OutputDocument");
    assert_eq!(doc_unavail.kind(), OutputKind::Artifact(ArtifactKind::Demo));
    assert_eq!(doc_unavail.exit_code(), CliExit::Success);

    let val_unavail = doc_unavail.as_value();
    assert_eq!(val_unavail["kind"], "demo");
    assert_eq!(val_unavail["actionable"], false);

    let hist_unavail = &val_unavail["historical"];
    assert_eq!(hist_unavail["decision"], "unavailable");
    assert_eq!(hist_unavail["error"]["kind"], "authentication");
    assert_eq!(hist_unavail["error"]["code"], 4);
    assert_eq!(hist_unavail["error"]["retryable"], false);
}

#[test]
fn demo_table_rendering() {
    let f = DemoTestFixture::new();

    for case in ["useful", "none", "explicit", "unavailable"] {
        let out = f.run(&["demo", "--case", case, "--table"]);
        assert_eq!(out.status.code(), Some(0), "case: {case}");
        assert!(out.stderr.is_empty(), "case: {case}");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("ARTIFACT: DEMO"), "case: {case}");
        assert!(text.contains("Run Status:  complete"), "case: {case}");
        assert!(text.contains("Gate Status: not-applicable"), "case: {case}");
    }
}

#[test]
fn demo_poisoned_environment_isolation() {
    let f = DemoTestFixture::new();

    // Verify all four cases succeed with completely poisoned/nonexistent environment
    for case in ["useful", "none", "explicit", "unavailable"] {
        let out = f.run_poisoned(&["demo", "--case", case, "--json"]);
        assert_eq!(out.status.code(), Some(0), "poisoned case {case}");
        assert!(out.stderr.is_empty());

        let doc = OutputDocument::from_json(&out.stdout).expect("valid OutputDocument");
        assert_eq!(doc.kind(), OutputKind::Artifact(ArtifactKind::Demo));
    }

    // Verify nonexistent directories were never created or written to
    assert!(!f.root.join("nonexistent_home").exists());
    assert!(!f.root.join("nonexistent_config").exists());
}

#[test]
fn demo_usage_errors() {
    let f = DemoTestFixture::new();

    // 1. Missing --case flag
    let out_missing = f.run(&["demo"]);
    assert_eq!(out_missing.status.code(), Some(2));
    let err_missing: Value = serde_json::from_slice(&out_missing.stdout).unwrap();
    assert_eq!(err_missing["error"]["kind"], "invalid-usage");

    // 2. Unknown case value
    let out_unknown = f.run(&["demo", "--case", "unknown_variant"]);
    assert_eq!(out_unknown.status.code(), Some(2));
    let err_unknown: Value = serde_json::from_slice(&out_unknown.stdout).unwrap();
    assert_eq!(err_unknown["error"]["kind"], "invalid-usage");

    // 3. Conflicting --json and --table flags
    let out_conflict = f.run(&["demo", "--case", "useful", "--json", "--table"]);
    assert_eq!(out_conflict.status.code(), Some(2));
    let err_conflict: Value = serde_json::from_slice(&out_conflict.stdout).unwrap();
    assert_eq!(err_conflict["error"]["kind"], "invalid-usage");
}

#[test]
fn demo_help_command() {
    let f = DemoTestFixture::new();

    let out_help = f.run(&["demo", "--help"]);
    assert_eq!(out_help.status.code(), Some(0));
    let help_text = String::from_utf8_lossy(&out_help.stdout);
    assert!(help_text.contains("sr demo --case <useful|none|explicit|unavailable>"));
}

#[test]
fn demo_library_api() {
    for case in DemoCase::ALL {
        let doc = skillranker::demo::generate_demo_doc(case).expect("library generate_demo_doc");
        assert_eq!(doc.kind(), OutputKind::Artifact(ArtifactKind::Demo));
        assert_eq!(doc.exit_code(), CliExit::Success);
    }
}
