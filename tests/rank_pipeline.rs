//! Integration tests for two-stage rank pipeline, publication revalidation,
//! and CLI invocation boundary.
//!
//! Satisfies contract boundary `p4_two_stage_pipeline` (sr-roadmap-l1i.5.11).

use asupersync::Cx;
use serde_json::{Value, json};
use skillranker::config::ConfigSources;
use skillranker::context::source::SourceOptions;
use skillranker::effects::{EffectGate, Scope};
use skillranker::jev::OriginScopedCredential;
use skillranker::jev::client::TransportError;
use skillranker::jev::codec::{Request, Response};
use skillranker::limits::DurationMillis;
use skillranker::output::Decision;
use skillranker::pipeline::{JevTransport, RankArgs, execute_pipeline};
use skillranker::privacy::{EffectFlags, NetworkConsent};
use skillranker::roster::LocalPath;
use skillranker::runtime::{EntryClock, ProcessInvocation};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

static FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

type ResponseGenerator = Box<dyn Fn(&Request) -> Result<Response, TransportError> + Send + Sync>;

#[derive(Clone, Default)]
struct DynamicMockTransport {
    generators: Arc<Mutex<Vec<ResponseGenerator>>>,
    pub recorded_requests: Arc<Mutex<Vec<Request>>>,
}

