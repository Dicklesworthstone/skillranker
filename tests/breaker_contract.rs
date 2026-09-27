#![cfg(any(target_os = "linux", target_os = "macos"))]
//! The provider circuit breaker (sr-roadmap-l1i.7.7) through the real binary
//! against the loopback TLS Jev fixture: shared cooldown across processes,
//! process-local protection when persistence is off or the store is unusable,
//! authentication failures that never open the circuit, and a half-open probe
//! that closes it. Timing-dependent transitions (doubling, lease expiry,
//! obsolete generations) are unit-tested in `src/breaker.rs`. Synthetic data only.

mod support;

use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Home {
    root: PathBuf,
}

impl Home {
    fn new() -> Self {
        let root = support::private_store_dir("breaker");
        for dir in ["home", "config", "cache", "data", "workspace"] {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(root.join(dir))
                .unwrap();
        }
        for (name, description) in [
            ("alpha", "Runs and repairs failing rust tests."),
            ("beta", "Drafts release notes from git history."),
        ] {
            let dir = root.join("workspace/.claude/skills").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {description}\n---\nBody.\n"),
            )
            .unwrap();
        }
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
            .env("TYPESAFE_API_KEY", "synthetic-breaker-canary")
            .env("TYPESAFE_ENDPOINT", format!("https://localhost:{port}"))
            .env("SSL_CERT_FILE", self.root.join("fixture-ca.pem"))
            .current_dir(self.root.join("workspace"))
            .args(args);
        command
    }

    fn run(&self, port: u16, args: &[&str]) -> Output {
        self.command(port, args).output().unwrap()
    }

    /// `sr budget` as seen from the endpoint on `port`.
    fn budget(&self, port: u16, args: &[&str]) -> Value {
        let mut full = vec!["budget", "--json"];
        full.extend_from_slice(args);
        let out = self.run(port, &full);
        assert!(out.status.success(), "{full:?}: {}", describe(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn context(&self, turn: &str) -> String {
        let path = self.root.join("workspace").join(format!(
            "context-{}.json",
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let value = json!({
            "schema_version": 1, "harness": "claude_code", "producer_id": "breaker-contract",
            "workspace_root": self.root.join("workspace").to_str().unwrap(),
            "session_id": "s1", "agent_id": null, "branch_id": null, "context_epoch": null,
            "current_request": {"event_id": turn,
                "text": format!("Our rust test suite started failing ({turn}); find out why."),
                "attachments_omitted": false, "essential_attachment_missing": false},
            "events": [], "explicit_skill_references": [], "supplied_loads": []
        });
        std::fs::write(&path, value.to_string()).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn rank_args<'a>(context: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
        let mut args = vec![
            "rank",
            "--context",
            context,
            "--allow-network",
            "--no-cache",
            "--timeout-ms",
            "12000",
            "--json",
        ];
        args.extend_from_slice(extra);
        args
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

/// The loopback TLS Jev fixture.
struct Provider {
    child: Child,
    lines: BufReader<ChildStdout>,
    port: u16,
}

impl Provider {
    fn start(home: &Home) -> Self {
        Self::scenario(home, "useful", &[])
    }

    /// A scenario with optional mutation target and text, as the fixture takes.
    fn scenario(home: &Home, scenario: &str, args: &[&str]) -> Self {
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
            .args(args)
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

    /// Requests the provider answered.
    fn finish(mut self) -> usize {
        let mut done = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        std::io::Write::write_all(&mut done, b"DONE").unwrap();
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

fn breaker_db(home: &Home) -> PathBuf {
    home.root.join("cache/sr/breaker.sqlite3")
}

fn warning_kinds(out: &Output) -> Vec<String> {
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    doc["warnings"]
        .as_array()
        .map(|w| {
            w.iter()
                .filter_map(|w| w["kind"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn three_transient_failures_open_the_circuit_for_the_next_process() {
    let home = Home::new();
    home.budget(1, &["--max-attempts", "100", "--window", "1h", "--apply"]);
    let provider = Provider::scenario(&home, "always-503", &[]);
    let port = provider.port;
    let first = home.run(port, &Home::rank_args(&home.context("a"), &[]));
    // The fourth retry is refused by the circuit that the third failure opened.
    let second = home.run(port, &Home::rank_args(&home.context("b"), &[]));
    assert_eq!(provider.finish(), 3, "only three failing sends in total");
    assert_eq!(kind(&first), (Some(4), "provider-cooldown".to_owned()));
    assert_eq!(kind(&second), (Some(4), "provider-cooldown".to_owned()));
    assert!(breaker_db(&home).exists());
    assert!(!warning_kinds(&second).contains(&"breaker-process-local".to_owned()));
    // A cooldown refusal is decided before the allowance charges anything.
    assert_eq!(home.budget(port, &[])["charged_attempts"], 3);
}

#[test]
fn without_persistence_protection_is_process_local() {
    let home = Home::new();
    let provider = Provider::scenario(&home, "always-503", &[]);
    let port = provider.port;
    for turn in ["a", "b"] {
        let out = home.run(
            port,
            &Home::rank_args(&home.context(turn), &["--no-persist"]),
        );
        assert_eq!(
            kind(&out),
            (Some(4), "provider-cooldown".to_owned()),
            "{turn}"
        );
    }
    // Each process opened its own circuit after three failures.
    assert_eq!(provider.finish(), 6);
    assert!(
        !breaker_db(&home).exists(),
        "no-persist created breaker state"
    );
}

#[test]
fn an_unusable_store_degrades_visibly_to_process_local_protection() {
    let home = Home::new();
    std::fs::create_dir_all(breaker_db(&home)).unwrap(); // a directory where the file belongs
    let provider = Provider::scenario(&home, "always-503", &[]);
    let out = home.run(provider.port, &Home::rank_args(&home.context("a"), &[]));
    assert_eq!(provider.finish(), 3);
    assert_eq!(kind(&out), (Some(4), "provider-cooldown".to_owned()));
    assert!(
        warning_kinds(&out).contains(&"breaker-process-local".to_owned()),
        "{}",
        describe(&out)
    );
}

#[test]
fn authentication_failures_never_open_the_circuit() {
    let home = Home::new();
    let provider = Provider::scenario(&home, "unauthorized", &[]);
    let port = provider.port;
    for turn in ["a", "b", "c", "d"] {
        let out = home.run(port, &Home::rank_args(&home.context(turn), &[]));
        assert_eq!(kind(&out), (Some(4), "authentication".to_owned()), "{turn}");
    }
    assert_eq!(
        provider.finish(),
        4,
        "each rejected key is tried once, never retried"
    );
}

#[test]
fn a_successful_half_open_probe_closes_the_circuit_for_everyone() {
    let home = Home::new();
    let failing = Provider::scenario(&home, "always-503", &[]);
    let port = failing.port;
    home.run(port, &Home::rank_args(&home.context("a"), &[]));
    assert_eq!(failing.finish(), 3);
    // The origin recovered and the cooldown has passed (as if 30 s elapsed).
    // A fresh fixture listens on a new port, and the port is part of the
    // origin, so the recorded open circuit moves to the healthy origin.
    let healthy = Provider::start(&home);
    let port = healthy.port;
    let db = rusqlite::Connection::open(breaker_db(&home)).unwrap();
    let changed = db
        .execute(
            "UPDATE breaker_state SET origin = ?1, open_until_ms = 0 WHERE open = 1",
            [format!("https://localhost:{port}")],
        )
        .unwrap();
    assert_eq!(changed, 1);
    drop(db);
    let refused = rusqlite::Connection::open(breaker_db(&home))
        .unwrap()
        .query_row(
            "SELECT count(*) FROM breaker_state WHERE open = 1",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(refused, 1, "the circuit starts open");
    let probe = home.run(port, &Home::rank_args(&home.context("probe"), &[]));
    let after = home.run(port, &Home::rank_args(&home.context("after"), &[]));
    assert_eq!(healthy.finish(), 4);
    assert!(probe.status.success(), "{}", describe(&probe));
    assert!(after.status.success(), "{}", describe(&after));
}
