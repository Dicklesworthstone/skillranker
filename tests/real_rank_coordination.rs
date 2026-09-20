#![cfg(unix)]
//! Real process coordination and fenced cache publication acceptance tests (sr-roadmap-l1i.5.29).
//!
//! Validates the production CLI single-flight coordination and fenced cache publication boundary:
//! 1. Ordinary two-consumer success: Concurrent CLI processes with the same request share a single
//!    provider evaluation pair (1 wide, 1 rerank); followers incur 0 new provider calls/tokens.
//!    Subsequent execution exact offline cache reuse is proven.
//! 2. Follower deadline bounding while leader is active: A follower whose deadline approaches
//!    while an active leader is in progress makes ZERO provider attempts and exits cleanly.
//! 3. Expired/superseded leader exclusion: A stale leader whose lease expired cannot overwrite
//!    a newer generation's cache response in `cache.sqlite3` and cannot complete a successor's lease.
//! 4. Completed lease with absent/partial cache: A completed lease with an absent or partial pair
//!    in the store forces leadership reacquisition before evaluation.
//! 5. `--no-cache`, `--no-persist`, and storage unavailable paths preserve documented behavior.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static NEXT: AtomicU64 = AtomicU64::new(0);
const CONSENT: &str = "[network]\nenabled = true\n";
const TASK: &str = "Please run and repair failing rust tests";

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(user_config: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "sr-real-coord-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir_all(root.join("workspace/.claude/skills")).unwrap();
        std::fs::create_dir_all(root.join("config/sr")).unwrap();
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::write(root.join("config/sr/config.toml"), user_config).unwrap();
        let f = Self { root };
        f.skill("alpha", "Runs and repairs failing rust tests.");
        f.skill("beta", "Drafts release notes from git history.");
        f
    }

    fn workspace(&self) -> PathBuf {
        self.root.join("workspace")
    }

    fn skill_file(&self, name: &str) -> PathBuf {
        self.workspace()
            .join(".claude/skills")
            .join(name)
            .join("SKILL.md")
    }

    fn skill(&self, name: &str, description: &str) {
        let file = self.skill_file(name);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            file,
            format!("---\nname: {name}\ndescription: {description}\n---\nBody.\n"),
        )
        .unwrap();
    }

    fn claude_session(&self, session: &str, request: &str) -> PathBuf {
        let workspace = std::fs::canonicalize(self.workspace()).unwrap();
        let workspace_str = workspace.to_str().unwrap();
        let name: String = workspace_str.replace('/', "-");
        let directory = self.root.join("home/.claude/projects").join(name);
        std::fs::create_dir_all(&directory).unwrap();
        let record = |n: u8, parent: Option<String>, text: &str| {
            json!({
                "type": "user",
                "uuid": format!("{session}-{n}"),
                "parentUuid": parent,
                "cwd": workspace_str,
                "sessionId": session,
                "timestamp": "2026-09-19T10:00:00Z",
                "message": {"role": "user", "content": text}
            })
            .to_string()
        };
        let path = directory.join(format!("{session}.jsonl"));
        let lines = [
            record(1, None, "Earlier question about codebase"),
            record(2, Some(format!("{session}-1")), request),
        ];
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    fn cache_dir(&self) -> PathBuf {
        let dir = std::path::Path::new("/tmp").join(format!(
            "sr-coord-cache-{}",
            self.root.file_name().unwrap().to_string_lossy()
        ));
        if !dir.exists() {
            std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        }
        dir
    }

    fn sr_command(&self, provider: &Provider, extra: &[&str]) -> Command {
        self.sr_command_at(provider.port, extra)
    }

    fn sr_command_at(&self, port: u16, extra: &[&str]) -> Command {
        let ca = self.root.join("fixture-ca.pem");
        std::fs::write(&ca, include_bytes!("fixtures/jev-tls/ca.pem")).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_sr"));
        command
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_CACHE_HOME", self.cache_dir())
            .env("TYPESAFE_API_KEY", "synthetic-acceptance-canary")
            .env("TYPESAFE_ENDPOINT", format!("https://localhost:{port}"))
            .env("SSL_CERT_FILE", &ca)
            .current_dir(self.workspace())
            .args(["rank", "--json"]);
        if !extra.contains(&"--offline") {
            command.arg("--allow-network");
        }
        command.args(extra);
        command
    }
}

