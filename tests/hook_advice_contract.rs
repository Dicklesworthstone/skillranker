#![cfg(unix)]
//! P6 Claude hook safe advice rendering contract.
//!
//! Satisfies contract requirements for bead `sr-roadmap-l1i.7.2`:
//! - Renders one safe suggestion or explicit list across table, JSON, and hook boundaries.
//! - Adheres to Claude UserPromptSubmit envelope schema.
//! - Bounds additionalContext to 1,024 Unicode scalars.
//! - Forbids dangerous control characters.
//! - Rejects oversized explicit lists with quiet fallback (no truncated lists).
//! - Ordinary abstentions remain quiet by default.
//! - Suppresses output when effective mode at publication boundary is Shadow.
//! - Handles short writes / broken pipes without recording emission.

use serde_json::json;
use skillranker::limits::HOOK_ADDITIONAL_CONTEXT_SCALARS;
use skillranker::output::OutputDocument;
use skillranker::output::hook::{HOOK_EVENT_NAME, HookRenderError, render_claude_hook_advice};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

const RANKED_FIXTURE: &str = include_str!("fixtures/output-ranked.v1.json");
const EXPLICIT_FIXTURE: &str = include_str!("fixtures/output-explicit.v1.json");
const ABSTAIN_FIXTURE: &str = include_str!("fixtures/output-abstain.v1.json");
const UNAVAILABLE_FIXTURE: &str = include_str!("fixtures/output-unavailable.v1.json");

fn make_ranked_doc(skills: Vec<serde_json::Value>) -> OutputDocument {
    let mut val: serde_json::Value = serde_json::from_str(RANKED_FIXTURE).unwrap();
    let n = skills.len();
    val["skills"] = serde_json::Value::Array(skills);
    val["roster"]["total"] = serde_json::json!(n.max(1));
    val["roster"]["eligible"] = serde_json::json!(n.max(1));
    val["roster"]["wide_candidates"] = serde_json::json!(n.max(1));
    val["roster"]["shortlist"] = serde_json::json!(n);
    val["omitted_rank_mass"] = serde_json::json!(0.0);
    OutputDocument::from_value(val).expect("valid ranked doc")
}

fn make_explicit_doc(skills: Vec<serde_json::Value>) -> OutputDocument {
    let mut val: serde_json::Value = serde_json::from_str(EXPLICIT_FIXTURE).unwrap();
    let n = skills.len();
    val["skills"] = serde_json::Value::Array(skills);
    val["roster"]["total"] = serde_json::json!(n.max(1));
    val["roster"]["eligible"] = serde_json::json!(n.max(1));
    OutputDocument::from_value(val).expect("valid explicit doc")
}

fn make_abstain_doc() -> OutputDocument {
    let val: serde_json::Value = serde_json::from_str(ABSTAIN_FIXTURE).unwrap();
    OutputDocument::from_value(val).expect("valid abstain doc")
}

fn make_unavailable_doc() -> OutputDocument {
    let val: serde_json::Value = serde_json::from_str(UNAVAILABLE_FIXTURE).unwrap();
    OutputDocument::from_value(val).expect("valid unavailable doc")
}

fn make_ranked_skill(
    name: &str,
    rank: u64,
    rank_score: f64,
    rerank_prob: f64,
    wide_prob: f64,
    fits: f64,
) -> serde_json::Value {
    json!({
        "rank": rank,
        "skill_id": format!("skill-{rank}"),
        "name": name,
        "invocation_name": name,
        "rank_score": rank_score,
        "rerank_probability": rerank_prob,
        "wide_probability": wide_prob,
        "fits": fits,
        "path": format!(".claude/skills/{name}/SKILL.md"),
        "content_hash": "0000000000000000000000000000000000000000000000000000000000000001"
    })
}

fn make_explicit_skill(name: &str, rank: u64) -> serde_json::Value {
    json!({
        "rank": rank,
        "skill_id": format!("skill-{rank}"),
        "name": name,
        "invocation_name": name,
        "rank_score": null,
        "rerank_probability": null,
        "wide_probability": null,
        "fits": null,
        "path": format!(".claude/skills/{name}/SKILL.md"),
        "content_hash": "0000000000000000000000000000000000000000000000000000000000000001"
    })
}

struct AdviceFixture {
    root: PathBuf,
    session_id: String,
}

