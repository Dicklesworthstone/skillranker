#![cfg(unix)]
//! Real binary checks of how `sr rank` reads its inputs and derives its
//! effects: bounded regular-file context and roster reads, the trusted context
//! profile, and gate-derived source restrictions. No provider is contacted:
//! every case runs offline or as a dry run.
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    // Intentionally retained: repository policy forbids automatic tree deletion.
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "sr-rank-inputs-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        for dir in ["workspace/.claude/skills", "home", "config/sr"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let f = Self { root };
        for name in ["alpha", "beta"] {
            let dir = f.workspace().join(".claude/skills").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\ndescription: {name} helps with rust tests.\n---\nBody.\n"),
            )
            .unwrap();
        }
        f
    }
    fn workspace(&self) -> PathBuf {
        self.root.join("workspace")
    }
    fn context(&self, name: &str, tool_output: &str) -> PathBuf {
        let path = self.workspace().join(name);
        let event = |id: &str, role: &str, kind: &str, text: &str, tool: Value| {
            json!({"event_id": id, "parent_id": null, "turn_id": "turn-1", "agent_id": null,
                   "branch_id": null, "role": role, "kind": kind, "timestamp_unix_ms": null,
                   "text": text, "tool": tool})
        };
        let context = json!({
            "schema_version": 1,
            "harness": "claude_code",
            "producer_id": "synthetic-test",
            "workspace_root": self.workspace().to_string_lossy(),
            "session_id": "session-1",
            "agent_id": null,
            "branch_id": null,
            "context_epoch": null,
            "current_request": {"event_id": "request-2", "text": "Fix the failing rust test.",
                                "attachments_omitted": false, "essential_attachment_missing": false},
            "events": [
                event("request-1", "user", "message", "Run the rust tests.", Value::Null),
                event("call-1", "assistant", "tool_invocation", "", json!({
                    "call_id": "c1", "name": "Bash", "status": "succeeded",
                    "arguments": "cargo test", "result": tool_output})),
                event("request-2", "user", "message", "Fix the failing rust test.", Value::Null),
            ],
            "explicit_skill_references": [],
            "supplied_loads": []
        });
        std::fs::write(&path, serde_json::to_vec(&context).unwrap()).unwrap();
        path
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_sr"))
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .current_dir(self.workspace())
            .args(args)
            .output()
            .unwrap()
    }
    fn error_kind(&self, args: &[&str]) -> (Option<i32>, String) {
        let output = self.run(args);
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        (
            output.status.code(),
            value["error"]["kind"].as_str().unwrap_or("").to_owned(),
        )
    }
}

fn dry_run_request_bytes(output: &Output) -> u64 {
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    value["dry_run"]["request_bytes"].as_u64().unwrap()
}

#[test]
fn context_files_are_bounded_regular_files() {
    let f = Fixture::new();
    // Positive twin: an ordinary context file previews.
    f.context("context.json", "test result: FAILED");
    let ok = f.run(&["rank", "--context", "context.json", "--dry-run", "--json"]);
    assert!(dry_run_request_bytes(&ok) > 0);
    // Oversized: refused before parsing, with a typed kind and no path.
    let big = f.workspace().join("big.json");
    std::fs::write(&big, vec![b' '; 1024 * 1024 + 1]).unwrap();
    let (code, kind) = f.error_kind(&["rank", "--context", "big.json", "--dry-run", "--json"]);
    assert_eq!((code, kind.as_str()), (Some(7), "oversized-input"));
    // A FIFO is refused without blocking.
    nix::unistd::mkfifo(
        &f.workspace().join("fifo.json"),
        nix::sys::stat::Mode::S_IRWXU,
    )
    .unwrap();
    let (code, kind) = f.error_kind(&["rank", "--context", "fifo.json", "--dry-run", "--json"]);
    assert_eq!((code, kind.as_str()), (Some(7), "unsupported-input"));
    // A symlink leaving its directory is refused.
    let outside = f.root.join("outside.json");
    std::fs::copy(f.workspace().join("context.json"), &outside).unwrap();
    std::os::unix::fs::symlink(&outside, f.workspace().join("link.json")).unwrap();
    let (code, kind) = f.error_kind(&["rank", "--context", "link.json", "--dry-run", "--json"]);
    assert_eq!((code, kind.as_str()), (Some(7), "unsupported-input"));
    let text = String::from_utf8_lossy(
        &f.run(&["rank", "--context", "link.json", "--dry-run", "--json"])
            .stdout,
    )
    .into_owned();
    assert!(
        !text.contains(f.root.to_str().unwrap()),
        "no path is echoed"
    );
}

