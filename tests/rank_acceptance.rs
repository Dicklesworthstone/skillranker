#![cfg(unix)]
//! Rank pipeline acceptance through a real loopback TLS provider.
//!
//! `execute_pipeline` runs with trusted user configuration on disk and a real
//! `JevClient` that trusts only the synthetic fixture CA in addition to the
//! public roots. `tests/fixtures/jev-tls/provider_server.py` answers every
//! question from the request it receives and reports each request it served,
//! so request counts are observed at the provider, not only in the output.
//!
//! The pipeline binds a synthetic credential to the fixture origin exactly as
//! it binds a real one. No-claim: the `sr` binary itself cannot trust a
//! fixture CA, so this proves pipeline branches over real TLS with an injected
//! client, not the binary's own client or live Jev behavior.
use asupersync::tls::Certificate;
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
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
const CONSENT: &str = "[network]\nenabled = true\n";

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    // Intentionally retained: repository policy forbids automatic tree deletion.
    fn new(user_config: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "sr-rank-acceptance-{}-{}-{}",
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
        std::fs::write(root.join("config/sr/config.toml"), user_config).unwrap();
        let f = Self { root };
        f.skill("alpha", "Runs and repairs failing rust tests.");
        f.skill("beta", "Drafts release notes from git history.");
        f
    }
    fn workspace(&self) -> PathBuf {
        self.root.join("workspace")
    }
    fn user_config(&self) -> PathBuf {
        self.root.join("config/sr/config.toml")
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
    fn context(&self, request: &str) -> PathBuf {
        let path = self.workspace().join("context.json");
        let message = |id: &str, text: &str| {
            json!({"event_id": id, "parent_id": null, "turn_id": "turn-1", "agent_id": null,
                   "branch_id": null, "role": "user", "kind": "message",
                   "timestamp_unix_ms": null, "text": text, "tool": null})
        };
        let context = json!({
            "schema_version": 1,
            "harness": "claude_code",
            "producer_id": "synthetic-test",
            "workspace_root": self.workspace().to_string_lossy(),
            "session_id": "session-1",
            "agent_id": null,
            "branch_id": null,
            "context_epoch": null,
            "current_request": {"event_id": "request-1", "text": request,
                                "attachments_omitted": false, "essential_attachment_missing": false},
            "events": [message("request-1", request)],
            "explicit_skill_references": [],
            "supplied_loads": []
        });
        std::fs::write(&path, serde_json::to_vec(&context).unwrap()).unwrap();
        path
    }
    fn args(&self, request: &str) -> RankArgs {
        let flags = EffectFlags {
            offline: false,
            allow_network: false,
            dry_run: false,
            no_cache: true,
            no_ledger: true,
            no_persist: true,
            save_case: false,
        };
        RankArgs {
            workspace: self.workspace(),
            user_config_root: Some(self.root.join("config")),
            home: None,
            sources: ConfigSources {
                environment: vec![(
                    OsString::from("TYPESAFE_API_KEY"),
                    OsString::from("synthetic-acceptance-canary"),
                )],
                ..Default::default()
            },
            gate: EffectGate::new(flags, Scope::Rank).unwrap(),
            source_options: SourceOptions {
                context: Some(LocalPath::new(self.context(request))),
                ..Default::default()
            },
            require_skills: Vec::new(),
            roster_file: None,
            explain: false,
            why_not: None,
            output_json: true,
            output_table: false,
            dry_run: false,
        }
    }
}

struct Provider {
    child: Child,
    lines: BufReader<ChildStdout>,
    port: u16,
}

