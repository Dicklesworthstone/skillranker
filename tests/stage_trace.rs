//! Comprehensive integration tests for stage trace explanations and --why-not diagnostics.
//!
//! Satisfies contract boundary `p4_explain_exclusion_stages` (sr-roadmap-l1i.5.14).

mod support;

use asupersync::Cx;
use serde_json::json;
use skillranker::config::ConfigSources;
use skillranker::context::source::SourceOptions;
use skillranker::effects::{EffectGate, Scope};
use skillranker::identity::SkillId;
use skillranker::jev::OriginScopedCredential;
use skillranker::jev::client::TransportError;
use skillranker::jev::codec::{Request, Response};
use skillranker::limits::DurationMillis;
use skillranker::output::{CliExit, Decision, OutputDocument, OutputKind, TraceStage};
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
    let root = std::env::temp_dir().join(format!("sr-trace-test-{}-{}", std::process::id(), id));
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

fn create_restricted_skill(
    dir: &Path,
    name: &str,
    desc: &str,
    manual_only: bool,
    body: &str,
) -> PathBuf {
    let skill_dir = dir.join(name);
    fs::create_dir_all(&skill_dir).unwrap();
    let file = skill_dir.join("SKILL.md");
    let frontmatter = if manual_only {
        format!(
            "---\nname: {name}\ndescription: {desc}\nuser-invocable: true\ndisable-model-invocation: true\n---\n{body}\n"
        )
    } else {
        format!("---\nname: {name}\ndescription: {desc}\n---\n{body}\n")
    };
    fs::write(&file, frontmatter).unwrap();
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

fn mock_generators() -> Vec<ResponseGenerator> {
    let wide_gen: ResponseGenerator = Box::new(|req: &Request| {
        let q_which = match &req.questions()["which"] {
            skillranker::jev::codec::Question::Choice { criteria, .. } => criteria,
            _ => panic!("expected choice for which"),
        };
        let keys: Vec<_> = q_which.keys().cloned().collect();
        let n = keys.len();
        let mut probs = serde_json::Map::new();
        if n == 1 {
            probs.insert(keys[0].clone(), json!(1.0));
        } else {
            probs.insert(keys[0].clone(), json!(0.50));
            let rest_count = n - 1;
            let mut acc = 0.0;
            for (i, k) in keys[1..].iter().enumerate() {
                if i == rest_count - 1 {
                    probs.insert(k.clone(), json!(0.50 - acc));
                } else {
                    let p = 0.50 / rest_count as f64;
                    probs.insert(k.clone(), json!(p));
                    acc += p;
                }
            }
        }
        let first_choice = keys[0].clone();

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
        if skill_keys.len() == 1 {
            probs.insert(skill_keys[0].clone(), json!(0.90));
        } else {
            probs.insert(skill_keys[0].clone(), json!(0.50));
            let rest_count = skill_keys.len() - 1;
            let mut acc = 0.0;
            for (i, k) in skill_keys[1..].iter().enumerate() {
                if i == rest_count - 1 {
                    probs.insert(k.clone(), json!(0.40 - acc));
                } else {
                    let p = 0.40 / rest_count as f64;
                    probs.insert(k.clone(), json!(p));
                    acc += p;
                }
            }
        }
        answers.insert(
            "rerank".into(),
            json!({
                "type": "choice",
                "choice": skill_keys[0].clone(),
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

    vec![wide_gen, rerank_gen]
}

#[test]
fn test_paired_runs_identical_evaluation_with_and_without_explain() {
    let (_root, workspace) = create_test_env();
    let skills_dir = workspace.join(".claude/skills");
    create_skill(&skills_dir, "skill_a", "Alpha skill", "Alpha body");
    create_skill(&skills_dir, "skill_b", "Beta skill", "Beta body");

    let ctx_file = create_context_file(&workspace, "Help with skill_a workflow");

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
        context: Some(LocalPath::new(ctx_file.clone())),
        ..Default::default()
    };

    let mut sources = ConfigSources::default();
    sources
        .environment
        .push(("TYPESAFE_API_KEY".into(), "test-key-123".into()));

    // Run A: explain=false, why_not=None
    let clock_a = test_clock();
    let inv_a = ProcessInvocation::from_clock(clock_a).unwrap();
    let cx_a = inv_a.request_cx().unwrap();
    let transport_a = MockJevTransport::new(mock_generators());
    let args_a = RankArgs {
        workspace: workspace.clone(),
        user_config_root: None,
        home: None,
        cache_dir: Some(support::private_store_dir("cache")),
        sources: sources.clone(),
        gate,
        source_options: source_options.clone(),
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
    let doc_a = inv_a
        .runtime()
        .block_on(async { execute_pipeline(&inv_a, &cx_a, args_a, Some(&transport_a)).await })
        .expect("pipeline A succeeds");

    // Run B: explain=true, why_not=Some("skill_a")
    let clock_b = test_clock();
    let inv_b = ProcessInvocation::from_clock(clock_b).unwrap();
    let cx_b = inv_b.request_cx().unwrap();
    let transport_b = MockJevTransport::new(mock_generators());
    let args_b = RankArgs {
        workspace: workspace.clone(),
        user_config_root: None,
        home: None,
        cache_dir: Some(support::private_store_dir("cache")),
        sources: sources.clone(),
        gate,
        source_options: source_options.clone(),
        require_skills: Vec::new(),
        shortlist_ids: Vec::new(),
        roster_file: None,
        explain: true,
        why_not: Some(SkillId::new("skill_a").unwrap()),
        cursor: None,
        output_json: true,
        output_table: false,
        dry_run: false,
        save_case: None,
        ledger_dir: Some(support::private_store_dir("ledger")),
    };
    let doc_b = inv_b
        .runtime()
        .block_on(async { execute_pipeline(&inv_b, &cx_b, args_b, Some(&transport_b)).await })
        .expect("pipeline B succeeds");

    // Run C: explain=true, why_not=None
    let clock_c = test_clock();
    let inv_c = ProcessInvocation::from_clock(clock_c).unwrap();
    let cx_c = inv_c.request_cx().unwrap();
    let transport_c = MockJevTransport::new(mock_generators());
    let args_c = RankArgs {
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
        explain: true,
        why_not: None,
        cursor: None,
        output_json: true,
        output_table: false,
        dry_run: false,
        save_case: None,
        ledger_dir: Some(support::private_store_dir("ledger")),
    };
    let doc_c = inv_c
        .runtime()
        .block_on(async { execute_pipeline(&inv_c, &cx_c, args_c, Some(&transport_c)).await })
        .expect("pipeline C succeeds");

    let val_a = doc_a.as_value();
    let val_b = doc_b.as_value();
    let val_c = doc_c.as_value();

    // 1. Decisions are identical
    assert_eq!(val_a["decision"], "ranked");
    assert_eq!(val_b["decision"], "ranked");
    assert_eq!(val_c["decision"], "ranked");

    // 2. Candidate sets & scores are identical
    assert_eq!(val_a["skills"], val_b["skills"]);
    assert_eq!(val_a["skills"], val_c["skills"]);

    // 3. Provider request counts & tokens are identical
    assert_eq!(val_a["usage"], val_b["usage"]);
    assert_eq!(val_a["usage"], val_c["usage"]);
    assert_eq!(val_a["usage"]["requests"], 2);
    assert_eq!(val_a["usage"]["http_attempts"], 2);

    // 4. Trace is absent on run A, present on run B and C
    assert!(val_a.get("trace").is_none());
    assert!(val_b.get("trace").is_some());
    assert!(val_c.get("trace").is_some());

    // 5. In Run B with why-not skill_a, 8 entries exist and all passed
    let trace_b = &val_b["trace"];
    assert_eq!(trace_b["total"], 8);
    let entries_b = trace_b["entries"].as_array().expect("entries array");
    assert_eq!(entries_b.len(), 8);
    for entry in entries_b {
        assert_eq!(entry["skill_id"], "skill_a");
        assert_eq!(entry["status"], "passed");
    }

    // 6. Output documents round-trip through from_json cleanly
    let roundtrip_b = OutputDocument::from_json(&doc_b.to_json().unwrap()).unwrap();
    assert_eq!(roundtrip_b.kind(), OutputKind::Decision(Decision::Ranked));
    assert_eq!(roundtrip_b.exit_code(), CliExit::Success);
}

#[test]
fn test_unknown_skill_id_produces_not_in_snapshot_without_broadening_discovery() {
    let (_root, workspace) = create_test_env();
    let skills_dir = workspace.join(".claude/skills");
    create_skill(&skills_dir, "known_skill", "Known skill", "Known body");

    let ctx_file = create_context_file(&workspace, "Help with known_skill");

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
        .push(("TYPESAFE_API_KEY".into(), "test-key-123".into()));

    let clock = test_clock();
    let inv = ProcessInvocation::from_clock(clock).unwrap();
    let cx = inv.request_cx().unwrap();
    let transport = MockJevTransport::new(mock_generators());

    let unknown_id = SkillId::new("nonexistent_skill_xyz").unwrap();
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
        explain: true,
        why_not: Some(unknown_id),
        cursor: None,
        output_json: true,
        output_table: false,
        dry_run: false,
        save_case: None,
        ledger_dir: Some(support::private_store_dir("ledger")),
    };

    let doc = inv
        .runtime()
        .block_on(async { execute_pipeline(&inv, &cx, args, Some(&transport)).await })
        .expect("pipeline succeeds with unknown why-not");

    let val = doc.as_value();
    let trace = &val["trace"];
    assert_eq!(trace["total"], 8);
    let entries = trace["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 8);

    // Entry 0: discovery -> not-in-snapshot with null value, threshold, reason
    assert_eq!(entries[0]["skill_id"], "nonexistent_skill_xyz");
    assert_eq!(entries[0]["stage"], "discovery");
    assert_eq!(entries[0]["status"], "not-in-snapshot");
    assert!(entries[0]["value"].is_null());
    assert!(entries[0]["threshold"].is_null());
    assert!(entries[0]["reason"].is_null());
    assert_eq!(entries[0]["hint"], "inspect-roster");

    // Entries 1..8: all subsequent stages must be not-evaluated with null value, threshold, reason
    let expected_stages = [
        "visibility",
        "local-policy",
        "quill-admission",
        "wide-shortlist",
        "fit-none",
        "ordering",
        "publication",
    ];
    for (i, stage_name) in expected_stages.iter().enumerate() {
        let entry = &entries[i + 1];
        assert_eq!(entry["skill_id"], "nonexistent_skill_xyz");
        assert_eq!(entry["stage"], *stage_name);
        assert_eq!(entry["status"], "not-evaluated");
        assert!(entry["value"].is_null());
        assert!(entry["threshold"].is_null());
        assert!(entry["reason"].is_null());
    }

    // Normal ranking of known_skill was unaffected
    let skills = val["skills"].as_array().expect("skills");
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0]["invocation_name"], "known_skill");
}

#[test]
fn test_early_exclusion_marks_subsequent_stages_not_evaluated() {
    let (_root, workspace) = create_test_env();
    let skills_dir = workspace.join(".claude/skills");
    create_skill(&skills_dir, "agent_skill", "Agent invocable skill", "Body");
    create_restricted_skill(
        &skills_dir,
        "manual_skill",
        "Manual only skill",
        true,
        "Manual body",
    );

    let ctx_file = create_context_file(&workspace, "Do work");

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
        .push(("TYPESAFE_API_KEY".into(), "test-key-123".into()));

    let clock = test_clock();
    let inv = ProcessInvocation::from_clock(clock).unwrap();
    let cx = inv.request_cx().unwrap();
    let transport = MockJevTransport::new(mock_generators());

    let manual_id = SkillId::new("manual_skill").unwrap();
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
        explain: true,
        why_not: Some(manual_id),
        cursor: None,
        output_json: true,
        output_table: false,
        dry_run: false,
        save_case: None,
        ledger_dir: Some(support::private_store_dir("ledger")),
    };

    let doc = inv
        .runtime()
        .block_on(async { execute_pipeline(&inv, &cx, args, Some(&transport)).await })
        .expect("pipeline succeeds");

    let val = doc.as_value();
    let trace = &val["trace"];
    let entries = trace["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 8);

    // 1. discovery passed
    assert_eq!(entries[0]["stage"], "discovery");
    assert_eq!(entries[0]["status"], "passed");
    assert_eq!(entries[0]["reason"], "discovered");

    // 2. visibility excluded (manual-only)
    assert_eq!(entries[1]["stage"], "visibility");
    assert_eq!(entries[1]["status"], "excluded");
    assert_eq!(entries[1]["reason"], "manual-only");
    assert_eq!(entries[1]["hint"], "request-explicitly");

    // 3..8: all subsequent stages MUST be not-evaluated
    for entry in &entries[2..] {
        assert_eq!(entry["status"], "not-evaluated");
        assert!(entry["value"].is_null());
        assert!(entry["threshold"].is_null());
        assert!(entry["reason"].is_null());
    }
}

#[test]
fn test_every_exclusion_reason_with_success_twin() {
    let (_root, workspace) = create_test_env();
    let skills_dir = workspace.join(".claude/skills");
    create_skill(&skills_dir, "eligible_twin", "Eligible twin", "Body");
    create_skill(&skills_dir, "excluded_twin", "Excluded twin", "Body");

    // Configure excluded_twin in workspace config exclusions
    let config_dir = workspace.join(".sr");
    fs::create_dir_all(&config_dir).unwrap();
    let config_file = config_dir.join("config.toml");
    fs::write(
        &config_file,
        "[ranking]\nexclude_skills = [\"excluded_twin\"]\n",
    )
    .unwrap();

    let ctx_file = create_context_file(&workspace, "Test twin comparison");

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
        .push(("TYPESAFE_API_KEY".into(), "test-key-123".into()));

    // Run for excluded_twin
    let clock_ex = test_clock();
    let inv_ex = ProcessInvocation::from_clock(clock_ex).unwrap();
    let cx_ex = inv_ex.request_cx().unwrap();
    let transport_ex = MockJevTransport::new(mock_generators());
    let args_ex = RankArgs {
        workspace: workspace.clone(),
        user_config_root: None,
        home: None,
        cache_dir: Some(support::private_store_dir("cache")),
        sources: sources.clone(),
        gate,
        source_options: source_options.clone(),
        require_skills: Vec::new(),
        shortlist_ids: Vec::new(),
        roster_file: None,
        explain: true,
        why_not: Some(SkillId::new("excluded_twin").unwrap()),
        cursor: None,
        output_json: true,
        output_table: false,
        dry_run: false,
        save_case: None,
        ledger_dir: Some(support::private_store_dir("ledger")),
    };
    let doc_ex = inv_ex
        .runtime()
        .block_on(async { execute_pipeline(&inv_ex, &cx_ex, args_ex, Some(&transport_ex)).await })
        .expect("pipeline excluded twin succeeds");

    let val_ex = doc_ex.as_value();
    let entries_ex = val_ex["trace"]["entries"].as_array().unwrap();
    assert_eq!(entries_ex[0]["stage"], "discovery");
    assert_eq!(entries_ex[0]["status"], "passed");
    assert_eq!(entries_ex[1]["stage"], "visibility");
    assert_eq!(entries_ex[1]["status"], "passed");
    assert_eq!(entries_ex[2]["stage"], "local-policy");
    assert_eq!(entries_ex[2]["status"], "excluded");
    assert_eq!(entries_ex[2]["reason"], "excluded");
    assert_eq!(entries_ex[2]["hint"], "review-exclusions");

    // Success twin: eligible_twin
    let clock_el = test_clock();
    let inv_el = ProcessInvocation::from_clock(clock_el).unwrap();
    let cx_el = inv_el.request_cx().unwrap();
    let transport_el = MockJevTransport::new(mock_generators());
    let args_el = RankArgs {
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
        explain: true,
        why_not: Some(SkillId::new("eligible_twin").unwrap()),
        cursor: None,
        output_json: true,
        output_table: false,
        dry_run: false,
        save_case: None,
        ledger_dir: Some(support::private_store_dir("ledger")),
    };
    let doc_el = inv_el
        .runtime()
        .block_on(async { execute_pipeline(&inv_el, &cx_el, args_el, Some(&transport_el)).await })
        .expect("pipeline eligible twin succeeds");

    let val_el = doc_el.as_value();
    let entries_el = val_el["trace"]["entries"].as_array().unwrap();
    assert_eq!(entries_el[0]["stage"], "discovery");
    assert_eq!(entries_el[0]["status"], "passed");
    assert_eq!(entries_el[1]["stage"], "visibility");
    assert_eq!(entries_el[1]["status"], "passed");
    assert_eq!(entries_el[2]["stage"], "local-policy");
    assert_eq!(entries_el[2]["status"], "passed");
    assert_eq!(entries_el[2]["reason"], "policy-admitted");
}

#[test]
fn test_stage_trace_contract_and_round_trip() {
    let (_root, workspace) = create_test_env();
    let skills_dir = workspace.join(".claude/skills");
    create_skill(&skills_dir, "skill_one", "Skill one description", "Body");

    let ctx_file = create_context_file(&workspace, "Help with skill_one");

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
        .push(("TYPESAFE_API_KEY".into(), "test-key-123".into()));

    let clock = test_clock();
    let inv = ProcessInvocation::from_clock(clock).unwrap();
    let cx = inv.request_cx().unwrap();
    let transport = MockJevTransport::new(mock_generators());

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
        explain: true,
        why_not: Some(SkillId::new("skill_one").unwrap()),
        cursor: None,
        output_json: true,
        output_table: false,
        dry_run: false,
        save_case: None,
        ledger_dir: Some(support::private_store_dir("ledger")),
    };

    let doc = inv
        .runtime()
        .block_on(async { execute_pipeline(&inv, &cx, args, Some(&transport)).await })
        .expect("pipeline succeeds");

    let json_bytes = doc.to_json().unwrap();
    let doc_parsed = OutputDocument::from_json(&json_bytes).unwrap();
    assert_eq!(doc_parsed.kind(), OutputKind::Decision(Decision::Ranked));

    let val = doc_parsed.as_value();
    let trace = &val["trace"];
    assert_eq!(
        trace["cursor"]["snapshot_id"],
        val["roster"]["provenance"]["snapshot_id"]
    );
    assert_eq!(trace["cursor"]["offset"], 0);
    assert!(trace["next_offset"].is_null());

    let entries = trace["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 8);
    for (i, stage) in TraceStage::ALL.iter().enumerate() {
        assert_eq!(entries[i]["stage"], stage.as_str());
    }
}

#[test]
fn test_table_view_trace_rendering_and_recovery_hints() {
    let (_root, workspace) = create_test_env();
    let skills_dir = workspace.join(".claude/skills");
    create_skill(&skills_dir, "clean_skill", "Clean skill", "Body");

    let ctx_file = create_context_file(&workspace, "Help with clean_skill");

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
        .push(("TYPESAFE_API_KEY".into(), "test-key-123".into()));

    let clock = test_clock();
    let inv = ProcessInvocation::from_clock(clock).unwrap();
    let cx = inv.request_cx().unwrap();
    let transport = MockJevTransport::new(mock_generators());

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
        explain: true,
        why_not: Some(SkillId::new("clean_skill").unwrap()),
        cursor: None,
        output_json: false,
        output_table: true,
        dry_run: false,
        save_case: None,
        ledger_dir: Some(support::private_store_dir("ledger")),
    };

    let doc = inv
        .runtime()
        .block_on(async { execute_pipeline(&inv, &cx, args, Some(&transport)).await })
        .expect("pipeline succeeds");

    let table_str = doc.render_table();

    // Contains table header
    assert!(table_str.contains("STAGE TRACE (WHY-NOT / EXPLANATION)"));
    assert!(table_str.contains("SKILL ID"));
    assert!(table_str.contains("STAGE"));
    assert!(table_str.contains("STATUS"));
    assert!(table_str.contains("VALUE"));
    assert!(table_str.contains("THRESHOLD"));
    assert!(table_str.contains("REASON"));

    // ANSI escape codes were stripped
    assert!(!table_str.contains("\x1b[31;1m"));
    assert!(!table_str.contains("\x1b[0m"));
    assert!(table_str.contains("clean_skill"));

    // Stage rows are present
    assert!(table_str.contains("discovery"));
    assert!(table_str.contains("visibility"));
    assert!(table_str.contains("local-policy"));
    assert!(table_str.contains("publication"));
}
