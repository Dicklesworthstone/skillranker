#![cfg(any(target_os = "linux", target_os = "macos"))]
//! The setup journey (sr-roadmap-l1i.7.9, I05) in one isolated home: offline
//! demo, doctor, the first missing prerequisite at each step (key, network,
//! roster), a first rank that needs no ledger, an installer that reports the
//! recorded-trial prerequisites without enabling them or leaking private
//! configuration, and finally a recorded shadow hook trial. The loopback TLS
//! Jev fixture stands in for the provider; data is synthetic.

mod support;

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
const CANARY: &str = "synthetic-onboarding-canary-key";

struct Home {
    root: PathBuf,
}

impl Home {
    fn new() -> Self {
        let root = support::private_store_dir("onboarding");
        for dir in ["home", "config", "cache", "data", "state", "workspace"] {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(root.join(dir))
                .unwrap();
        }
        std::fs::write(
            root.join("fixture-ca.pem"),
            include_bytes!("fixtures/jev-tls/ca.pem"),
        )
        .unwrap();
        Self { root }
    }

    fn add_skill(&self) {
        let dir = self.root.join("workspace/.claude/skills/rust-repair");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: rust-repair\ndescription: Runs and repairs failing rust tests.\n---\nBody.\n",
        )
        .unwrap();
    }

    /// Files under the private state roots, which setup must not create
    /// before it is asked to.
    fn state_files(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut stack: Vec<PathBuf> = ["config", "cache", "data", "state"]
            .iter()
            .map(|dir| self.root.join(dir))
            .collect();
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    found.push(path);
                }
            }
        }
        found
    }

    fn command(&self, port: u16, key: bool, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sr"));
        command
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("TYPESAFE_ENDPOINT", format!("https://localhost:{port}"))
            .env("SSL_CERT_FILE", self.root.join("fixture-ca.pem"))
            .current_dir(self.root.join("workspace"))
            .args(args);
        if key {
            command.env("TYPESAFE_API_KEY", CANARY);
        }
        command
    }

    fn run(&self, port: u16, key: bool, args: &[&str]) -> Output {
        self.command(port, key, args).output().unwrap()
    }

    fn context(&self) -> String {
        let path = self.root.join("workspace/context.json");
        let value = json!({
            "schema_version": 1, "harness": "claude_code", "producer_id": "onboarding",
            "workspace_root": self.root.join("workspace").to_str().unwrap(),
            "session_id": "onboarding-session", "agent_id": null, "branch_id": null,
            "context_epoch": null,
            "current_request": {"event_id": "turn-1",
                "text": "Our rust test suite started failing; find out why.",
                "attachments_omitted": false, "essential_attachment_missing": false},
            "events": [], "explicit_skill_references": [], "supplied_loads": []
        });
        std::fs::write(&path, value.to_string()).unwrap();
        path.to_string_lossy().into_owned()
    }
}

