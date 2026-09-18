//! Comprehensive verification of safe, readable table rendering and format dispatch.
//!
//! Satisfies contract boundary `p4_table_and_decision_views` (sr-roadmap-l1i.5.13).
//!
//! Verifies:
//! 1. Accurate display width calculation for narrow ASCII, wide CJK, emoji, and combining characters.
//! 2. Complete sanitization of malicious ANSI escape codes (CSI, OSC) and control characters.
//! 3. Clean column-aligned table rendering for Ranked decisions.
//! 4. Explicit directive decision views explaining local resolution and provider inference bypass.
//! 5. Readable Abstain and Unavailable decision views with unresolved explicit tables.
//! 6. CLI format dispatch: `--table` vs `--json`, TTY/non-TTY rules, mutual exclusivity.
//! 7. Separation of stdout data and stderr diagnostics; non-disclosure of canary secrets.

use serde_json::{Value, json};
use skillranker::output::table::{
    char_width, pad_to_width, sanitize_terminal_text, str_width, truncate_to_width,
};
use skillranker::output::{ErrorKind, OutputDocument};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

const RANKED: &str = include_str!("fixtures/output-ranked.v1.json");
const EXPLICIT: &str = include_str!("fixtures/output-explicit.v1.json");
const ABSTAIN: &str = include_str!("fixtures/output-abstain.v1.json");

static FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

struct CliFixture {
    root: PathBuf,
    workspace: PathBuf,
}

impl CliFixture {
    fn new() -> Self {
        let id = FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("sr-table-test-{}-{}", std::process::id(), id));
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
            "session_id": "session-table-1",
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

