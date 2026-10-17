#![cfg(any(target_os = "linux", target_os = "macos"))]
//! Phase P7 acceptance chain: evidence-backed advisory rollout and outage-safe
//! rollback integrated at one revision.
//!
//! Satisfies contract boundary `p7_acceptance_gate` (sr-roadmap-l1i.8.8) mapped
//! in `tests/contract_matrix.toml`. Exercises the real `sr` binary against the
//! loopback TLS Jev fixture:
//! 1. Managed hook installation and prerequisite inspection;
//! 2. Request allowance guard;
//! 3. Default shadow turn (recorded, zero stdout);
//! 4. Controlled advisory opt-in (`[hook]\nmode = "advisory"`) emitting a
//!    bounded `UserPromptSubmit` recommendation and recording the exposure;
//! 5. Manual usability feedback (`sr feedback`) and scoped snooze (`sr snooze`);
//! 6. Local return to shadow (`[hook]\nmode = "shadow"`) verified under provider
//!    outage (`always-503`), plus full `sr stats --json` reconciliation.

mod support;

use serde_json::{Value, json};
use skillranker::limits::HOOK_ADDITIONAL_CONTEXT_SCALARS;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
const CANARY: &str = "synthetic-p7-gate-canary-key";
const SESSION: &str = "p7-gate-session";

struct Home {
    root: PathBuf,
}

impl Home {
    fn new() -> Self {
        let root = support::private_store_dir("p7-gate");
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

    fn write_trusted_config(&self, body: &str) {
        let dir = self.root.join("config/sr");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .unwrap();
        let path = dir.join("config.toml");
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

    fn run(&self, port: u16, args: &[&str]) -> Output {
        self.command(port, args).output().unwrap()
    }

    fn json(&self, port: u16, args: &[&str]) -> Value {
        let out = self.run(port, args);
        assert!(out.status.success(), "{args:?}: {}", describe(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    }

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
            "hook_event_name": "UserPromptSubmit",
            "prompt": prompt,
            "session_id": SESSION,
            "transcript_path": transcript,
            "cwd": self.root.join("workspace"),
            "prompt_id": format!("p7-turn-{turn}"),
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

    fn top_skill_id(&self, event_id: &str) -> String {
        let db = rusqlite::Connection::open(self.root.join("data/sr/ledger.sqlite3")).unwrap();
        db.query_row(
            "SELECT skill_id FROM ranking_candidates WHERE event_id = ?1 AND rank_position = 1",
            [event_id],
            |row| row.get(0),
        )
        .unwrap()
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
fn all_p7_contracts_verified() {
    let home = Home::new();
    let settings = home.root.join("home/.claude/settings.json");
    let settings = settings.to_str().unwrap();

    // 1. Initialize ledger, trusted network consent, and managed hook entry.
    home.json(1, &["ledger", "init", "--json"]);
    home.write_trusted_config("[network]\nenabled = true\n");
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

    // 2. Shadow baseline turn: recorded as ranked, zero stdout emitted.
    assert_quiet(&home.hook_turn(
        port,
        0,
        "Our rust test suite started failing; find out why.",
    ));
    let events = home.events();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].1, "ranked", "{events:?}");

    // 3. Opt into advisory mode in trusted user config: ranked turn emits bounded
    //    UserPromptSubmit envelope and records the exposure.
    home.write_trusted_config("[network]\nenabled = true\n[hook]\nmode = \"advisory\"\n");
    let advisory_out = home.hook_turn(port, 1, "Diagnose the failing cargo test in the workspace.");
    assert_eq!(
        advisory_out.status.code(),
        Some(0),
        "{}",
        describe(&advisory_out)
    );
    assert!(!String::from_utf8_lossy(&advisory_out.stderr).contains(CANARY));
    let envelope: Value =
        serde_json::from_slice(&advisory_out.stdout).expect("valid advisory hook JSON");
    assert_eq!(
        envelope["hookSpecificOutput"]["hookEventName"],
        "UserPromptSubmit"
    );
    let context = envelope["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(context.contains("rust-repair"), "{context}");
    assert!(context.chars().count() <= HOOK_ADDITIONAL_CONTEXT_SCALARS.max());

    let events = home.events();
    assert_eq!(events.len(), 2, "{events:?}");
    let advisory_event_id = events[1].0.clone();
    let skill_id = home.top_skill_id(&advisory_event_id);

    // 4. Record explicit manual usability judgment (`sr feedback`) and interruption
    //    control (`sr snooze`), verifying stats report judged usefulness and muted turns.
    home.json(
        port,
        &[
            "feedback",
            &advisory_event_id,
            "--skill",
            &skill_id,
            "--verdict",
            "useful",
            "--reason",
            "matched-rust-failure",
            "--provenance",
            "p7-gate-trial",
            "--json",
        ],
    );
    home.json(
        port,
        &[
            "snooze",
            &advisory_event_id,
            "--all",
            "--for",
            "30m",
            "--apply",
            "--json",
        ],
    );
    assert_quiet(&home.hook_turn(
        port,
        2,
        "Another turn while all candidates are snoozed in advisory mode.",
    ));
    assert_eq!(provider.finish(), 4, "snoozed turn sent zero requests");
    home.json(
        port,
        &["snooze", &advisory_event_id, "--clear", "--apply", "--json"],
    );

    // 5. Return to shadow mode locally (`[hook]\nmode = "shadow"`) and verify
    //    outage-safe operation during a 503 provider failure.
    home.write_trusted_config("[network]\nenabled = true\n[hook]\nmode = \"shadow\"\n");
    let doctor = home.json(1, &["doctor", "--json"]);
    assert_eq!(doctor["checks"]["hook"]["mode"], "shadow", "{doctor}");

    let failing = Provider::start(&home, "always-503");
    assert_quiet(&home.hook_turn(
        failing.port,
        3,
        "Turn after local rollback to shadow during provider outage.",
    ));
    assert!(failing.finish() <= 3);

    // 6. Reconcile complete cohort denominators in `sr stats --json`.
    let stats = home.json(1, &["stats", "--json"]);
    assert_eq!(stats["turns"]["total_evaluated"], 4, "{stats}");
    assert_eq!(stats["turns"]["emitted_suggestions"], 1, "{stats}");
    assert_eq!(stats["judgments"]["useful"], 1, "{stats}");
}
