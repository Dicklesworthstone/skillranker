#![cfg(any(target_os = "linux", target_os = "macos"))]
//! Phase P7 advisory rollout and return-to-shadow fencing contract.
//!
//! Satisfies contract boundary `p7_advisory_rollout` (sr-roadmap-l1i.8.6,
//! sr-roadmap-l1i.8.9, sr-roadmap-l1i.8.10) mapped in `tests/contract_matrix.toml`:
//! 1. Default hook mode remains quiet shadow (zero stdout on ranked turns).
//! 2. Project configuration cannot opt into advisory mode.
//! 3. Trusted user configuration (`[hook]\nmode = "advisory"`) injects a single
//!    bounded `UserPromptSubmit` JSON envelope within 1,024 Unicode scalars and
//!    records the emitted suggestion in the ledger.
//! 4. `--shadow` CLI override forces quiet shadow even when advisory is configured.
//! 5. Returning to shadow or changing the policy receipt while a provider call is
//!    in flight fences the prepared recommendation before stdout publication, and
//!    subsequent turns remain quiet under provider outage (`503`).

mod support;

use serde_json::{Value, json};
use skillranker::limits::HOOK_ADDITIONAL_CONTEXT_SCALARS;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
const CANARY: &str = "synthetic-p7-rollout-canary-key";
const SESSION: &str = "p7-rollout-session";

struct Home {
    root: PathBuf,
}

