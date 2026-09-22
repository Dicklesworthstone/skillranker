//! Comprehensive verification of bounded versioned JSON and stable failure envelopes.
//!
//! Satisfies contract boundary `p4_versioned_json_failure_envelopes` (sr-roadmap-l1i.5.12).
//!
//! Verifies:
//! 1. All 28 ErrorKind variants map to their exact documented CLI exit codes.
//! 2. OutputDocument::failure_with_details sanitizes control characters, bounds text,
//!    and round-trips through strict from_json validation.
//! 3. Unresolved explicit references attach safely up to 32 items with typed reasons.
//! 4. CLI binary executions emit valid JSON envelopes on failure with matching exit codes.
//! 5. Private secrets and canary values are never leaked in error diagnostics.
//! 6. Report and replay artifacts strictly enforce non-actionability and honest gate statuses.

use serde_json::{Value, json};
use skillranker::output::{
    ArtifactKind, CliExit, ContractError, Decision, ErrorKind, MAX_TEXT_BYTES, OutputDocument,
    OutputKind, UnresolvedReason, UnresolvedReference, sanitize_diagnostic_text,
};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

struct CliFixture {
    root: PathBuf,
    workspace: PathBuf,
}

impl CliFixture {
    fn new() -> Self {
        let id = FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("sr-envelope-test-{}-{}", std::process::id(), id));
        let workspace = root.join("workspace");
        let skills_dir = workspace.join(".claude/skills");
        fs::create_dir_all(&skills_dir).unwrap();
        fs::create_dir_all(root.join("user/sr")).unwrap();
        Self { root, workspace }
    }

    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sr"));
        cmd.env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("user"))
            .current_dir(&self.workspace)
            .args(args);
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output().expect("execute sr binary")
    }

    fn create_context(&self, prompt: &str) -> PathBuf {
        let ctx_file = self.workspace.join("context.json");
        let ctx = json!({
            "schema_version": 1,
            "harness": "claude_code",
            "producer_id": null,
            "workspace_root": self.workspace.to_string_lossy(),
            "session_id": "session-envelope-1",
            "agent_id": null,
            "branch_id": null,
            "context_epoch": null,
            "current_request": {
                "event_id": null,
                "text": prompt,
                "attachments_omitted": false,
                "essential_attachment_missing": false
            },
            "events": [],
            "explicit_skill_references": [],
            "supplied_loads": []
        });
        fs::write(&ctx_file, serde_json::to_vec_pretty(&ctx).unwrap()).unwrap();
        ctx_file
    }

    fn create_skill(&self, name: &str) -> PathBuf {
        let skill_dir = self.workspace.join(".claude/skills").join(name);
        fs::create_dir_all(&skill_dir).unwrap();
        let file = skill_dir.join("SKILL.md");
        fs::write(
            &file,
            format!("---\nname: {name}\ndescription: Test skill {name}\n---\nBody of {name}\n"),
        )
        .unwrap();
        file
    }
}

impl Drop for CliFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn test_all_28_error_kinds_and_documented_exit_categories() {
    let expected_mappings = [
        // Category 2: Usage / Configuration
        (2u8, ErrorKind::InvalidUsage, "invalid-usage"),
        (
            2u8,
            ErrorKind::InvalidConfiguration,
            "invalid-configuration",
        ),
        // Category 3: Session
        (3u8, ErrorKind::MissingSession, "missing-session"),
        (3u8, ErrorKind::AmbiguousSession, "ambiguous-session"),
        (3u8, ErrorKind::AmbiguousBranch, "ambiguous-branch"),
        (3u8, ErrorKind::Superseded, "superseded"),
        // Category 4: Provider / Network / Budget
        (4u8, ErrorKind::ProviderFailure, "provider-failure"),
        (4u8, ErrorKind::Authentication, "authentication"),
        (4u8, ErrorKind::CredentialAbsent, "credential-absent"),
        (4u8, ErrorKind::NetworkFailure, "network-failure"),
        (4u8, ErrorKind::RequestBudget, "request-budget"),
        (4u8, ErrorKind::ProviderCooldown, "provider-cooldown"),
        (4u8, ErrorKind::BudgetState, "budget-state"),
        // Category 5: Roster / Retrieval
        (5u8, ErrorKind::EmptyRoster, "empty-roster"),
        (5u8, ErrorKind::UnusableRoster, "unusable-roster"),
        (5u8, ErrorKind::UnresolvedExplicit, "unresolved-explicit"),
        (5u8, ErrorKind::IncompleteRoster, "incomplete-roster"),
        (5u8, ErrorKind::RosterChanged, "roster-changed"),
        (5u8, ErrorKind::RetrievalEmpty, "retrieval-empty"),
        (5u8, ErrorKind::RetrievalFailure, "retrieval-failure"),
        // Category 6: Timeout
        (6u8, ErrorKind::Timeout, "timeout"),
        // Category 7: Input / Adapter
        (7u8, ErrorKind::MalformedInput, "malformed-input"),
        (7u8, ErrorKind::OversizedInput, "oversized-input"),
        (7u8, ErrorKind::UnsupportedInput, "unsupported-input"),
        (
            7u8,
            ErrorKind::UnsupportedSourceMode,
            "unsupported-source-mode",
        ),
        (7u8, ErrorKind::InsufficientContext, "insufficient-context"),
        (7u8, ErrorKind::OutputLimit, "output-limit"),
        // Category 8: Privacy
        (8u8, ErrorKind::NetworkDenied, "network-denied"),
        // Category 9: Storage
        (9u8, ErrorKind::StorageFailure, "storage-failure"),
        (9u8, ErrorKind::RevisionConflict, "revision-conflict"),
        // Category 10: Provider Contract
        (
            10u8,
            ErrorKind::InvalidProviderResponse,
            "invalid-provider-response",
        ),
        // Category 11: Cache Miss
        (11u8, ErrorKind::CacheMiss, "cache-miss"),
    ];