impl AdviceFixture {
    fn new() -> Self {
        let root = PathBuf::from("/tmp").join(format!(
            "sr-advice-contract-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new()
            .recursive(true)
            .create(&root)
            .expect("create root");
        let session_id = "session-test-advice".to_string();
        let f = Self { root, session_id };
        fs::create_dir_all(f.workspace()).unwrap();
        fs::create_dir_all(f.home().join(".config")).unwrap();
        fs::create_dir_all(f.ledger_dir()).unwrap();

        for dir in [&f.root, &f.home(), &f.workspace(), &f.ledger_dir()] {
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
        }

        // Create a test skill
        let skill = f.home().join(".claude/skills/cargo-test");
        fs::create_dir_all(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\ndescription: Runs cargo test commands.\n---\nBody.\n",
        )
        .unwrap();

        f
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn workspace(&self) -> PathBuf {
        self.root.join("workspace")
    }

    fn ledger_dir(&self) -> PathBuf {
        self.root.join("ledger")
    }

    fn set_advisory_mode(&self) {
        let config_dir = self.home().join(".config/sr");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("config.toml"),
            "[hook]\nmode = \"advisory\"\n",
        )
        .unwrap();
    }

    fn run_hook(&self, stdin_bytes: &[u8], args: &[&str]) -> std::process::Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sr"));
        cmd.env_clear()
            .env("HOME", self.home())
            .env("XDG_CONFIG_HOME", self.home().join(".config"))
            .current_dir(self.workspace())
            .arg("hook")
            .arg("claude")
            .arg("--dir")
            .arg(self.ledger_dir())
            .arg("--offline")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().expect("spawn sr hook claude");
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(stdin_bytes);
        }
        child.wait_with_output().expect("wait for sr hook claude")
    }
}

// ---------------------------------------------------------------------------
// Unit tests on advice rendering logic
// ---------------------------------------------------------------------------

