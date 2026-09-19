//! Real CLI regressions for explanations of local explicit resolution.
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "sr-trace-explicit-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        for name in ["agent", "manual"] {
            let dir = root.join("workspace/.claude/skills").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let restriction = if name == "manual" {
                "disable-model-invocation: true\nuser-invocable: true\n"
            } else {
                ""
            };
            std::fs::write(dir.join("SKILL.md"), format!(
                "---\nname: {name}\ndescription: Help review Rust code\n{restriction}---\nReview code carefully.\n"
            )).unwrap();
        }
        for name in ["home", "config", "data", "cache"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        let context = json!({
            "schema_version": 1, "harness": "claude_code", "producer_id": "trace-test",
            "workspace_root": root.join("workspace"), "session_id": "session-1",
            "agent_id": null, "branch_id": null, "context_epoch": null,
            "current_request": {"event_id": "request-1", "text": "Review the Rust changes.",
                "attachments_omitted": false, "essential_attachment_missing": false},
            "events": [], "explicit_skill_references": [], "supplied_loads": []
        });
        std::fs::write(
            root.join("workspace/context.json"),
            serde_json::to_vec(&context).unwrap(),
        )
        .unwrap();
        Self(root)
    }

    fn run(&self, required: &str, explain: bool, why_not: Option<&str>) -> Value {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sr"));
        command
            .env_clear()
            .env("HOME", self.0.join("home"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("XDG_DATA_HOME", self.0.join("data"))
            .env("XDG_CACHE_HOME", self.0.join("cache"))
            .current_dir(self.0.join("workspace"))
            .args([
                "rank",
                "--context",
                "context.json",
                "--offline",
                "--no-persist",
                "--json",
                "--require-skill",
                required,
            ]);
        if explain {
            command.arg("--explain");
        }
        if let Some(target) = why_not {
            command.args(["--why-not", target]);
        }
        let output = command.output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(0),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["decision"], "explicit");
        assert_eq!(value["usage"]["requests"], 0);
        assert_eq!(value["usage"]["http_attempts"], 0);
        value
    }
}

fn entry<'a>(value: &'a Value, stage: &str) -> &'a Value {
    value["trace"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["stage"] == stage)
        .unwrap()
}

fn resolved_explicit_trace(name: &str) {
    let fixture = Fixture::new();
    let plain = fixture.run(name, false, None);
    let explained = fixture.run(name, true, Some(name));
    assert_eq!(plain["skills"], explained["skills"]);
    assert_eq!(plain["roster"], explained["roster"]);
    assert_eq!(entry(&explained, "discovery")["status"], "passed");
    assert_eq!(entry(&explained, "visibility")["status"], "passed");
    assert_eq!(entry(&explained, "local-policy")["status"], "passed");
    for stage in ["quill-admission", "wide-shortlist", "fit-none", "ordering"] {
        let e = entry(&explained, stage);
        assert_eq!(e["status"], "not-evaluated", "{name}: {stage}");
        for field in ["value", "threshold", "reason"] {
            assert!(e[field].is_null());
        }
    }
    assert_eq!(entry(&explained, "publication")["status"], "passed");
    // With no why-not filter the trace concerns the resolved target, not the
    // first unrelated skill in the roster.
    let all = fixture.run(name, true, None);
    assert_eq!(all["trace"]["total"], 8);
    for e in all["trace"]["entries"].as_array().unwrap() {
        assert_eq!(e["skill_id"], plain["skills"][0]["skill_id"]);
    }
}

#[test]
fn explicit_manual_target_is_not_advisory_excluded() {
    resolved_explicit_trace("manual");
}

#[test]
fn explicit_agent_target_does_not_invent_retrieval_or_model_work() {
    resolved_explicit_trace("agent");
}

#[test]
fn unrequested_target_has_no_advisory_or_publication_claim() {
    let fixture = Fixture::new();
    let value = fixture.run("manual", true, Some("agent"));
    assert_eq!(entry(&value, "discovery")["status"], "passed");
    for stage in [
        "visibility",
        "local-policy",
        "quill-admission",
        "wide-shortlist",
        "fit-none",
        "ordering",
        "publication",
    ] {
        assert_eq!(entry(&value, stage)["status"], "not-evaluated", "{stage}");
    }
}

#[test]
fn unknown_target_stays_outside_snapshot() {
    let fixture = Fixture::new();
    let value = fixture.run("manual", true, Some("unknown"));
    assert_eq!(entry(&value, "discovery")["status"], "not-in-snapshot");
    for e in value["trace"]["entries"].as_array().unwrap().iter().skip(1) {
        assert_eq!(e["status"], "not-evaluated");
    }
}
