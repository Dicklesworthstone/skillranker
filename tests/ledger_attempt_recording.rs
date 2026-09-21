#![cfg(unix)]
//! Provider attempts recorded by a real ranking run (sr-roadmap-l1i.6.7).
//!
//! This is the harness that did not exist. Recording needs a provider, so the hook
//! and ledger suites never saw a recorded row: `tests/hook_contract.rs` checks that
//! `ledger status` exits zero, and the ranking suites never initialise a ledger.
//! Two real defects lived in that gap (sr-7jji), and a third — attempt rows that
//! were never written at all, because nothing mapped an attempt to a row — is what
//! these cases now hold closed.
//!
//! Every case runs the installed binary against the loopback TLS provider fixture
//! with an initialised ledger, then reads `provider_attempts` with a plain SQLite
//! connection. The fixture follows `tests/real_rank_coordination.rs`, which owns the
//! same provider server.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
const CONSENT: &str = "[network]\nenabled = true\n";
const TASK: &str = "Please run and repair failing rust tests";

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        // Owner-only directories under Linux's root-owned sticky /tmp: the store
        // refuses a group- or other-writable ancestor, and a worker's TMPDIR can
        // have one.
        let root = PathBuf::from("/tmp").join(format!(
            "sr-attempt-rec-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&root)
            .unwrap();
        for sub in ["workspace/.claude/skills", "config/sr", "home", "data"] {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .recursive(true)
                .create(root.join(sub))
                .unwrap();
        }
        std::fs::write(root.join("config/sr/config.toml"), CONSENT).unwrap();
        let f = Self { root };
        f.skill("alpha", "Runs and repairs failing rust tests.");
        f.skill("beta", "Drafts release notes from git history.");
        f
    }

    fn workspace(&self) -> PathBuf {
        self.root.join("workspace")
    }

    fn data_home(&self) -> PathBuf {
        self.root.join("data")
    }

    fn ledger_db(&self) -> PathBuf {
        self.data_home().join("sr/ledger.sqlite3")
    }

    fn skill(&self, name: &str, description: &str) {
        let file = self
            .workspace()
            .join(".claude/skills")
            .join(name)
            .join("SKILL.md");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            file,
            format!("---\nname: {name}\ndescription: {description}\n---\nBody.\n"),
        )
        .unwrap();
    }

    /// A transcript whose records carry `uuid`s, as Claude's own do.
    fn claude_session(&self, session: &str, request: &str) {
        let workspace = std::fs::canonicalize(self.workspace()).unwrap();
        let workspace_str = workspace.to_str().unwrap();
        let directory = self
            .root
            .join("home/.claude/projects")
            .join(workspace_str.replace('/', "-"));
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
        std::fs::write(
            directory.join(format!("{session}.jsonl")),
            [
                record(1, None, "Earlier question about codebase"),
                record(2, Some(format!("{session}-1")), request),
            ]
            .join("\n")
                + "\n",
        )
        .unwrap();
    }

    /// Appends another user turn, as Claude does between hook invocations: a new
    /// record with its own uuid, parented on the previous one.
    fn append_turn(&self, session: &str, request: &str, n: u8) {
        let workspace = std::fs::canonicalize(self.workspace()).unwrap();
        let workspace_str = workspace.to_str().unwrap();
        let directory = self
            .root
            .join("home/.claude/projects")
            .join(workspace_str.replace('/', "-"));
        let path = directory.join(format!("{session}.jsonl"));
        let record = json!({
            "type": "user",
            "uuid": format!("{session}-{n}"),
            "parentUuid": format!("{session}-{}", n - 1),
            "cwd": workspace_str,
            "sessionId": session,
            "timestamp": "2026-09-19T10:05:00Z",
            "message": {"role": "user", "content": request}
        })
        .to_string();
        let mut existing = std::fs::read_to_string(&path).unwrap();
        existing.push_str(&record);
        existing.push('\n');
        std::fs::write(&path, existing).unwrap();
    }

    /// Appends a `tool_use` record, as Claude writes one when the agent loads a skill.
    /// Separate from `append_tool_result` on purpose: the whole point of the joint case is
    /// that an ingestion pass can land between the two.
    fn append_tool_use(&self, session: &str, invocation: &str, n: u8) {
        self.append_record(
            session,
            n,
            json!({
                "type": "assistant",
                "message": {"role": "assistant", "content": [{
                    "type": "tool_use",
                    "id": "toolu-joint-1",
                    "name": "Skill",
                    "input": {"skill": invocation},
                }]},
            }),
        );
    }

    /// Appends the matching `tool_result`, which is what turns an attempt into a load.
    fn append_tool_result(&self, session: &str, n: u8) {
        self.append_record(
            session,
            n,
            json!({
                "type": "user",
                "message": {"role": "user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu-joint-1",
                    "content": "skill body",
                    "is_error": false,
                }]},
            }),
        );
    }

    fn append_record(&self, session: &str, n: u8, mut record: Value) {
        let workspace = std::fs::canonicalize(self.workspace()).unwrap();
        let workspace_str = workspace.to_str().unwrap();
        let object = record.as_object_mut().unwrap();
        object.insert("uuid".into(), json!(format!("{session}-{n}")));
        object.insert("parentUuid".into(), json!(format!("{session}-{}", n - 1)));
        object.insert("cwd".into(), json!(workspace_str));
        object.insert("sessionId".into(), json!(session));
        object.insert("timestamp".into(), json!("2026-09-19T10:06:00Z"));
        let path = self
            .root
            .join("home/.claude/projects")
            .join(workspace_str.replace('/', "-"))
            .join(format!("{session}.jsonl"));
        let mut existing = std::fs::read_to_string(&path).unwrap();
        existing.push_str(&record.to_string());
        existing.push('\n');
        std::fs::write(&path, existing).unwrap();
    }

    fn transcript(&self, session: &str) -> PathBuf {
        let workspace = std::fs::canonicalize(self.workspace()).unwrap();
        self.root
            .join("home/.claude/projects")
            .join(workspace.to_str().unwrap().replace('/', "-"))
            .join(format!("{session}.jsonl"))
    }

    fn command(&self, port: u16, args: &[&str]) -> Command {
        let ca = self.root.join("fixture-ca.pem");
        std::fs::write(&ca, include_bytes!("fixtures/jev-tls/ca.pem")).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_sr"));
        command
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_DATA_HOME", self.data_home())
            .env("TYPESAFE_API_KEY", "synthetic-acceptance-canary")
            .env("TYPESAFE_ENDPOINT", format!("https://localhost:{port}"))
            .env("SSL_CERT_FILE", &ca)
            .current_dir(self.workspace())
            .args(args);
        command
    }

    fn ledger_init(&self) {
        let out = self
            .command(1, &["ledger", "init", "--json"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "ledger init failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn rank(&self, port: u16) -> std::process::Output {
        self.rank_with(port, &[])
    }

    fn rank_with(&self, port: u16, extra: &[&str]) -> std::process::Output {
        self.rank_command(port, extra).output().unwrap()
    }

    fn rank_command(&self, port: u16, extra: &[&str]) -> Command {
        let mut args: Vec<&str> =
            vec!["rank", "--json", "--allow-network", "--timeout-ms", "12000"];
        args.extend_from_slice(extra);
        self.command(port, &args)
    }

    /// Polls for the invocation's own in-flight row, so a kill can be timed against
    /// observed state rather than a sleep.
    fn wait_for_event(&self, deadline: std::time::Duration) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < deadline {
            if !self.events().is_empty() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        false
    }

    fn attempts(&self) -> Vec<StoredAttempt> {
        let conn = rusqlite::Connection::open(self.ledger_db()).unwrap();
        let mut statement = conn
            .prepare(
                "SELECT attempt_id, owner_event_id, stage, request_fingerprint,
                        admitted_at_unix_ms, sent_at_unix_ms, completed_at_unix_ms,
                        status, input_tokens, output_tokens, http_status, error_kind
                 FROM provider_attempts ORDER BY admitted_at_unix_ms, attempt_id",
            )
            .unwrap();
        statement
            .query_map([], |row| {
                Ok(StoredAttempt {
                    attempt_id: row.get(0)?,
                    owner_event_id: row.get(1)?,
                    stage: row.get(2)?,
                    request_fingerprint: row.get(3)?,
                    admitted_at: row.get(4)?,
                    sent_at: row.get(5)?,
                    completed_at: row.get(6)?,
                    status: row.get(7)?,
                    input_tokens: row.get(8)?,
                    output_tokens: row.get(9)?,
                    http_status: row.get(10)?,
                    error_kind: row.get(11)?,
                })
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    /// Every event with the two fields a failed run's record turns on: what it
    /// decided, and whether it claimed a roster snapshot it never had.
    fn events(&self) -> Vec<(String, String, Option<String>, String)> {
        let conn = rusqlite::Connection::open(self.ledger_db()).unwrap();
        let mut statement = conn
            .prepare("SELECT event_id, decision, snapshot_id, reason FROM ranking_events")
            .unwrap();
        statement
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    fn observation_states(&self) -> Vec<String> {
        let conn = rusqlite::Connection::open(self.ledger_db()).unwrap();
        let mut statement = conn
            .prepare("SELECT evidence_state FROM observations ORDER BY observed_at_unix_ms")
            .unwrap();
        statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<String>, _>>()
            .unwrap()
    }

    fn event_ids(&self) -> Vec<String> {
        let conn = rusqlite::Connection::open(self.ledger_db()).unwrap();
        let mut statement = conn.prepare("SELECT event_id FROM ranking_events").unwrap();
        statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<String>, _>>()
            .unwrap()
    }
}

#[derive(Debug)]
struct StoredAttempt {
    attempt_id: String,
    owner_event_id: String,
    stage: String,
    request_fingerprint: String,
    admitted_at: i64,
    sent_at: Option<i64>,
    completed_at: Option<i64>,
    status: String,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    http_status: Option<i64>,
    error_kind: Option<String>,
}

struct Provider {
    child: Child,
    lines: BufReader<ChildStdout>,
    port: u16,
}

impl Provider {
    fn start(f: &Fixture, scenario: &str) -> Self {
        Self::start_with(f, scenario, &[])
    }

    fn start_with(f: &Fixture, scenario: &str, extra: &[&str]) -> Self {
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

    /// Stop the server and return how many requests it actually served, which is
    /// the provider's own count rather than the ledger's claim about it.
    fn finish(mut self) -> usize {
        let mut done = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        std::io::Write::write_all(&mut done, b"DONE").unwrap();
        drop(done);
        let mut served = 0usize;
        loop {
            let mut line = String::new();
            assert!(
                self.lines.read_line(&mut line).unwrap() > 0,
                "provider ended early"
            );
            let value: Value = serde_json::from_str(&line).unwrap();
            if value["done"] == true {
                assert_eq!(value["requests"].as_u64().unwrap() as usize, served);
                break;
            }
            if value["handshake_rejected"] == true {
                continue;
            }
            served += 1;
        }
        assert!(self.child.wait().unwrap().success(), "provider must finish");
        served
    }
}

#[test]
fn pre_admission_refusals_finalize_the_inflight_event() {
    for offline in [true, false] {
        let f = Fixture::new();
        f.claude_session("pre-admission", TASK);
        f.ledger_init();
        std::fs::write(f.root.join("config/sr/config.toml"), "").unwrap();
        let mut args = vec!["rank", "--json", "--timeout-ms", "12000"];
        if offline {
            args.push("--offline");
        }
        let provider = Provider::start(&f, "useful");
        let out = f.command(provider.port, &args).output().unwrap();
        assert_eq!(provider.finish(), 0, "refusal must precede any send");
        assert!(!out.status.success(), "expected refusal, offline={offline}");
        let document: Value = serde_json::from_slice(&out.stdout).unwrap();
        let reason = document["error"]["kind"].as_str().unwrap();
        let events = f.events();
        assert_eq!(events.len(), 1, "offline={offline}: {events:?}");
        assert_eq!(events[0].1, "unavailable");
        assert_eq!(events[0].3, reason, "offline={offline}");
        let conn = rusqlite::Connection::open(f.ledger_db()).unwrap();
        let exposure: String = conn
            .query_row("SELECT exposure_state FROM ranking_events", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(exposure, "prepared", "offline={offline}");
        assert!(f.attempts().is_empty(), "a refusal admitted no attempts");
    }
}

#[test]
fn disabled_ledger_records_neither_success_nor_failure() {
    for flag in ["--no-ledger", "--no-persist"] {
        for scenario in ["useful", "always-503"] {
            let f = Fixture::new();
            f.claude_session("disabled-ledger", TASK);
            f.ledger_init();
            let provider = Provider::start(&f, scenario);
            let out = f.rank_with(provider.port, &[flag]);
            let served = provider.finish();
            assert_eq!(
                out.status.code(),
                Some(if scenario == "useful" { 0 } else { 4 }),
                "{flag}/{scenario}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(served > 0, "{flag}/{scenario} must reach the provider");
            assert!(
                f.events().is_empty(),
                "{flag}/{scenario} wrote ranking events despite disabled ledger"
            );
            assert!(
                f.attempts().is_empty(),
                "{flag}/{scenario} wrote provider attempts despite disabled ledger"
            );
        }
    }
}

#[test]
fn a_ranking_run_records_one_row_per_provider_attempt() {
    let f = Fixture::new();
    f.claude_session("rec-happy", TASK);
    f.ledger_init();
    let provider = Provider::start(&f, "useful");
    let out = f.rank(provider.port);
    assert!(
        out.status.success(),
        "rank failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let served = provider.finish();

    let rows = f.attempts();
    // The provider's own count is the denominator: the ledger must agree with what
    // was actually sent, not with what the pipeline believed it sent.
    assert_eq!(
        rows.len(),
        served,
        "recorded {} attempts for {served} served requests: {rows:#?}",
        rows.len()
    );
    assert_eq!(served, 2, "a useful ranking sends wide then rerank");

    let events = f.event_ids();
    assert_eq!(events.len(), 1);
    for row in &rows {
        assert_eq!(
            row.owner_event_id, events[0],
            "every attempt belongs to the event that caused it"
        );
        assert!(
            row.attempt_id.starts_with(&format!("{}:", events[0])),
            "row key is owner-scoped: {}",
            row.attempt_id
        );
        assert_eq!(row.status, "completed");
        assert!(row.input_tokens.unwrap() > 0, "known usage is recorded");
        assert!(row.output_tokens.unwrap() > 0);
        assert_eq!(row.error_kind, None);
        assert_eq!(row.http_status, None);
        // Unix milliseconds, not milliseconds since process entry.
        assert!(
            row.admitted_at >= 1_700_000_000_000,
            "attempt dated {} is not a wall-clock time",
            row.admitted_at
        );
        assert!(row.admitted_at <= row.sent_at.unwrap());
        assert!(row.sent_at.unwrap() <= row.completed_at.unwrap());
    }
    let stages: Vec<&str> = rows.iter().map(|row| row.stage.as_str()).collect();
    assert!(stages.contains(&"wide") && stages.contains(&"rerank"));
    // Each stage names the request it actually sent, so one fingerprint cannot
    // stand in for both stages' identities.
    let wide = rows.iter().find(|r| r.stage == "wide").unwrap();
    let rerank = rows.iter().find(|r| r.stage == "rerank").unwrap();
    assert_ne!(wide.request_fingerprint, rerank.request_fingerprint);
    assert!(!wide.request_fingerprint.is_empty());
}

#[test]
fn a_failed_attempt_and_its_retry_are_both_recorded() {
    // `retry-wide` answers the first wide request with 503 and then succeeds, so
    // this run costs three attempts. A ledger that recorded only the successful
    // ones would understate spend exactly where spend is easiest to lose.
    let f = Fixture::new();
    f.claude_session("rec-retry", TASK);
    f.ledger_init();
    let provider = Provider::start(&f, "retry-wide");
    let out = f.rank(provider.port);
    assert!(
        out.status.success(),
        "rank failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let served = provider.finish();
    assert_eq!(served, 3, "one refused wide, one accepted wide, one rerank");

    let rows = f.attempts();
    assert_eq!(rows.len(), served, "{rows:#?}");
    let failed: Vec<_> = rows.iter().filter(|r| r.status == "failed").collect();
    assert_eq!(failed.len(), 1, "{rows:#?}");
    let failed = failed[0];
    assert_eq!(failed.stage, "wide");
    assert_eq!(failed.http_status, Some(503));
    assert_eq!(failed.error_kind.as_deref(), Some("http-status"));
    // The provider answered, so nothing here claims token counts it never sent.
    assert_eq!(failed.input_tokens, None);
    assert_eq!(failed.output_tokens, None);
    assert_eq!(
        rows.iter().filter(|r| r.status == "completed").count(),
        2,
        "{rows:#?}"
    );
}

#[test]
fn a_cache_served_rerun_adds_no_attempt_of_its_own() {
    // The second invocation answers from the exact cached pair. It must not appear
    // to have paid for a response the first one bought.
    let f = Fixture::new();
    f.claude_session("rec-cached", TASK);
    f.ledger_init();
    let provider = Provider::start(&f, "useful");
    let first = f.rank(provider.port);
    assert!(
        first.status.success(),
        "first rank failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let after_first = f.attempts().len();
    let second = f.rank(provider.port);
    assert!(
        second.status.success(),
        "second rank failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let served = provider.finish();

    assert_eq!(served, 2, "the second run sent nothing");
    let rows = f.attempts();
    assert_eq!(
        rows.len(),
        after_first,
        "a cache-served run recorded {} new attempts: {rows:#?}",
        rows.len() - after_first
    );
    assert_eq!(rows.len(), served);
}

#[test]
fn a_run_that_fails_after_paying_still_records_its_attempts() {
    // The case with no other record at all. A run that fails publishes no
    // decision, so before this the attempts it paid for existed only in the
    // emitted document of one process and nowhere durable.
    let f = Fixture::new();
    f.claude_session("rec-failed", TASK);
    f.ledger_init();
    let provider = Provider::start(&f, "always-503");
    let out = f.rank(provider.port);
    assert!(
        !out.status.success(),
        "a provider that only refuses must not produce a ranking"
    );
    assert_eq!(
        out.status.code(),
        Some(4),
        "provider/budget failures exit 4: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let served = provider.finish();
    assert!(served >= 1, "the run reached the provider");

    let rows = f.attempts();
    assert_eq!(
        rows.len(),
        served,
        "recorded {} attempts for {served} refused requests: {rows:#?}",
        rows.len()
    );
    let events = f.events();
    assert_eq!(events.len(), 1, "{events:#?}");
    let (event_id, decision, snapshot, reason) = &events[0];
    assert_eq!(decision, "unavailable");
    // A run that failed mid-flight has no decision whose membership a snapshot
    // could describe, so it must not claim one.
    assert_eq!(*snapshot, None);
    // The failure's typed kind, never its message.
    assert!(
        !reason.is_empty() && !reason.contains(' '),
        "reason is a kebab-case kind, got {reason:?}"
    );
    for row in &rows {
        assert_eq!(row.owner_event_id, *event_id);
        assert_eq!(row.stage, "wide", "the run never reached rerank");
        // The provider answered with a status, so the ending is established.
        assert_eq!(row.status, "failed", "{row:#?}");
        assert_eq!(row.http_status, Some(503));
        assert_eq!(row.error_kind.as_deref(), Some("http-status"));
        // It refused; it did not report usage, and absent is not zero.
        assert_eq!(row.input_tokens, None);
        assert_eq!(row.output_tokens, None);
        assert!(row.admitted_at >= 1_700_000_000_000, "{row:#?}");
        assert!(row.sent_at.unwrap() >= row.admitted_at);
    }
}

#[test]
fn two_paying_deliveries_of_one_event_record_both_costs() {
    // sr-qqlk, end to end. Two deliveries of the same turn are one event by design,
    // and with the cache unable to serve the repeat they both pay. Before the fix the
    // second delivery's event insert conflicted, the transaction rolled back, and its
    // two paid requests were recorded nowhere: four served, two rows.
    let f = Fixture::new();
    f.claude_session("rec-dup", TASK);
    f.ledger_init();
    let provider = Provider::start(&f, "useful");
    for delivery in 1..=2 {
        let out = f.rank_with(provider.port, &["--no-cache"]);
        assert!(
            out.status.success(),
            "delivery {delivery} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let served = provider.finish();
    assert_eq!(served, 4, "both deliveries sent wide and rerank");

    let events = f.events();
    assert_eq!(
        events.len(),
        1,
        "one turn is one event however often it is delivered: {events:#?}"
    );
    let rows = f.attempts();
    assert_eq!(
        rows.len(),
        served,
        "recorded {} attempts for {served} served requests: {rows:#?}",
        rows.len()
    );
    let owner = &events[0].0;
    let mut keys: Vec<&str> = rows.iter().map(|row| row.attempt_id.as_str()).collect();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), rows.len(), "every attempt has its own row key");
    for row in &rows {
        assert_eq!(&row.owner_event_id, owner);
        assert_eq!(row.status, "completed", "{row:#?}");
        assert!(row.input_tokens.unwrap() > 0);
    }
    assert_eq!(rows.iter().filter(|r| r.stage == "wide").count(), 2);
    assert_eq!(rows.iter().filter(|r| r.stage == "rerank").count(), 2);
}

#[test]
fn a_new_turn_with_identical_text_is_still_a_separate_event() {
    // Independent check of the sr-7jji identity derivation against the contract it
    // has to satisfy: "identical prompt text does not merge distinct turns". The
    // duplicate-delivery case above proves the same turn twice is one event; this is
    // the opposite direction, and getting it wrong would mean one event per session
    // forever with every later turn's cost silently dropped.
    let f = Fixture::new();
    f.claude_session("rec-turns", TASK);
    f.ledger_init();
    let provider = Provider::start(&f, "useful");
    let first = f.rank_with(provider.port, &["--no-cache"]);
    assert!(
        first.status.success(),
        "first turn failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    // A third record with the *same* text as the second: a new turn, not a repeat.
    f.append_turn("rec-turns", TASK, 3);
    let second = f.rank_with(provider.port, &["--no-cache"]);
    assert!(
        second.status.success(),
        "second turn failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let served = provider.finish();
    assert_eq!(served, 4, "each turn sent wide and rerank");

    let events = f.events();
    assert_eq!(
        events.len(),
        2,
        "two turns with the same text are two events: {events:#?}"
    );
    assert_ne!(events[0].0, events[1].0, "and they carry distinct ids");
    let rows = f.attempts();
    assert_eq!(rows.len(), served, "{rows:#?}");
    for (event_id, _, _, _) in &events {
        assert_eq!(
            rows.iter()
                .filter(|r| &r.owner_event_id == event_id)
                .count(),
            2,
            "each event owns its own pair: {rows:#?}"
        );
    }
}

#[test]
fn an_invocation_killed_mid_flight_is_still_recorded() {
    // sr-roadmap-l1i.6.7's hard-kill clause, which SilentFinch was right to insist is
    // not covered by failure-path tests: a process that dies while waiting on the
    // provider cannot record anything afterwards, so what matters is what it wrote
    // before sending. Without that write the ledger cannot distinguish "this
    // invocation never happened" from "it ran and may have paid".
    let f = Fixture::new();
    f.claude_session("rec-kill", TASK);
    f.ledger_init();
    // The wide stage sleeps for ten seconds, so the process is certainly still
    // waiting when the kill lands.
    let target = f.root.join("provider-target.txt");
    std::fs::write(&target, "unused").unwrap();
    let provider = Provider::start_with(&f, "slow-wide", &[target.to_str().unwrap(), "10"]);
    // No extra --timeout-ms: the default is already in the argument list and strict
    // parsing refuses a duplicate, which would kill the child before it recorded
    // anything and make this case pass for the wrong reason.
    let mut child = f
        .rank_command(provider.port, &[])
        // stdin must be null, not inherited: a piped stdin makes `sr` require an
        // explicit input mode, and the child would exit before recording anything.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // Timed against observed state, not a sleep: wait until the invocation has
    // written itself down, then kill it outright.
    assert!(
        f.wait_for_event(std::time::Duration::from_secs(20)),
        "the invocation never recorded itself before sending"
    );
    child.kill().expect("SIGKILL delivered");
    let status = child.wait().unwrap();
    assert!(
        !status.success(),
        "the process was supposed to be killed, not to finish"
    );

    let events = f.events();
    assert_eq!(events.len(), 1, "{events:#?}");
    let (_, decision, snapshot, reason) = &events[0];
    // Nothing was decided and nothing was delivered, which is what these two say.
    assert_eq!(decision, "unavailable");
    assert_eq!(reason, "in-flight");
    assert_eq!(*snapshot, None, "no roster membership was established");
    let conn = rusqlite::Connection::open(f.ledger_db()).unwrap();
    let exposure: String = conn
        .query_row("SELECT exposure_state FROM ranking_events", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        exposure, "generated",
        "a killed invocation leaves its row in flight"
    );
    // Its own recorded time is a real wall clock, so a reader can tell that the row
    // is older than any invocation deadline and therefore died rather than running.
    let created: i64 = conn
        .query_row("SELECT created_at_unix_ms FROM ranking_events", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(
        created >= 1_700_000_000_000,
        "created_at {created} is not wall clock"
    );
}

#[test]
fn an_invocation_killed_after_its_request_reached_the_wire_owns_that_attempt() {
    // sr-roadmap-l1i.6.7's reached-wire clause, which PurpleFrog was right to insist the
    // earlier hard-kill case did not cover. That case waited for the invocation's *event*
    // row, which is written before anything is sent, so it could kill before the request
    // ever touched transport: it establishes that an invocation started, and nothing about
    // which attempt was in flight. The expensive fact is narrower — that *this* attempt was
    // sent, and so the provider may already have done the work and charged for it. After a
    // kill, an attempt that was merely admitted and one that reached the wire are
    // indistinguishable unless something wrote down which happened, before the wait.
    let f = Fixture::new();
    f.claude_session("rec-wire", TASK);
    f.ledger_init();
    // `write-on-wide` writes the target the moment the provider has READ the wide request,
    // and `slow-wide` then holds the answer for ten seconds. So the marker is the
    // provider's own receipt that the request reached the wire, and the kill lands while
    // the response is still being awaited.
    let marker = f.root.join("reached-wire.txt");
    std::fs::write(&marker, "not-yet").unwrap();
    let provider = Provider::start_with(
        &f,
        "slow-wide+write-on-wide",
        &[marker.to_str().unwrap(), "10"],
    );
    let mut child = f
        .rank_command(provider.port, &[])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // Timed against the provider's own receipt rather than a sleep or our own row: those
    // would let the kill land before the send and make this case pass for the wrong reason.
    let start = std::time::Instant::now();
    let mut reached_wire = false;
    while start.elapsed() < std::time::Duration::from_secs(25) {
        if std::fs::read_to_string(&marker).unwrap_or_default().trim() != "not-yet" {
            reached_wire = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert!(
        reached_wire,
        "the provider never reported reading the wide request, so this case never got to \
         the state it exists to test"
    );
    child.kill().expect("SIGKILL delivered");
    let status = child.wait().unwrap();
    assert!(
        !status.success(),
        "the process was supposed to be killed, not to finish"
    );

    let attempts = f.attempts();
    assert_eq!(
        attempts.len(),
        1,
        "exactly the one attempt that was in flight should own a row: {attempts:#?}"
    );
    let attempt = &attempts[0];
    assert_eq!(attempt.stage, "wide", "{attempt:#?}");
    assert_eq!(
        attempt.status, "sent",
        "a request the provider is known to have read was not recorded as sent, so the \
         ledger cannot say a charge was possible: {attempt:#?}"
    );
    assert!(attempt.sent_at.is_some(), "{attempt:#?}");
    // Not dated as completed: it never returned, and claiming otherwise would turn an
    // unknown outcome into an observed one.
    assert!(
        attempt.completed_at.is_none(),
        "an attempt that never returned was dated as completed: {attempt:#?}"
    );
    // Usage unknown rather than zero. The provider may have done the whole job.
    assert!(
        attempt.input_tokens.is_none() && attempt.output_tokens.is_none(),
        "a killed attempt was recorded as having cost nothing: {attempt:#?}"
    );

    // And the event it belongs to is still the in-flight row, so the pair reads as
    // "an invocation ran, sent this request, and never came back".
    let events = f.events();
    assert_eq!(events.len(), 1, "{events:#?}");
    assert_eq!(events[0].1, "unavailable", "{events:#?}");
    assert_eq!(events[0].3, "in-flight", "{events:#?}");
    assert_eq!(attempt.owner_event_id, events[0].0, "{attempts:#?}");
}

/// The documented adoption loop, end to end, in the order a person actually performs it.
///
/// This is the joint acceptance case for sr-oufi and sr-an94. Neither bead's own tests
/// establish it, because each fixes one half of one row: sr-an94 makes the load count as a
/// load when its result arrives in a later ingestion pass, and sr-oufi makes the judgment
/// key on the same identity the candidates and observations use. Before both, this single
/// assertion failed twice over -- two rows for one skill, and the load recorded as an
/// attempt -- and either fix alone still leaves it failing.
///
/// Every step is the real command over the real seam: a ranking against the loopback TLS
/// provider, two `sr observe` passes over a transcript that grows between them, a judgment
/// supplied by the invocation name a person reads in the ranking output, and the report.
#[test]
fn the_documented_adoption_loop_reports_one_skill_as_one_row() {
    let f = Fixture::new();
    f.claude_session("adoption-loop", TASK);
    f.ledger_init();

    let provider = Provider::start(&f, "useful");
    let ranked = f.rank(provider.port);
    provider.finish();
    assert!(
        ranked.status.success(),
        "rank failed: {}",
        String::from_utf8_lossy(&ranked.stderr)
    );
    let document: Value = serde_json::from_slice(&ranked.stdout).unwrap();
    assert_eq!(document["decision"], "ranked");
    let event_id = document["event_id"]
        .as_str()
        .expect("a recorded event")
        .to_owned();
    let top = &document["skills"][0];
    let stable_id = top["skill_id"]
        .as_str()
        .expect("a stable skill id")
        .to_owned();
    let invocation = top["invocation_name"]
        .as_str()
        .expect("the name a person reads in the output")
        .to_owned();
    assert_ne!(
        stable_id, invocation,
        "this case is only meaningful while the two identities differ"
    );

    // The agent loads the skill. The tool_use is written first, and an observation pass
    // lands before its result -- the ordinary case for a periodic observe or a per-turn hook.
    let transcript = f.transcript("adoption-loop");
    let transcript_str = transcript.to_str().unwrap();
    f.append_tool_use("adoption-loop", &invocation, 3);
    let first = f
        .command(
            1,
            &[
                "observe",
                "--transcript",
                transcript_str,
                "--harness",
                "claude_code",
                "--json",
            ],
        )
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "first observe: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(
        f.observation_states(),
        vec!["attempted".to_string()],
        "an unpaired tool_use is an attempt, which is the correct first reading"
    );

    // The result arrives, and a later pass reads it.
    f.append_tool_result("adoption-loop", 4);
    let second = f
        .command(
            1,
            &[
                "observe",
                "--transcript",
                transcript_str,
                "--harness",
                "claude_code",
                "--json",
            ],
        )
        .output()
        .unwrap();
    assert!(
        second.status.success(),
        "second observe: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(
        f.observation_states(),
        vec!["loaded".to_string()],
        "sr-an94: the confirmation must complete the observation already recorded"
    );

    // The person judges the suggestion by the name they read, not by an opaque id.
    let feedback = f
        .command(
            1,
            &[
                "feedback",
                &event_id,
                "--skill",
                &invocation,
                "--verdict",
                "useful",
                "--json",
            ],
        )
        .output()
        .unwrap();
    assert!(
        feedback.status.success(),
        "feedback: {}",
        String::from_utf8_lossy(&feedback.stderr)
    );

    // The report a reader actually looks at.
    let stats = f
        .command(1, &["stats", "--by-skill", "--json"])
        .output()
        .unwrap();
    assert!(
        stats.status.success(),
        "stats: {}",
        String::from_utf8_lossy(&stats.stderr)
    );
    let report: Value = serde_json::from_slice(&stats.stdout).unwrap();
    let rows = report["by_skill"].as_array().expect("--by-skill cohort");
    let named: Vec<&str> = rows
        .iter()
        .filter_map(|row| row["skill_id"].as_str())
        .collect();
    // Every candidate of the ranking legitimately gets a row, so the cohort is not expected to
    // hold exactly one. What must be true is that THIS skill holds exactly one row with all
    // three figures on it, and that the invocation name never becomes a key of its own.
    assert!(
        !named.contains(&invocation.as_str()),
        "the supplied invocation name must not appear as a skill key; saw {named:?}"
    );
    let mine: Vec<&Value> = rows
        .iter()
        .filter(|row| row["skill_id"].as_str() == Some(stable_id.as_str()))
        .collect();
    assert_eq!(
        mine.len(),
        1,
        "the recommended, loaded and judged skill must occupy exactly one row; saw {named:?}"
    );
    let row = mine[0];
    assert_eq!(
        row["top1_recommendations"].as_u64(),
        Some(1),
        "the recommendation belongs on this row: {row}"
    );
    assert_eq!(
        row["observed_loads"].as_u64(),
        Some(1),
        "sr-an94: the split-pass load must count as a load on the same row: {row}"
    );
    assert_eq!(
        row["judged_useful"].as_u64(),
        Some(1),
        "sr-oufi: the judgment supplied by name must land on the same row: {row}"
    );
    for other in rows
        .iter()
        .filter(|row| row["skill_id"].as_str() != Some(stable_id.as_str()))
    {
        assert_eq!(other["judged_useful"].as_u64(), Some(0), "other: {other}");
        assert_eq!(other["observed_loads"].as_u64(), Some(0), "other: {other}");
    }
}