impl Provider {
    fn start(f: &Fixture, scenario: &str, extra: &[&std::ffi::OsStr]) -> Self {
        let directory = f.root.join("provider");
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
    fn client(&self) -> JevClient {
        let endpoint =
            EndpointConfig::from_base_origin_str(&format!("https://localhost:{}", self.port))
                .unwrap();
        let root = Certificate::from_pem(include_bytes!("fixtures/jev-tls/ca.pem"))
            .unwrap()
            .remove(0);
        JevClient::with_additional_roots(endpoint, vec![root]).unwrap()
    }
    /// Ends the provider and returns the stages it actually served.
    fn finish(mut self) -> Vec<Value> {
        let mut done = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        done.write_all(b"DONE").unwrap();
        drop(done);
        let mut served = Vec::new();
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
            assert_eq!(
                value["authorization"], true,
                "the synthetic credential is bound to the fixture origin"
            );
            served.push(value);
        }
        assert!(self.child.wait().unwrap().success(), "provider must finish");
        served
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

type Outcome = Result<Value, (u8, &'static str)>;

fn rank(f: &Fixture, provider: &Provider, request: &str, total_ms: u64) -> Outcome {
    let clock = EntryClock::capture_with(
        DurationMillis::new("acceptance-total", total_ms, 30_000).unwrap(),
        DurationMillis::new("acceptance-cleanup", 200, 30_000).unwrap(),
    )
    .unwrap();
    let invocation = ProcessInvocation::from_clock(clock).unwrap();
    let cx = invocation.request_cx().unwrap();
    let client = provider.client();
    let args = f.args(request);
    let result = invocation
        .runtime()
        .block_on(async { execute_pipeline(&clock, &cx, args, Some(&client)).await });
    assert!(invocation.shutdown(), "owned runtime must shut down");
    result
        .map(|doc| doc.as_value().clone())
        .map_err(|(code, kind, _)| (code, kind))
}

fn stages(served: &[Value]) -> Vec<&str> {
    served
        .iter()
        .map(|s| s["stage"].as_str().unwrap())
        .collect()
}

fn usage(value: &Value) -> (u64, u64, u64, u64) {
    let u = &value["usage"];
    (
        u["requests"].as_u64().unwrap(),
        u["http_attempts"].as_u64().unwrap(),
        u["input_tokens"].as_u64().unwrap(),
        u["output_tokens"].as_u64().unwrap(),
    )
}

const TASK: &str = "The rust tests are failing; find and repair the failing test.";

#[test]
fn a_useful_evaluation_ranks_after_wide_and_rerank() {
    let f = Fixture::new(CONSENT);
    let provider = Provider::start(&f, "useful", &[]);
    let value = rank(&f, &provider, TASK, 10_000).expect("ranked");
    let served = provider.finish();
    assert_eq!(stages(&served), ["wide", "rerank"]);
    assert_eq!(value["decision"], "ranked", "{value}");
    assert!(!value["skills"].as_array().unwrap().is_empty());
    assert_eq!(usage(&value), (2, 2, 220, 55));
    // Output reports what ran: real counts, the provider's returned model
    // next to the requested alias, and distinct candidate-set digests.
    assert_eq!(value["roster"]["wide_candidates"], 2);
    assert_eq!(value["roster"]["shortlist"], 2);
    assert_eq!(value["model"]["requested"], "jev-latest");
    assert_eq!(value["model"]["wide_returned"], "jev-test");
    assert_eq!(value["model"]["rerank_returned"], "jev-test");
    let provenance = &value["roster"]["provenance"];
    assert_ne!(provenance["wide_set_id"], provenance["rerank_set_id"]);
}

#[test]
fn low_need_abstains_after_one_request() {
    let f = Fixture::new(CONSENT);
    let provider = Provider::start(&f, "low-need", &[]);
    let value = rank(&f, &provider, TASK, 10_000).expect("abstain");
    let served = provider.finish();
    assert_eq!(stages(&served), ["wide"]);
    assert_eq!(value["decision"], "abstain", "{value}");
    assert!(value["skills"].as_array().unwrap().is_empty());
    assert!(value["none_probability"].is_null(), "rerank never ran");
    assert!(value["needs_skill"].as_f64().unwrap() < 0.3);
    assert_eq!(value["model"]["wide_returned"], "jev-test");
    assert!(value["model"]["rerank_returned"].is_null());
    assert!(value["roster"]["provenance"]["wide_set_id"].is_string());
    assert!(value["roster"]["provenance"]["rerank_set_id"].is_null());
    assert_eq!(usage(&value), (1, 1, 100, 25));
}

#[test]
fn a_none_winner_and_low_fits_abstain_after_both_requests() {
    for scenario in ["none", "low-fit"] {
        let f = Fixture::new(CONSENT);
        let provider = Provider::start(&f, scenario, &[]);
        let value = rank(&f, &provider, TASK, 10_000).expect("abstain");
        let served = provider.finish();
        assert_eq!(stages(&served), ["wide", "rerank"], "{scenario}");
        assert_eq!(value["decision"], "abstain", "{scenario}: {value}");
        assert!(value["skills"].as_array().unwrap().is_empty(), "{scenario}");
        assert!(value["none_probability"].is_f64(), "{scenario}: rerank ran");
        assert_eq!(value["model"]["rerank_returned"], "jev-test", "{scenario}");
        assert_eq!(usage(&value), (2, 2, 220, 55), "{scenario}");
    }
}

#[test]
fn an_explicit_request_resolves_locally_without_a_provider_call() {
    let f = Fixture::new(CONSENT);
    let provider = Provider::start(&f, "useful", &[]);
    let value =
        rank(&f, &provider, "Please use skill alpha to fix this.", 10_000).expect("explicit");
    let served = provider.finish();
    assert!(served.is_empty(), "explicit resolution sends nothing");
    assert_eq!(value["decision"], "explicit", "{value}");
    assert_eq!(value["skills"][0]["invocation_name"], "alpha");
    assert_eq!(usage(&value), (0, 0, 0, 0));
}

#[test]
fn a_partial_roster_still_ranks_a_verified_target() {
    let f = Fixture::new(CONSENT);
    // A malformed skill is an excluded record; the others remain rankable.
    let broken = f.skill_file("broken");
    std::fs::create_dir_all(broken.parent().unwrap()).unwrap();
    std::fs::write(&broken, "---\nname: [unterminated\n---\nBody.\n").unwrap();
    let provider = Provider::start(&f, "useful", &[]);
    let value = rank(&f, &provider, TASK, 10_000).expect("ranked");
    let served = provider.finish();
    assert_eq!(stages(&served), ["wide", "rerank"]);
    assert_eq!(value["decision"], "ranked", "{value}");
    let names: Vec<&str> = value["skills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["invocation_name"].as_str().unwrap())
        .collect();
    assert!(!names.contains(&"broken"), "{names:?}");
}

#[test]
fn a_roster_change_during_rerank_withholds_the_result() {
    let f = Fixture::new(CONSENT);
    let alpha = f.skill_file("alpha");
    let provider = Provider::start(&f, "touch-on-rerank", &[alpha.as_os_str()]);
    let outcome = rank(&f, &provider, TASK, 10_000);
    let served = provider.finish();
    assert_eq!(stages(&served), ["wide", "rerank"]);
    assert_eq!(outcome, Err((5, "roster-changed")));
}

#[test]
fn withdrawn_consent_denies_the_rerank_send() {
    let f = Fixture::new(CONSENT);
    let config = f.user_config();
    let provider = Provider::start(
        &f,
        "write-on-wide",
        &[config.as_os_str(), "[network]\nenabled = false\n".as_ref()],
    );
    let outcome = rank(&f, &provider, TASK, 10_000);
    let served = provider.finish();
    assert_eq!(
        stages(&served),
        ["wide"],
        "no rerank after consent is withdrawn"
    );
    assert_eq!(outcome.map_err(|(code, _)| code), Err(8));
}

#[test]
fn a_changed_exclusion_supersedes_the_evaluation() {
    let f = Fixture::new(CONSENT);
    let config = f.user_config();
    let changed = format!("{CONSENT}[ranking]\nexclude_skills = [\"alpha\"]\n");
    let provider = Provider::start(&f, "write-on-wide", &[config.as_os_str(), changed.as_ref()]);
    let outcome = rank(&f, &provider, TASK, 10_000);
    let served = provider.finish();
    assert_eq!(
        stages(&served),
        ["wide"],
        "no rerank under a changed policy"
    );
    assert_eq!(outcome, Err((3, "superseded")));
}

#[test]
fn an_irrelevant_config_edit_still_ranks() {
    let f = Fixture::new(CONSENT);
    let config = f.user_config();
    let same = format!("# rewritten by the user; same settings\n{CONSENT}");
    let provider = Provider::start(&f, "write-on-wide", &[config.as_os_str(), same.as_ref()]);
    let value = rank(&f, &provider, TASK, 10_000).expect("ranked");
    let served = provider.finish();
    assert_eq!(stages(&served), ["wide", "rerank"]);
    assert_eq!(value["decision"], "ranked", "{value}");
}

#[test]
fn a_late_rerank_answer_is_never_published() {
    let f = Fixture::new(CONSENT);
    let provider = Provider::start(&f, "late-rerank", &["".as_ref(), "4".as_ref()]);
    let started = std::time::Instant::now();
    let outcome = rank(&f, &provider, TASK, 2_000);
    let elapsed = started.elapsed();
    let served = provider.finish();
    assert_eq!(stages(&served), ["wide", "rerank"]);
    assert_eq!(outcome.map_err(|(code, _)| code), Err(6));
    assert!(
        elapsed < std::time::Duration::from_millis(3_500),
        "the deadline, not the provider, ends the run: {elapsed:?}"
    );
}
