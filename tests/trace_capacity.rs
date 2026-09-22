//! Regression for explanation-only failures at legal shortlist sizes.
mod support;

use asupersync::Cx;
use serde_json::json;
use skillranker::config::{ConfigSources, RawValue};
use skillranker::context::source::SourceOptions;
use skillranker::effects::{EffectGate, Scope};
use skillranker::identity::SkillId;
use skillranker::jev::OriginScopedCredential;
use skillranker::jev::client::TransportError;
use skillranker::jev::codec::{Request, Response};
use skillranker::limits::DurationMillis;
use skillranker::output::{Decision, MAX_TRACE_PAGE_ITEMS, OutputDocument, OutputKind};
use skillranker::pipeline::{JevTransport, RankArgs, execute_pipeline};
use skillranker::privacy::{EffectFlags, NetworkConsent};
use skillranker::roster::LocalPath;
use skillranker::runtime::{EntryClock, ProcessInvocation};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

static FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

type ResponseGenerator = Box<dyn Fn(&Request) -> Result<Response, TransportError> + Send + Sync>;

#[derive(Clone, Default)]
struct MockJevTransport {
    generators: Arc<Mutex<Vec<ResponseGenerator>>>,
    pub recorded_requests: Arc<Mutex<Vec<Request>>>,
}

