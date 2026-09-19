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
use skillranker::identity::HarnessId;
use skillranker::jev::client::JevClient;
use skillranker::jev::endpoint::EndpointConfig;
use skillranker::limits::DurationMillis;
use skillranker::pipeline::{RankArgs, execute_pipeline};
use skillranker::privacy::EffectFlags;
use skillranker::roster::LocalPath;
use skillranker::runtime::{EntryClock, ProcessInvocation};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
const CONSENT: &str = "[network]\nenabled = true\n";
/// Network-capable runs with no persistent state.
const UNCACHED: EffectFlags = EffectFlags {
    offline: false,
    allow_network: false,
    dry_run: false,
    no_cache: true,
    no_ledger: true,
    no_persist: true,
    save_case: false,
};
/// Default persistence: the response cache reads and writes.
const CACHED: EffectFlags = EffectFlags {
    no_cache: false,
    no_persist: false,
    ..UNCACHED
};

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
    fn context_in(&self, session: &str, request: &str) -> PathBuf {
        self.context_turns(session, &[], request)
    }
    /// Earlier user messages precede the current request in the session.
    fn context_turns(&self, session: &str, earlier: &[&str], request: &str) -> PathBuf {
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
            "session_id": session,
            "agent_id": null,
            "branch_id": null,
            "context_epoch": null,
            "current_request": {"event_id": "request-1", "text": request,
                                "attachments_omitted": false, "essential_attachment_missing": false},
            "events": earlier
                .iter()
                .enumerate()
                .map(|(i, text)| message(&format!("earlier-{i}"), text))
                .chain([message("request-1", request)])
                .collect::<Vec<_>>(),
            "explicit_skill_references": [],
            "supplied_loads": []
        });
        std::fs::write(&path, serde_json::to_vec(&context).unwrap()).unwrap();
        path
    }
    /// A synthetic Claude Code transcript: an earlier user message, then the
    /// current request, each record with its native identity.
    fn claude_transcript(&self, name: &str, earlier: &str, request: &str) -> SourceOptions {
        let path = self.workspace().join(name);
        let record = |uuid: &str, parent: Option<&str>, text: &str| {
            json!({"type": "user", "uuid": uuid, "parentUuid": parent,
                   "message": {"role": "user", "content": text}})
            .to_string()
        };
        let lines = [
            record("user-1", None, earlier),
            record("user-2", Some("user-1"), request),
        ];
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        SourceOptions {
            transcript: Some(LocalPath::new(path)),
            harness: Some(HarnessId::new("claude_code").unwrap()),
            ..Default::default()
        }
    }
    /// Records a session where Claude keeps it:
    /// `$HOME/.claude/projects/<encoded workspace>/<session>.jsonl`, with the
    /// older (`/` only) or newer (every non-alphanumeric) encoding. `cwd` is
    /// the working directory its records claim, this workspace by default.
    fn claude_session(
        &self,
        session: &str,
        request: &str,
        newer_encoding: bool,
        cwd: Option<&str>,
    ) -> PathBuf {
        let workspace = std::fs::canonicalize(self.workspace()).unwrap();
        let workspace = workspace.to_str().unwrap();
        let name: String = if newer_encoding {
            workspace
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect()
        } else {
            workspace.replace('/', "-")
        };
        let directory = self.root.join("home/.claude/projects").join(name);
        std::fs::create_dir_all(&directory).unwrap();
        let cwd = cwd.unwrap_or(workspace);
        let record = |n: u8, parent: Option<String>, text: &str| {
            json!({"type": "user", "uuid": format!("{session}-{n}"), "parentUuid": parent,
                   "cwd": cwd, "sessionId": session,
                   "message": {"role": "user", "content": text}})
            .to_string()
        };
        let path = directory.join(format!("{session}.jsonl"));
        let lines = [
            record(1, None, EARLIER),
            record(2, Some(format!("{session}-1")), request),
        ];
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }
    fn args(&self, request: &str) -> RankArgs {
        self.args_with(request, "session-1", UNCACHED, None)
    }
    /// A private cache directory under root-owned sticky /tmp: RCH's TMPDIR
    /// can have group-writable ancestors, which the store rightly refuses.
    fn cache_dir(&self) -> PathBuf {
        let dir = std::path::Path::new("/tmp").join(format!(
            "sr-rank-cache-{}",
            self.root.file_name().unwrap().to_string_lossy()
        ));
        if !dir.exists() {
            std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        }
        dir
    }
    fn args_with(
        &self,
        request: &str,
        session: &str,
        flags: EffectFlags,
        cache_dir: Option<PathBuf>,
    ) -> RankArgs {
        RankArgs {
            workspace: self.workspace(),
            user_config_root: Some(self.root.join("config")),
            home: None,
            cache_dir,
            sources: ConfigSources {
                environment: vec![(
                    OsString::from("TYPESAFE_API_KEY"),
                    OsString::from("synthetic-acceptance-canary"),
                )],
                ..Default::default()
            },
            gate: EffectGate::new(flags, Scope::Rank).unwrap(),
            source_options: SourceOptions {
                context: Some(LocalPath::new(self.context_in(session, request))),
                ..Default::default()
            },
            require_skills: Vec::new(),
            shortlist_ids: Vec::new(),
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
    fn finish(self) -> Vec<Value> {
        let (served, rejected) = self.finish_with_rejections();
        assert_eq!(rejected, 0, "every handshake must succeed");
        served
    }
    /// Ends the provider; returns served requests and rejected handshakes.
    fn finish_with_rejections(mut self) -> (Vec<Value>, usize) {
        let mut done = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        done.write_all(b"DONE").unwrap();
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
            assert_eq!(
                value["authorization"], true,
                "the synthetic credential is bound to the fixture origin"
            );
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

type Outcome = Result<Value, (u8, &'static str)>;

fn rank(f: &Fixture, provider: &Provider, request: &str, total_ms: u64) -> Outcome {
    rank_args(provider, f.args(request), total_ms)
}

fn rank_args(provider: &Provider, args: RankArgs, total_ms: u64) -> Outcome {
    let clock = EntryClock::capture_with(
        DurationMillis::new("acceptance-total", total_ms, 30_000).unwrap(),
        DurationMillis::new("acceptance-cleanup", 200, 30_000).unwrap(),
    )
    .unwrap();
    let invocation = ProcessInvocation::from_clock(clock).unwrap();
    let cx = invocation.request_cx().unwrap();
    let client = provider.client();
    let result = invocation
        .runtime()
        .block_on(async { execute_pipeline(&invocation, &cx, args, Some(&client)).await });
    // Shutdown may only use the time left before expiry; record both so a
    // failure shows whether draining or a late return exhausted it.
    let returned_ms = clock.now().as_millis();
    let left_ms = clock.remaining_until_expiry().as_millis();
    assert!(
        invocation.shutdown(),
        "owned runtime must shut down: returned at {returned_ms} ms of {total_ms}, {left_ms} ms left"
    );
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

/// A failure after input admission is a full unavailable decision that keeps
/// the stages that ran and the usage already incurred.
fn unavailable(outcome: Outcome, code: u64, kind: &str) -> Value {
    let value = outcome.expect("a full unavailable decision");
    assert_eq!(value["decision"], "unavailable", "{value}");
    assert_eq!(value["error"]["code"], code, "{value}");
    assert_eq!(value["error"]["kind"], kind, "{value}");
    assert!(value["skills"].as_array().unwrap().is_empty());
    value
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
    assert_eq!(value["quality"]["history_windowed"], false);
    assert_eq!(value["quality"]["prompt_complete"], true);
    let kinds: Vec<&str> = value["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["kind"].as_str().unwrap())
        .collect();
    assert!(!kinds.contains(&"malformed-metadata"), "{kinds:?}");
    // Claude's precedence is provisional: every result says so, and the
    // decision carries the caveat first, one per resolved binding.
    for skill in value["skills"].as_array().unwrap() {
        assert_eq!(skill["visibility"], "unverified", "{skill}");
    }
    assert_eq!(value["warnings"][0]["kind"], "unverified-visibility");
    assert_eq!(value["warnings"][0]["count"], 2);
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
    assert_eq!(value["skills"][0]["visibility"], "unverified");
    assert_eq!(value["warnings"][0]["kind"], "unverified-visibility");
    assert_eq!(usage(&value), (0, 0, 0, 0));
}

/// Asserts alpha never reached the provider: the wide Choice offered only beta
/// and none, and no request carried alpha's description.
fn alpha_never_sent(served: &[Value]) {
    assert_eq!(served[0]["options"], 2, "beta plus none");
    for request in served {
        let body = request["body"].as_str().unwrap();
        assert!(
            !body.contains("Runs and repairs failing rust tests"),
            "{body}"
        );
    }
}

#[test]
fn a_prompt_exclusion_removes_the_skill_before_any_send() {
    let f = Fixture::new(CONSENT);
    let provider = Provider::start(&f, "useful", &[]);
    let request = format!("{TASK} Don't use skill alpha.");
    let value = rank(&f, &provider, &request, 10_000).expect("ranked");
    let served = provider.finish();
    assert_eq!(stages(&served), ["wide", "rerank"]);
    alpha_never_sent(&served);
    assert_eq!(value["decision"], "ranked", "{value}");
    for skill in value["skills"].as_array().unwrap() {
        assert_eq!(skill["invocation_name"], "beta", "{skill}");
    }
}

#[test]
fn an_exclusion_from_an_earlier_turn_still_applies() {
    let f = Fixture::new(CONSENT);
    let provider = Provider::start(&f, "useful", &[]);
    let mut args = f.args(TASK);
    let context = f.context_turns(
        "session-1",
        &["Do not use skill alpha in this session."],
        TASK,
    );
    args.source_options.context = Some(LocalPath::new(context));
    let value = rank_args(&provider, args, 10_000).expect("ranked");
    let served = provider.finish();
    alpha_never_sent(&served);
    assert_eq!(value["decision"], "ranked", "{value}");
    for skill in value["skills"].as_array().unwrap() {
        assert_eq!(skill["invocation_name"], "beta", "{skill}");
    }
}

#[test]
fn a_configured_exclusion_conflicts_with_an_explicit_request() {
    let f = Fixture::new(&format!(
        "{CONSENT}[ranking]\nexclude_skills = [\"alpha\"]\n"
    ));
    let provider = Provider::start(&f, "useful", &[]);
    let outcome = rank(&f, &provider, "Please use skill alpha to fix this.", 10_000);
    let served = provider.finish();
    assert!(served.is_empty(), "a conflict sends nothing");
    // The explicit-resolution failure envelope: no guess, no advisory skills.
    let value = outcome.expect("an unavailable decision");
    assert_eq!(value["decision"], "unavailable", "{value}");
    assert_eq!(value["error"]["code"], 5, "{value}");
    assert_eq!(value["error"]["kind"], "unresolved-explicit", "{value}");
    assert!(
        value
            .get("skills")
            .is_none_or(|s| s.as_array().unwrap().is_empty())
    );
    assert_eq!(value["unresolved"][0]["reference"], "alpha", "{value}");
    assert_eq!(value["unresolved"][0]["reason"], "restricted", "{value}");
}

const EARLIER: &str = "Earlier: summarise how the workspace builds.";

#[test]
fn a_native_transcript_sends_its_latest_request_once() {
    let f = Fixture::new(CONSENT);
    let provider = Provider::start(&f, "useful", &[]);
    // The same conversation as normalized context and as a Claude transcript.
    let mut normalized = f.args(TASK);
    let context = f.context_turns("session-1", &[EARLIER], TASK);
    normalized.source_options.context = Some(LocalPath::new(context));
    rank_args(&provider, normalized, 10_000).expect("ranked");
    let mut native = f.args(TASK);
    native.source_options = f.claude_transcript("claude.jsonl", EARLIER, TASK);
    let value = rank_args(&provider, native, 10_000).expect("ranked");
    let served = provider.finish();
    assert_eq!(stages(&served), ["wide", "rerank", "wide", "rerank"]);
    assert_eq!(value["decision"], "ranked", "{value}");
    let count = |body: &Value, text: &str| body.as_str().unwrap().matches(text).count();
    let request = "find and repair the failing test";
    assert!(count(&served[2]["body"], "summarise how the workspace builds") > 0);
    assert_eq!(
        count(&served[2]["body"], request),
        count(&served[0]["body"], request),
        "the transcript's latest request is not repeated as history"
    );
}

#[test]
fn native_transcripts_never_share_a_cached_response() {
    let f = Fixture::new(CONSENT);
    let cache = f.cache_dir();
    let provider = Provider::start(&f, "useful", &[]);
    // Two sessions whose files, and so request bytes, are identical.
    for name in ["first.jsonl", "second.jsonl"] {
        let mut args = f.args_with(TASK, "session-1", CACHED, Some(cache.clone()));
        args.source_options = f.claude_transcript(name, EARLIER, TASK);
        let value = rank_args(&provider, args, 10_000).expect("ranked");
        assert_eq!(value["cache"]["hit"], false, "{name}: {value}");
    }
    assert_eq!(
        stages(&provider.finish()),
        ["wide", "rerank", "wide", "rerank"]
    );
}

const RELEASE: &str = "Write the changelog entry for version 2.4 of this project.";

#[test]
fn bare_rank_uses_the_only_session_of_this_workspace() {
    let f = Fixture::new(CONSENT);
    std::fs::create_dir_all(f.root.join("home")).unwrap();
    f.claude_session("s-only", TASK, false, None);
    // A transcript whose records name another workspace is never a candidate.
    f.claude_session(
        "s-foreign",
        RELEASE,
        false,
        Some("/data/projects/elsewhere"),
    );
    let provider = Provider::start(&f, "useful", &[]);
    let (code, value) = run_bare(&f, &provider, &[]);
    let served = provider.finish();
    assert_eq!(code, Some(0), "{value}");
    assert_eq!(value["decision"], "ranked", "{value}");
    assert_eq!(
        value["warnings"][0]["kind"], "discovered-session",
        "{value}"
    );
    assert_eq!(value["warnings"][0]["count"], 1);
    let body = served[0]["body"].as_str().unwrap();
    assert!(body.contains("find and repair the failing test"), "{body}");
    assert!(!body.contains("changelog entry for version 2.4"), "{body}");
}

#[test]
fn bare_rank_needs_latest_to_choose_between_sessions() {
    let f = Fixture::new(CONSENT);
    std::fs::create_dir_all(f.root.join("home")).unwrap();
    let older = f.claude_session("s-older", TASK, false, None);
    let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3_600);
    std::fs::File::options()
        .write(true)
        .open(older)
        .unwrap()
        .set_modified(hour_ago)
        .unwrap();
    f.claude_session("s-newer", RELEASE, true, None);
    let provider = Provider::start(&f, "useful", &[]);
    let (code, value) = run_bare(&f, &provider, &[]);
    assert_eq!(code, Some(3), "{value}");
    assert_eq!(value["error"]["kind"], "ambiguous-session", "{value}");
    let (code, value) = run_bare(&f, &provider, &["--latest"]);
    let served = provider.finish();
    assert_eq!(code, Some(0), "{value}");
    assert_eq!(value["warnings"][0]["kind"], "latest-session", "{value}");
    assert_eq!(value["warnings"][0]["count"], 2);
    // Only the --latest run sent anything, and it sent the newer session.
    assert_eq!(stages(&served), ["wide", "rerank"]);
    let body = served[0]["body"].as_str().unwrap();
    assert!(body.contains("changelog entry for version 2.4"), "{body}");
    assert!(!body.contains("find and repair the failing test"), "{body}");
}

#[test]
fn bare_rank_does_not_choose_around_an_unresolved_transcript() {
    let f = Fixture::new(CONSENT);
    std::fs::create_dir_all(f.root.join("home")).unwrap();
    let known = f.claude_session("s-known", TASK, false, None);
    std::fs::write(known.parent().unwrap().join("s-unresolved.jsonl"), "{\n").unwrap();
    let provider = Provider::start(&f, "useful", &[]);
    for extra in [&[][..], &["--latest"][..]] {
        let (code, value) = run_bare(&f, &provider, extra);
        assert_eq!(code, Some(3), "{value}");
        assert_eq!(value["error"]["kind"], "insufficient-context", "{value}");
    }
    assert!(
        provider.finish().is_empty(),
        "uncertain selection sends nothing"
    );
    // Explicitly choosing the verified file bypasses discovery; an unrelated
    // unresolved neighbor must not make that valid source unusable.
    let provider = Provider::start(&f, "useful", &[]);
    let (code, value) = run_bare(
        &f,
        &provider,
        &[
            "--transcript",
            known.to_str().unwrap(),
            "--harness",
            "claude_code",
        ],
    );
    assert_eq!(code, Some(0), "{value}");
    assert_eq!(value["decision"], "ranked", "{value}");
    assert_eq!(stages(&provider.finish()), ["wide", "rerank"]);
}

#[test]
fn bare_rank_without_a_session_reports_missing_session() {
    let f = Fixture::new(CONSENT);
    std::fs::create_dir_all(f.root.join("home")).unwrap();
    let provider = Provider::start(&f, "useful", &[]);
    let (code, value) = run_bare(&f, &provider, &[]);
    assert!(provider.finish().is_empty());
    assert_eq!(code, Some(3), "{value}");
    assert_eq!(value["error"]["kind"], "missing-session", "{value}");
}

#[test]
fn a_native_session_repeat_is_cached_and_never_shared() {
    let f = Fixture::new(CONSENT);
    let cache = f.cache_dir();
    let provider = Provider::start(&f, "useful", &[]);
    let workspace = std::fs::canonicalize(f.workspace()).unwrap();
    let run = |path: PathBuf| {
        let mut args = f.args_with(TASK, "session-1", CACHED, Some(cache.clone()));
        args.workspace = workspace.clone();
        args.source_options = SourceOptions {
            transcript: Some(LocalPath::new(path)),
            harness: Some(HarnessId::new("claude_code").unwrap()),
            ..Default::default()
        };
        rank_args(&provider, args, 10_000).expect("ranked")
    };
    let first = f.claude_session("s-a", TASK, false, None);
    let second = f.claude_session("s-b", TASK, false, None);
    assert_eq!(run(first.clone())["cache"]["hit"], false);
    assert_eq!(
        run(first)["cache"]["hit"],
        true,
        "an exact repeat of one session"
    );
    assert_eq!(run(second)["cache"]["hit"], false, "another session");
    let served = provider.finish();
    assert_eq!(stages(&served), ["wide", "rerank", "wide", "rerank"]);
    // The two sessions sent identical bytes: only their identity differs.
    assert_eq!(served[0]["body"], served[2]["body"]);
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
    // The exclusion is disclosed as a bounded warning, not silently dropped.
    let malformed = value["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["kind"] == "malformed-metadata")
        .unwrap_or_else(|| panic!("{value}"));
    assert_eq!(malformed["count"], 1);
}

#[test]
fn a_roster_change_during_rerank_withholds_the_result() {
    let f = Fixture::new(CONSENT);
    let alpha = f.skill_file("alpha");
    let provider = Provider::start(&f, "touch-on-rerank", &[alpha.as_os_str()]);
    let outcome = rank(&f, &provider, TASK, 10_000);
    let served = provider.finish();
    assert_eq!(stages(&served), ["wide", "rerank"]);
    let value = unavailable(outcome, 5, "roster-changed");
    assert!(value["none_probability"].is_f64(), "rerank ran");
    assert_eq!(usage(&value), (2, 2, 220, 55));
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
    let value = unavailable(outcome, 8, "network-denied");
    assert_eq!(
        value["roster"]["shortlist"], 2,
        "the shortlist was committed"
    );
    assert!(
        value["model"]["rerank_returned"].is_null(),
        "rerank never ran"
    );
    assert!(value["none_probability"].is_null());
    assert_eq!(usage(&value), (1, 1, 100, 25));
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
    let value = unavailable(outcome, 3, "superseded");
    assert_eq!(usage(&value), (1, 1, 100, 25));
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
    let value = unavailable(outcome, 6, "timeout");
    // The late attempt may have been billed: it is unknown usage, not zero.
    assert_eq!(usage(&value), (2, 2, 100, 25));
    assert_eq!(value["usage"]["unknown_usage_attempts"], 1);
    assert!(
        elapsed < std::time::Duration::from_millis(3_500),
        "the deadline, not the provider, ends the run: {elapsed:?}"
    );
}

#[test]
fn a_transient_wide_failure_is_retried_within_the_allowance() {
    let f = Fixture::new(CONSENT);
    let provider = Provider::start(&f, "retry-wide", &[]);
    let value = rank(&f, &provider, TASK, 10_000).expect("ranked");
    let served = provider.finish();
    assert_eq!(stages(&served), ["wide", "wide", "rerank"]);
    assert_eq!(served[0]["status"], 503);
    assert_eq!(value["decision"], "ranked", "{value}");
    // Two logical requests over three attempts; the 503 returned no usage.
    assert_eq!(usage(&value), (2, 3, 220, 55));
    assert_eq!(value["usage"]["unknown_usage_attempts"], 1);
}

#[test]
fn persistent_provider_failure_stops_at_the_attempt_allowance() {
    let f = Fixture::new(CONSENT);
    let provider = Provider::start(&f, "always-503", &[]);
    let outcome = rank(&f, &provider, TASK, 10_000);
    let served = provider.finish();
    assert_eq!(
        stages(&served),
        ["wide", "wide", "wide", "wide"],
        "four HTTP attempts per invocation at most"
    );
    let value = unavailable(outcome, 4, "request-budget");
    assert_eq!(usage(&value), (1, 4, 0, 0));
    assert_eq!(value["usage"]["unknown_usage_attempts"], 4);
}

#[test]
fn an_authentication_failure_is_not_retried() {
    let f = Fixture::new(CONSENT);
    let provider = Provider::start(&f, "unauthorized", &[]);
    let outcome = rank(&f, &provider, TASK, 10_000);
    let served = provider.finish();
    assert_eq!(stages(&served), ["wide"], "authentication is never retried");
    let value = unavailable(outcome, 4, "authentication");
    assert_eq!(usage(&value), (1, 1, 0, 0));
}

#[test]
fn an_exact_repeat_is_served_from_the_persistent_cache() {
    let f = Fixture::new(CONSENT);
    let cache = f.cache_dir();
    let provider = Provider::start(&f, "useful", &[]);
    let args = || f.args_with(TASK, "session-1", CACHED, Some(cache.clone()));
    let first = rank_args(&provider, args(), 10_000).expect("ranked");
    let second = rank_args(&provider, args(), 10_000).expect("ranked");
    let served = provider.finish();
    assert_eq!(
        stages(&served),
        ["wide", "rerank"],
        "the repeat sends nothing"
    );
    assert_eq!(first["decision"], "ranked", "{first}");
    assert_eq!(first["cache"]["hit"], false);
    assert_eq!(second["decision"], "ranked", "{second}");
    assert_eq!(second["skills"], first["skills"]);
    assert_eq!(second["cache"]["hit"], true);
    assert_eq!(second["cache"]["wide_hit"], true);
    assert_eq!(second["cache"]["rerank_hit"], true);
    assert!(second["cache"]["age_ms"].is_u64());
    assert_eq!(usage(&second), (0, 0, 0, 0), "a cache hit incurs no usage");
    let file = std::fs::metadata(cache.join("cache.sqlite3")).unwrap();
    assert_eq!(file.mode() & 0o7777, 0o600, "the store is owner-only");
}

#[test]
fn a_different_session_never_reuses_a_response() {
    let f = Fixture::new(CONSENT);
    let cache = f.cache_dir();
    let provider = Provider::start(&f, "useful", &[]);
    rank_args(
        &provider,
        f.args_with(TASK, "session-1", CACHED, Some(cache.clone())),
        10_000,
    )
    .expect("ranked");
    let other = rank_args(
        &provider,
        f.args_with(TASK, "session-2", CACHED, Some(cache)),
        10_000,
    )
    .expect("ranked");
    let served = provider.finish();
    assert_eq!(stages(&served), ["wide", "rerank", "wide", "rerank"]);
    assert_eq!(other["cache"]["hit"], false);
    assert_eq!(usage(&other), (2, 2, 220, 55));
}

#[test]
fn offline_serves_a_complete_cached_pair() {
    let f = Fixture::new(CONSENT);
    let cache = f.cache_dir();
    let provider = Provider::start(&f, "useful", &[]);
    let online = rank_args(
        &provider,
        f.args_with(TASK, "session-1", CACHED, Some(cache.clone())),
        10_000,
    )
    .expect("ranked");
    let offline = EffectFlags {
        offline: true,
        ..CACHED
    };
    let local = rank_args(
        &provider,
        f.args_with(TASK, "session-1", offline, Some(cache)),
        10_000,
    )
    .expect("ranked from cache");
    let served = provider.finish();
    assert_eq!(stages(&served), ["wide", "rerank"]);
    assert_eq!(local["decision"], "ranked", "{local}");
    assert_eq!(local["skills"], online["skills"]);
    assert_eq!(usage(&local), (0, 0, 0, 0));
}

#[test]
fn a_cached_wide_answer_is_never_paired_with_a_fresh_rerank() {
    let f = Fixture::new(CONSENT);
    let cache = f.cache_dir();
    // The first run records its wide answer, then its rerank never arrives.
    // Its budget leaves a loaded host time to reach the wide send.
    let late = Provider::start(&f, "late-rerank", &["".as_ref(), "8".as_ref()]);
    let first = rank_args(
        &late,
        f.args_with(TASK, "session-1", CACHED, Some(cache.clone())),
        5_000,
    );
    assert_eq!(stages(&late.finish()), ["wide", "rerank"]);
    unavailable(first, 6, "timeout");
    // The repeat refreshes the whole pair rather than reusing the wide answer.
    let provider = Provider::start(&f, "useful", &[]);
    let value = rank_args(
        &provider,
        f.args_with(TASK, "session-1", CACHED, Some(cache)),
        10_000,
    )
    .expect("ranked");
    assert_eq!(stages(&provider.finish()), ["wide", "rerank"]);
    assert_eq!(value["cache"]["hit"], false);
    assert_eq!(usage(&value), (2, 2, 220, 55));
}

#[test]
fn a_cached_low_need_answer_needs_no_rerank() {
    let f = Fixture::new(CONSENT);
    let cache = f.cache_dir();
    let provider = Provider::start(&f, "low-need", &[]);
    let args = || f.args_with(TASK, "session-1", CACHED, Some(cache.clone()));
    rank_args(&provider, args(), 10_000).expect("abstain");
    let second = rank_args(&provider, args(), 10_000).expect("abstain");
    assert_eq!(stages(&provider.finish()), ["wide"]);
    assert_eq!(second["decision"], "abstain", "{second}");
    assert_eq!(second["cache"]["wide_hit"], true);
    assert_eq!(second["cache"]["rerank_hit"], false);
    assert_eq!(usage(&second), (0, 0, 0, 0));
}

#[test]
fn disabled_persistence_never_creates_a_store() {
    for flags in [
        EffectFlags {
            no_cache: true,
            ..CACHED
        },
        EffectFlags {
            no_persist: true,
            ..CACHED
        },
        EffectFlags {
            dry_run: true,
            ..CACHED
        },
    ] {
        let f = Fixture::new(CONSENT);
        let cache = f.cache_dir();
        let provider = Provider::start(&f, "useful", &[]);
        let value = rank_args(
            &provider,
            f.args_with(TASK, "session-1", flags, Some(cache.clone())),
            10_000,
        )
        .expect("a decision");
        provider.finish();
        // A dry run answers with a preview; the others with a decision.
        assert!(
            value["decision"].is_string() || value["kind"] == "preview",
            "{value}"
        );
        assert!(
            !cache.join("cache.sqlite3").exists(),
            "{flags:?} must not create a store"
        );
    }
}

/// The real `sr` binary against the fixture provider. The fixture CA is
/// trusted only through `SSL_CERT_FILE`, the standard trusted-environment
/// root override; endpoint, key and consent come from the environment and
/// trusted user configuration exactly as a user supplies them.
fn run_sr(
    f: &Fixture,
    provider: &Provider,
    trust_fixture: bool,
    request: &str,
) -> (Option<i32>, Value) {
    run_sr_with(f, provider, trust_fixture, request, &[])
}

fn sr_command(
    f: &Fixture,
    provider: &Provider,
    trust_fixture: bool,
    request: &str,
    extra: &[&str],
) -> Command {
    let context = f.context_in("session-1", request);
    let mut command = sr_rank(f, provider, trust_fixture);
    command
        .args(["--context", context.to_str().unwrap()])
        .args(extra);
    command
}

/// `sr rank --json` in the fixture workspace, with no source selected.
fn sr_rank(f: &Fixture, provider: &Provider, trust_fixture: bool) -> Command {
    let ca = f.root.join("fixture-ca.pem");
    std::fs::write(&ca, include_bytes!("fixtures/jev-tls/ca.pem")).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_sr"));
    command
        .env_clear()
        .env("HOME", f.root.join("home"))
        .env("XDG_CONFIG_HOME", f.root.join("config"))
        .env("XDG_CACHE_HOME", f.cache_dir())
        .env("TYPESAFE_API_KEY", "synthetic-acceptance-canary")
        .env(
            "TYPESAFE_ENDPOINT",
            format!("https://localhost:{}", provider.port),
        )
        .current_dir(f.workspace())
        .args(["rank", "--json"]);
    if trust_fixture {
        command.env("SSL_CERT_FILE", &ca);
    }
    command
}

/// Runs `sr rank` with no source, so it must discover the session itself.
fn run_bare(f: &Fixture, provider: &Provider, extra: &[&str]) -> (Option<i32>, Value) {
    let output = sr_rank(f, provider, true).args(extra).output().unwrap();
    let text = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);
    assert!(!text.contains("synthetic-acceptance-canary"));
    (
        output.status.code(),
        serde_json::from_slice(&output.stdout).unwrap(),
    )
}

fn run_sr_with(
    f: &Fixture,
    provider: &Provider,
    trust_fixture: bool,
    request: &str,
    extra: &[&str],
) -> (Option<i32>, Value) {
    let mut command = sr_command(f, provider, trust_fixture, request, extra);
    let output = command.output().unwrap();
    let text = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);
    assert!(
        !text.contains("synthetic-acceptance-canary"),
        "the key never reaches output"
    );
    (
        output.status.code(),
        serde_json::from_slice(&output.stdout).unwrap(),
    )
}

#[test]
fn the_sr_binary_honors_a_longer_configured_deadline() {
    // Trusted user configuration allows 15 s; the wide answer takes 3.5 s,
    // longer than the default 3 s deadline.
    let f = Fixture::new(&format!("{CONSENT}[ranking]\ntimeout_ms = 15000\n"));
    std::fs::create_dir_all(f.root.join("home")).unwrap();
    let provider = Provider::start(&f, "slow-wide", &["".as_ref(), "3.5".as_ref()]);
    let (code, value) = run_sr_with(&f, &provider, true, TASK, &[]);
    let served = provider.finish();
    assert_eq!(stages(&served), ["wide", "rerank"]);
    assert_eq!(code, Some(0), "{value}");
    assert_eq!(value["decision"], "ranked", "{value}");
}

#[test]
fn the_sr_binary_honors_a_shorter_timeout_flag() {
    // The wide answer takes 2 s, inside the default 3 s deadline but not
    // inside the requested 800 ms.
    let f = Fixture::new(CONSENT);
    std::fs::create_dir_all(f.root.join("home")).unwrap();
    let provider = Provider::start(&f, "slow-wide", &["".as_ref(), "2".as_ref()]);
    let started = std::time::Instant::now();
    let (code, value) = run_sr_with(&f, &provider, true, TASK, &["--timeout-ms", "800"]);
    let elapsed = started.elapsed();
    let (served, _) = provider.finish_with_rejections();
    assert!(!stages(&served).contains(&"rerank"), "{served:?}");
    assert_eq!(code, Some(6), "{value}");
    assert_eq!(value["error"]["kind"], "timeout", "{value}");
    assert!(
        elapsed < std::time::Duration::from_millis(2_000),
        "the requested deadline, not the provider, ends the run: {elapsed:?}"
    );
}

#[test]
fn the_sr_binary_ranks_over_real_tls_and_serves_its_repeat_from_cache() {
    let f = Fixture::new(CONSENT);
    std::fs::create_dir_all(f.root.join("home")).unwrap();
    let provider = Provider::start(&f, "useful", &[]);
    let (code, first) = run_sr(&f, &provider, true, TASK);
    let (repeat_code, repeat) = run_sr(&f, &provider, true, TASK);
    let served = provider.finish();
    assert_eq!(code, Some(0), "{first}");
    assert_eq!(first["decision"], "ranked", "{first}");
    assert_eq!(usage(&first), (2, 2, 220, 55));
    assert_eq!(
        stages(&served),
        ["wide", "rerank"],
        "the repeat sends nothing"
    );
    assert_eq!(repeat_code, Some(0), "{repeat}");
    assert_eq!(repeat["skills"], first["skills"]);
    assert_eq!(repeat["cache"]["hit"], true);
    assert_eq!(usage(&repeat), (0, 0, 0, 0));
    let store = f.cache_dir().join("sr").join("cache.sqlite3");
    assert_eq!(
        std::fs::metadata(store).unwrap().mode() & 0o7777,
        0o600,
        "the platform cache store is owner-only"
    );
}

#[test]
fn the_sr_binary_reports_a_provider_refusal_with_its_usage() {
    let f = Fixture::new(CONSENT);
    std::fs::create_dir_all(f.root.join("home")).unwrap();
    let provider = Provider::start(&f, "unauthorized", &[]);
    let (code, value) = run_sr(&f, &provider, true, TASK);
    assert_eq!(stages(&provider.finish()), ["wide"]);
    assert_eq!(code, Some(4), "{value}");
    assert_eq!(value["decision"], "unavailable");
    assert_eq!(value["error"]["kind"], "authentication");
    assert!(
        value["event_id"].is_string(),
        "a full decision after admission"
    );
    assert_eq!(value["usage"]["requests"], 1);
}

#[test]
fn the_sr_binary_never_trusts_the_fixture_without_the_trusted_root() {
    let f = Fixture::new(CONSENT);
    std::fs::create_dir_all(f.root.join("home")).unwrap();
    let provider = Provider::start(&f, "useful", &[]);
    let (code, value) = run_sr(&f, &provider, false, TASK);
    let (served, rejected) = provider.finish_with_rejections();
    assert!(
        served.is_empty(),
        "no request crosses an untrusted connection"
    );
    assert_eq!(
        rejected, 1,
        "one attempt; TLS verification is never retried"
    );
    assert_eq!(code, Some(4), "{value}");
    assert_eq!(value["decision"], "unavailable");
    assert_eq!(value["error"]["kind"], "network-failure");
}

/// A skill's stable ID as `sr roster --json` reports it.
fn skill_id(f: &Fixture, invocation: &str) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_sr"))
        .env_clear()
        .env("HOME", f.root.join("home"))
        .env("XDG_CONFIG_HOME", f.root.join("config"))
        .current_dir(f.workspace())
        .args(["roster", "--json"])
        .output()
        .unwrap();
    let listing: Value = serde_json::from_slice(&output.stdout).unwrap();
    listing["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["invocation_name"] == invocation)
        .and_then(|r| r["skill_id"].as_str())
        .unwrap()
        .to_owned()
}

#[test]
fn the_dry_run_request_is_the_exact_bytes_a_stateless_rank_sends() {
    let f = Fixture::new(CONSENT);
    std::fs::create_dir_all(f.root.join("home")).unwrap();
    let provider = Provider::start(&f, "useful", &[]);
    let (code, preview) = run_sr_with(&f, &provider, true, TASK, &["--dry-run"]);
    let (sent_code, ranked) = run_sr_with(&f, &provider, true, TASK, &["--no-persist"]);
    let served = provider.finish();
    assert_eq!(code, Some(0), "{preview}");
    assert_eq!(preview["kind"], "preview");
    assert_eq!(preview["actionable"], false);
    assert_eq!(preview["stateless"], true);
    assert!(preview["local_decision"].is_null());
    assert_eq!(preview["effects"]["network"]["state"], "blocked");
    assert!(preview["disclosure"]["disclosed_bytes"].is_u64());
    assert_eq!(sent_code, Some(0), "{ranked}");
    assert_eq!(
        stages(&served),
        ["wide", "rerank"],
        "the preview sent nothing"
    );
    let previewed = &preview["provider_request"]["stages"][0];
    assert_eq!(previewed["stage"], "wide");
    assert_eq!(
        previewed["request"], served[0]["body"],
        "the preview is byte-identical to the wide request a --no-persist run sends"
    );
    assert_eq!(
        previewed["request_bytes"].as_u64().unwrap() as usize,
        served[0]["body"].as_str().unwrap().len()
    );
    assert!(
        !f.cache_dir().join("sr").exists(),
        "neither the preview nor --no-persist creates a store"
    );
}

#[test]
fn a_dry_run_previews_the_rerank_for_supplied_shortlist_evidence() {
    let f = Fixture::new(CONSENT);
    std::fs::create_dir_all(f.root.join("home")).unwrap();
    let alpha = skill_id(&f, "alpha");
    let provider = Provider::start(&f, "useful", &[]);
    let (code, preview) = run_sr_with(
        &f,
        &provider,
        true,
        TASK,
        &["--dry-run", "--shortlist-ids", &alpha],
    );
    assert!(provider.finish().is_empty(), "a preview sends nothing");
    assert_eq!(code, Some(0), "{preview}");
    let stages = preview["provider_request"]["stages"].as_array().unwrap();
    assert_eq!(stages.len(), 2);
    assert_eq!(stages[1]["stage"], "rerank");
    assert_eq!(stages[1]["candidates"], 1);
    assert!(
        stages[1]["request"]
            .as_str()
            .unwrap()
            .contains("Runs and repairs failing rust tests."),
        "the rerank request carries the supplied skill"
    );
}

#[test]
fn absent_or_invalid_stage_two_evidence_is_never_guessed() {
    let f = Fixture::new(CONSENT);
    std::fs::create_dir_all(f.root.join("home")).unwrap();
    let provider = Provider::start(&f, "useful", &[]);
    // Absent: the preview stops at the wide request.
    let (code, preview) = run_sr_with(&f, &provider, true, TASK, &["--dry-run"]);
    assert_eq!(code, Some(0), "{preview}");
    let stages = preview["provider_request"]["stages"].as_array().unwrap();
    assert_eq!(stages.len(), 1);
    // Invalid: an ID that is not a wide candidate, a duplicate, and evidence
    // outside a dry run are all usage errors.
    let alpha = skill_id(&f, "alpha");
    let cases: [&[&str]; 3] = [
        &["--dry-run", "--shortlist-ids", "s_not_a_candidate"],
        &[
            "--dry-run",
            "--shortlist-ids",
            &alpha,
            "--shortlist-ids",
            &alpha,
        ],
        &["--shortlist-ids", &alpha],
    ];
    for extra in cases {
        let (code, value) = run_sr_with(&f, &provider, true, TASK, extra);
        assert_eq!(code, Some(2), "{extra:?}: {value}");
        assert_eq!(value["error"]["kind"], "invalid-usage", "{extra:?}");
    }
    assert!(provider.finish().is_empty(), "nothing was sent");
}

#[test]
fn a_dry_run_reports_a_local_result_without_a_request() {
    let f = Fixture::new(CONSENT);
    std::fs::create_dir_all(f.root.join("home")).unwrap();
    let provider = Provider::start(&f, "useful", &[]);
    let (code, preview) = run_sr_with(
        &f,
        &provider,
        true,
        "Please use skill alpha to fix this.",
        &["--dry-run"],
    );
    assert!(provider.finish().is_empty());
    assert_eq!(code, Some(0), "{preview}");
    assert_eq!(preview["kind"], "preview");
    assert!(
        preview["provider_request"].is_null(),
        "no request would be made"
    );
    assert_eq!(preview["local_decision"]["decision"], "explicit");
    assert_eq!(
        preview["local_decision"]["skills"][0]["invocation_name"],
        "alpha"
    );
}

#[test]
fn a_dry_run_reports_a_local_abstention_without_a_request() {
    // Trusted configuration excludes every skill: local policy ends the run.
    let f = Fixture::new(&format!(
        "{CONSENT}[ranking]\nexclude_skills = [\"alpha\", \"beta\"]\n"
    ));
    std::fs::create_dir_all(f.root.join("home")).unwrap();
    let provider = Provider::start(&f, "useful", &[]);
    let (code, preview) = run_sr_with(&f, &provider, true, TASK, &["--dry-run"]);
    assert!(provider.finish().is_empty());
    assert_eq!(code, Some(0), "{preview}");
    assert_eq!(preview["kind"], "preview");
    assert!(
        preview["provider_request"].is_null(),
        "no request would be made"
    );
    assert_eq!(preview["local_decision"]["decision"], "abstain");
    assert_eq!(preview["local_decision"]["usage"]["requests"], 0);
}

#[test]
fn a_windowed_history_still_ranks_and_says_so() {
    let f = Fixture::new(CONSENT);
    let provider = Provider::start(&f, "useful", &[]);
    let args = f.args(TASK);
    // Replace the context with thirty earlier turns: more than the renderer's
    // message window, so history is deliberately bounded.
    let message = |id: String, text: String| {
        json!({"event_id": id, "parent_id": null, "turn_id": "turn-1", "agent_id": null,
               "branch_id": null, "role": "user", "kind": "message",
               "timestamp_unix_ms": null, "text": text, "tool": null})
    };
    let mut events: Vec<Value> = (0..30)
        .map(|i| message(format!("earlier-{i}"), format!("Step {i}: ran cargo test.")))
        .collect();
    events.push(message("request-1".into(), TASK.into()));
    let context = json!({
        "schema_version": 1, "harness": "claude_code", "producer_id": "synthetic-test",
        "workspace_root": f.workspace().to_string_lossy(), "session_id": "session-1",
        "agent_id": null, "branch_id": null, "context_epoch": null,
        "current_request": {"event_id": "request-1", "text": TASK,
                            "attachments_omitted": false, "essential_attachment_missing": false},
        "events": events, "explicit_skill_references": [], "supplied_loads": []
    });
    std::fs::write(
        f.workspace().join("context.json"),
        serde_json::to_vec(&context).unwrap(),
    )
    .unwrap();
    let value = rank_args(&provider, args, 10_000).expect("ranked");
    assert_eq!(stages(&provider.finish()), ["wide", "rerank"]);
    assert_eq!(value["decision"], "ranked", "{value}");
    assert_eq!(value["quality"]["history_windowed"], true);
    assert_eq!(value["quality"]["prompt_complete"], true);
    assert_eq!(value["quality"]["task_anchor_known"], true);
}

#[test]
fn persistence_reports_disabled_only_when_the_user_disabled_it() {
    let f = Fixture::new(CONSENT);
    let provider = Provider::start(&f, "low-need", &[]);
    // --no-ledger (and --no-persist) disable recording.
    let disabled = rank_args(&provider, f.args(TASK), 10_000).expect("abstain");
    // By default recording is wanted, but this build has no ledger yet.
    let default = EffectFlags {
        no_ledger: false,
        ..CACHED
    };
    let wanted = rank_args(
        &provider,
        f.args_with(TASK, "session-1", default, None),
        10_000,
    )
    .expect("abstain");
    provider.finish();
    assert_eq!(disabled["persistence"], "disabled", "{disabled}");
    assert_eq!(wanted["persistence"], "unavailable", "{wanted}");
}

#[test]
fn concurrent_identical_requests_share_one_provider_evaluation() {
    let f = Fixture::new(CONSENT);
    std::fs::create_dir_all(f.root.join("home")).unwrap();
    // The wide answer takes long enough that the second process arrives
    // while the first still holds the single-flight lease.
    let provider = Provider::start(&f, "slow-wide", &["".as_ref(), "1.2".as_ref()]);
    // Single flight, not latency, is under test: a generous deadline keeps a
    // loaded host from ending the follower's wait early, in which case it
    // rightly sends itself.
    const TIMEOUT: &[&str] = &["--timeout-ms", "20000"];
    // Build both commands first: building one writes the context file, which
    // must not change under a running process.
    let mut commands = [
        sr_command(&f, &provider, true, TASK, TIMEOUT),
        sr_command(&f, &provider, true, TASK, TIMEOUT),
    ];
    let children: Vec<_> = commands
        .iter_mut()
        .map(|command| {
            command
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let outputs: Vec<_> = children
        .into_iter()
        .map(|child| child.wait_with_output().unwrap())
        .collect();
    let served = provider.finish();
    assert_eq!(
        stages(&served),
        ["wide", "rerank"],
        "one provider evaluation for both processes"
    );
    let values: Vec<Value> = outputs
        .iter()
        .map(|output| {
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(output.status.code(), Some(0), "{value}");
            assert_eq!(value["decision"], "ranked", "{value}");
            value
        })
        .collect();
    assert_eq!(values[0]["skills"], values[1]["skills"]);
    let followers = values.iter().filter(|v| v["cache"]["hit"] == true).count();
    assert_eq!(
        followers, 1,
        "exactly one process was served the leader's pair"
    );
}