impl Home {
    fn new() -> Self {
        let root = support::private_store_dir("p7-rollout");
        for dir in [
            "home",
            "config",
            "config/sr",
            "cache",
            "data",
            "state",
            "workspace",
        ] {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(root.join(dir))
                .unwrap();
        }
        let skill = root.join("workspace/.claude/skills/rust-repair");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: rust-repair\ndescription: Runs and repairs failing rust tests.\n---\nBody.\n",
        )
        .unwrap();
        std::fs::write(
            root.join("fixture-ca.pem"),
            include_bytes!("fixtures/jev-tls/ca.pem"),
        )
        .unwrap();
        Self { root }
    }

    fn config_path(&self) -> PathBuf {
        self.root.join("config/sr/config.toml")
    }

    fn write_trusted_config(&self, body: &str) {
        let dir = self.root.join("config/sr");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .unwrap();
        let path = self.config_path();
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn command(&self, port: u16, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sr"));
        command
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("TYPESAFE_API_KEY", CANARY)
            .env("TYPESAFE_ENDPOINT", format!("https://localhost:{port}"))
            .env("SSL_CERT_FILE", self.root.join("fixture-ca.pem"))
            .current_dir(self.root.join("workspace"))
            .args(args);
        command
    }

    fn json(&self, port: u16, args: &[&str]) -> Value {
        let out = self.command(port, args).output().unwrap();
        assert!(out.status.success(), "{args:?}: {}", describe(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn hook_turn(&self, port: u16, turn: usize, prompt: &str, extra_args: &[&str]) -> Output {
        let transcript = self.root.join("workspace/transcript.jsonl");
        let mut records = String::new();
        for i in 0..=turn {
            records.push_str(
                &(json!({"type": "user", "uuid": format!("u{i}"),
                         "parentUuid": if i == 0 { Value::Null } else { json!(format!("a{}", i - 1)) },
                         "sessionId": SESSION,
                         "message": {"role": "user", "content": prompt}})
                .to_string()
                    + "\n"),
            );
            if i < turn {
                records.push_str(
                    &(json!({"type": "assistant", "uuid": format!("a{i}"),
                             "parentUuid": format!("u{i}"), "sessionId": SESSION,
                             "message": {"role": "assistant",
                                         "content": [{"type": "text", "text": "Done."}]}})
                    .to_string()
                        + "\n"),
                );
            }
        }
        std::fs::write(&transcript, records).unwrap();
        let payload = json!({
            "hook_event_name": "UserPromptSubmit",
            "prompt": prompt,
            "session_id": SESSION,
            "transcript_path": transcript,
            "cwd": self.root.join("workspace"),
            "prompt_id": format!("p7-rollout-turn-{turn}"),
        });
        let mut args = vec!["hook", "claude"];
        args.extend_from_slice(extra_args);
        let mut child = self
            .command(port, &args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
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

fn assert_quiet(out: &Output) {
    assert_eq!(out.status.code(), Some(0), "{}", describe(out));
    assert!(
        out.stdout.is_empty(),
        "expected quiet stdout: {}",
        describe(out)
    );
    assert!(!String::from_utf8_lossy(&out.stderr).contains(CANARY));
}

struct Provider {
    child: Child,
    lines: BufReader<ChildStdout>,
    port: u16,
}

impl Provider {
    fn start(home: &Home, scenario: &str, extra_args: &[&str]) -> Self {
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
        let mut cmd = Command::new("/usr/bin/python3");
        cmd.arg(dir.join("provider_server.py"))
            .arg(scenario)
            .args(extra_args)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = cmd.spawn().unwrap();
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
fn advisory_mode_injection() {
    let home = Home::new();
    home.json(1, &["ledger", "init", "--json"]);
    home.write_trusted_config("[network]\nenabled = true\n");

    let provider = Provider::start(&home, "useful", &[]);
    let port = provider.port;
    home.json(
        port,
        &[
            "budget",
            "--max-attempts",
            "30",
            "--window",
            "1h",
            "--apply",
            "--json",
        ],
    );

    // 1. Default mode is shadow: ranked turn is recorded, zero stdout emitted.
    assert_quiet(&home.hook_turn(
        port,
        0,
        "Our rust test suite started failing; diagnose the panic.",
        &[],
    ));
    let stats = home.json(1, &["stats", "--json"]);
    assert_eq!(stats["turns"]["total_evaluated"], 1, "{stats}");
    assert_eq!(stats["turns"]["emitted_suggestions"], 0, "{stats}");

    // 2. Project config attempting to enable advisory mode is refused before Jev calls.
    std::fs::create_dir_all(home.root.join("workspace/.sr")).unwrap();
    std::fs::write(
        home.root.join("workspace/.sr/config.toml"),
        "[hook]\nmode = \"advisory\"\n",
    )
    .unwrap();
    assert_quiet(&home.hook_turn(
        port,
        1,
        "Second turn with forbidden project hook.mode.",
        &[],
    ));
    std::fs::write(home.root.join("workspace/.sr/config.toml"), "").unwrap();

    // 3. Trusted user opt-in (`[hook]\nmode = "advisory"`) emits bounded envelope on ranked turn.
    home.write_trusted_config("[network]\nenabled = true\n[hook]\nmode = \"advisory\"\n");
    let out = home.hook_turn(
        port,
        2,
        "Investigate the failing rust unit test in this crate.",
        &[],
    );
    assert_eq!(out.status.code(), Some(0), "{}", describe(&out));
    assert!(!String::from_utf8_lossy(&out.stderr).contains(CANARY));
    let wire = String::from_utf8(out.stdout.clone()).expect("utf8 stdout");
    assert!(
        !wire.trim().is_empty(),
        "advisory mode must emit envelope: {}",
        describe(&out)
    );
    let envelope: Value = serde_json::from_str(wire.trim()).expect("valid hook JSON");
    assert_eq!(
        envelope["hookSpecificOutput"]["hookEventName"],
        "UserPromptSubmit"
    );
    let context = envelope["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("additionalContext string");
    assert!(
        context.contains("Suggested skill for the next step: rust-repair."),
        "{context}"
    );
    assert!(
        context.chars().count() <= HOOK_ADDITIONAL_CONTEXT_SCALARS.max(),
        "{context}"
    );
    assert!(
        !context
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\t')),
        "{context}"
    );

    let stats = home.json(1, &["stats", "--json"]);
    assert_eq!(stats["turns"]["emitted_suggestions"], 1, "{stats}");

    // 4. `--shadow` CLI flag overrides advisory config and suppresses stdout.
    assert_quiet(&home.hook_turn(
        port,
        3,
        "Run another check with --shadow override.",
        &["--shadow"],
    ));
    let stats = home.json(1, &["stats", "--json"]);
    assert_eq!(stats["turns"]["emitted_suggestions"], 1, "{stats}");
    assert_eq!(provider.finish(), 6);

    // 5. In-flight return to shadow fences pending advice before stdout publication.
    let fencing_provider = Provider::start(
        &home,
        "useful+write-on-rerank",
        &[
            home.config_path().to_str().unwrap(),
            "[network]\nenabled = true\n[hook]\nmode = \"shadow\"\n",
        ],
    );
    let fencing_port = fencing_provider.port;
    home.json(
        fencing_port,
        &[
            "budget",
            "--max-attempts",
            "30",
            "--window",
            "1h",
            "--apply",
            "--json",
        ],
    );
    assert_quiet(&home.hook_turn(
        fencing_port,
        4,
        "Turn where operator returns to shadow while rerank is in flight.",
        &[],
    ));
    assert_eq!(fencing_provider.finish(), 2);
    let stats = home.json(1, &["stats", "--json"]);
    assert_eq!(
        stats["turns"]["emitted_suggestions"], 1,
        "fenced in-flight turn must not record an emission: {stats}"
    );

    // 6. Outage-safe shadow behavior after rollback while provider returns 503.
    let failing = Provider::start(&home, "always-503", &[]);
    assert_quiet(&home.hook_turn(
        failing.port,
        5,
        "Turn during provider outage after return to shadow.",
        &[],
    ));
    assert!(failing.finish() <= 3);
}