fn describe(out: &Output) -> String {
    format!(
        "exit {:?}\n{}\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn kind(out: &Output) -> (Option<i32>, String) {
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    (
        out.status.code(),
        doc["error"]["kind"].as_str().unwrap_or("").to_owned(),
    )
}

struct Provider {
    child: Child,
    lines: BufReader<ChildStdout>,
    port: u16,
}

impl Provider {
    fn start(home: &Home) -> Self {
        let dir = home
            .root
            .join(format!("provider-{}", NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir(&dir).unwrap();
        for (name, bytes) in [
            (
                "provider_server.py",
                &include_bytes!("fixtures/jev-tls/provider_server.py")[..],
            ),
            (
                "server.pem",
                &include_bytes!("fixtures/jev-tls/server.pem")[..],
            ),
            (
                "server.key",
                &include_bytes!("fixtures/jev-tls/server.key")[..],
            ),
        ] {
            std::fs::write(dir.join(name), bytes).unwrap();
        }
        let mut child = Command::new("/usr/bin/python3")
            .arg(dir.join("provider_server.py"))
            .arg("useful")
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap());
        let mut hello = String::new();
        lines.read_line(&mut hello).unwrap();
        let hello: Value = serde_json::from_str(&hello).unwrap();
        let port = u16::try_from(hello["port"].as_u64().unwrap()).unwrap();
        Self { child, lines, port }
    }

    fn finish(mut self) -> usize {
        let mut done = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        done.write_all(b"DONE").unwrap();
        drop(done);
        let mut served = 0;
        loop {
            let mut line = String::new();
            assert!(
                self.lines.read_line(&mut line).unwrap() > 0,
                "provider ended early"
            );
            let value: Value = serde_json::from_str(&line).unwrap();
            if value["done"] == true {
                break;
            }
            served += usize::from(value["handshake_rejected"] != true);
        }
        let _ = self.child.wait();
        served
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn the_setup_journey_from_demo_to_a_recorded_shadow_trial() {
    let home = Home::new();

    // 1. The demo explains the product offline, with no key, configuration,
    //    roster or ledger, and creates no state.
    let demo = home.run(1, false, &["demo", "--case", "useful", "--json"]);
    assert!(demo.status.success(), "{}", describe(&demo));
    let doc: Value = serde_json::from_slice(&demo.stdout).unwrap();
    assert_eq!(doc["actionable"], false, "{doc}");
    assert!(home.state_files().is_empty(), "{:?}", home.state_files());

    // 2. Doctor names each missing prerequisite and its next step.
    let doctor = home.run(1, false, &["doctor", "--json"]);
    assert!(doctor.status.success(), "{}", describe(&doctor));
    let checks = serde_json::from_slice::<Value>(&doctor.stdout).unwrap()["checks"].clone();
    assert_eq!(checks["credential"]["state"], "absent", "{checks}");
    assert!(checks["credential"]["next_step"].is_string());
    assert_eq!(checks["network"]["state"], "not-authorized", "{checks}");
    assert!(checks["network"]["next_step"].is_string());
    assert_eq!(checks["ledger"]["state"], "not-available", "{checks}");
    assert_ne!(checks["roster"]["state"], "ready", "{checks}");

    // 3. The first missing prerequisite at each step of a rank.
    let context = home.context();
    let empty = home.run(
        1,
        true,
        &["rank", "--context", &context, "--allow-network", "--json"],
    );
    assert_eq!(
        kind(&empty).0,
        Some(5),
        "empty roster: {}",
        describe(&empty)
    );
    home.add_skill();
    let no_key = home.run(
        1,
        false,
        &["rank", "--context", &context, "--allow-network", "--json"],
    );
    assert_eq!(kind(&no_key), (Some(4), "credential-absent".to_owned()));
    let denied = home.run(1, true, &["rank", "--context", &context, "--json"]);
    assert_eq!(kind(&denied), (Some(8), "network-denied".to_owned()));

    // 4. A first authorized rank needs no ledger, and makes none.
    let provider = Provider::start(&home);
    let first = home.run(
        provider.port,
        true,
        &["rank", "--context", &context, "--allow-network", "--json"],
    );
    assert_eq!(provider.finish(), 2);
    assert!(first.status.success(), "{}", describe(&first));
    let doc: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(doc["decision"], "ranked", "{doc}");
    assert_ne!(doc["persistence"]["ledger"], "recorded", "{doc}");

    // 5. The installer reports both recorded-trial prerequisites, enables
    //    neither, and never prints the key or configuration contents.
    let settings = home.root.join("home/.claude/settings.json");
    let settings_arg = settings.to_str().unwrap();
    let preview = home.run(
        1,
        true,
        &["install-hook", "claude", "--settings-file", settings_arg],
    );
    assert!(preview.status.success(), "{}", describe(&preview));
    let text = String::from_utf8_lossy(&preview.stdout);
    assert!(text.contains("ledger: missing"), "{text}");
    assert!(text.contains("trusted network consent: missing"), "{text}");
    assert!(
        !text.contains(CANARY),
        "the key leaked into installer output"
    );
    assert!(!settings.exists(), "a preview wrote settings");
    assert!(
        !home.root.join("config/sr/config.toml").exists(),
        "the installer enabled network consent"
    );

    // 6. The user initializes the ledger and grants trusted consent, then
    //    installs; the report now shows both satisfied.
    let init = home.run(1, false, &["ledger", "init", "--json"]);
    assert!(init.status.success(), "{}", describe(&init));
    std::fs::create_dir_all(home.root.join("config/sr")).unwrap();
    std::fs::write(
        home.root.join("config/sr/config.toml"),
        "[network]\nenabled = true\n",
    )
    .unwrap();
    let applied = home.run(
        1,
        true,
        &[
            "install-hook",
            "claude",
            "--settings-file",
            settings_arg,
            "--apply",
        ],
    );
    assert!(applied.status.success(), "{}", describe(&applied));
    let text = String::from_utf8_lossy(&applied.stdout);
    assert!(text.contains("ledger: initialized"), "{text}");
    assert!(text.contains("trusted network consent: enabled"), "{text}");
    assert!(!text.contains(CANARY));
    assert!(!std::fs::read_to_string(&settings).unwrap().contains(CANARY));

    // 7. A recorded shadow trial: the hook ranks with trusted consent (no
    //    --allow-network), publishes nothing, and records the event.
    let transcript = home.root.join("workspace/transcript.jsonl");
    let prompt = "Our rust test suite started failing; find out why.";
    std::fs::write(
        &transcript,
        json!({"type": "user", "uuid": "u1", "parentUuid": null,
               "sessionId": "onboarding-hook", "message": {"role": "user", "content": prompt}})
        .to_string()
            + "\n",
    )
    .unwrap();
    let payload = json!({
        "hook_event_name": "UserPromptSubmit", "prompt": prompt,
        "session_id": "onboarding-hook", "transcript_path": transcript,
        "cwd": home.root.join("workspace"),
    });
    let provider = Provider::start(&home);
    let mut hook = home
        .command(provider.port, true, &["hook", "claude"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    hook.stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let hook = hook.wait_with_output().unwrap();
    assert_eq!(provider.finish(), 2, "{}", describe(&hook));
    assert_eq!(hook.status.code(), Some(0));
    assert!(hook.stdout.is_empty(), "shadow mode injected advice");
    let stats = home.run(1, false, &["stats", "--json"]);
    let stats: Value = serde_json::from_slice(&stats.stdout).unwrap();
    assert_eq!(stats["retained"]["total_events"], 1, "{stats}");
    let shadow = &stats["turns"]["by_channel"][0];
    assert_eq!(shadow["channel"], "shadow", "{stats}");
    assert_eq!(shadow["evaluated_turns"], 1, "{stats}");
    assert_eq!(shadow["emitted"], 0, "shadow mode emits nothing");
    assert_eq!(stats["provider"]["total_attempts"], 2, "{stats}");
}