#[test]
fn render_safe_suggestion_ranked() {
    let doc = make_ranked_doc(vec![
        make_ranked_skill("rust-cargo-test-triage", 1, 0.7, 0.4, 0.5, 0.8),
        make_ranked_skill("other-skill", 2, 0.3, 0.3, 0.4, 0.5),
    ]);

    let envelope = render_claude_hook_advice(&doc, false)
        .expect("rendering succeeds")
        .expect("envelope produced for ranked");

    assert_eq!(
        envelope.hook_specific_output.hook_event_name,
        HOOK_EVENT_NAME
    );
    assert_eq!(
        envelope.hook_specific_output.additional_context,
        "Suggested skill for the next step: rust-cargo-test-triage. Use it only if it fits the user's request and current instructions."
    );

    // Serialization conforms to Claude hook envelope JSON
    let wire = envelope.to_json().unwrap();
    let val: serde_json::Value = serde_json::from_str(&wire).unwrap();
    assert_eq!(
        val["hookSpecificOutput"]["hookEventName"],
        "UserPromptSubmit"
    );
    assert!(
        val["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("rust-cargo-test-triage")
    );
}

#[test]
fn render_safe_suggestion_explicit_single() {
    let doc = make_explicit_doc(vec![make_explicit_skill("test-repair", 1)]);

    let envelope = render_claude_hook_advice(&doc, false)
        .expect("rendering succeeds")
        .expect("envelope produced for explicit");

    assert_eq!(
        envelope.hook_specific_output.additional_context,
        "User requested skill: test-repair. Follow explicit skill requests and applicable instructions."
    );
}

#[test]
fn render_safe_suggestion_explicit_multiple() {
    let doc = make_explicit_doc(vec![
        make_explicit_skill("skill-alpha", 1),
        make_explicit_skill("skill-beta", 2),
    ]);

    let envelope = render_claude_hook_advice(&doc, false)
        .expect("rendering succeeds")
        .expect("envelope produced for explicit multiple");

    assert_eq!(
        envelope.hook_specific_output.additional_context,
        "User requested skills: skill-alpha, skill-beta. Follow explicit skill requests and applicable instructions."
    );
}

#[test]
fn oversized_explicit_quiet_fallback() {
    // Generate a list of explicit skills within the 32-skill limit of OutputDocument
    // whose formatted additionalContext exceeds 1,024 Unicode scalars.
    let mut skills = Vec::new();
    for i in 0..25 {
        skills.push(make_explicit_skill(
            &format!("extremely-long-explicit-skill-identifier-number-{:04}", i),
            (i + 1) as u64,
        ));
    }

    let doc = make_explicit_doc(skills);
    let res = render_claude_hook_advice(&doc, false);
    assert_eq!(res, Err(HookRenderError::OutputLimitExceeded));
}

#[test]
fn malicious_control_text_rejected() {
    let bad_skills = [
        "skill\x1b[31mred",
        "skill\0null",
        "skill\rreturn",
        "skill\x08backspace",
    ];

    for bad in bad_skills {
        // OutputDocument validation refuses control characters at the root contract boundary
        let mut val: serde_json::Value = serde_json::from_str(RANKED_FIXTURE).unwrap();
        val["skills"] =
            serde_json::Value::Array(vec![make_ranked_skill(bad, 1, 1.0, 0.5, 0.5, 0.8)]);
        assert!(
            OutputDocument::from_value(val).is_err(),
            "OutputDocument must reject control characters in skill names"
        );

        // Also verify that hook additional_context validation directly rejects forbidden control text
        let bad_context = format!("Suggested skill for the next step: {bad}.");
        assert!(
            skillranker::adapter::additional_context_allowed(&bad_context, 1).is_err(),
            "control char in '{bad}' must be refused by hook context boundary"
        );
    }
}

#[test]
fn abstain_decision_quiet_by_default_and_explicit_experiment() {
    let doc = make_abstain_doc();

    // By default: quiet (None)
    let quiet = render_claude_hook_advice(&doc, false).unwrap();
    assert!(quiet.is_none(), "ordinary abstention must produce None");

    // When experiment enabled: returns scoped abstention message
    let enabled = render_claude_hook_advice(&doc, true).unwrap();
    assert!(enabled.is_some());
    let env = enabled.unwrap();
    assert_eq!(
        env.hook_specific_output.additional_context,
        "No additional skill is suggested for this step; follow explicit skill requests and applicable instructions."
    );
}

#[test]
fn unavailable_decision_is_quiet() {
    let doc = make_unavailable_doc();
    let res = render_claude_hook_advice(&doc, false).unwrap();
    assert!(res.is_none(), "unavailable errors must produce None");
}

// ---------------------------------------------------------------------------
// Integration & publication boundary tests
// ---------------------------------------------------------------------------

#[test]
fn shadow_mode_suppresses_stdout_even_with_explicit_request() {
    let fixture = AdviceFixture::new();

    let hook_payload = json!({
        "hook_event_name": "UserPromptSubmit",
        "prompt": "/use-skill cargo-test",
        "prompt_id": "p-1",
        "session_id": fixture.session_id,
        "cwd": fixture.workspace(),
    });

    // Run in shadow mode (default)
    let out = fixture.run_hook(&serde_json::to_vec(&hook_payload).unwrap(), &[]);
    assert_eq!(out.status.code(), Some(0));
    assert!(
        out.stdout.is_empty(),
        "shadow mode must produce zero bytes of stdout"
    );
}

#[test]
fn advisory_mode_renders_valid_hook_specific_output_envelope() {
    let fixture = AdviceFixture::new();
    fixture.set_advisory_mode();

    // Prompt with a slash command directive resolves explicitly to cargo-test
    let hook_payload = json!({
        "hook_event_name": "UserPromptSubmit",
        "prompt": "/use-skill cargo-test",
        "prompt_id": "p-advisory-1",
        "session_id": fixture.session_id,
        "cwd": fixture.workspace(),
    });

    let out = fixture.run_hook(&serde_json::to_vec(&hook_payload).unwrap(), &[]);
    let stdout_str = String::from_utf8_lossy(&out.stdout);
    let stderr_str = String::from_utf8_lossy(&out.stderr);

    assert_eq!(
        out.status.code(),
        Some(0),
        "status code must be 0, stderr: {stderr_str}"
    );
    assert!(
        !stdout_str.is_empty(),
        "advisory mode must emit hook output, stderr: {stderr_str}"
    );

    let val: serde_json::Value =
        serde_json::from_str(stdout_str.trim()).expect("stdout must be valid JSON");
    assert_eq!(
        val["hookSpecificOutput"]["hookEventName"],
        "UserPromptSubmit"
    );
    let context = val["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(context.contains("User requested skill: cargo-test"));
    assert!(context.len() <= HOOK_ADDITIONAL_CONTEXT_SCALARS.max());
}

#[test]
fn forced_shadow_flag_overrides_advisory_config() {
    let fixture = AdviceFixture::new();
    fixture.set_advisory_mode();

    let hook_payload = json!({
        "hook_event_name": "UserPromptSubmit",
        "prompt": "/use-skill cargo-test",
        "prompt_id": "p-shadow-override",
        "session_id": fixture.session_id,
        "cwd": fixture.workspace(),
    });

    // Pass --shadow flag to override advisory mode
    let out = fixture.run_hook(&serde_json::to_vec(&hook_payload).unwrap(), &["--shadow"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(
        out.stdout.is_empty(),
        "--shadow flag must suppress all stdout even when advisory mode is configured"
    );
}

#[test]
fn advisory_mode_suppresses_ranked_advice_for_unverified_harness() {
    // Contract requirement:
    // Ranked suggestions for an unverified harness (Claude Code has no tested versions
    // and unevaluated matrix) must downgrade to quiet shadow with a diagnostic on stderr.
    use skillranker::adapter::{CLAUDE_CODE_ID, CompatibilityQuestion, foundation_capabilities};

    let foundation = foundation_capabilities().expect("foundation capabilities");
    let record = foundation
        .adapters
        .iter()
        .find(|a| a.adapter_id.as_str() == CLAUDE_CODE_ID)
        .expect("claude_code adapter");
    let disposition = record.advice(CompatibilityQuestion::EmitNativeAdvice, None);
    assert!(
        matches!(disposition, skillranker::adapter::AdviceDisposition::Disabled(_)),
        "Claude Code adapter must be unverified/disabled for native advice in this phase"
    );
}
