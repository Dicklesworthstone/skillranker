#![cfg(unix)]
//! Production ranking over partial filesystem inventories. Only Jev transport
//! is replaced: real discovery, reads, option maps, both codecs, eligibility,
//! scoring and publication revalidation run. No provider is contacted.

use asupersync::Cx;
use serde_json::json;
use skillranker::config::ConfigSources;
use skillranker::context::source::SourceOptions;
use skillranker::effects::{EffectGate, Scope};
use skillranker::identity::ContentHash;
use skillranker::jev::OriginScopedCredential;
use skillranker::jev::client::{TransportError, TransportErrorKind};
use skillranker::jev::codec::{Question, Request, Response};
use skillranker::jev::retry::RetryAfter;
use skillranker::limits::{DISCOVERY_FILES, DISCOVERY_PARSED_BYTES, DurationMillis};
use skillranker::output::{CliExit, Decision, OutputDocument, OutputKind};
use skillranker::pipeline::{JevTransport, RankArgs, execute_pipeline};
use skillranker::privacy::{EffectFlags, NetworkConsent};
use skillranker::roster::LocalPath;
use skillranker::runtime::{EntryClock, ProcessInvocation};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

const CANARY: &str = "rejected-slot-private-canary";

#[derive(Default)]
struct Transport {
    requests: Mutex<Vec<Request>>,
    move_during_rerank: Option<(PathBuf, PathBuf)>,
}

impl JevTransport for Transport {
    fn send<'a>(
        &'a self,
        request: &'a Request,
        _credential: Option<&'a OriginScopedCredential>,
        _consent: NetworkConsent,
        _cx: &'a Cx,
        _clock: &'a EntryClock,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Response, TransportError>> + Send + 'a>,
    > {
        let rerank = request.questions().contains_key("rerank");
        {
            let mut requests = self.requests.lock().unwrap();
            assert_eq!(
                requests.len(),
                if rerank { 1 } else { 0 },
                "exactly wide, then rerank"
            );
            requests.push(request.clone());
        }
        if rerank
            && let Some((from, to)) = &self.move_during_rerank
        {
            fs::rename(from, to).unwrap();
        }
        let response = response(request);
        Box::pin(async move { response })
    }
}

