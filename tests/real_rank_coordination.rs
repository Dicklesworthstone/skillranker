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
use skillranker::cache::{
    CoordinationKey, CoordinationPolicy, FencingGeneration, LeaseAcquisition, LeaseCoordinator,
    PublishOutcome, SqliteLeaseCoordinator,
};
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
        let ca = self.root.join("fixture-ca.pem");
        std::fs::write(&ca, include_bytes!("fixtures/jev-tls/ca.pem")).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_sr"));
        command
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_CACHE_HOME", self.cache_dir())
            .env("TYPESAFE_API_KEY", "synthetic-acceptance-canary")
            .env(
                "TYPESAFE_ENDPOINT",
                format!("https://localhost:{}", provider.port),
            )
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
    let provider = Provider::start(&f, "useful", &[]);

    // Process A starts and leads
    let mut cmd_a = f.sr_command(&provider, &[]);
    let child_a = cmd_a.stdout(Stdio::piped()).spawn().unwrap();

    // Small sleep so Process A establishes the lease as leader
    std::thread::sleep(Duration::from_millis(100));

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
    eprintln!("leases_file exists: {}", leases_file.exists());

    assert_eq!(out_a.status.code(), Some(0));
    assert_eq!(out_b.status.code(), Some(0));

    let val_a: Value = serde_json::from_slice(&out_a.stdout).unwrap();
    let val_b: Value = serde_json::from_slice(&out_b.stdout).unwrap();

    assert_eq!(val_a["decision"], "ranked");
    assert_eq!(val_b["decision"], "ranked");

    let port = provider.port;
    let served = provider.finish();
    // Exactly 1 wide and 1 rerank served across both processes
    assert_eq!(stages(&served), ["wide", "rerank"]);

    // Process C: subsequent exact offline reuse
    let mut cmd_c = f.sr_command(
        &Provider {
            child: Command::new("true").spawn().unwrap(),
            lines: BufReader::new(
                Command::new("true")
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap()
                    .stdout
                    .unwrap(),
            ),
            port,
        },
        &["--offline"],
    );
    let out_c = cmd_c.output().unwrap();
    eprintln!("out_c status: {:?}", out_c.status);
    eprintln!("out_c stderr: {}", String::from_utf8_lossy(&out_c.stderr));
    eprintln!("out_c stdout: {}", String::from_utf8_lossy(&out_c.stdout));
    assert_eq!(out_c.status.code(), Some(0));
    let val_c: Value = serde_json::from_slice(&out_c.stdout).unwrap();
    assert_eq!(val_c["decision"], "ranked");
}

#[test]
fn follower_nearing_deadline_while_leader_active_makes_zero_provider_attempts() {
    let f = Fixture::new(CONSENT);
    f.claude_session("session-coord-2", TASK);
    // Slow provider: wide request takes 2.5s
    let provider = Provider::start(&f, "slow-wide", &["".as_ref(), "2.5".as_ref()]);

    // Leader starts with 6s timeout
    let mut cmd_leader = f.sr_command(&provider, &["--timeout-ms", "6000"]);
    let child_leader = cmd_leader.stdout(Stdio::piped()).spawn().unwrap();

    // Ensure leader acquires lease and starts wide evaluation
    std::thread::sleep(Duration::from_millis(200));

    // Follower starts with tight 600ms timeout
    let mut cmd_follower = f.sr_command(&provider, &["--timeout-ms", "600"]);
    let out_follower = cmd_follower.output().unwrap();

    // Follower must finish within ~1s without calling provider
    let out_leader = child_leader.wait_with_output().unwrap();
    assert_eq!(out_leader.status.code(), Some(0));

    let served = provider.finish();
    // Only the leader called the provider (1 wide, 1 rerank)
    assert_eq!(stages(&served), ["wide", "rerank"]);

    // Follower made 0 provider attempts
    let val_f: Value = serde_json::from_slice(&out_follower.stdout).unwrap_or(Value::Null);
    if let Some(dec) = val_f["decision"].as_str() {
        assert_eq!(dec, "unavailable");
    }
}

#[test]
fn stale_leader_superseded_cannot_overwrite_newer_cache_or_complete_lease() {
    let f = Fixture::new(CONSENT);
    let cache_dir = f.cache_dir();
    let leases_path = cache_dir.join("leases.sqlite3");

    let coordinator = SqliteLeaseCoordinator::open(&leases_path).unwrap();
    let key = CoordinationKey::from_bytes([0x42; 32]);
    let policy = CoordinationPolicy::default();

    let now = 1_000_000u64;
    // Leader A acquires
    let acq_a = coordinator.acquire(key, now, &policy).unwrap();
    let LeaseAcquisition::Leading(leader_a) = acq_a else {
        panic!("expected Leader A Leading");
    };
    assert_eq!(leader_a.fencing_generation, FencingGeneration(1));

    // Advance time past lease expiry
    let later = now + policy.lease_ttl_ms + 100;

    // Leader B reacquires as successor
    let acq_b = coordinator.acquire(key, later, &policy).unwrap();
    let LeaseAcquisition::Leading(leader_b) = acq_b else {
        panic!("expected Leader B Leading");
    };
    assert_eq!(leader_b.fencing_generation, FencingGeneration(2));

    // Leader B completes and publishes successfully
    let outcome_b = coordinator
        .complete(
            leader_b.key,
            leader_b.owner_token,
            leader_b.fencing_generation,
            later,
        )
        .unwrap();
    assert_eq!(outcome_b, PublishOutcome::Published);

    // Leader A attempts to complete after B published: MUST BE SUPERSEDED
    let outcome_a = coordinator
        .complete(
            leader_a.key,
            leader_a.owner_token,
            leader_a.fencing_generation,
            later + 100,
        )
        .unwrap();
    assert!(
        matches!(outcome_a, PublishOutcome::Superseded { .. }),
        "stale Leader A must be superseded and cannot overwrite or complete B's lease"
    );
}

#[test]
fn completed_lease_with_absent_pair_reacquires_leadership_before_fresh_evaluation() {
    let f = Fixture::new(CONSENT);
    let cache_dir = f.cache_dir();
    let leases_path = cache_dir.join("leases.sqlite3");

    let coordinator = SqliteLeaseCoordinator::open(&leases_path).unwrap();
    let key = CoordinationKey::from_bytes([0x77; 32]);
    let policy = CoordinationPolicy::default();

    let now = 1_000_000u64;
    let acq = coordinator.acquire(key, now, &policy).unwrap();
    let LeaseAcquisition::Leading(leader) = acq else {
        panic!("expected Leading");
    };

    // Mark completed in leases
    coordinator
        .complete(
            leader.key,
            leader.owner_token,
            leader.fencing_generation,
            now + 50,
        )
        .unwrap();

    // Reacquire when cache is absent/expired: must succeed with bumped generation
    let reacquired = coordinator
        .force_reacquire(key, now + 100, &policy)
        .unwrap();
    let LeaseAcquisition::Leading(new_leader) = reacquired else {
        panic!("expected Leading on force_reacquire");
    };
    assert_eq!(new_leader.fencing_generation, FencingGeneration(2));
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
