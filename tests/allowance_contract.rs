#![cfg(any(target_os = "linux", target_os = "macos"))]
//! The trusted shared HTTP-attempt allowance (sr-roadmap-l1i.7.4, .7.5):
//! `sr budget` setup with a crash-safe activation intent, and durable debits
//! before each send, through the real binary against the loopback TLS Jev
//! fixture. Synthetic skills and sessions only.

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
        let root = support::private_store_dir("allowance");
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

    fn guard(&self) -> PathBuf {
        self.root.join("config/sr/allowance.toml")
    }

    fn accounting(&self) -> PathBuf {
        self.root.join("cache/sr/allowance.sqlite3")
    }

    fn command(&self, port: u16, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sr"));
        command
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("TYPESAFE_API_KEY", "synthetic-allowance-canary")
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
            "schema_version": 1, "harness": "claude_code", "producer_id": "allowance-contract",
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

#[test]
fn inspection_and_preview_write_nothing_and_apply_is_idempotent() {
    let home = Home::new();
    let inspect = home.budget(1, &[]);
    assert_eq!(inspect["mode"], "inspect");
    assert_eq!(inspect["health"], "disabled");
    assert!(inspect["guard"].is_null());
    assert_eq!(inspect["window"]["kind"], "fixed-utc");
    let (start, end) = (
        inspect["window"]["start_unix_ms"].as_u64().unwrap(),
        inspect["window"]["end_unix_ms"].as_u64().unwrap(),
    );
    assert_eq!((start % 3_600_000, end - start), (0, 3_600_000));

    let preview = home.budget(1, &["--max-attempts", "5", "--window", "1h"]);
    assert_eq!(preview["mode"], "preview");
    assert_eq!(preview["plan"]["generation"], 1);
    assert!(!home.guard().exists() && !home.accounting().exists());

    let applied = home.budget(1, &["--max-attempts", "5", "--window", "1h", "--apply"]);
    assert_eq!(applied["health"], "ready", "{applied}");
    assert_eq!(applied["guard"]["max_attempts"], 5);
    assert_eq!(applied["remaining_attempts"], 5);
    let again = home.budget(1, &["--max-attempts", "5", "--window", "1h", "--apply"]);
    assert_eq!(again["plan"]["unchanged"], true);
    assert_eq!(again["guard"]["generation"], 1);

    for bad in [
        &["--max-attempts", "5"][..],
        &["--max-attempts", "0", "--window", "1h"][..],
        &["--max-attempts", "10001", "--window", "1h"][..],
        &["--max-attempts", "5", "--window", "30m"][..],
        &["--apply"][..],
    ] {
        let mut args = vec!["budget"];
        args.extend_from_slice(bad);
        assert_eq!(home.run(1, &args).status.code(), Some(2), "{bad:?}");
    }
}

#[test]
fn an_exhausted_window_refuses_the_next_send_after_charging_the_wide_one() {
    let home = Home::new();
    home.budget(1, &["--max-attempts", "3", "--window", "1h", "--apply"]);
    // One provider, so both rankings share one endpoint origin: the port is
    // part of the origin, and each origin has its own count.
    let provider = Provider::start(&home);
    let port = provider.port;
    let first = home.context("first");
    let out = home.run(port, &Home::rank_args(&first, &[]));
    assert!(out.status.success(), "{}", describe(&out));
    let report = home.budget(port, &[]);
    assert_eq!(report["charged_attempts"], 2, "{report}");

    // The third admission is the wide request; rerank finds nothing left, and
    // the wide winner is never promoted.
    let second = home.context("second");
    let out = home.run(port, &Home::rank_args(&second, &[]));
    assert_eq!(
        provider.finish(),
        3,
        "two full rankings' worth of sends were refused after the charged wide one"
    );
    assert_eq!(kind(&out), (Some(4), "request-budget".to_owned()));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["decision"], "unavailable", "{doc}");

    let report = home.budget(port, &[]);
    assert_eq!(report["charged_attempts"], 3, "{report}");
    assert_eq!(report["remaining_attempts"], 0);

    // Another origin is a separate scope.
    let other = home.budget(1, &[]);
    assert_eq!(other["charged_attempts"], 0, "{other}");

    // Raising the limit keeps the charges already made.
    let raised = home.budget(port, &["--max-attempts", "4", "--window", "1h", "--apply"]);
    assert_eq!(raised["guard"]["generation"], 2);
    assert_eq!(raised["charged_attempts"], 3);
    assert_eq!(raised["remaining_attempts"], 1);
}