    fn create_skill(&self, name: &str, display_name: &str) -> PathBuf {
        let skill_dir = self.workspace.join(".claude/skills").join(name);
        fs::create_dir_all(&skill_dir).unwrap();
        let file = skill_dir.join("SKILL.md");
        fs::write(
            &file,
            format!("---\nname: {display_name}\ndescription: Test skill {name}\n---\nBody of {name}\n"),
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
fn test_narrow_and_wide_unicode_display_cells() {
    // 1. Single character widths
    assert_eq!(char_width('A'), 1);
    assert_eq!(char_width('1'), 1);
    assert_eq!(char_width('_'), 1);
    assert_eq!(char_width(' '), 1);

    // CJK and fullwidth ideographs = 2 cells
    assert_eq!(char_width('文'), 2);
    assert_eq!(char_width('本'), 2);
    assert_eq!(char_width('한'), 2);
    assert_eq!(char_width('글'), 2);
    assert_eq!(char_width('ア'), 2);
    assert_eq!(char_width('カ'), 2);
    assert_eq!(char_width('！'), 2); // Fullwidth exclamation mark

    // Emojis and miscellaneous symbols = 2 cells
    assert_eq!(char_width('⚡'), 2);
    assert_eq!(char_width('🦀'), 2);
    assert_eq!(char_width('🔍'), 2);

    // Control and combining characters = 0 cells
    assert_eq!(char_width('\x00'), 0);
    assert_eq!(char_width('\x1b'), 0);
    assert_eq!(char_width('\u{0301}'), 0); // Combining acute accent
    assert_eq!(char_width('\u{200B}'), 0); // Zero-width space

    // 2. String width computations
    assert_eq!(str_width("Hello World"), 11);
    assert_eq!(str_width("Rust 🦀 Analyzer"), 16); // 5 ("Rust ") + 2 (🦀) + 9 (" Analyzer") = 16
    assert_eq!(str_width("日本語スキル"), 12); // 6 CJK chars * 2 = 12

    // 3. Padding to exact column width
    let s = "Rust 🦀"; // width 7
    let padded = pad_to_width(s, 12, true);
    assert_eq!(str_width(&padded), 12);
    assert_eq!(padded, "Rust 🦀     ");

    // 4. Safe truncation without splitting wide characters
    let mixed = "Skill: 日本語"; // 7 ASCII + 3 CJK (width 13)
    let trunc_10 = truncate_to_width(mixed, 10);
    // "Skill: " is 7. "日" is 2 (total 9). "本" needs 2 (total 11 > 10). So fits "Skill: 日"
    assert_eq!(trunc_10, "Skill: 日");
    assert_eq!(str_width(&trunc_10), 9);
}

#[test]
fn test_malicious_ansi_and_control_sequence_sanitization() {
    // CSI color escape sequences
    let csi_text = "\x1b[31;1mRed Alert\x1b[0m Normal";
    assert_eq!(sanitize_terminal_text(csi_text), "Red Alert Normal");

    // Terminal clear / cursor repositioning escape codes
    let cursor_tamper = "\x1b[2J\x1b[H\x1b[?25lEvil Terminal Manipulation";
    assert_eq!(sanitize_terminal_text(cursor_tamper), "Evil Terminal Manipulation");

    // OSC 8 Hyperlink injection
    let osc_link = "\x1b]8;;https://attacker.example.com/payload\x07Click Here\x1b]8;;\x07";
    assert_eq!(sanitize_terminal_text(osc_link), "Click Here");

    // OSC with string terminator \x1b\
    let osc_st = "\x1b]8;;https://malicious.org\x1b\\Dangerous\x1b]8;;\x1b\\";
    assert_eq!(sanitize_terminal_text(osc_st), "Dangerous");

    // Embedded NUL and control characters
    let controls = "Line 1\x00\x07\x08With Controls";
    assert_eq!(sanitize_terminal_text(controls), "Line 1   With Controls");
}

#[test]
fn test_ranked_table_rendering() {
    let doc = OutputDocument::from_json(RANKED.as_bytes()).expect("valid ranked fixture");
    let table_str = doc.render_table();

    // Contains table headers
    assert!(table_str.contains("RANK"));
    assert!(table_str.contains("COMMAND"));
    assert!(table_str.contains("SKILL ID"));
    assert!(table_str.contains("SCORE"));
    assert!(table_str.contains("PROB"));
    assert!(table_str.contains("FIT"));
    assert!(table_str.contains("NAME"));

    // Renders data cleanly from fixture
    assert!(table_str.contains("rust-test-triage"));
    assert!(table_str.contains("s_01"));
    assert!(table_str.contains("0.888889"));
    assert!(table_str.contains("rust-code-review"));
    assert!(table_str.contains("s_02"));

    // Verify malicious ANSI is neutralized in terminal rendering
    let malicious = "\x1b[31mExploit\x1b[0m";
    assert_eq!(sanitize_terminal_text(malicious), "Exploit");
}

#[test]
fn test_explicit_directive_decision_view() {
    let doc = OutputDocument::from_json(EXPLICIT.as_bytes()).expect("valid explicit fixture");
    let view_str = doc.render_table();

    assert!(view_str.contains("EXPLICIT SKILL DIRECTIVE"));
    assert!(view_str.contains("Provider inference was bypassed"));
    assert!(view_str.contains("COMMAND"));
    assert!(view_str.contains("SKILL ID"));
    assert!(view_str.contains("NAME"));
    assert!(view_str.contains("rust-test-triage"));
    assert!(view_str.contains("Required"));
}

#[test]
fn test_abstain_and_unavailable_decision_views() {
    // 1. Abstain view
    let doc_abstain = OutputDocument::from_json(ABSTAIN.as_bytes()).expect("valid abstain fixture");
    let abstain_str = doc_abstain.render_table();

    assert!(abstain_str.contains("DECISION: ABSTAIN"));
    assert!(abstain_str.contains("Reason: local-exclusion"));
    assert!(abstain_str.contains("No skills recommended for this turn"));

    // 2. Unavailable view with unresolved explicit references
    let doc_unavail = OutputDocument::failure_with_details(
        ErrorKind::UnresolvedExplicit,
        "Explicit skill reference unresolved",
        "Verify installed skills in the roster",
        false,
    )
    .with_unresolved(vec![skillranker::output::UnresolvedReference {
        reference: "missing-security-audit".into(),
        reason: skillranker::output::UnresolvedReason::Missing,
    }])
    .expect("valid failure doc");

    let unavail_str = doc_unavail.render_table();
    assert!(unavail_str.contains("UNAVAILABLE (exit 5)"));
    assert!(unavail_str.contains("Error Kind: unresolved-explicit"));
    assert!(unavail_str.contains("Message:    Explicit skill reference unresolved"));
    assert!(unavail_str.contains("Hint:       Verify installed skills in the roster"));
    assert!(unavail_str.contains("Unresolved Explicit Requirements:"));
    assert!(unavail_str.contains("missing-security-audit"));
    assert!(unavail_str.contains("missing"));
}

#[test]
fn test_cli_format_dispatch_table_vs_json() {
    let f = CliFixture::new();
    let canary = "secret-canary-never-disclose-778899";

    f.create_skill("skill_test", "Test Skill Helper");
    let ctx_file = f.create_context("Please use skill: skill_test");

    // 1. Explicit --table forces table output even on non-TTY
    let out_table = f.run(
        &[
            "rank",
            "--context",
            ctx_file.to_str().unwrap(),
            "--require-skill",
            "skill_test",
            "--offline",
            "--table",
        ],
        &[("TYPESAFE_API_KEY", canary)],
    );
    assert_eq!(out_table.status.code(), Some(0));
    let stdout_table = String::from_utf8_lossy(&out_table.stdout);
    assert!(stdout_table.contains("EXPLICIT SKILL DIRECTIVE"));
    assert!(stdout_table.contains("skill_test"));
    assert!(stdout_table.contains("Test Skill Helper"));
    assert!(stdout_table.contains("Required"));
    // Never echoes the canary key
    assert!(!stdout_table.contains(canary));
    assert!(!String::from_utf8_lossy(&out_table.stderr).contains(canary));

    // 2. Explicit --json forces valid JSON output
    let out_json = f.run(
        &[
            "rank",
            "--context",
            ctx_file.to_str().unwrap(),
            "--require-skill",
            "skill_test",
            "--offline",
            "--json",
        ],
        &[],
    );
    assert_eq!(out_json.status.code(), Some(0));
    let val_json: Value = serde_json::from_slice(&out_json.stdout).expect("valid JSON stdout");
    assert_eq!(val_json["decision"], "explicit");

    // 3. Mutually exclusive flags: --table and --json conflict with exit code 2
    let out_conflict = f.run(
        &[
            "rank",
            "--context",
            ctx_file.to_str().unwrap(),
            "--table",
            "--json",
        ],
        &[],
    );
    assert_eq!(out_conflict.status.code(), Some(2));
    let val_conflict: Value = serde_json::from_slice(&out_conflict.stdout).expect("error JSON stdout");
    assert_eq!(val_conflict["error"]["code"], 2);
    assert_eq!(val_conflict["error"]["kind"], "invalid-usage");

    // 4. Default on non-TTY (piped output) is JSON
    let out_default = f.run(
        &[
            "rank",
            "--context",
            ctx_file.to_str().unwrap(),
            "--require-skill",
            "skill_test",
            "--offline",
        ],
        &[],
    );
    assert_eq!(out_default.status.code(), Some(0));
    let val_default: Value = serde_json::from_slice(&out_default.stdout).expect("valid JSON default on pipe");
    assert_eq!(val_default["decision"], "explicit");
}