    assert_eq!(expected_mappings.len(), ErrorKind::ALL.len());

    for (expected_code, kind, expected_name) in expected_mappings {
        assert_eq!(kind.as_str(), expected_name);
        assert_eq!(kind.exit_code() as u8, expected_code);

        for retryable in [false, true] {
            let doc = OutputDocument::failure_with_details(
                kind,
                &format!("Detailed failure message for {expected_name}"),
                "Detailed diagnostic hint.",
                retryable,
            );

            assert_eq!(doc.kind(), OutputKind::Decision(Decision::Unavailable));
            assert_eq!(doc.exit_code() as u8, expected_code);

            let wire = doc.to_json().expect("serializes to wire bytes");
            let roundtrip = OutputDocument::from_json(&wire).expect("roundtrip parses cleanly");

            assert_eq!(roundtrip.kind(), doc.kind());
            assert_eq!(roundtrip.exit_code(), doc.exit_code());

            let val = roundtrip.as_value();
            assert_eq!(val["schema_version"], 1);
            assert_eq!(val["decision"], "unavailable");
            assert_eq!(val["error"]["code"], expected_code);
            assert_eq!(val["error"]["kind"], expected_name);
            assert_eq!(val["error"]["retryable"], retryable);
        }
    }
}

#[test]
fn test_control_character_sanitization_and_length_bounding() {
    let dirty_message = "Line 1\nLine 2\r\nLine 3\tTab\x00Null\x07Bell";
    let sanitized = sanitize_diagnostic_text(dirty_message, "fallback");
    assert!(!sanitized.chars().any(char::is_control));
    assert!(sanitized.contains("Line 1 Line 2  Line 3 Tab Null Bell"));

    // Empty text falls back to default
    assert_eq!(
        sanitize_diagnostic_text("   \n\t  ", "default message"),
        "default message"
    );

    // Huge message is safely bounded without splitting unicode multi-byte characters
    let huge = "🦀".repeat(3000); // 12,000 bytes
    let bounded = sanitize_diagnostic_text(&huge, "fallback");
    assert!(bounded.len() <= MAX_TEXT_BYTES);
    assert!(bounded.chars().all(|c| c == '🦀'));

    // Verify OutputDocument::failure_with_details handles dirty input cleanly
    let doc = OutputDocument::failure_with_details(
        ErrorKind::MalformedInput,
        dirty_message,
        "Another\nDirty\x00Hint",
        false,
    );
    let wire = doc.to_json().expect("clean serialization");
    let parsed = OutputDocument::from_json(&wire).expect("valid wire document");
    assert_eq!(parsed.kind(), OutputKind::Decision(Decision::Unavailable));
}