#[test]
fn without_persistence_a_guard_refuses_sends_but_explicit_requests_resolve() {
    let home = Home::new();
    home.budget(1, &["--max-attempts", "50", "--window", "1h", "--apply"]);
    let context = home.context("stateless");
    let provider = Provider::start(&home);
    let out = home.run(provider.port, &Home::rank_args(&context, &["--no-persist"]));
    assert_eq!(provider.finish(), 0);
    assert_eq!(kind(&out), (Some(4), "budget-state".to_owned()));

    let provider = Provider::start(&home);
    let out = home.run(
        provider.port,
        &Home::rank_args(&context, &["--no-persist", "--require-skill", "beta"]),
    );
    assert_eq!(provider.finish(), 0);
    assert!(out.status.success(), "{}", describe(&out));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["decision"], "explicit");

    // Positive twin: with no guard, a stateless run sends and creates no state.
    std::fs::rename(home.guard(), home.root.join("guard.bak")).unwrap();
    let provider = Provider::start(&home);
    let out = home.run(provider.port, &Home::rank_args(&context, &["--no-persist"]));
    assert_eq!(provider.finish(), 2);
    assert!(out.status.success(), "{}", describe(&out));
}

#[test]
fn an_interrupted_setup_blocks_sends_until_apply_resumes_it() {
    let home = Home::new();
    home.budget(1, &["--max-attempts", "20", "--window", "1h", "--apply"]);
    // A crash after the intent was published, before accounting caught up.
    let guard = std::fs::read_to_string(home.guard()).unwrap();
    std::fs::write(
        home.guard(),
        guard
            .replace("state = \"ready\"", "state = \"intent\"")
            .replace("generation = 1", "generation = 2")
            .replace("max_attempts = 20", "max_attempts = 30"),
    )
    .unwrap();
    let context = home.context("blocked");
    let provider = Provider::start(&home);
    let out = home.run(provider.port, &Home::rank_args(&context, &[]));
    assert_eq!(provider.finish(), 0, "an incomplete setup admitted a send");
    assert_eq!(kind(&out), (Some(4), "budget-state".to_owned()));
    assert_eq!(home.budget(1, &[])["health"], "setup-incomplete");

    let resumed = home.budget(1, &["--max-attempts", "30", "--window", "1h", "--apply"]);
    assert_eq!(resumed["plan"]["resumes_intent"], true, "{resumed}");
    assert_eq!(resumed["guard"]["generation"], 2);
    assert_eq!(resumed["health"], "ready");
    let provider = Provider::start(&home);
    let out = home.run(provider.port, &Home::rank_args(&context, &[]));
    assert_eq!(provider.finish(), 2);
    assert!(out.status.success(), "{}", describe(&out));
}

#[test]
fn concurrent_processes_never_exceed_the_window() {
    let home = Home::new();
    home.budget(1, &["--max-attempts", "5", "--window", "1h", "--apply"]);
    let provider = Provider::start(&home);
    let contexts: Vec<String> = (0..4).map(|i| home.context(&format!("c{i}"))).collect();
    let children: Vec<Child> = contexts
        .iter()
        .map(|context| {
            home.command(provider.port, &Home::rank_args(context, &[]))
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let outputs: Vec<Output> = children
        .into_iter()
        .map(|child| child.wait_with_output().unwrap())
        .collect();
    let port = provider.port;
    let served = provider.finish();
    assert!(served <= 5, "{served} sends exceeded the allowance of 5");
    let report = home.budget(port, &[]);
    assert_eq!(
        report["charged_attempts"].as_u64(),
        Some(served as u64),
        "every send was charged exactly once: {report}"
    );
    let refused = outputs
        .iter()
        .filter(|out| kind(out) == (Some(4), "request-budget".to_owned()))
        .count();
    assert!(refused >= 1, "demand of 8 attempts met no refusal");
    for out in &outputs {
        let code = out.status.code();
        assert!(matches!(code, Some(0) | Some(4)), "{}", describe(out));
    }
}