fn response(request: &Request) -> Result<Response, TransportError> {
    let mut answers = serde_json::Map::new();
    for (id, question) in request.questions() {
        let answer = match question {
            Question::Choice { criteria, .. } => {
                let mut probabilities = BTreeMap::new();
                let choice = if id == "phase" {
                    assert!(criteria.contains_key("implementing"));
                    for key in criteria.keys() {
                        probabilities.insert(key.clone(), 1.0 / criteria.len() as f64);
                    }
                    "implementing".to_owned()
                } else {
                    assert!(matches!(id.as_str(), "which" | "rerank"));
                    assert!(criteria.contains_key("__none__"));
                    let real: Vec<_> = criteria.keys().filter(|k| *k != "__none__").collect();
                    assert!(!real.is_empty());
                    probabilities.insert("__none__".into(), 0.1);
                    for key in &real {
                        probabilities.insert((*key).clone(), 0.9 / real.len() as f64);
                    }
                    real[0].clone()
                };
                json!({"type":"choice", "choice":choice,
                       "probabilities":probabilities, "confidence":0.8})
            }
            Question::Noul { .. } => {
                assert!(id.starts_with("gate::") || id.starts_with("fits::"));
                let probability = if id == "gate::context_suffices" {
                    0.1
                } else {
                    0.9
                };
                json!({"type":"noul", "noul":probability})
            }
        };
        answers.insert(id.clone(), answer);
    }
    let bytes = serde_json::to_vec(&json!({
        "model":"jev-partial-roster-fixture", "answers":answers,
        "usage":{"input_tokens":100, "output_tokens":25}
    }))
    .unwrap();
    request
        .decode_response(&bytes)
        .map_err(|error| TransportError {
            kind: TransportErrorKind::Response(error),
            http_attempt_started: true,
            retry_after: RetryAfter::Absent,
        })
}

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
    home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "sr-rank-partial-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        let workspace = root.join("workspace");
        let home = root.join("home");
        for directory in [
            &workspace,
            &home,
            &root.join("config/sr"),
            &root.join("outside"),
        ] {
            fs::create_dir_all(directory).unwrap();
        }
        let prompt = "Help repair the failing Rust tests in this project.";
        let context = json!({
            "schema_version":1, "harness":"claude_code", "producer_id":"partial-roster-test",
            "workspace_root":workspace.to_string_lossy(), "session_id":"session-1",
            "agent_id":null, "branch_id":null, "context_epoch":null,
            "current_request":{"event_id":"request-1", "text":prompt,
                "attachments_omitted":false, "essential_attachment_missing":false},
            "events":[{"event_id":"request-1", "parent_id":null, "turn_id":"turn-1",
                "agent_id":null, "branch_id":null, "role":"user", "kind":"message",
                "timestamp_unix_ms":null, "text":prompt, "tool":null}],
            "explicit_skill_references":[], "supplied_loads":[]
        });
        fs::write(
            workspace.join("context.json"),
            serde_json::to_vec(&context).unwrap(),
        )
        .unwrap();
        Self {
            root,
            workspace,
            home,
        } // Retained; no cleanup of fixture trees.
    }

    fn slot(&self, personal: bool, name: &str) -> PathBuf {
        let base = if personal {
            &self.home
        } else {
            &self.workspace
        };
        let path = base.join(".claude/skills").join(name).join("SKILL.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        path
    }

    fn skill(&self, personal: bool, name: &str, manual: bool) -> ContentHash {
        let body = format!(
            "---\nname: {name}\ndescription: Repair Rust tests with a structured procedure.\n\
             disable-model-invocation: {manual}\n---\nProcedure for {name}; personal={personal}.\n"
        );
        fs::write(self.slot(personal, name), &body).unwrap();
        ContentHash::from_bytes(body.as_bytes())
    }

    fn gap(&self, kind: &str, personal: bool, name: &str) -> PathBuf {
        let slot = self.slot(personal, name);
        match kind {
            "symlink" => {
                // Move the empty directory aside: deletion is not needed.
                fs::rename(
                    slot.parent().unwrap(),
                    self.root.join("retained-empty-dir"),
                )
                .unwrap();
                fs::write(self.root.join("outside/SKILL.md"), CANARY).unwrap();
                symlink(self.root.join("outside"), slot.parent().unwrap()).unwrap();
            }
            "oversized" => {
                fs::write(&slot, CANARY).unwrap();
                fs::OpenOptions::new()
                    .write(true)
                    .open(&slot)
                    .unwrap()
                    .set_len(DISCOVERY_PARSED_BYTES.max() as u64 + 1)
                    .unwrap();
            }
            "fifo" => nix::unistd::mkfifo(&slot, nix::sys::stat::Mode::S_IRWXU).unwrap(),
            "directory" => fs::create_dir(&slot).unwrap(),
            _ => panic!("unexpected fixture kind"),
        }
        slot.parent().unwrap().to_owned()
    }

    fn run(&self, transport: &Transport) -> OutputDocument {
        let clock = EntryClock::capture_with(
            DurationMillis::new("total", 30_000, 30_000).unwrap(),
            DurationMillis::new("cleanup", 500, 30_000).unwrap(),
        )
        .unwrap();
        let invocation = ProcessInvocation::from_clock(clock).unwrap();
        let cx = invocation.request_cx().unwrap();
        let mut sources = ConfigSources::default();
        sources
            .environment
            .push(("TYPESAFE_API_KEY".into(), "test-api-key-partial".into()));
        let gate = EffectGate::new(
            EffectFlags {
                offline: false,
                allow_network: true,
                dry_run: false,
                no_cache: true,
                no_ledger: true,
                no_persist: true,
                save_case: false,
            },
            Scope::Rank,
        )
        .unwrap();
        let args = RankArgs {
            workspace: self.workspace.clone(),
            user_config_root: Some(self.root.join("config")),
            home: Some(self.home.clone()),
            cache_dir: None,
            ledger_dir: None,
            sources,
            gate,
            source_options: SourceOptions {
                context: Some(LocalPath::new(self.workspace.join("context.json"))),
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
        let result = invocation.runtime().block_on(execute_pipeline(
            &invocation,
            &cx,
            args,
            Some(transport),
        ));
        assert!(invocation.shutdown());
        let doc = result.expect("admitted pipeline results use the full decision envelope");
        let value = doc.as_value();
        assert_eq!(value["usage"]["requests"], 2, "{value}");
        assert_eq!(value["usage"]["http_attempts"], 2, "{value}");
        assert_eq!(value["usage"]["input_tokens"], 200);
        assert_eq!(value["usage"]["output_tokens"], 50);
        assert_eq!(value["usage"]["unknown_usage_attempts"], 0);
        assert_eq!(value["persistence"], "disabled");
        doc
    }
}

fn assert_requests(transport: &Transport, eligible: usize, root: &Path) {
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for (request, stage) in requests.iter().zip(["which", "rerank"]) {
        let Question::Choice { criteria, .. } = &request.questions()[stage] else {
            panic!("expected a choice");
        };
        assert_eq!(criteria.len(), eligible + 1);
        let payload = String::from_utf8(request.to_json().unwrap()).unwrap();
        assert!(
            !payload.contains(CANARY),
            "rejected content or names reached the provider"
        );
        assert!(
            !payload.contains(root.to_str().unwrap()),
            "local fixture path leaked"
        );
        assert!(
            !payload.contains("test-api-key-partial"),
            "credential leaked into request body"
        );
    }
}

fn assert_ranked(doc: &OutputDocument, expected: &BTreeMap<&str, ContentHash>) {
    assert_eq!(
        doc.kind(),
        OutputKind::Decision(Decision::Ranked),
        "{}",
        doc.as_value()
    );
    assert_eq!(doc.exit_code(), CliExit::Success);
    let value = doc.as_value();
    assert_eq!(value["roster"]["eligible"], expected.len());
    assert_eq!(value["roster"]["partial"], true);
    let skills = value["skills"].as_array().unwrap();
    let names: BTreeSet<_> = skills
        .iter()
        .map(|skill| skill["invocation_name"].as_str().unwrap())
        .collect();
    assert_eq!(names, expected.keys().copied().collect());
    for skill in skills {
        let name = skill["invocation_name"].as_str().unwrap();
        assert_eq!(skill["content_hash"], expected[name].as_str());
        let probability = skill["rerank_probability"].as_f64().unwrap();
        assert!(probability > value["none_probability"].as_f64().unwrap());
    }
}

#[test]
fn unchanged_discovery_gaps_allow_complete_two_stage_ranking() {
    for kind in ["symlink", "oversized"] {
        let fixture = Fixture::new();
        fixture.gap(kind, false, CANARY);
        let expected = ["alpha", "beta", "gamma"]
            .into_iter()
            .map(|name| (name, fixture.skill(true, name, false)))
            .collect();
        let transport = Transport::default();
        let doc = fixture.run(&transport);
        assert_ranked(&doc, &expected);
        assert_requests(&transport, 3, &fixture.root);
    }
}

#[test]
fn invalid_personal_overrides_are_excluded_from_both_provider_stages() {
    for (kind, name) in [
        ("fifo", CANARY),
        ("directory", CANARY),
        ("oversized", CANARY),
        ("symlink", "SKILL.md"),
    ] {
        let fixture = Fixture::new();
        let hash = fixture.skill(false, "alpha", false);
        fixture.skill(false, name, false);
        fixture.gap(kind, true, name);
        let transport = Transport::default();
        let doc = fixture.run(&transport);
        assert_ranked(&doc, &BTreeMap::from([("alpha", hash)]));
        assert_requests(&transport, 1, &fixture.root);
    }
}

#[test]
fn real_entry_ceiling_keeps_personal_precedence_through_ranking() {
    let fixture = Fixture::new();
    let mut expected = BTreeMap::new();
    for name in ["alpha", "gamma"] {
        expected.insert(name, fixture.skill(false, name, false));
    }
    fixture.skill(false, "beta", false);
    fixture.skill(true, "beta", true);
    expected.insert("delta", fixture.skill(true, "delta", false));
    let references = fixture
        .workspace
        .join(".claude/skills/alpha/references");
    fs::create_dir_all(&references).unwrap();
    for index in 0..=DISCOVERY_FILES.max() {
        fs::write(references.join(format!("note-{index:05}.txt")), "note").unwrap();
    }
    let transport = Transport::default();
    let doc = fixture.run(&transport);
    assert_ranked(&doc, &expected);
    assert_requests(&transport, 3, &fixture.root);
}

#[test]
fn a_competitor_appearing_during_rerank_revokes_publication_but_preserves_usage() {
    for kind in ["symlink", "oversized"] {
        let fixture = Fixture::new();
        fixture.skill(false, "alpha", false);
        let from = fixture.gap(kind, true, CANARY);
        let transport = Transport {
            move_during_rerank: Some((from, fixture.home.join(".claude/skills/alpha"))),
            ..Default::default()
        };
        let doc = fixture.run(&transport);
        assert_eq!(doc.kind(), OutputKind::Decision(Decision::Unavailable));
        assert_eq!(doc.exit_code(), CliExit::Roster);
        let value = doc.as_value();
        assert_eq!(value["error"]["kind"], "roster-changed", "{value}");
        assert!(value["skills"].as_array().unwrap().is_empty());
        assert_requests(&transport, 1, &fixture.root);
    }
}

#[test]
fn a_skill_directory_named_like_its_file_is_not_rejected_as_a_file_slot() {
    let fixture = Fixture::new();
    let hash = fixture.skill(false, "SKILL.md", false);
    let expected = BTreeMap::from([("SKILL.md", hash)]);
    let transport = Transport::default();
    let doc = fixture.run(&transport);
    assert_ranked(&doc, &expected);
    assert_requests(&transport, 1, &fixture.root);
}