#[test]
fn explicit_roster_files_are_bounded_regular_files() {
    let f = Fixture::new();
    f.context("context.json", "ok");
    nix::unistd::mkfifo(
        &f.workspace().join("roster.fifo"),
        nix::sys::stat::Mode::S_IRWXU,
    )
    .unwrap();
    let (code, kind) = f.error_kind(&[
        "rank",
        "--context",
        "context.json",
        "--roster",
        "roster.fifo",
        "--dry-run",
        "--json",
    ]);
    assert_eq!(code, Some(5));
    assert_eq!(kind, "unusable-roster");
}

#[test]
fn the_trusted_minimal_profile_is_honored() {
    let f = Fixture::new();
    f.context("context.json", &"error: ".repeat(400));
    let standard = dry_run_request_bytes(&f.run(&[
        "rank",
        "--context",
        "context.json",
        "--dry-run",
        "--json",
    ]));
    // Trusted user configuration selects the minimal disclosure profile.
    std::fs::write(
        f.root.join("config/sr/config.toml"),
        "[context]\nprofile = \"minimal\"\n",
    )
    .unwrap();
    let minimal = dry_run_request_bytes(&f.run(&[
        "rank",
        "--context",
        "context.json",
        "--dry-run",
        "--json",
    ]));
    // Standard carries history and a bounded tool excerpt; minimal omits both,
    // so the same context renders strictly fewer request bytes.
    assert!(
        minimal < standard,
        "minimal {minimal} must render less than standard {standard}"
    );
}

#[test]
fn a_dry_run_refuses_cass_before_any_child_or_read() {
    let f = Fixture::new();
    let (code, kind) = f.error_kind(&[
        "rank",
        "--session",
        "/synthetic/session.jsonl",
        "--dry-run",
        "--json",
    ]);
    assert_eq!((code, kind.as_str()), (Some(7), "unsupported-source-mode"));
    let (code, kind) = f.error_kind(&[
        "rank",
        "--session",
        "/synthetic/session.jsonl",
        "--offline",
        "--json",
    ]);
    assert_eq!((code, kind.as_str()), (Some(7), "unsupported-source-mode"));
    assert!(!Path::new("/synthetic/session.jsonl").exists());
}

#[test]
fn claude_user_skills_come_from_home_not_the_config_root() {
    let f = Fixture::new();
    f.context("context.json", "ok");
    // A user-level skill exists only under $HOME/.claude/skills.
    let dir = f.root.join("home/.claude/skills/gamma");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\ndescription: gamma formats rust code.\n---\nBody.\n",
    )
    .unwrap();
    let output = f.run(&[
        "rank",
        "--context",
        "context.json",
        "--require-skill",
        "gamma",
        "--offline",
        "--json",
    ]);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{value}");
    assert_eq!(value["decision"], "explicit");
    assert_eq!(value["skills"][0]["invocation_name"], "gamma");
    // Negative twin: the same skill under the configuration root is not a
    // Claude skill root and stays invisible.
    let g = Fixture::new();
    g.context("context.json", "ok");
    let dir = g.root.join("config/.claude/skills/gamma");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\ndescription: gamma formats rust code.\n---\nBody.\n",
    )
    .unwrap();
    let output = g.run(&[
        "rank",
        "--context",
        "context.json",
        "--require-skill",
        "gamma",
        "--offline",
        "--json",
    ]);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_ne!(value["decision"], "explicit", "{value}");
}
