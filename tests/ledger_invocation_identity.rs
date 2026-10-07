#![cfg(any(target_os = "linux", target_os = "macos"))]
//! Repeated public-library rankings must retain every real TLS attempt and
//! timestamp it against its own invocation, including duplicate event delivery.
//! Synthetic loopback provider data; real HTTP/TLS, pipeline and SQLite.

mod support;

use asupersync::tls::Certificate;
use rusqlite::Connection;
use serde_json::{Value, json};
use skillranker::config::ConfigSources;
use skillranker::context::source::SourceOptions;
use skillranker::effects::{EffectGate, Scope};
use skillranker::jev::client::JevClient;
use skillranker::jev::endpoint::EndpointConfig;
use skillranker::limits::DurationMillis;
use skillranker::pipeline::{RankArgs, execute_pipeline};
use skillranker::privacy::EffectFlags;
use skillranker::roster::LocalPath;
use skillranker::runtime::{EntryClock, ProcessInvocation};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn unix_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

fn invocation() -> ProcessInvocation {
    ProcessInvocation::from_clock(
        EntryClock::capture_with(
            DurationMillis::new("test", 12_000, 30_000).unwrap(),
            DurationMillis::new("cleanup", 200, 30_000).unwrap(),
        )
        .unwrap(),
    )
    .unwrap()
}

struct Fixture {
    root: PathBuf,
    provider: Child,
    lines: BufReader<ChildStdout>,
    port: u16,
}

impl Fixture {
    fn new(scenario: &str) -> Self {
        let root = support::private_store_dir("invocation-identity");
        for dir in ["workspace/.claude/skills", "ledger", "provider"] {
            fs::DirBuilder::new()
                .mode(0o700)
                .recursive(true)
                .create(root.join(dir))
                .unwrap();
        }
        for name in ["alpha", "beta"] {
            let dir = root.join("workspace/.claude/skills").join(name);
            fs::create_dir(&dir).unwrap();
            fs::write(dir.join("SKILL.md"), format!("---\nname: {name}\ndescription: Diagnose failing Rust tests\n---\nRead and repair Rust test failures.\n")).unwrap();
        }
        let setup = invocation();
        skillranker::storage::init_ledger(
            &setup,
            &setup.request_cx().unwrap(),
            skillranker::storage::LedgerLocation::Directory(root.join("ledger")),
        )
        .unwrap();
        assert!(setup.shutdown());
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
            fs::write(root.join("provider").join(name), bytes).unwrap();
        }
        let mut provider = Command::new("/usr/bin/python3")
            .arg(root.join("provider/provider_server.py"))
            .arg(scenario)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(provider.stdout.take().unwrap());
        let mut hello = String::new();
        assert!(lines.read_line(&mut hello).unwrap() > 0);
        let port = u16::try_from(
            serde_json::from_str::<Value>(&hello).unwrap()["port"]
                .as_u64()
                .unwrap(),
        )
        .unwrap();
        Self {
            root,
            provider,
            lines,
            port,
        }
    }

    fn rank(&self, event: &str) -> Value {
        self.rank_for_harness(event, "claude_code")
    }

    fn rank_for_harness(&self, event: &str, harness: &str) -> Value {
        let workspace = self.root.join("workspace");
        let context = workspace.join("context.json");
        fs::write(
            &context,
            json!({
                "schema_version": 1, "harness": harness, "producer_id": "invocation-test",
                "workspace_root": workspace, "session_id": "session", "agent_id": null,
                "branch_id": null, "context_epoch": null,
                "current_request": {"event_id": event, "text": "Diagnose our failing Rust tests",
                    "attachments_omitted": false, "essential_attachment_missing": false},
                "events": [], "explicit_skill_references": [], "supplied_loads": []
            })
            .to_string(),
        )
        .unwrap();
        let origin = format!("https://localhost:{}", self.port);
        let client = JevClient::with_additional_roots(
            EndpointConfig::from_base_origin_str(&origin).unwrap(),
            Certificate::from_pem(include_bytes!("fixtures/jev-tls/ca.pem")).unwrap(),
        )
        .unwrap();
        let mut sources = ConfigSources::default();
        sources.environment.extend([
            (
                "TYPESAFE_API_KEY".into(),
                "synthetic-invocation-canary".into(),
            ),
            ("TYPESAFE_ENDPOINT".into(), origin.into()),
            ("SR_TIMEOUT_MS".into(), "12000".into()),
        ]);
        let args = RankArgs {
            workspace,
            user_config_root: None,
            home: None,
            cache_dir: None,
            ledger_dir: Some(self.root.join("ledger")),
            sources,
            gate: EffectGate::new(
                EffectFlags {
                    allow_network: true,
                    no_cache: true,
                    ..Default::default()
                },
                Scope::Rank,
            )
            .unwrap(),
            source_options: SourceOptions {
                context: Some(LocalPath::new(context)),
                ..Default::default()
            },
            require_skills: Vec::new(),
            shortlist_ids: Vec::new(),
            roster_file: None,
            explain: false,
            why_not: None,
            cursor: None,
            output_json: true,
            output_table: false,
            dry_run: false,
            save_case: None,
        };
        let inv = invocation();
        let cx = inv.request_cx().unwrap();
        let document = inv
            .runtime()
            .block_on(execute_pipeline(&inv, &cx, args, Some(&client)))
            .unwrap();
        assert_eq!(
            document.exit_code(),
            skillranker::output::CliExit::Success,
            "{document:?}"
        );
        let result = document.as_value().clone();
        assert!(inv.shutdown());
        result
    }

    fn database(&self) -> Connection {
        Connection::open(
            self.root
                .join("ledger")
                .join(skillranker::storage::LEDGER_FILE),
        )
        .unwrap()
    }

    fn finish(mut self) -> Vec<Value> {
        let mut done = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        done.write_all(b"DONE").unwrap();
        drop(done);
        let mut served = Vec::new();
        loop {
            let mut line = String::new();
            assert!(self.lines.read_line(&mut line).unwrap() > 0);
            let value: Value = serde_json::from_str(&line).unwrap();
            if value["done"] == true {
                break;
            }
            assert_ne!(value["handshake_rejected"], true);
            served.push(value);
        }
        assert!(self.provider.wait().unwrap().success());
        served
    }
}

