//! Integration tests for two-stage rank pipeline, publication revalidation,
//! and CLI invocation boundary.
//!
//! Satisfies contract boundary `p4_two_stage_pipeline` (sr-roadmap-l1i.5.11).

mod support;

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
    create_skill(
        &skills_dir,
        "rust_testing",
        "Runs cargo test suites",
        "Use cargo test.",
    );
    create_skill(
        &skills_dir,
        "git_helper",
        "Git workflow automation",
        "Use git commands.",
    );

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

    let source_options = SourceOptions {
        context: Some(LocalPath::new(ctx_file)),
        ..Default::default()
    };

    let args = RankArgs {
        workspace: workspace.clone(),
        user_config_root: None,
        home: None,
        cache_dir: Some(support::private_store_dir("cache")),
        sources: ConfigSources::default(),
        gate,
        source_options,
        require_skills: vec![skillranker::identity::SkillId::new("rust_testing").unwrap()],
        shortlist_ids: Vec::new(),
        roster_file: None,
        explain: false,
        why_not: None,
        cursor: None,
        output_json: true,
        output_table: false,
        dry_run: false,
        save_case: None,
        ledger_dir: Some(support::private_store_dir("ledger")),
    };

    let doc = invocation
        .runtime()
        .block_on(async { execute_pipeline(&invocation, &cx, args, None).await })
        .expect("pipeline execution succeeded");

    assert_eq!(
        doc.kind(),
        skillranker::output::OutputKind::Decision(Decision::Explicit)
    );
    assert_eq!(doc.exit_code(), skillranker::output::CliExit::Success);

    let val = doc.as_value();
    assert_eq!(val["decision"], "explicit");
    let skills = val["skills"].as_array().expect("skills array");
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0]["invocation_name"], "rust_testing");
    assert_eq!(val["usage"]["requests"], 0);
    assert_eq!(val["usage"]["http_attempts"], 0);
    assert!(
        !val["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["kind"] == "ledger-finalization-unconfirmed"),
        "disabled ledger recording is not a failed write"
    );
}

#[test]
fn test_ranked_flow_with_mock_jev() {
    let (_root, workspace) = create_test_env();
    let skills_dir = workspace.join(".claude/skills");
    create_skill(
        &skills_dir,
        "skill_a",
        "Skill Alpha description",
        "Skill Alpha body",
    );
    create_skill(
        &skills_dir,
        "skill_b",
        "Skill Beta description",
        "Skill Beta body",
    );

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

    let source_options = SourceOptions {
        context: Some(LocalPath::new(ctx_file)),
        ..Default::default()
    };

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
        for p in [
            "planning",
            "implementing",
            "debugging",
            "testing",
            "reviewing",
            "releasing",
            "conversing",
            "other",
        ] {
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
        home: None,
        cache_dir: Some(support::private_store_dir("cache")),
        sources,
        gate,
        source_options,
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
        ledger_dir: Some(support::private_store_dir("ledger")),
    };

    let doc = invocation
        .runtime()
        .block_on(async { execute_pipeline(&invocation, &cx, args, Some(&mock_transport)).await })
        .expect("pipeline execution succeeded");

    assert_eq!(
        doc.kind(),
        skillranker::output::OutputKind::Decision(Decision::Ranked)
    );
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
    create_skill(
        &skills_dir,
        "skill_a",
        "Skill Alpha description",
        "Skill Alpha body",
    );

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

    let source_options = SourceOptions {
        context: Some(LocalPath::new(ctx_file)),
        ..Default::default()
    };

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
        for p in [
            "planning",
            "implementing",
            "debugging",
            "testing",
            "reviewing",
            "releasing",
            "conversing",
            "other",
        ] {
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
        home: None,
        cache_dir: Some(support::private_store_dir("cache")),
        sources,
        gate,
        source_options,
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
        ledger_dir: Some(support::private_store_dir("ledger")),
    };

    let doc = invocation
        .runtime()
        .block_on(async { execute_pipeline(&invocation, &cx, args, Some(&mock_transport)).await })
        .expect("pipeline execution succeeded");

    assert_eq!(
        doc.kind(),
        skillranker::output::OutputKind::Decision(Decision::Abstain)
    );
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
    create_skill(
        &skills_dir,
        "skill_a",
        "Skill Alpha description",
        "Skill Alpha body",
    );

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

    let source_options = SourceOptions {
        context: Some(LocalPath::new(ctx_file)),
        ..Default::default()
    };

    let args = RankArgs {
        workspace: workspace.clone(),
        user_config_root: None,
        home: None,
        cache_dir: Some(support::private_store_dir("cache")),
        sources: ConfigSources::default(),
        gate,
        source_options,
        require_skills: Vec::new(),
        shortlist_ids: Vec::new(),
        roster_file: None,
        explain: false,
        why_not: None,
        cursor: None,
        output_json: true,
        output_table: false,
        dry_run: true,
        save_case: None,
        ledger_dir: Some(support::private_store_dir("ledger")),
    };

    let doc = invocation
        .runtime()
        .block_on(async { execute_pipeline(&invocation, &cx, args, None).await })
        .expect("pipeline execution succeeded");

    assert_eq!(
        doc.kind(),
        skillranker::output::OutputKind::Artifact(skillranker::output::ArtifactKind::Preview)
    );
    assert_eq!(doc.exit_code(), skillranker::output::CliExit::Success);

    // A stateless preview: never an actionable decision, nothing sent.
    let val = doc.as_value();
    assert_eq!(val["actionable"], false);
    assert!(val.get("decision").is_none());
    assert!(val["local_decision"].is_null());
    assert_eq!(val["provider_request"]["stages"][0]["stage"], "wide");
}