#[test]
fn test_unresolved_explicit_skill_references() {
    let refs = vec![
        UnresolvedReference {
            reference: "skill-missing-1".into(),
            reason: UnresolvedReason::Missing,
        },
        UnresolvedReference {
            reference: "skill-ambiguous-2".into(),
            reason: UnresolvedReason::Ambiguous,
        },
        UnresolvedReference {
            reference: "skill-restricted-3".into(),
            reason: UnresolvedReason::Restricted,
        },
    ];

    let doc = OutputDocument::failure_with_details(
        ErrorKind::UnresolvedExplicit,
        "Failed to resolve required skills",
        "Inspect roster availability.",
        false,
    )
    .with_unresolved(refs)
    .expect("valid document with unresolved references");

    let wire = doc.to_json().expect("valid serialization");
    let parsed = OutputDocument::from_json(&wire).expect("valid from_json");

    let val = parsed.as_value();
    let unresolved = val["unresolved"].as_array().expect("unresolved array");
    assert_eq!(unresolved.len(), 3);
    assert_eq!(unresolved[0]["reference"], "skill-missing-1");
    assert_eq!(unresolved[0]["reason"], "missing");
    assert_eq!(unresolved[1]["reference"], "skill-ambiguous-2");
    assert_eq!(unresolved[1]["reason"], "ambiguous");
    assert_eq!(unresolved[2]["reference"], "skill-restricted-3");
    assert_eq!(unresolved[2]["reason"], "restricted");

    // Bounded cap at 32 items
    let too_many: Vec<UnresolvedReference> = (0..33)
        .map(|i| UnresolvedReference {
            reference: format!("skill-{i}"),
            reason: UnresolvedReason::Missing,
        })
        .collect();

    let overflow =
        OutputDocument::failure(ErrorKind::UnresolvedExplicit, false).with_unresolved(too_many);
    assert_eq!(overflow.unwrap_err(), ContractError::LimitExceeded);
}

#[test]
fn test_cli_binary_failure_envelopes() {
    let f = CliFixture::new();

    // 1. Bare `sr` is `sr rank` (AGENTS.md: "Bare sr ranks once"). With no
    // session source it fails with a typed envelope, not a usage error.
    let out_bare = f.run(&["--json"], &[]);
    assert_ne!(out_bare.status.code(), Some(0));
    assert_ne!(out_bare.status.code(), Some(2));
    let val_bare: Value = serde_json::from_slice(&out_bare.stdout).expect("valid JSON stdout");
    assert_eq!(val_bare["schema_version"], 1);
    assert_eq!(val_bare["decision"], "unavailable");
    assert_eq!(
        val_bare["error"]["code"].as_i64(),
        out_bare.status.code().map(i64::from)
    );
    assert_ne!(val_bare["error"]["kind"], "invalid-usage");
    assert_eq!(val_bare["error"]["retryable"], false);

    // 2. Conflicting flags -> exit 2 invalid-usage
    let out_conflicts = f.run(&["rank", "--offline", "--allow-network", "--json"], &[]);
    assert_eq!(out_conflicts.status.code(), Some(2));
    let val_conflicts: Value =
        serde_json::from_slice(&out_conflicts.stdout).expect("valid JSON stdout");
    assert_eq!(val_conflicts["error"]["code"], 2);
    assert_eq!(val_conflicts["error"]["kind"], "invalid-usage");

    // 3. Network denied when allow-network is missing -> exit 8 network-denied
    f.create_skill("skill_1");
    let ctx_file = f.create_context("Need help with coding");
    let out_net = f.run(
        &["rank", "--context", ctx_file.to_str().unwrap(), "--json"],
        &[("TYPESAFE_API_KEY", "test-secret-key-443322")],
    );
    assert_eq!(out_net.status.code(), Some(8));
    let val_net: Value = serde_json::from_slice(&out_net.stdout).expect("valid JSON stdout");
    assert_eq!(val_net["schema_version"], 1);
    assert_eq!(val_net["decision"], "unavailable");
    assert_eq!(val_net["error"]["code"], 8);
    assert_eq!(val_net["error"]["kind"], "network-denied");
    // Ensure API key secret was never leaked in stdout or stderr
    assert!(!String::from_utf8_lossy(&out_net.stdout).contains("test-secret-key-443322"));
    assert!(!String::from_utf8_lossy(&out_net.stderr).contains("test-secret-key-443322"));

    // 4. Empty roster -> exit 5 empty-roster
    let empty_workspace = f.root.join("empty_workspace");
    fs::create_dir_all(&empty_workspace).unwrap();
    let empty_ctx = empty_workspace.join("context.json");
    let ctx_data = json!({
        "schema_version": 1,
        "harness": "claude_code",
        "producer_id": null,
        "workspace_root": empty_workspace.to_string_lossy(),
        "session_id": "session-empty",
        "agent_id": null,
        "branch_id": null,
        "context_epoch": null,
        "current_request": {
            "event_id": null,
            "text": "hello",
            "attachments_omitted": false,
            "essential_attachment_missing": false
        },
        "events": [],
        "explicit_skill_references": [],
        "supplied_loads": []
    });
    fs::write(&empty_ctx, serde_json::to_vec(&ctx_data).unwrap()).unwrap();

    // An empty roster needs an empty home too: user skills live in
    // $HOME/.claude/skills, so the ambient home must not leak in.
    let out_empty = Command::new(env!("CARGO_BIN_EXE_sr"))
        .env_clear()
        .env("HOME", f.root.join("empty_home"))
        .current_dir(&empty_workspace)
        .args([
            "rank",
            "--context",
            empty_ctx.to_str().unwrap(),
            "--offline",
            "--json",
        ])
        .output()
        .expect("run binary");
    assert_eq!(out_empty.status.code(), Some(5));
    let val_empty: Value = serde_json::from_slice(&out_empty.stdout).expect("valid JSON stdout");
    assert_eq!(val_empty["error"]["code"], 5);
    assert_eq!(val_empty["error"]["kind"], "empty-roster");

    // 5. Unresolved explicit requirement -> exit 5 unresolved-explicit with structured unresolved list
    let out_unresolved = f.run(
        &[
            "rank",
            "--context",
            ctx_file.to_str().unwrap(),
            "--require-skill",
            "nonexistent-skill-xyz",
            "--offline",
            "--json",
        ],
        &[],
    );
    assert_eq!(out_unresolved.status.code(), Some(5));
    let val_unresolved: Value =
        serde_json::from_slice(&out_unresolved.stdout).expect("valid JSON stdout");
    assert_eq!(val_unresolved["decision"], "unavailable");
    assert_eq!(val_unresolved["error"]["code"], 5);
    assert_eq!(val_unresolved["error"]["kind"], "unresolved-explicit");
    let unres = val_unresolved["unresolved"]
        .as_array()
        .expect("unresolved array present");
    assert_eq!(unres.len(), 1);
    assert_eq!(unres[0]["reference"], "nonexistent-skill-xyz");
    assert_eq!(unres[0]["reason"], "missing");

    // 6. Malformed context input -> exit 7 malformed-input
    let malformed_ctx = f.workspace.join("malformed.json");
    fs::write(&malformed_ctx, b"not valid json {{{").unwrap();
    let out_malformed = f.run(
        &[
            "rank",
            "--context",
            malformed_ctx.to_str().unwrap(),
            "--offline",
            "--json",
        ],
        &[],
    );
    assert_eq!(out_malformed.status.code(), Some(7));
    let val_malformed: Value =
        serde_json::from_slice(&out_malformed.stdout).expect("valid JSON stdout");
    assert_eq!(val_malformed["error"]["code"], 7);
    assert_eq!(val_malformed["error"]["kind"], "malformed-input");
}