impl DynamicMockTransport {
    fn new(generators: Vec<ResponseGenerator>) -> Self {
        Self {
            generators: Arc::new(Mutex::new(generators)),
            recorded_requests: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl JevTransport for DynamicMockTransport {
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
    let root = std::env::temp_dir().join(format!("sr-rank-test-{}-{}", std::process::id(), id));
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

#[test]
fn test_explicit_directive_bypasses_inference() {
    let (_root, workspace) = create_test_env();
    let skills_dir = workspace.join(".claude/skills");
    create_skill(&skills_dir, "rust_testing", "Runs cargo test suites", "Use cargo test.");
    create_skill(&skills_dir, "git_helper", "Git workflow automation", "Use git commands.");

    let ctx_file = create_context_file(&workspace, "Please use skill: rust_testing to run tests");

    let clock = test_clock();
    let invocation = ProcessInvocation::from_clock(clock).unwrap();
    let cx = invocation.request_cx().unwrap();

    let flags = EffectFlags {
        offline: true,
        allow_network: false,
        dry_run: false,
        no_cache: true,
        no_ledger: true,
        no_persist: true,
        save_case: false,
    };
    let gate = EffectGate::new(flags, Scope::Rank).unwrap();

    let mut source_options = SourceOptions::default();
    source_options.context = Some(LocalPath::new(ctx_file));

    let args = RankArgs {
        workspace: workspace.clone(),
        user_config_root: None,
        sources: ConfigSources::default(),
        gate,
        source_options,
        require_skills: vec![skillranker::identity::SkillId::new("rust_testing").unwrap()],
        roster_file: None,
        explain: false,
        why_not: None,
        output_json: true,
        output_table: false,
        dry_run: false,
    };

    let doc = invocation
        .runtime()
        .block_on(async { execute_pipeline(&clock, &cx, args, None).await })
        .expect("pipeline execution succeeded");

    assert_eq!(doc.kind(), skillranker::output::OutputKind::Decision(Decision::Explicit));
    assert_eq!(doc.exit_code(), skillranker::output::CliExit::Success);

    let val = doc.as_value();
    assert_eq!(val["decision"], "explicit");
    let skills = val["skills"].as_array().expect("skills array");
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0]["invocation_name"], "rust_testing");
    assert_eq!(val["usage"]["requests"], 0);
    assert_eq!(val["usage"]["http_attempts"], 0);
}

#[test]
fn test_ranked_flow_with_mock_jev() {
    let (_root, workspace) = create_test_env();
    let skills_dir = workspace.join(".claude/skills");
    create_skill(&skills_dir, "skill_a", "Skill Alpha description", "Skill Alpha body");
    create_skill(&skills_dir, "skill_b", "Skill Beta description", "Skill Beta body");

    let ctx_file = create_context_file(&workspace, "Help me refactor the pipeline");

    let clock = test_clock();
    let invocation = ProcessInvocation::from_clock(clock).unwrap();
    let cx = invocation.request_cx().unwrap();

    let flags = EffectFlags {
        offline: false,
        allow_network: true,
        dry_run: false,
        no_cache: true,
        no_ledger: true,
        no_persist: true,
        save_case: false,
    };
    let gate = EffectGate::new(flags, Scope::Rank).unwrap();

    let mut source_options = SourceOptions::default();
    source_options.context = Some(LocalPath::new(ctx_file));

    let mut sources = ConfigSources::default();
    sources
        .environment
        .push(("TYPESAFE_API_KEY".into(), "test-api-key-xyz".into()));

    // Generator 1: Wide response
    let wide_gen: ResponseGenerator = Box::new(|req: &Request| {
        let q_which = match &req.questions()["which"] {
            skillranker::jev::codec::Question::Choice { criteria, .. } => criteria,
            _ => panic!("expected choice for which"),
        };
        let mut probs = serde_json::Map::new();
        let n = q_which.len() as f64;
        for k in q_which.keys() {
            probs.insert(k.clone(), json!(1.0 / n));
        }
        let first_choice = q_which.keys().next().unwrap().clone();

        let mut phase_probs = serde_json::Map::new();
        for p in ["planning", "implementing", "debugging", "testing", "reviewing", "releasing", "conversing", "other"] {
            phase_probs.insert(p.into(), json!(0.125));
        }

        let resp_json = json!({
            "model": "jev-test",
            "answers": {
                "which": {
                    "type": "choice",
                    "choice": first_choice,
                    "probabilities": probs,
                    "confidence": 0.8
                },
                "gate::specialized_method": {"type": "noul", "noul": 0.85},
                "gate::material_help": {"type": "noul", "noul": 0.90},
                "gate::context_suffices": {"type": "noul", "noul": 0.10},
                "phase": {
                    "type": "choice",
                    "choice": "implementing",
                    "probabilities": phase_probs,
                    "confidence": 0.5
                }
            },
            "usage": {"input_tokens": 100, "output_tokens": 25}
        });
        req.decode_response(&serde_json::to_vec(&resp_json).unwrap())
            .map_err(wrap_codec_err)
    });

    // Generator 2: Rerank response
    let rerank_gen: ResponseGenerator = Box::new(|req: &Request| {
        let q_choice = match &req.questions()["rerank"] {
            skillranker::jev::codec::Question::Choice { criteria, .. } => criteria,
            _ => panic!("expected choice for rerank"),
        };
        let mut answers = serde_json::Map::new();
        let mut probs = serde_json::Map::new();
        let mut skill_keys = Vec::new();
        for k in q_choice.keys() {
            if k != "__none__" {
                skill_keys.push(k.clone());
            }
        }
        probs.insert("__none__".into(), json!(0.10));
        probs.insert(skill_keys[0].clone(), json!(0.60));
        if skill_keys.len() > 1 {
            probs.insert(skill_keys[1].clone(), json!(0.30));
        }
        answers.insert(
            "rerank".into(),
            json!({
                "type": "choice",
                "choice": skill_keys[0],
                "probabilities": probs,
                "confidence": 0.85
            }),
        );
        for k in &skill_keys {
            answers.insert(format!("fits::{k}"), json!({"type": "noul", "noul": 0.80}));
        }

        let resp_json = json!({
            "model": "jev-test",
            "answers": answers,
            "usage": {"input_tokens": 120, "output_tokens": 30}
        });
        req.decode_response(&serde_json::to_vec(&resp_json).unwrap())
            .map_err(wrap_codec_err)
    });

    let mock_transport = DynamicMockTransport::new(vec![wide_gen, rerank_gen]);

    let args = RankArgs {
        workspace: workspace.clone(),
        user_config_root: None,
        sources,
        gate,
        source_options,
        require_skills: Vec::new(),
        roster_file: None,
        explain: false,
        why_not: None,
        output_json: true,
        output_table: false,
        dry_run: false,
    };

    let doc = invocation
        .runtime()
        .block_on(async { execute_pipeline(&clock, &cx, args, Some(&mock_transport)).await })
        .expect("pipeline execution succeeded");

    assert_eq!(doc.kind(), skillranker::output::OutputKind::Decision(Decision::Ranked));
    assert_eq!(doc.exit_code(), skillranker::output::CliExit::Success);

    let val = doc.as_value();
    assert_eq!(val["decision"], "ranked");
    let skills = val["skills"].as_array().expect("skills array");
    assert!(!skills.is_empty());
    assert_eq!(skills[0]["rank"], 1);
    assert!(skills[0]["rank_score"].as_f64().unwrap() > 0.0);
    assert_eq!(val["usage"]["requests"], 2);
    assert_eq!(val["usage"]["http_attempts"], 2);
}

#[test]
fn test_low_need_abstention() {
    let (_root, workspace) = create_test_env();
    let skills_dir = workspace.join(".claude/skills");
    create_skill(&skills_dir, "skill_a", "Skill Alpha description", "Skill Alpha body");

    let ctx_file = create_context_file(&workspace, "Simple query not needing skills");

    let clock = test_clock();
    let invocation = ProcessInvocation::from_clock(clock).unwrap();
    let cx = invocation.request_cx().unwrap();

    let flags = EffectFlags {
        offline: false,
        allow_network: true,
        dry_run: false,
        no_cache: true,
        no_ledger: true,
        no_persist: true,
        save_case: false,
    };
    let gate = EffectGate::new(flags, Scope::Rank).unwrap();

    let mut source_options = SourceOptions::default();
    source_options.context = Some(LocalPath::new(ctx_file));

    let mut sources = ConfigSources::default();
    sources
        .environment
        .push(("TYPESAFE_API_KEY".into(), "test-api-key-xyz".into()));

    // Wide gate produces mean(0.1, 0.1, 1 - 0.9) = 0.1 < 0.30 -> LowNeed!
    let wide_gen: ResponseGenerator = Box::new(|req: &Request| {
        let q_which = match &req.questions()["which"] {
            skillranker::jev::codec::Question::Choice { criteria, .. } => criteria,
            _ => panic!("expected choice for which"),
        };
        let mut probs = serde_json::Map::new();
        let n = q_which.len() as f64;
        for k in q_which.keys() {
            probs.insert(k.clone(), json!(1.0 / n));
        }
        let first_choice = q_which.keys().next().unwrap().clone();

        let mut phase_probs = serde_json::Map::new();
        for p in ["planning", "implementing", "debugging", "testing", "reviewing", "releasing", "conversing", "other"] {
            phase_probs.insert(p.into(), json!(0.125));
        }

        let resp_json = json!({
            "model": "jev-test",
            "answers": {
                "which": {
                    "type": "choice",
                    "choice": first_choice,
                    "probabilities": probs,
                    "confidence": 0.5
                },
                "gate::specialized_method": {"type": "noul", "noul": 0.10},
                "gate::material_help": {"type": "noul", "noul": 0.10},
                "gate::context_suffices": {"type": "noul", "noul": 0.90},
                "phase": {
                    "type": "choice",
                    "choice": "conversing",
                    "probabilities": phase_probs,
                    "confidence": 0.5
                }
            },
            "usage": {"input_tokens": 50, "output_tokens": 15}
        });
        req.decode_response(&serde_json::to_vec(&resp_json).unwrap())
            .map_err(wrap_codec_err)
    });

    let mock_transport = DynamicMockTransport::new(vec![wide_gen]);

    let args = RankArgs {
        workspace: workspace.clone(),
        user_config_root: None,
        sources,
        gate,
        source_options,
        require_skills: Vec::new(),
        roster_file: None,
        explain: false,
        why_not: None,
        output_json: true,
        output_table: false,
        dry_run: false,
    };

    let doc = invocation
        .runtime()
        .block_on(async { execute_pipeline(&clock, &cx, args, Some(&mock_transport)).await })
        .expect("pipeline execution succeeded");

    assert_eq!(doc.kind(), skillranker::output::OutputKind::Decision(Decision::Abstain));
    assert_eq!(doc.exit_code(), skillranker::output::CliExit::Success);

    let val = doc.as_value();
    assert_eq!(val["decision"], "abstain");
    assert_eq!(val["reason"], "low-need");
    // Only 1 wide request, 0 rerank requests
    assert_eq!(mock_transport.recorded_requests.lock().unwrap().len(), 1);
}

#[test]
fn test_dry_run_preview_without_network() {
    let (_root, workspace) = create_test_env();
    let skills_dir = workspace.join(".claude/skills");
    create_skill(&skills_dir, "skill_a", "Skill Alpha description", "Skill Alpha body");

    let ctx_file = create_context_file(&workspace, "Perform dry-run preview");

    let clock = test_clock();
    let invocation = ProcessInvocation::from_clock(clock).unwrap();
    let cx = invocation.request_cx().unwrap();

    let flags = EffectFlags {
        offline: false,
        allow_network: false,
        dry_run: true,
        no_cache: true,
        no_ledger: true,
        no_persist: true,
        save_case: false,
    };
    let gate = EffectGate::new(flags, Scope::Rank).unwrap();

    let mut source_options = SourceOptions::default();
    source_options.context = Some(LocalPath::new(ctx_file));

    let args = RankArgs {
        workspace: workspace.clone(),
        user_config_root: None,
        sources: ConfigSources::default(),
        gate,
        source_options,
        require_skills: Vec::new(),
        roster_file: None,
        explain: false,
        why_not: None,
        output_json: true,
        output_table: false,
        dry_run: true,
    };

    let doc = invocation
        .runtime()
        .block_on(async { execute_pipeline(&clock, &cx, args, None).await })
        .expect("pipeline execution succeeded");

    assert_eq!(doc.kind(), skillranker::output::OutputKind::Decision(Decision::Abstain));
    assert_eq!(doc.exit_code(), skillranker::output::CliExit::Success);

    let val = doc.as_value();
    assert_eq!(val["decision"], "abstain");
    assert_eq!(val["reason"], "dry-run-preview");
    assert!(val.get("dry_run").is_some());
    assert_eq!(val["dry_run"]["dry_run"], true);
    assert_eq!(val["dry_run"]["stage"], "wide");
}

#[test]
fn test_cli_bare_sr_and_rank_flags() {
    let (_root, workspace) = create_test_env();
    let skills_dir = workspace.join(".claude/skills");
    create_skill(&skills_dir, "skill_a", "Skill Alpha description", "Skill Alpha body");
    let ctx_file = create_context_file(&workspace, "Use skill: skill_a");

    // Test 1: CLI dry run with explicit context
    let output = Command::new(env!("CARGO_BIN_EXE_sr"))
        .current_dir(&workspace)
        .args(&["rank", "--context", ctx_file.to_str().unwrap(), "--require-skill", "skill_a", "--dry-run", "--json"])
        .output()
        .expect("execute sr binary");

    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}, stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let val: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    // Explicit directive takes precedence even in dry-run
    assert_eq!(val["decision"], "explicit");

    // Test 2: Conflict detection (e.g. --offline with --allow-network)
    let output_conflict = Command::new(env!("CARGO_BIN_EXE_sr"))
        .current_dir(&workspace)
        .args(&["rank", "--offline", "--allow-network"])
        .output()
        .expect("execute sr binary");

    assert_eq!(output_conflict.status.code(), Some(2));
    let err_val: Value = serde_json::from_slice(&output_conflict.stdout).expect("valid error JSON");
    assert_eq!(err_val["error"]["kind"], "invalid-usage");
}

#[test]
fn test_low_fit_abstention() {
    let (_root, workspace) = create_test_env();
    let skills_dir = workspace.join(".claude/skills");
    create_skill(&skills_dir, "skill_a", "Skill Alpha description", "Skill Alpha body");

    let ctx_file = create_context_file(&workspace, "Help me refactor the pipeline");

    let clock = test_clock();
    let invocation = ProcessInvocation::from_clock(clock).unwrap();
    let cx = invocation.request_cx().unwrap();

    let flags = EffectFlags {
        offline: false,
        allow_network: true,
        dry_run: false,
        no_cache: true,
        no_ledger: true,
        no_persist: true,
        save_case: false,
    };
    let gate = EffectGate::new(flags, Scope::Rank).unwrap();

    let mut source_options = SourceOptions::default();
    source_options.context = Some(LocalPath::new(ctx_file));

    let mut sources = ConfigSources::default();
    sources
        .environment
        .push(("TYPESAFE_API_KEY".into(), "test-api-key-xyz".into()));

    // Generator 1: Wide response (passes gate)
    let wide_gen: ResponseGenerator = Box::new(|req: &Request| {
        let q_which = match &req.questions()["which"] {
            skillranker::jev::codec::Question::Choice { criteria, .. } => criteria,
            _ => panic!("expected choice for which"),
        };
        let mut probs = serde_json::Map::new();
        let n = q_which.len() as f64;
        for k in q_which.keys() {
            probs.insert(k.clone(), json!(1.0 / n));
        }
        let first_choice = q_which.keys().next().unwrap().clone();

        let mut phase_probs = serde_json::Map::new();
        for p in ["planning", "implementing", "debugging", "testing", "reviewing", "releasing", "conversing", "other"] {
            phase_probs.insert(p.into(), json!(0.125));
        }

        let resp_json = json!({
            "model": "jev-test",
            "answers": {
                "which": {
                    "type": "choice",
                    "choice": first_choice,
                    "probabilities": probs,
                    "confidence": 0.8
                },
                "gate::specialized_method": {"type": "noul", "noul": 0.85},
                "gate::material_help": {"type": "noul", "noul": 0.90},
                "gate::context_suffices": {"type": "noul", "noul": 0.10},
                "phase": {
                    "type": "choice",
                    "choice": "implementing",
                    "probabilities": phase_probs,
                    "confidence": 0.5
                }
            },
            "usage": {"input_tokens": 100, "output_tokens": 25}
        });
        req.decode_response(&serde_json::to_vec(&resp_json).unwrap())
            .map_err(wrap_codec_err)
    });

    // Generator 2: Rerank response with fits < 0.30
    let rerank_gen: ResponseGenerator = Box::new(|req: &Request| {
        let q_choice = match &req.questions()["rerank"] {
            skillranker::jev::codec::Question::Choice { criteria, .. } => criteria,
            _ => panic!("expected choice for rerank"),
        };
        let mut answers = serde_json::Map::new();
        let mut probs = serde_json::Map::new();
        let mut skill_keys = Vec::new();
        for k in q_choice.keys() {
            if k != "__none__" {
                skill_keys.push(k.clone());
            }
        }
        probs.insert("__none__".into(), json!(0.10));
        probs.insert(skill_keys[0].clone(), json!(0.90));
        answers.insert(
            "rerank".into(),
            json!({
                "type": "choice",
                "choice": skill_keys[0],
                "probabilities": probs,
                "confidence": 0.85
            }),
        );
        for k in &skill_keys {
            // Fit 0.20 is below default threshold 0.30!
            answers.insert(format!("fits::{k}"), json!({"type": "noul", "noul": 0.20}));
        }

        let resp_json = json!({
            "model": "jev-test",
            "answers": answers,
            "usage": {"input_tokens": 120, "output_tokens": 30}
        });
        req.decode_response(&serde_json::to_vec(&resp_json).unwrap())
            .map_err(wrap_codec_err)
    });

    let mock_transport = DynamicMockTransport::new(vec![wide_gen, rerank_gen]);

    let args = RankArgs {
        workspace: workspace.clone(),
        user_config_root: None,
        sources,
        gate,
        source_options,
        require_skills: Vec::new(),
        roster_file: None,
        explain: false,
        why_not: None,
        output_json: true,
        output_table: false,
        dry_run: false,
    };

    let doc = invocation
        .runtime()
        .block_on(async { execute_pipeline(&clock, &cx, args, Some(&mock_transport)).await })
        .expect("pipeline execution succeeded");

    assert_eq!(doc.kind(), skillranker::output::OutputKind::Decision(Decision::Abstain));
    assert_eq!(doc.exit_code(), skillranker::output::CliExit::Success);

    let val = doc.as_value();
    assert_eq!(val["decision"], "abstain");
    assert_eq!(val["reason"], "low-fit");
}

#[test]
fn test_none_winner_abstention() {
    let (_root, workspace) = create_test_env();
    let skills_dir = workspace.join(".claude/skills");
    create_skill(&skills_dir, "skill_a", "Skill Alpha description", "Skill Alpha body");

    let ctx_file = create_context_file(&workspace, "Help me refactor the pipeline");

    let clock = test_clock();
    let invocation = ProcessInvocation::from_clock(clock).unwrap();
    let cx = invocation.request_cx().unwrap();

    let flags = EffectFlags {
        offline: false,
        allow_network: true,
        dry_run: false,
        no_cache: true,
        no_ledger: true,
        no_persist: true,
        save_case: false,
    };
    let gate = EffectGate::new(flags, Scope::Rank).unwrap();

    let mut source_options = SourceOptions::default();
    source_options.context = Some(LocalPath::new(ctx_file));

    let mut sources = ConfigSources::default();
    sources
        .environment
        .push(("TYPESAFE_API_KEY".into(), "test-api-key-xyz".into()));

    // Generator 1: Wide response (passes gate)
    let wide_gen: ResponseGenerator = Box::new(|req: &Request| {
        let q_which = match &req.questions()["which"] {
            skillranker::jev::codec::Question::Choice { criteria, .. } => criteria,
            _ => panic!("expected choice for which"),
        };
        let mut probs = serde_json::Map::new();
        let n = q_which.len() as f64;
        for k in q_which.keys() {
            probs.insert(k.clone(), json!(1.0 / n));
        }
        let first_choice = q_which.keys().next().unwrap().clone();

        let mut phase_probs = serde_json::Map::new();
        for p in ["planning", "implementing", "debugging", "testing", "reviewing", "releasing", "conversing", "other"] {
            phase_probs.insert(p.into(), json!(0.125));
        }

        let resp_json = json!({
            "model": "jev-test",
            "answers": {
                "which": {
                    "type": "choice",
                    "choice": first_choice,
                    "probabilities": probs,
                    "confidence": 0.8
                },
                "gate::specialized_method": {"type": "noul", "noul": 0.85},
                "gate::material_help": {"type": "noul", "noul": 0.90},
                "gate::context_suffices": {"type": "noul", "noul": 0.10},
                "phase": {
                    "type": "choice",
                    "choice": "implementing",
                    "probabilities": phase_probs,
                    "confidence": 0.5
                }
            },
            "usage": {"input_tokens": 100, "output_tokens": 25}
        });
        req.decode_response(&serde_json::to_vec(&resp_json).unwrap())
            .map_err(wrap_codec_err)
    });

    // Generator 2: Rerank response where __none__ wins!
    let rerank_gen: ResponseGenerator = Box::new(|req: &Request| {
        let q_choice = match &req.questions()["rerank"] {
            skillranker::jev::codec::Question::Choice { criteria, .. } => criteria,
            _ => panic!("expected choice for rerank"),
        };
        let mut answers = serde_json::Map::new();
        let mut probs = serde_json::Map::new();
        let mut skill_keys = Vec::new();
        for k in q_choice.keys() {
            if k != "__none__" {
                skill_keys.push(k.clone());
            }
        }
        probs.insert("__none__".into(), json!(0.80));
        probs.insert(skill_keys[0].clone(), json!(0.20));
        answers.insert(
            "rerank".into(),
            json!({
                "type": "choice",
                "choice": "__none__",
                "probabilities": probs,
                "confidence": 0.85
            }),
        );
        for k in &skill_keys {
            answers.insert(format!("fits::{k}"), json!({"type": "noul", "noul": 0.80}));
        }

        let resp_json = json!({
            "model": "jev-test",
            "answers": answers,
            "usage": {"input_tokens": 120, "output_tokens": 30}
        });
        req.decode_response(&serde_json::to_vec(&resp_json).unwrap())
            .map_err(wrap_codec_err)
    });

    let mock_transport = DynamicMockTransport::new(vec![wide_gen, rerank_gen]);

    let args = RankArgs {
        workspace: workspace.clone(),
        user_config_root: None,
        sources,
        gate,
        source_options,
        require_skills: Vec::new(),
        roster_file: None,
        explain: false,
        why_not: None,
        output_json: true,
        output_table: false,
        dry_run: false,
    };

    let doc = invocation
        .runtime()
        .block_on(async { execute_pipeline(&clock, &cx, args, Some(&mock_transport)).await })
        .expect("pipeline execution succeeded");

    assert_eq!(doc.kind(), skillranker::output::OutputKind::Decision(Decision::Abstain));
    assert_eq!(doc.exit_code(), skillranker::output::CliExit::Success);

    let val = doc.as_value();
    assert_eq!(val["decision"], "abstain");
    assert_eq!(val["reason"], "no-shortlist-match");
}

