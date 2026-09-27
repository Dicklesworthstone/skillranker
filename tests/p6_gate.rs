#![cfg(any(target_os = "linux", target_os = "macos"))]
//! Phase P6 acceptance chain: a recorded, bounded Claude shadow integration,
//! integrated at one revision.
//!
//! Satisfies the local half of contract boundary `p6_acceptance_gate`
//! (sr-roadmap-l1i.7.13) mapped in `tests/contract_matrix.toml`. One isolated
//! home runs the P6 controls through the real binary against the loopback TLS
//! Jev fixture (synthetic data only):
//! 1. managed hook installation that reports its prerequisites;
//! 2. a request allowance;
//! 3. recorded shadow turns that inject nothing;
//! 4. a scoped snooze that silences the next turn without a provider request;
//! 5. a failing provider that stays quiet;
//! 6. uninstallation.
//!
//! Real-harness conformance (7.11) and measured latency (7.12) are separate
//! evidence: this chain uses the fixture, not Claude Code.

mod support;

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
const CANARY: &str = "synthetic-p6-gate-canary-key";
const SESSION: &str = "p6-gate-session";

struct Home {
    root: PathBuf,
}

impl Home {
    fn new() -> Self {
        let root = support::private_store_dir("p6-gate");
        for dir in ["home", "config", "cache", "data", "state", "workspace"] {
            std::fs::DirBuilder::new()
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

    fn run(&self, port: u16, args: &[&str]) -> Output {
        self.command(port, args).output().unwrap()
    }

    fn json(&self, port: u16, args: &[&str]) -> Value {
        let out = self.run(port, args);
        assert!(out.status.success(), "{args:?}: {}", describe(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    }

    /// One user turn delivered to `sr hook claude` as Claude Code would.
    fn hook_turn(&self, port: u16, turn: usize, prompt: &str) -> Output {
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
            "hook_event_name": "UserPromptSubmit", "prompt": prompt,
            "session_id": SESSION, "transcript_path": transcript,
            "cwd": self.root.join("workspace"), "prompt_id": format!("p6-turn-{turn}"),
        });
        let mut child = self
            .command(port, &["hook", "claude"])
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

    fn events(&self) -> Vec<(String, String, String)> {
        let db = rusqlite::Connection::open(self.root.join("data/sr/ledger.sqlite3")).unwrap();
        let mut statement = db
            .prepare(
                "SELECT event_id, decision, reason FROM ranking_events ORDER BY created_at_unix_ms",
            )
            .unwrap();
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
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

/// A hook turn is quiet: exit zero, nothing injected, the key never printed.
fn assert_quiet(out: &Output) {
    assert_eq!(out.status.code(), Some(0), "{}", describe(out));
    assert!(
        out.stdout.is_empty(),
        "shadow mode injected: {}",
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
    fn start(home: &Home, scenario: &str) -> Self {
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
            .arg(scenario)
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
fn all_p6_contracts_verified() {
    let home = Home::new();
    let settings = home.root.join("home/.claude/settings.json");
    let settings = settings.to_str().unwrap();

    // 1. Installation previews, reports what is missing, and changes nothing
    //    else; apply merges the managed entry idempotently.
    let preview = home.run(1, &["install-hook", "claude", "--settings-file", settings]);
    assert!(preview.status.success(), "{}", describe(&preview));
    let text = String::from_utf8_lossy(&preview.stdout);
    assert!(
        text.contains("ledger: missing") && text.contains("consent: missing"),
        "{text}"
    );
    home.json(1, &["ledger", "init", "--json"]);
    std::fs::create_dir_all(home.root.join("config/sr")).unwrap();
    std::fs::write(
        home.root.join("config/sr/config.toml"),
        "[network]\nenabled = true\n",
    )
    .unwrap();
    for _ in 0..2 {
        let applied = home.run(
            1,
            &[
                "install-hook",
                "claude",
                "--settings-file",
                settings,
                "--apply",
            ],
        );
        assert!(applied.status.success(), "{}", describe(&applied));
    }
    let installed = std::fs::read_to_string(settings).unwrap();
    assert_eq!(installed.matches("hook claude").count(), 1, "{installed}");
    assert!(!installed.contains(CANARY));

    // 2. A request allowance for this user and endpoint.
    let provider = Provider::start(&home, "useful");
    let port = provider.port;
    home.json(
        port,
        &[
            "budget",
            "--max-attempts",
            "20",
            "--window",
            "1h",
            "--apply",
            "--json",
        ],
    );

    // 3. A recorded shadow turn: two charged sends, nothing injected.
    let prompt = "Our rust test suite started failing; find out why.";
    assert_quiet(&home.hook_turn(port, 0, prompt));
    let events = home.events();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].1, "ranked", "{events:?}");

    // 4. Snooze every advisory candidate for this session: the next turn is
    //    quiet, abstains as snoozed, and sends nothing.
    home.json(
        port,
        &[
            "snooze",
            &events[0].0,
            "--all",
            "--for",
            "30m",
            "--apply",
            "--json",
        ],
    );
    assert_quiet(&home.hook_turn(port, 1, prompt));
    let events = home.events();
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(
        (events[1].1.as_str(), events[1].2.as_str()),
        ("abstain", "snoozed"),
        "{events:?}"
    );
    assert_eq!(provider.finish(), 2, "the snoozed turn sent nothing");
    let budget = home.json(port, &["budget", "--json"]);
    assert_eq!(budget["charged_attempts"], 2, "{budget}");
    home.json(
        port,
        &["snooze", &events[0].0, "--clear", "--apply", "--json"],
    );

    // 5. A failing provider: the turn is quiet and recorded as unavailable.
    let failing = Provider::start(&home, "always-503");
    assert_quiet(&home.hook_turn(failing.port, 2, prompt));
    assert!(
        failing.finish() <= 3,
        "the circuit bounds a failing endpoint"
    );
    let events = home.events();
    assert_eq!(events.len(), 3, "{events:?}");
    assert_eq!(events[2].1, "unavailable", "{events:?}");

    // Statistics see the shadow cohort; nothing was ever emitted.
    let stats = home.json(1, &["stats", "--json"]);
    assert_eq!(stats["turns"]["emitted_suggestions"], 0, "{stats}");
    assert_eq!(stats["turns"]["total_evaluated"], 3, "{stats}");

    // 6. Uninstallation removes only the managed entry.
    let removed = home.run(
        1,
        &[
            "uninstall-hook",
            "claude",
            "--settings-file",
            settings,
            "--apply",
        ],
    );
    assert!(removed.status.success(), "{}", describe(&removed));
    assert!(
        !std::fs::read_to_string(settings)
            .unwrap()
            .contains("hook claude")
    );
}