#[test]
fn test_artifact_report_semantics_never_imply_passed_gates() {
    let base_report = json!({
        "schema_version": 1,
        "kind": "report",
        "actionable": false,
        "run_status": "complete",
        "gate_status": "not-established",
        "completeness": {
            "cases_requested": 10,
            "cases_completed": 10,
            "stages_required": 20,
            "stages_completed": 20,
            "evidence_compatible": true
        },
        "evidence_origin": "synthetic"
    });

    // Valid report with gate_status: not-established
    let doc = OutputDocument::from_value(base_report.clone()).expect("valid report document");
    assert_eq!(doc.kind(), OutputKind::Artifact(ArtifactKind::Report));
    assert_eq!(doc.exit_code(), CliExit::Success);

    // Synthetic origin CANNOT claim gate_status: passed
    let mut synthetic_passed = base_report.clone();
    synthetic_passed["gate_status"] = json!("passed");
    assert_eq!(
        OutputDocument::from_value(synthetic_passed).unwrap_err(),
        ContractError::InconsistentFields
    );

    // Partial run status CANNOT claim gate_status: passed
    let mut partial_passed = base_report.clone();
    partial_passed["evidence_origin"] = json!("recorded");
    partial_passed["run_status"] = json!("partial");
    partial_passed["gate_status"] = json!("passed");
    assert_eq!(
        OutputDocument::from_value(partial_passed).unwrap_err(),
        ContractError::InconsistentFields
    );

    // Actionable must never be true for artifacts
    let mut actionable_artifact = base_report.clone();
    actionable_artifact["actionable"] = json!(true);
    assert_eq!(
        OutputDocument::from_value(actionable_artifact).unwrap_err(),
        ContractError::InconsistentFields
    );
}