struct Provider {
    child: Child,
    lines: BufReader<ChildStdout>,
    pub port: u16,
}

impl Provider {
    fn start(f: &Fixture, scenario: &str, extra: &[&std::ffi::OsStr]) -> Self {
        let directory = f
            .root
            .join(format!("provider-{}", NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir(&directory).unwrap();
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
            std::fs::write(directory.join(name), bytes).unwrap();
        }
        let mut child = Command::new("/usr/bin/python3")
            .arg(directory.join("provider_server.py"))
            .arg(scenario)
            .args(extra)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        lines.read_line(&mut line).unwrap();
        let hello: Value = serde_json::from_str(&line).unwrap();
        let port = u16::try_from(hello["port"].as_u64().unwrap()).unwrap();
        Self { child, lines, port }
    }

    fn finish(self) -> Vec<Value> {
        let (served, rejected) = self.finish_with_rejections();
        assert_eq!(rejected, 0, "every handshake must succeed");
        served
    }

    fn finish_with_rejections(mut self) -> (Vec<Value>, usize) {
        let mut done = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        std::io::Write::write_all(&mut done, b"DONE").unwrap();
        drop(done);
        let mut served = Vec::new();
        let mut rejected = 0;
        loop {
            let mut line = String::new();
            assert!(
                self.lines.read_line(&mut line).unwrap() > 0,
                "provider ended early"
            );
            let value: Value = serde_json::from_str(&line).unwrap();
            if value["done"] == true {
                assert_eq!(value["requests"].as_u64().unwrap() as usize, served.len());
                break;
            }
            if value["handshake_rejected"] == true {
                rejected += 1;
                continue;
            }
            served.push(value);
        }
        assert!(self.child.wait().unwrap().success(), "provider must finish");
        (served, rejected)
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn stages(served: &[Value]) -> Vec<&str> {
    served
        .iter()
        .map(|entry| entry["stage"].as_str().unwrap())
        .collect()
}

#[test]
fn ordinary_two_consumer_success_incurs_one_pair_and_subsequent_exact_offline_reuse() {
    let f = Fixture::new(CONSENT);
    f.claude_session("session-coord-1", TASK);
    let marker = f.root.join("concurrent-wide-started");
    let provider = Provider::start(
        &f,
        "slow-wide+write-on-wide",
        &[marker.as_os_str(), "1".as_ref()],
    );

    // Process A starts and leads
    let mut cmd_a = f.sr_command(&provider, &[]);
    let child_a = cmd_a.stdout(Stdio::piped()).spawn().unwrap();

    wait_for_marker(&marker);

    // Process B starts concurrently with same request
    let mut cmd_b = f.sr_command(&provider, &[]);
    let child_b = cmd_b.stdout(Stdio::piped()).spawn().unwrap();

    let out_a = child_a.wait_with_output().unwrap();
    let out_b = child_b.wait_with_output().unwrap();

    eprintln!("out_a stdout: {}", String::from_utf8_lossy(&out_a.stdout));
    eprintln!("out_a stderr: {}", String::from_utf8_lossy(&out_a.stderr));
    eprintln!("out_b stdout: {}", String::from_utf8_lossy(&out_b.stdout));
    eprintln!("out_b stderr: {}", String::from_utf8_lossy(&out_b.stderr));

    let cache_file = f.cache_dir().join("sr").join("cache.sqlite3");
    eprintln!("cache_file exists: {}", cache_file.exists());
    let leases_file = f.cache_dir().join("sr").join("leases.sqlite3");
    assert!(
        !leases_file.exists(),
        "production created a separate lease database"
    );

    assert_eq!(out_a.status.code(), Some(0));
    assert_eq!(out_b.status.code(), Some(0));

    let val_a: Value = serde_json::from_slice(&out_a.stdout).unwrap();
    let val_b: Value = serde_json::from_slice(&out_b.stdout).unwrap();

    assert_eq!(val_a["decision"], "ranked");
    assert_eq!(val_b["decision"], "ranked");
    assert_eq!(val_a["usage"]["requests"], 2);
    assert_eq!(val_b["usage"]["requests"], 0);
    assert_eq!(val_b["usage"]["http_attempts"], 0);

    let port = provider.port;
    let served = provider.finish();
    // Exactly 1 wide and 1 rerank served across both processes
    assert_eq!(stages(&served), ["wide", "rerank"]);

    // Process C: subsequent exact offline reuse
    let mut cmd_c = f.sr_command_at(port, &["--offline"]);
    let out_c = cmd_c.output().unwrap();
    eprintln!("out_c status: {:?}", out_c.status);
    eprintln!("out_c stderr: {}", String::from_utf8_lossy(&out_c.stderr));
    eprintln!("out_c stdout: {}", String::from_utf8_lossy(&out_c.stdout));
    assert_eq!(out_c.status.code(), Some(0));
    let val_c: Value = serde_json::from_slice(&out_c.stdout).unwrap();
    assert_eq!(val_c["decision"], "ranked");
    assert_eq!(val_c["usage"]["http_attempts"], 0);
    assert_eq!(val_c["usage"]["requests"], 0);
}

#[test]
fn follower_nearing_deadline_while_leader_active_makes_zero_provider_attempts() {
    let f = Fixture::new(CONSENT);
    f.claude_session("session-coord-2", TASK);
    let marker = f.root.join("follower-wide-started");
    let provider = Provider::start(
        &f,
        "slow-wide+write-on-wide",
        &[marker.as_os_str(), "2.5".as_ref()],
    );

    // Leader starts with 6s timeout
    let mut cmd_leader = f.sr_command(&provider, &["--timeout-ms", "6000"]);
    let child_leader = cmd_leader.stdout(Stdio::piped()).spawn().unwrap();

    wait_for_marker(&marker);

    // Follower starts with tight 600ms timeout
    let mut cmd_follower = f.sr_command(&provider, &["--timeout-ms", "600"]);
    let out_follower = cmd_follower.output().unwrap();

    // Follower must finish within ~1s without calling provider
    let out_leader = child_leader.wait_with_output().unwrap();
    assert_eq!(out_leader.status.code(), Some(0));

    let served = provider.finish();
    // Only the leader called the provider (1 wide, 1 rerank)
    assert_eq!(stages(&served), ["wide", "rerank"]);

    assert_eq!(out_follower.status.code(), Some(6));
    let val_f: Value = serde_json::from_slice(&out_follower.stdout).unwrap();
    assert_eq!(val_f["decision"], "unavailable");
    assert_eq!(val_f["usage"]["http_attempts"], 0);
    assert_eq!(val_f["usage"]["requests"], 0);
}

fn wait_for_marker(marker: &std::path::Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !marker.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "leader never reached provider"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

// Keep a stopped child owned even if an assertion fails.
struct OwnedRank(Option<Child>);
impl OwnedRank {
    fn wait(mut self) -> std::process::Output {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if self.0.as_mut().unwrap().try_wait().unwrap().is_some() {
                return self.0.take().unwrap().wait_with_output().unwrap();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "rank child did not terminate"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for OwnedRank {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn stale_leader_superseded_cannot_overwrite_newer_cache_or_complete_lease() {
    let f = Fixture::new(CONSENT);
    f.claude_session("stale-owner", TASK);
    let marker = f.root.join("wide-started");
    let provider = Provider::start(
        &f,
        "slow-wide+write-on-wide",
        &[marker.as_os_str(), "2".as_ref()],
    );
    let spawn = || {
        let mut command = f.sr_command(&provider, &["--timeout-ms", "12000"]);
        OwnedRank(Some(
            command
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        ))
    };
    let a = spawn();
    wait_for_marker(&marker);
    let pid = nix::unistd::Pid::from_raw(i32::try_from(a.0.as_ref().unwrap().id()).unwrap());
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGSTOP).unwrap();
    let leases_path = f.cache_dir().join("sr/cache.sqlite3");
    let conn = rusqlite::Connection::open(&leases_path).unwrap();
    conn.busy_timeout(Duration::from_millis(25)).unwrap();
    // A is stopped after sending its wide request and before receiving it.
    // Advance durable lease expiry, then let a real successor run and publish.
    assert_eq!(
        conn.execute(
            "UPDATE sr_coordination_leases SET acquired_at_unix_ms=0, expires_at_unix_ms=0 WHERE is_completed=0",
            []
        )
        .unwrap(),
        1
    );
    let b = spawn().wait();
    assert_eq!(
        b.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&b.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&b.stdout).unwrap()["decision"],
        "ranked"
    );
    let cache = rusqlite::Connection::open(f.cache_dir().join("sr/cache.sqlite3")).unwrap();
    let snapshot = || {
        cache.prepare("SELECT stage, fingerprint, response, received_at_unix_ms FROM sr_cache_response ORDER BY stage,fingerprint")
            .unwrap().query_map([], |r| Ok((r.get::<_,String>(0)?, r.get::<_,Vec<u8>>(1)?, r.get::<_,Vec<u8>>(2)?, r.get::<_,i64>(3)?)))
            .unwrap().collect::<Result<Vec<_>,_>>().unwrap()
    };
    let bodies = snapshot();
    assert_eq!(bodies.len(), 2, "successor must publish a complete pair");
    let completed: (i64, i64) = conn
        .query_row(
            "SELECT fencing_generation,is_completed FROM sr_coordination_leases",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(completed, (2, 1));
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGCONT).unwrap();
    let a = a.wait();
    assert_eq!(
        a.status.code(),
        Some(6),
        "{}",
        String::from_utf8_lossy(&a.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&a.stdout).unwrap()["decision"],
        "unavailable"
    );
    assert_eq!(
        snapshot(),
        bodies,
        "resumed stale owner overwrote successor cache"
    );
    let after: (i64, i64) = conn
        .query_row(
            "SELECT fencing_generation,is_completed FROM sr_coordination_leases",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(after, completed);
    assert_eq!(stages(&provider.finish()), ["wide", "wide", "rerank"]);
}

#[test]
fn completed_lease_with_absent_pair_reacquires_leadership_before_fresh_evaluation() {
    let f = Fixture::new(CONSENT);
    f.claude_session("expired-pair", TASK);
    let provider = Provider::start(&f, "useful", &[]);
    let run = || {
        let mut command = f.sr_command(&provider, &["--timeout-ms", "12000"]);
        OwnedRank(Some(
            command
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        ))
        .wait()
    };
    let first = run();
    assert_eq!(
        first.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let cache = rusqlite::Connection::open(f.cache_dir().join("sr/cache.sqlite3")).unwrap();
    // Expire only rerank, preserving the exact key, namespace and wide answer.
    assert_eq!(
        cache
            .execute(
                "UPDATE sr_cache_response SET received_at_unix_ms=0 WHERE stage='rerank'",
                []
            )
            .unwrap(),
        1
    );
    let second = run();
    assert_eq!(
        second.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&second.stdout).unwrap()["decision"],
        "ranked"
    );
    let leases = rusqlite::Connection::open(f.cache_dir().join("sr/cache.sqlite3")).unwrap();
    let state: (i64, i64, i64) = leases
        .query_row(
            "SELECT count(*),max(fencing_generation),min(is_completed) FROM sr_coordination_leases",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        state,
        (1, 2, 1),
        "same request must reacquire the completed lease"
    );
    assert_eq!(
        stages(&provider.finish()),
        ["wide", "rerank", "wide", "rerank"]
    );
}

#[test]
fn no_cache_and_no_persist_preserve_documented_effects() {
    let f = Fixture::new(CONSENT);
    f.claude_session("session-nocache-1", TASK);
    let provider = Provider::start(&f, "useful", &[]);

    // Process with --no-cache: runs successfully without caching
    let mut cmd = f.sr_command(&provider, &["--no-cache"]);
    let out = cmd.output().unwrap();
    eprintln!("status: {:?}", out.status);
    eprintln!("stderr: {}", String::from_utf8_lossy(&out.stderr));
    eprintln!("stdout: {}", String::from_utf8_lossy(&out.stdout));
    assert_eq!(out.status.code(), Some(0));
    let val: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(val["decision"], "ranked");

    let served = provider.finish();
    assert_eq!(stages(&served), ["wide", "rerank"]);
}

#[test]
fn cache_generation_change_during_provider_work_keeps_valid_answer() {
    let f = Fixture::new(CONSENT);
    f.claude_session("cache-generation-changed", TASK);
    let marker = f.root.join("cache-change-wide");
    let provider = Provider::start(
        &f,
        "slow-wide+write-on-wide",
        &[marker.as_os_str(), "2".as_ref()],
    );
    let mut command = f.sr_command(&provider, &["--timeout-ms", "12000"]);
    let child = OwnedRank(Some(
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    wait_for_marker(&marker);
    let cache = rusqlite::Connection::open(f.cache_dir().join("sr/cache.sqlite3")).unwrap();
    assert_eq!(
        cache
            .execute("UPDATE sr_cache_meta SET generation=generation+1", [])
            .unwrap(),
        1
    );
    let output = child.wait();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let doc: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(doc["decision"], "ranked");
    assert_eq!(doc["usage"]["requests"], 2);
    assert!(
        doc["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["kind"] == "cache-recording-unavailable")
    );
    let rows: i64 = cache
        .query_row("SELECT count(*) FROM sr_cache_response", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        rows, 0,
        "stale store generation must not record either response"
    );
    assert_eq!(stages(&provider.finish()), ["wide", "rerank"]);
}

#[test]
fn busy_optional_lease_completion_keeps_answer_without_claiming_completion() {
    let f = Fixture::new(CONSENT);
    f.claude_session("busy-completion", TASK);
    let marker = f.root.join("busy-rerank");
    let provider = Provider::start(
        &f,
        "late-rerank+write-on-rerank",
        &[marker.as_os_str(), "2".as_ref()],
    );
    let mut command = f.sr_command(&provider, &["--timeout-ms", "12000"]);
    let child = OwnedRank(Some(
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    wait_for_marker(&marker);
    let lease = rusqlite::Connection::open(f.cache_dir().join("sr/cache.sqlite3")).unwrap();
    lease.execute_batch("BEGIN IMMEDIATE").unwrap();
    let output = child.wait();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let doc: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(doc["decision"], "ranked");
    assert_eq!(doc["usage"]["requests"], 2);
    for kind in [
        "cache-recording-unavailable",
        "coordination-completion-unconfirmed",
    ] {
        assert!(
            doc["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|w| w["kind"] == kind)
        );
    }
    assert_eq!(
        lease
            .query_row("SELECT is_completed FROM sr_coordination_leases", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    lease.execute_batch("ROLLBACK").unwrap();
    let cache = rusqlite::Connection::open(f.cache_dir().join("sr/cache.sqlite3")).unwrap();
    assert_eq!(
        cache
            .query_row(
                "SELECT count(*) FROM sr_cache_response WHERE stage='rerank'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    assert_eq!(stages(&provider.finish()), ["wide", "rerank"]);
}