#[test]
fn real_cli_previews_final_selected_wire_for_both_stages_without_effects() {
    use std::process::Stdio;
    let (root, workspace) = create_test_env();
    create_skill(
        &workspace.join(".claude/skills"),
        "skill_a",
        "Synthetic é界 guidance",
        "Synthetic procedure.",
    );
    let context = create_context_file(&workspace, "Plan a synthetic é界 exercise");
    let run = |provider: &str, shortlist: Option<&str>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sr"));
        command
            .current_dir(&workspace)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("SR_PROVIDER", provider)
            .env("CLOUDFLARE_ACCOUNT_ID", "0123456789abcdef0123456789abcdef")
            .stdin(Stdio::null())
            .args([
                "rank",
                "--context",
                context.to_str().unwrap(),
                "--dry-run",
                "--no-persist",
                "--json",
                "--timeout-ms",
                "20000",
            ]);
        if let Some(shortlist) = shortlist {
            command.args(["--shortlist-ids", shortlist]);
        }
        let output = command.output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let roster = Command::new(env!("CARGO_BIN_EXE_sr"))
        .current_dir(&workspace)
        .env_clear()
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .stdin(Stdio::null())
        .args(["roster", "--json"])
        .output()
        .unwrap();
    assert_eq!(
        roster.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&roster.stderr)
    );
    let roster: Value = serde_json::from_slice(&roster.stdout).unwrap();
    let id = roster["records"][0]["skill_id"].as_str().unwrap();
    let native = run("cloudflare", Some(id));
    let typesafe = run("typesafe", Some(id));
    for preview in [&native, &typesafe] {
        assert_eq!(preview["kind"], "preview");
        assert_eq!(preview["actionable"], false);
        assert_eq!(preview["stateless"], true);
        assert_eq!(
            preview["provider_request"]["stages"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert!(
            !serde_json::to_string(preview)
                .unwrap()
                .contains("0123456789abcdef0123456789abcdef")
        );
        for effect in ["unverified_children", "cross_process_coordination"] {
            assert_eq!(
                preview["effects"][effect], false,
                "{effect}: {}",
                preview["effects"]
            );
        }
        assert_eq!(preview["effects"]["network"]["state"], "blocked");
        for effect in ["response_cache", "ledger", "runtime_state"] {
            assert_eq!(preview["effects"][effect]["state"], "disabled");
        }
    }
    for index in 0..2 {
        let native_stage = &native["provider_request"]["stages"][index];
        let typesafe_stage = &typesafe["provider_request"]["stages"][index];
        let native_text = native_stage["request"].as_str().unwrap();
        let typesafe_text = typesafe_stage["request"].as_str().unwrap();
        assert_eq!(native_stage["request_bytes"], native_text.len());
        assert!(native_text.len() > native_text.chars().count());
        assert_eq!(typesafe_stage["request_bytes"], typesafe_text.len());
        let native_wire: Value = serde_json::from_str(native_text).unwrap();
        let mut logical: Value = serde_json::from_str(typesafe_text).unwrap();
        logical.as_object_mut().unwrap().remove("model");
        assert_eq!(
            native_wire,
            json!({"model":"typesafe/jev", "input":logical})
        );
    }
    for state in ["data", "cache"] {
        assert!(!root.join(state).exists(), "dry-run created {state}");
    }
}

#[test]
fn test_cli_bare_sr_and_rank_flags() {
    let (_root, workspace) = create_test_env();
    let skills_dir = workspace.join(".claude/skills");
    create_skill(
        &skills_dir,
        "skill_a",
        "Skill Alpha description",
        "Skill Alpha body",
    );
    let ctx_file = create_context_file(&workspace, "Use skill: skill_a");

    // Test 1: CLI dry run with explicit context
    let output = Command::new(env!("CARGO_BIN_EXE_sr"))
        .current_dir(&workspace)
        .env("HOME", _root.join("home"))
        .env("XDG_CONFIG_HOME", _root.join("config"))
        .args([
            "rank",
            "--context",
            ctx_file.to_str().unwrap(),
            "--require-skill",
            "skill_a",
            "--dry-run",
            "--json",
        ])
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
    // Explicit directive takes precedence even in dry-run, reported inside the
    // preview with no provider request.
    assert_eq!(val["kind"], "preview");
    assert_eq!(val["local_decision"]["decision"], "explicit");
    assert!(val["provider_request"].is_null());

    // Test 2: Conflict detection (e.g. --offline with --allow-network)
    let output_conflict = Command::new(env!("CARGO_BIN_EXE_sr"))
        .current_dir(&workspace)
        .env("HOME", _root.join("home"))
        .env("XDG_CONFIG_HOME", _root.join("config"))
        .args(["rank", "--offline", "--allow-network"])
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
    create_skill(
        &skills_dir,
        "skill_a",
        "Skill Alpha description",
        "Skill Alpha body",
    );

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

    let source_options = SourceOptions {
        context: Some(LocalPath::new(ctx_file)),
        ..Default::default()
    };

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
        for p in [
            "planning",
            "implementing",
            "debugging",
            "testing",
            "reviewing",
            "releasing",
            "conversing",
            "other",
        ] {
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
        home: None,
        cache_dir: Some(support::private_store_dir("cache")),
        sources,
        gate,
        source_options,
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
        ledger_dir: Some(support::private_store_dir("ledger")),
    };

    let doc = invocation
        .runtime()
        .block_on(async { execute_pipeline(&invocation, &cx, args, Some(&mock_transport)).await })
        .expect("pipeline execution succeeded");

    assert_eq!(
        doc.kind(),
        skillranker::output::OutputKind::Decision(Decision::Abstain)
    );
    assert_eq!(doc.exit_code(), skillranker::output::CliExit::Success);

    let val = doc.as_value();
    assert_eq!(val["decision"], "abstain");
    assert_eq!(val["reason"], "low-fit");
}

#[test]
fn test_none_winner_abstention() {
    let (_root, workspace) = create_test_env();
    let skills_dir = workspace.join(".claude/skills");
    create_skill(
        &skills_dir,
        "skill_a",
        "Skill Alpha description",
        "Skill Alpha body",
    );

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

    let source_options = SourceOptions {
        context: Some(LocalPath::new(ctx_file)),
        ..Default::default()
    };

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
        for p in [
            "planning",
            "implementing",
            "debugging",
            "testing",
            "reviewing",
            "releasing",
            "conversing",
            "other",
        ] {
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
        home: None,
        cache_dir: Some(support::private_store_dir("cache")),
        sources,
        gate,
        source_options,
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
        ledger_dir: Some(support::private_store_dir("ledger")),
    };

    let doc = invocation
        .runtime()
        .block_on(async { execute_pipeline(&invocation, &cx, args, Some(&mock_transport)).await })
        .expect("pipeline execution succeeded");

    assert_eq!(
        doc.kind(),
        skillranker::output::OutputKind::Decision(Decision::Abstain)
    );
    assert_eq!(doc.exit_code(), skillranker::output::CliExit::Success);

    let val = doc.as_value();
    assert_eq!(val["decision"], "abstain");
    assert_eq!(val["reason"], "no-shortlist-match");
}

#[test]
fn an_evaluation_send_whose_request_changed_since_its_preview_is_withheld() {
    // Not the digest of this request's wide stage.
    assert_withheld_for(skillranker::pipeline::WidePreview::Digest([0u8; 32]));
}

#[test]
fn an_evaluation_send_whose_preview_sent_nothing_is_withheld() {
    // The preview ended locally; a live run that now reaches the provider
    // would disclose a request nobody previewed.
    assert_withheld_for(skillranker::pipeline::WidePreview::NoRequest);
}

fn assert_withheld_for(preview: skillranker::pipeline::WidePreview) {
    let (_root, workspace) = create_test_env();
    let skills_dir = workspace.join(".claude/skills");
    create_skill(
        &skills_dir,
        "skill_a",
        "Skill Alpha description",
        "Alpha body",
    );
    create_skill(
        &skills_dir,
        "skill_b",
        "Skill Beta description",
        "Beta body",
    );
    let context_file = create_context_file(&workspace, "Help me refactor the pipeline");
    let context = fs::read(&context_file).unwrap();

    let invocation = ProcessInvocation::from_clock(test_clock()).unwrap();
    let cx = invocation.request_cx().unwrap();
    let gate = EffectGate::new(
        EffectFlags {
            allow_network: true,
            no_persist: true,
            ..Default::default()
        },
        Scope::Rank,
    )
    .unwrap();
    let mut sources = ConfigSources::default();
    sources
        .environment
        .push(("TYPESAFE_API_KEY".into(), "test-api-key-xyz".into()));
    let args = RankArgs {
        workspace: workspace.clone(),
        user_config_root: None,
        home: None,
        cache_dir: None,
        sources,
        gate,
        source_options: SourceOptions {
            context: Some(LocalPath::new(PathBuf::from("eval-case:changed"))),
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
        ledger_dir: None,
    };
    // No response is queued: any send would panic the mock transport.
    let transport = DynamicMockTransport::new(Vec::new());
    let mut evidence = skillranker::pipeline::StageEvidence::default();
    let outcome =
        invocation
            .runtime()
            .block_on(skillranker::pipeline::execute_pipeline_with_context(
                &invocation,
                &cx,
                args,
                Some(&transport),
                context,
                &mut evidence,
                Some(preview),
            ));
    let kind = match &outcome {
        Err((_, kind, _)) => (*kind).to_owned(),
        Ok(doc) => doc.as_value()["error"]["kind"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    };
    let detail = match &outcome {
        Err(failure) => format!("{failure:?}"),
        Ok(doc) => doc.as_value().to_string(),
    };
    assert_eq!(kind, "superseded", "{detail}");
    assert!(transport.recorded_requests.lock().unwrap().is_empty());
    // Marked as a preview refusal, so the batch neither charges attempts nor
    // counts an operational failure.
    assert!(evidence.preview_refused, "{detail}");
    if let Ok(doc) = &outcome {
        assert_eq!(doc.as_value()["usage"]["http_attempts"], 0, "{detail}");
    }
    assert!(invocation.shutdown());
}

fn overrun_failure_finalization_fixture(
    hold_ledger_lock: bool,
    past_deadline_ms: u64,
) -> (Value, Vec<String>, u64) {
    // sr-9fzp: the wide send blocks, uncooperatively, past the whole deadline.
    // The run must still finalize its ledger row as a failure instead of
    // leaving it in-flight.
    let (_root, workspace) = create_test_env();
    let skills_dir = workspace.join(".claude/skills");
    create_skill(
        &skills_dir,
        "skill_a",
        "Skill Alpha description",
        "Alpha body",
    );
    create_skill(
        &skills_dir,
        "skill_b",
        "Skill Beta description",
        "Beta body",
    );
    let ctx_file = create_context_file(&workspace, "Help me refactor the pipeline");
    let ledger = support::private_store_dir("overrun-ledger");

    let setup_clock = test_clock();
    let setup = ProcessInvocation::from_clock(setup_clock).unwrap();
    let setup_cx = setup.request_cx().unwrap();
    skillranker::storage::init_ledger(
        &setup,
        &setup_cx,
        skillranker::storage::LedgerLocation::Directory(ledger.clone()),
    )
    .unwrap();

    let clock = EntryClock::capture_with(
        DurationMillis::new("test", 3_000, 30_000).unwrap(),
        DurationMillis::new("cleanup", 200, 30_000).unwrap(),
    )
    .unwrap();
    let invocation = ProcessInvocation::from_clock(clock).unwrap();
    let cx = invocation.request_cx().unwrap();
    let gate = EffectGate::new(
        EffectFlags {
            allow_network: true,
            no_cache: true,
            ..Default::default()
        },
        Scope::Rank,
    )
    .unwrap();
    let mut sources = ConfigSources::default();
    sources
        .environment
        .push(("TYPESAFE_API_KEY".into(), "test-api-key-xyz".into()));
    let held_connection = Arc::new(Mutex::new(None));
    let hold = held_connection.clone();
    let locked_ledger = ledger.clone();
    let stall: ResponseGenerator = Box::new(move |_| {
        if hold_ledger_lock {
            let connection =
                rusqlite::Connection::open(locked_ledger.join(skillranker::storage::LEDGER_FILE))
                    .unwrap();
            connection.execute_batch("BEGIN IMMEDIATE").unwrap();
            *hold.lock().unwrap() = Some(connection);
        }
        // The overrun is relative to process entry, matching the production
        // deadline. Sleeping 3,050ms from this send adds variable startup time
        // and can accidentally move the ordinary case beyond the bounded
        // failure-recording window. Test that later boundary separately.
        let wake_at_ms = clock.deadline().expires_at().as_millis() + past_deadline_ms;
        std::thread::sleep(std::time::Duration::from_millis(
            wake_at_ms.saturating_sub(clock.now().as_millis()),
        ));
        Err(TransportError {
            kind: skillranker::jev::client::TransportErrorKind::Deadline,
            http_attempt_started: true,
            retry_after: skillranker::jev::retry::RetryAfter::Absent,
        })
    });
    let transport = DynamicMockTransport::new(vec![stall]);
    let args = RankArgs {
        workspace: workspace.clone(),
        user_config_root: None,
        home: None,
        cache_dir: None,
        sources,
        gate,
        source_options: SourceOptions {
            context: Some(LocalPath::new(ctx_file)),
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
        ledger_dir: Some(ledger.clone()),
    };
    let outcome = invocation
        .runtime()
        .block_on(async { execute_pipeline(&invocation, &cx, args, Some(&transport)).await });
    assert!(
        clock.now().as_millis() > 3_000,
        "the run did not overrun its deadline"
    );
    let elapsed_ms = clock.now().as_millis();
    let doc = outcome
        .expect("admitted timeout retains its unavailable decision")
        .as_value()
        .clone();
    // Release the planted lock only after production finalization has returned.
    drop(held_connection.lock().unwrap().take());
    let db = rusqlite::Connection::open(ledger.join(skillranker::storage::LEDGER_FILE)).unwrap();
    let reasons: Vec<String> = db
        .prepare("SELECT reason FROM ranking_events")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(doc["decision"], "unavailable");
    assert_eq!(doc["error"]["kind"], "timeout");
    assert_eq!(doc["usage"]["http_attempts"], 1);
    assert_eq!(doc["usage"]["unknown_usage_attempts"], 1);
    (doc, reasons, elapsed_ms)
}

#[test]
fn a_run_that_overruns_its_deadline_records_its_failure_not_in_flight() {
    let (doc, reasons, elapsed_ms) = overrun_failure_finalization_fixture(false, 50);
    assert_eq!(reasons.len(), 1, "{reasons:?} after {elapsed_ms} ms");
    assert_eq!(
        reasons[0], "timeout",
        "an overrun run must record its timeout, not stay in-flight; elapsed={elapsed_ms}ms, document={doc}"
    );
    assert!(
        !doc["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["kind"] == "ledger-finalization-unconfirmed")
    );
}

#[test]
fn an_unconfirmed_failure_ledger_write_preserves_timeout_and_unknown_usage() {
    let (doc, reasons, elapsed_ms) = overrun_failure_finalization_fixture(true, 50);
    assert_eq!(reasons, ["in-flight"], "locked write after {elapsed_ms} ms");
    let warning = doc["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["kind"] == "ledger-finalization-unconfirmed")
        .expect("a real busy SQLite finalization must be visible");
    assert_eq!(warning["count"], 1);
}

#[test]
fn a_run_past_the_failure_recording_window_preserves_unknown_ledger_outcome() {
    let (doc, reasons, elapsed_ms) = overrun_failure_finalization_fixture(false, 650);
    assert!(elapsed_ms >= 3_650);
    assert_eq!(reasons, ["in-flight"]);
    let warnings: Vec<_> = doc["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|w| w["kind"] == "ledger-finalization-unconfirmed")
        .collect();
    assert_eq!(warnings.len(), 1, "{doc}");
    assert_eq!(warnings[0]["count"], 1);
}

fn successful_ledger_finalization_fixture(
    low_gate: bool,
    hold_lock: bool,
    initialize_ledger: bool,
) -> (Value, Option<String>) {
    let (_root, workspace) = create_test_env();
    for name in ["skill_a", "skill_b"] {
        create_skill(
            &workspace.join(".claude/skills"),
            name,
            "Refactor Rust",
            "Review Rust code.",
        );
    }
    let context = create_context_file(&workspace, "Help me refactor the pipeline");
    let ledger = support::private_store_dir("successful-finalization");
    if initialize_ledger {
        let setup = ProcessInvocation::from_clock(test_clock()).unwrap();
        skillranker::storage::init_ledger(
            &setup,
            &setup.request_cx().unwrap(),
            skillranker::storage::LedgerLocation::Directory(ledger.clone()),
        )
        .unwrap();
        assert!(setup.shutdown());
    }

    // Acquire a real SQLite writer lock only after the final request reaches
    // the transport, so the in-flight event and attempt journal already exist.
    let held = Arc::new(Mutex::new(None));
    let generators: Vec<ResponseGenerator> = (0..if low_gate { 1 } else { 2 }).map(|stage| {
        let held = held.clone();
        let ledger = ledger.clone();
        Box::new(move |request: &Request| {
            if hold_lock && (low_gate || stage == 1) {
                let connection = rusqlite::Connection::open(ledger.join(skillranker::storage::LEDGER_FILE)).unwrap();
                connection.execute_batch("BEGIN IMMEDIATE").unwrap();
                *held.lock().unwrap() = Some(connection);
            }
            let key = if stage == 0 { "which" } else { "rerank" };
            let skillranker::jev::codec::Question::Choice { criteria, .. } = &request.questions()[key] else {
                panic!("expected choice question");
            };
            let skills: Vec<_> = criteria.keys().filter(|key| *key != "__none__").collect();
            assert_eq!(skills.len(), 2);
            let probabilities = json!({"__none__": 0.1, (skills[0]): 0.6, (skills[1]): 0.3});
            let mut answers = serde_json::Map::new();
            answers.insert(key.into(), json!({"type":"choice", "choice":skills[0], "probabilities":probabilities, "confidence":0.8}));
            if stage == 0 {
                let gate = if low_gate { 0.05 } else { 0.9 };
                answers.insert("gate::specialized_method".into(), json!({"type":"noul", "noul":gate}));
                answers.insert("gate::material_help".into(), json!({"type":"noul", "noul":gate}));
                answers.insert("gate::context_suffices".into(), json!({"type":"noul", "noul":1.0-gate}));
                let probabilities: serde_json::Map<String, Value> = ["planning", "implementing", "debugging", "testing", "reviewing", "releasing", "conversing", "other"].into_iter().map(|phase| (phase.into(), json!(0.125))).collect();
                answers.insert("phase".into(), json!({"type":"choice", "choice":"implementing", "probabilities":probabilities, "confidence":0.5}));
            } else {
                for skill in skills {
                    answers.insert(format!("fits::{skill}"), json!({"type":"noul", "noul":0.8}));
                }
            }
            request.decode_response(&serde_json::to_vec(&json!({"model":"jev-test", "answers":answers, "usage":{"input_tokens":100,"output_tokens":25}})).unwrap()).map_err(wrap_codec_err)
        }) as ResponseGenerator
    }).collect();
    let transport = DynamicMockTransport::new(generators);
    let invocation = ProcessInvocation::from_clock(test_clock()).unwrap();
    let cx = invocation.request_cx().unwrap();
    let mut sources = ConfigSources::default();
    sources
        .environment
        .push(("TYPESAFE_API_KEY".into(), "test-api-key-xyz".into()));
    let args = RankArgs {
        workspace,
        user_config_root: None,
        home: None,
        cache_dir: None,
        ledger_dir: Some(ledger.clone()),
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
    let document = invocation
        .runtime()
        .block_on(execute_pipeline(&invocation, &cx, args, Some(&transport)))
        .unwrap();
    assert_eq!(document.exit_code(), skillranker::output::CliExit::Success);
    assert!(invocation.shutdown());
    drop(held.lock().unwrap().take());
    let file = ledger.join(skillranker::storage::LEDGER_FILE);
    let reason = if initialize_ledger {
        let db = rusqlite::Connection::open(file).unwrap();
        Some(
            db.query_row("SELECT reason FROM ranking_events", [], |row| row.get(0))
                .unwrap(),
        )
    } else {
        assert!(
            !file.exists(),
            "optional recording must not initialize a ledger"
        );
        None
    };
    (document.as_value().clone(), reason)
}

#[test]
fn successful_rankings_report_unconfirmed_ledger_finalization() {
    for low_gate in [false, true] {
        let (healthy, healthy_reason) =
            successful_ledger_finalization_fixture(low_gate, false, true);
        let (locked, locked_reason) = successful_ledger_finalization_fixture(low_gate, true, true);
        let decision = if low_gate { "abstain" } else { "ranked" };
        assert_eq!(healthy["decision"], decision);
        assert_eq!(locked["decision"], decision);
        assert!(healthy_reason.is_some());
        assert_ne!(healthy_reason.as_deref(), Some("in-flight"));
        assert_eq!(locked_reason.as_deref(), Some("in-flight"));
        assert_eq!(locked["usage"], healthy["usage"]);
        assert_eq!(
            locked["usage"]["http_attempts"],
            if low_gate { 1 } else { 2 }
        );
        assert_eq!(
            locked["usage"]["input_tokens"],
            if low_gate { 100 } else { 200 }
        );
        assert_eq!(
            locked["usage"]["output_tokens"],
            if low_gate { 25 } else { 50 }
        );
        assert_eq!(locked["usage"]["unknown_usage_attempts"], 0);
        let warnings: Vec<_> = locked["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|w| w["kind"] == "ledger-finalization-unconfirmed")
            .collect();
        assert_eq!(warnings.len(), 1, "{locked}");
        assert_eq!(warnings[0]["count"], 1);
        assert!(
            !healthy["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|w| w["kind"] == "ledger-finalization-unconfirmed")
        );
    }
}

#[test]
fn successful_rankings_report_missing_ledger_without_initializing_it() {
    for low_gate in [false, true] {
        let (document, reason) = successful_ledger_finalization_fixture(low_gate, false, false);
        assert_eq!(
            document["decision"],
            if low_gate { "abstain" } else { "ranked" }
        );
        assert!(reason.is_none());
        assert_eq!(
            document["usage"]["http_attempts"],
            if low_gate { 1 } else { 2 }
        );
        let warnings: Vec<_> = document["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|w| w["kind"] == "ledger-finalization-unconfirmed")
            .collect();
        assert_eq!(warnings.len(), 1, "{document}");
        assert_eq!(warnings[0]["count"], 1);
    }
}
