#![cfg(any(target_os = "linux", target_os = "macos"))]
//! Real CLI/filesystem checks for explicitly configured roots in normalized
//! contexts. These establish local layout behavior, not native harness support.

use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::{DirBuilderExt, symlink};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = fs::canonicalize("/tmp").unwrap().join(format!(
            "sr-normalized-roots-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        for child in ["workspace/.sr", "home/.config/sr", "external"] {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(root.join(child))
                .unwrap();
        }
        Self { root }
    }

    fn skill(&self, base: &str, name: &str, flags: &str, description: &str) {
        let dir = self.root.join(base).join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), format!(
            "---\nname: {name}\ndescription: {description}\n{flags}---\nDiagnose failing Rust tests.\n"
        )).unwrap();
    }

    fn project_config(&self, text: &str) {
        fs::write(self.root.join("workspace/.sr/config.toml"), text).unwrap();
    }

    fn user_root(&self) {
        fs::write(
            self.root.join("home/.config/sr/config.toml"),
            format!(
                "[roster]\nroots=['{}']\n",
                self.root.join("external").display()
            ),
        )
        .unwrap();
    }

    fn run(&self, harness: &str, extra: &[&str]) -> Output {
        let workspace = self.root.join("workspace");
        let context = workspace.join("context.json");
        fs::write(&context, json!({
            "schema_version": 1, "harness": harness, "producer_id": "configured-roots-test",
            "workspace_root": workspace, "session_id": "session", "agent_id": null,
            "branch_id": null, "context_epoch": null,
            "current_request": {"event_id": "request", "text": "Diagnose our failing Rust tests",
                "attachments_omitted": false, "essential_attachment_missing": false},
            "events": [], "explicit_skill_references": [], "supplied_loads": []
        }).to_string()).unwrap();
        Command::new(env!("CARGO_BIN_EXE_sr"))
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("home/.config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .current_dir(workspace)
            .stdin(Stdio::null())
            .args(["rank", "--context"])
            .arg(context)
            .args(["--no-persist", "--json", "--timeout-ms", "20000"])
            .args(extra)
            .output()
            .unwrap()
    }

    fn document(&self, harness: &str, extra: &[&str], exit: i32) -> Value {
        let output = self.run(harness, extra);
        assert_eq!(
            output.status.code(),
            Some(exit),
            "stderr: {}\nstdout: {}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        let value = serde_json::from_slice(&output.stdout).unwrap();
        for state in ["data", "cache"] {
            assert!(
                !self.root.join(state).exists(),
                "stateless ranking created {state}"
            );
        }
        value
    }
}

#[test]
fn configured_roots_preview_excludes_implicit_harness_roots() {
    for harness in ["normalized", "codex"] {
        let f = Fixture::new();
        f.skill(
            "workspace/custom",
            "alpha",
            "",
            "Configured project guidance",
        );
        f.skill("external", "beta", "", "Configured trusted guidance");
        f.skill(
            "workspace/.claude/skills",
            "implicit-claude",
            "",
            "Implicit Claude canary",
        );
        f.skill(
            "home/.claude/skills",
            "implicit-personal",
            "",
            "Implicit personal canary",
        );
        f.skill(
            "home/.codex/skills",
            "implicit-codex",
            "",
            "Implicit Codex canary",
        );
        symlink("custom", f.root.join("workspace/alias")).unwrap();
        f.project_config("[roster]\nroots=['custom','alias']\n");
        f.user_root();
        let preview = f.document(harness, &["--dry-run"], 0);
        assert_eq!(preview["actionable"], false);
        assert_eq!(preview["stateless"], true);
        let stage = &preview["provider_request"]["stages"][0];
        assert_eq!(
            stage["candidates"], 2,
            "aliases must not duplicate candidates"
        );
        let request = stage["request"].as_str().unwrap();
        assert!(request.contains("Configured project guidance"));
        assert!(request.contains("Configured trusted guidance"));
        for canary in [
            "Implicit Claude canary",
            "Implicit personal canary",
            "Implicit Codex canary",
        ] {
            assert!(!request.contains(canary));
        }
        assert_eq!(preview["effects"]["network"]["state"], "blocked");
        for effect in ["response_cache", "ledger", "runtime_state"] {
            assert_eq!(preview["effects"][effect]["state"], "disabled");
        }
    }
}

#[test]
fn configured_explicit_resolution_retains_harness_and_unverified_visibility() {
    for harness in ["normalized", "codex", "omp", "grok"] {
        let f = Fixture::new();
        f.skill("workspace/custom", "alpha", "", "Configured guidance");
        f.project_config("[roster]\nroots=['custom']\n");
        let doc = f.document(harness, &["--offline", "--require-skill", "alpha"], 0);
        assert_eq!(doc["decision"], "explicit");
        assert_eq!(doc["harness"], harness);
        assert_eq!(doc["skills"][0]["invocation_name"], "alpha");
        assert_eq!(doc["skills"][0]["visibility"], "unverified");
        assert_eq!(doc["usage"]["http_attempts"], 0);
        let warning = doc["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["kind"] == "unverified-visibility")
            .unwrap();
        assert!(!warning["message"].as_str().unwrap().contains("Claude"));
    }
}

#[test]
fn duplicate_configured_names_have_no_assumed_precedence() {
    let f = Fixture::new();
    f.skill("workspace/custom", "alpha", "", "Project definition");
    f.skill("external", "alpha", "", "Trusted definition");
    f.project_config("[roster]\nroots=['custom']\n");
    f.user_root();
    let doc = f.document("normalized", &["--offline", "--require-skill", "alpha"], 5);
    assert_eq!(doc["error"]["kind"], "unresolved-explicit");
    assert_eq!(doc["unresolved"][0]["reason"], "ambiguous");
    assert_eq!(doc["decision"], "unavailable");
}

#[test]
fn configured_restrictions_and_exclusions_remain_authoritative() {
    let f = Fixture::new();
    f.skill(
        "workspace/custom",
        "manual",
        "disable-model-invocation: true\n",
        "Manual workflow",
    );
    f.skill(
        "workspace/custom",
        "forbidden",
        "user-invocable: false\ndisable-model-invocation: true\n",
        "Forbidden workflow",
    );
    f.skill("workspace/custom", "excluded", "", "Excluded workflow");
    f.project_config("[roster]\nroots=['custom']\n[ranking]\nexclude_skills=['excluded']\n");
    let manual = f.document("normalized", &["--offline", "--require-skill", "manual"], 0);
    assert_eq!(manual["skills"][0]["invocation_name"], "manual");
    for name in ["forbidden", "excluded"] {
        let denied = f.document("normalized", &["--offline", "--require-skill", name], 5);
        assert_eq!(denied["error"]["kind"], "unresolved-explicit");
        assert_eq!(denied["unresolved"][0]["reason"], "restricted");
    }
}

#[test]
fn project_configured_root_cannot_escape_the_workspace() {
    let f = Fixture::new();
    f.skill("external", "outside", "", "Outside canary");
    f.skill("workspace/inside", "allowed", "", "Contained guidance");
    symlink(f.root.join("external"), f.root.join("workspace/escape")).unwrap();
    f.project_config("[roster]\nroots=['escape','inside']\n");
    let denied = f.document(
        "normalized",
        &["--offline", "--require-skill", "outside"],
        5,
    );
    assert_eq!(denied["error"]["kind"], "unresolved-explicit");
    assert_eq!(denied["unresolved"][0]["reason"], "missing");
    // An unreadable selected root cannot prove that "allowed" has no competitor.
    // Keep this fail-closed rule; authorize the positive scope explicitly.
    let uncertain = f.document(
        "normalized",
        &["--offline", "--require-skill", "allowed"],
        5,
    );
    assert_eq!(uncertain["unresolved"][0]["reason"], "restricted");
    f.project_config("[roster]\nroots=['inside']\n");
    let allowed = f.document(
        "normalized",
        &["--offline", "--require-skill", "allowed"],
        0,
    );
    assert_eq!(allowed["skills"][0]["invocation_name"], "allowed");
}

#[test]
fn non_claude_context_without_configured_roots_does_not_guess_an_inventory() {
    let f = Fixture::new();
    f.skill(
        "workspace/.claude/skills",
        "implicit",
        "",
        "Unselected skill",
    );
    let doc = f.document("normalized", &["--dry-run"], 5);
    assert_eq!(doc["error"]["kind"], "unusable-roster");
    assert_eq!(doc["decision"], "unavailable");
}

#[test]
fn project_configuration_still_cannot_authorize_an_absolute_root() {
    let f = Fixture::new();
    f.project_config(&format!(
        "[roster]\nroots=['{}']\n",
        f.root.join("external").display()
    ));
    let doc = f.document("normalized", &["--dry-run"], 2);
    assert_eq!(doc["error"]["kind"], "invalid-configuration");
}