impl MockJevTransport {
    fn new(generators: Vec<ResponseGenerator>) -> Self {
        Self {
            generators: Arc::new(Mutex::new(generators)),
            recorded_requests: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl JevTransport for MockJevTransport {
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
        self.recorded_requests.lock().unwrap().push(request.clone());
        let generator = self.generators.lock().unwrap().remove(0);
        let res = generator(request);
        Box::pin(async move { res })
    }
}

fn wrap_codec_err(e: skillranker::jev::codec::CodecError) -> TransportError {
    TransportError {
        kind: skillranker::jev::client::TransportErrorKind::Response(e),
        http_attempt_started: true,
        retry_after: skillranker::jev::retry::RetryAfter::Absent,
    }
}

fn create_test_env() -> (PathBuf, PathBuf) {
    let id = FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let root =
        std::env::temp_dir().join(format!("sr-trace-capacity-{}-{}", std::process::id(), id));
    let workspace = root.join("workspace");
    let skills_dir = workspace.join(".claude/skills");
    fs::create_dir_all(&skills_dir).unwrap();
    (root, workspace)
}

fn create_skill(dir: &Path, name: &str, desc: &str, body: &str) -> PathBuf {
    let skill_dir = dir.join(name);
    fs::create_dir_all(&skill_dir).unwrap();
    let file = skill_dir.join("SKILL.md");
    fs::write(
        &file,
        format!("---\nname: {name}\ndescription: {desc}\n---\n{body}\n"),
    )
    .unwrap();
    file
}

fn create_context_file(workspace: &Path, user_prompt: &str) -> PathBuf {
    let context_file = workspace.join("context.json");
    let ctx = json!({
        "schema_version": 1,
        "harness": "claude_code",
        "producer_id": "synthetic-test",
        "workspace_root": workspace.to_string_lossy(),
        "session_id": "session-1",
        "agent_id": null,
        "branch_id": null,
        "context_epoch": null,
        "current_request": {
            "event_id": "request-1",
            "text": user_prompt,
            "attachments_omitted": false,
            "essential_attachment_missing": false
        },
        "events": [
            {
                "event_id": "request-1",
                "parent_id": null,
                "turn_id": "turn-1",
                "agent_id": null,
                "branch_id": null,
                "role": "user",
                "kind": "message",
                "timestamp_unix_ms": null,
                "text": user_prompt,
                "tool": null
            }
        ],
        "explicit_skill_references": [],
        "supplied_loads": []
    });
    fs::write(&context_file, serde_json::to_vec(&ctx).unwrap()).unwrap();
    context_file
}

fn test_clock() -> EntryClock {
    EntryClock::capture_with(
        DurationMillis::new("test", 10_000, 30_000).unwrap(),
        DurationMillis::new("cleanup", 500, 30_000).unwrap(),
    )
    .unwrap()
}

// The transport is a deterministic protocol fixture, not live Jev evidence.
// Every real option must beat none, including at K=32; using the ordinary
// fixture's none=0.10 would silently filter most candidates and miss this bug.
fn all_eligible_response(req: &Request) -> Result<Response, TransportError> {
    use skillranker::jev::codec::Question;
    let mut answers = serde_json::Map::new();
    for (name, question) in req.questions() {
        let answer = match question {
            Question::Choice { criteria, .. } => {
                let real: Vec<_> = criteria
                    .keys()
                    .filter(|id| id.as_str() != "__none__")
                    .collect();
                assert!(!real.is_empty());
                let probabilities: serde_json::Map<String, serde_json::Value> = criteria
                    .keys()
                    .map(|id| {
                        (
                            id.clone(),
                            json!(if id == "__none__" {
                                0.0
                            } else {
                                1.0 / real.len() as f64
                            }),
                        )
                    })
                    .collect();
                json!({"type":"choice", "choice":real[0], "probabilities":probabilities, "confidence":0.5})
            }
            Question::Noul { .. } => {
                json!({"type":"noul", "noul":if name == "gate::context_suffices" { 0.0 } else { 1.0 }})
            }
        };
        answers.insert(name.clone(), answer);
    }
    req.decode_response(
        &serde_json::to_vec(&json!({
            "model":"jev-test", "answers":answers,
            "usage":{"input_tokens":100,"output_tokens":25}
        }))
        .unwrap(),
    )
    .map_err(wrap_codec_err)
}

fn check_capacity(count: usize) {
    let (_root, workspace) = create_test_env();
    for index in 0..count {
        create_skill(
            &workspace.join(".claude/skills"),
            &format!("skill_{index:02}"),
            "Help diagnose and fix Rust tests",
            "Inspect tests and correct the implementation.",
        );
    }
    let context = create_context_file(&workspace, "Diagnose the failing Rust tests.");
    let mut sources = ConfigSources::default();
    sources
        .environment
        .push(("TYPESAFE_API_KEY".into(), "test-key-123".into()));
    sources.cli = vec![
        ("ranking.top".into(), RawValue::Integer(count as i64)),
        ("ranking.shortlist".into(), RawValue::Integer(count as i64)),
    ];
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
    let run = |explain, why_not| {
        let clock = test_clock();
        let invocation = ProcessInvocation::from_clock(clock).unwrap();
        let cx = invocation.request_cx().unwrap();
        let transport = MockJevTransport::new(vec![
            Box::new(all_eligible_response),
            Box::new(all_eligible_response),
        ]);
        let args = RankArgs {
            workspace: workspace.clone(),
            user_config_root: None,
            home: None,
            cache_dir: Some(support::private_store_dir("cache")),
            sources: sources.clone(),
            gate,
            source_options: SourceOptions {
                context: Some(LocalPath::new(context.clone())),
                ..Default::default()
            },
            require_skills: Vec::new(),
            shortlist_ids: Vec::new(),
            roster_file: None,
            explain,
            why_not,
            cursor: None,
            output_json: true,
            output_table: false,
            dry_run: false,
            save_case: None,
            ledger_dir: Some(support::private_store_dir("ledger")),
        };
        let doc = invocation
            .runtime()
            .block_on(execute_pipeline(&invocation, &cx, args, Some(&transport)))
            .expect("explanations must not invalidate a successful ranking");
        let requests: Vec<_> = transport
            .recorded_requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.to_json().unwrap())
            .collect();
        assert_eq!(
            doc.kind(),
            OutputKind::Decision(Decision::Ranked),
            "K={count}, explain={explain}"
        );
        assert_eq!(doc.as_value()["skills"].as_array().unwrap().len(), count);
        assert_eq!(requests.len(), 2);
        OutputDocument::from_json(&doc.to_json().unwrap()).expect("valid bounded output");
        (doc, requests)
    };
    let (plain, plain_requests) = run(false, None);
    let (explained, explained_requests) = run(true, None);
    assert_eq!(
        plain_requests, explained_requests,
        "explanation changed provider bytes"
    );
    for field in ["decision", "skills", "usage"] {
        assert_eq!(
            plain.as_value()[field],
            explained.as_value()[field],
            "changed {field}, K={count}"
        );
    }
    let entries = explained.as_value()["trace"]["entries"].as_array().unwrap();
    assert!(!entries.is_empty());
    assert_eq!(entries.len(), (count * 8).min(MAX_TRACE_PAGE_ITEMS));
    assert_eq!(explained.as_value()["trace"]["total"], json!(count * 8));
    assert_eq!(
        explained.as_value()["trace"]["next_offset"],
        if count * 8 > MAX_TRACE_PAGE_ITEMS {
            json!(MAX_TRACE_PAGE_ITEMS)
        } else {
            serde_json::Value::Null
        },
        "the first page must report omitted trace entries truthfully"
    );

    // Even a target outside the automatic first page remains explainable by ID
    // without changing admission, scoring, or either provider request.
    let last = SkillId::new(format!("skill_{:02}", count - 1)).unwrap();
    let (targeted, targeted_requests) = run(true, Some(last.clone()));
    assert_eq!(plain_requests, targeted_requests);
    assert_eq!(plain.as_value()["skills"], targeted.as_value()["skills"]);
    let targeted_entries = targeted.as_value()["trace"]["entries"].as_array().unwrap();
    assert_eq!(targeted_entries.len(), 8);
    assert!(
        targeted_entries
            .iter()
            .all(|entry| entry["skill_id"] == json!(last))
    );
    eprintln!(
        "trace-capacity K={count} decision=ranked requests=2 explain_requests_unchanged=true entries={} targeted_entries=8",
        entries.len()
    );
}

#[test]
fn explanation_at_page_boundary_preserves_ranking() {
    check_capacity(16);
}
#[test]
fn explanation_over_page_boundary_preserves_ranking() {
    check_capacity(17);
}
#[test]
fn explanation_at_maximum_shortlist_preserves_ranking() {
    check_capacity(32);
}
