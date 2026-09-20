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
        self.command(
            port,
            &["rank", "--json", "--allow-network", "--timeout-ms", "12000"],
        )
        .output()
        .unwrap()
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