#[test]
fn normalized_configured_roots_rank_through_real_tls_and_record_the_actual_harness() {
    let f = Fixture::new("useful");
    fs::create_dir_all(f.root.join("workspace/.sr")).unwrap();
    // Selecting this directory explicitly does not claim a native Claude event.
    fs::write(
        f.root.join("workspace/.sr/config.toml"),
        "[roster]\nroots=['.claude/skills']\n",
    )
    .unwrap();
    let doc = f.rank_for_harness("normalized-request", "normalized");
    assert_eq!(doc["decision"], "ranked");
    assert_eq!(doc["harness"], "normalized");
    assert!(
        doc["skills"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["visibility"] == "unverified")
    );
    assert_eq!(doc["usage"]["http_attempts"], 2);
    let db = f.database();
    let (harness, attempts): (String, i64) = db
        .query_row(
            "SELECT s.adapter, (SELECT count(*) FROM provider_attempts) FROM ranking_events e JOIN roster_snapshots s ON s.snapshot_id = e.snapshot_id",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(harness, "normalized");
    assert_eq!(attempts, 2);
    assert_eq!(f.finish().len(), 2);
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.provider.kill();
        let _ = self.provider.wait();
    }
}

#[test]
fn later_library_rankings_date_attempts_with_their_own_entry_clock() {
    let f = Fixture::new("useful");
    let first = f.rank("first-request");
    assert_eq!(first["decision"], "ranked");
    let db = f.database();
    let first_rows: Vec<(String, i64)> = db
        .prepare(
            "SELECT attempt_id, admitted_at_unix_ms FROM provider_attempts ORDER BY attempt_id",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(first_rows.len(), 2);
    // Separate invocation entry times; no fabricated timestamp or clock reset.
    std::thread::sleep(Duration::from_millis(250));
    let before = unix_ms();
    let second = f.rank("second-request");
    let after = unix_ms();
    assert_eq!(second["decision"], "ranked");
    let second_rows: Vec<(i64, i64, i64, String)> = db.prepare("SELECT admitted_at_unix_ms, sent_at_unix_ms, completed_at_unix_ms, status FROM provider_attempts WHERE owner_event_id = ?1")
        .unwrap().query_map([second["event_id"].as_str().unwrap()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(second_rows.len(), 2);
    for (admitted, sent, completed, status) in second_rows {
        assert!(
            admitted >= before as i64 && completed <= after as i64,
            "attempt must belong to its invocation's real wall-clock interval: before={before}, admitted={admitted}, sent={sent}, completed={completed}, after={after}"
        );
        assert!(admitted <= sent && sent <= completed);
        assert_eq!(status, "completed");
    }
    for (id, admitted) in first_rows {
        let unchanged: i64 = db
            .query_row(
                "SELECT admitted_at_unix_ms FROM provider_attempts WHERE attempt_id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unchanged, admitted);
    }
    assert_eq!(f.finish().len(), 4, "two real TLS stages per ranking");
}

#[test]
fn duplicate_event_library_rankings_preserve_each_provider_attempt() {
    for (scenario, decision, per_rank) in [("useful", "ranked", 2), ("low-need", "abstain", 1)] {
        let f = Fixture::new(scenario);
        let first = f.rank("same-request");
        let second = f.rank("same-request");
        assert_eq!(first["decision"], decision);
        assert_eq!(second["decision"], decision);
        assert_eq!(
            first["event_id"], second["event_id"],
            "duplicate delivery is one event"
        );
        let db = f.database();
        let (count, tokens, completed): (i64, i64, i64) = db.query_row(
            "SELECT count(*), sum(input_tokens), sum(status = 'completed') FROM provider_attempts", [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        ).unwrap();
        assert_eq!(
            f.finish().len(),
            per_rank * 2,
            "server observes both invocations"
        );
        assert_eq!(
            count,
            (per_rank * 2) as i64,
            "separate invocations cannot share attempt rows"
        );
        assert_eq!(
            completed, count,
            "journal and settlement refer to the same rows"
        );
        assert_eq!(
            tokens,
            if per_rank == 2 { 440 } else { 200 },
            "all observed usage retained"
        );
        let events: i64 = db
            .query_row("SELECT count(*) FROM ranking_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(events, 1, "duplicate delivery retains one ranking event");
    }
}
