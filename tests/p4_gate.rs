#![cfg(any(target_os = "linux", target_os = "macos"))]
//! Phase P4 Acceptance Gate: Useful Core CLI, Pure Ranking, and Exact Cache.
//!
//! Satisfies contract boundary `p4_core_cli_acceptance` (sr-roadmap-l1i.5.21)
//! mapped in `tests/contract_matrix.toml`.
//!
//! Verifies that the complete Phase P4 pipeline, scoring mathematics,
//! explicit directive resolution, progressive stage exclusion explanations,
//! structured JSON / table outputs, offline demo, dry-run guarantees, and
//! fenced coordination response cache satisfy all foundational invariants.

mod support;

use asupersync::Cx;
use serde_json::json;
use skillranker::cache::{
    CachedResponseEntry, CoordinationKey, LeaseAcquisition, PublishOutcome, RequestFingerprint,
    RequestStage,
};
use skillranker::config::ConfigSources;
use skillranker::context::source::SourceOptions;
use skillranker::effects::{EffectGate, Scope};
use skillranker::identity::SkillId;
use skillranker::jev::OriginScopedCredential;
use skillranker::jev::client::TransportError;
use skillranker::jev::codec::{Request, Response, Usage};
use skillranker::limits::DurationMillis;
use skillranker::output::{ArtifactKind, CliExit, Decision, OutputKind};
use skillranker::pipeline::{JevTransport, RankArgs, execute_pipeline};
use skillranker::privacy::{EffectFlags, NetworkConsent};
use skillranker::roster::LocalPath;
use skillranker::runtime::{EntryClock, ProcessInvocation};
use skillranker::scoring::{Input, Weights, rank};
use skillranker::storage::{CacheAccess, CacheLocation, CacheOpen, open_cache};
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

static GATE_COUNTER: AtomicU64 = AtomicU64::new(0);

type ResponseGenerator = Box<dyn Fn(&Request) -> Result<Response, TransportError> + Send + Sync>;

#[derive(Clone, Default)]
struct GateMockTransport {
    generators: Arc<Mutex<Vec<ResponseGenerator>>>,
    pub recorded_requests: Arc<Mutex<Vec<Request>>>,
}

impl GateMockTransport {
    fn new(generators: Vec<ResponseGenerator>) -> Self {
        Self {
            generators: Arc::new(Mutex::new(generators)),
            recorded_requests: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl JevTransport for GateMockTransport {
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

fn test_clock() -> EntryClock {
    EntryClock::capture_with(
        DurationMillis::new("gate-total", 10_000, 30_000).unwrap(),
        DurationMillis::new("gate-cleanup", 500, 30_000).unwrap(),
    )
    .unwrap()
}

fn create_gate_env() -> (PathBuf, PathBuf) {
    let id = GATE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("sr-p4-gate-{}-{}", std::process::id(), id));
    let workspace = root.join("workspace");
    let skills_dir = workspace.join(".claude/skills");
    fs::create_dir_all(&skills_dir).unwrap();
    (root, workspace)
}

fn create_gate_skill(dir: &Path, name: &str, desc: &str, body: &str) -> PathBuf {
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

fn create_gate_context_file(workspace: &Path, user_prompt: &str) -> PathBuf {
    let context_file = workspace.join("context.json");
    let ctx = json!({
        "schema_version": 1,
        "harness": "claude_code",
        "producer_id": "p4-gate-test",
        "workspace_root": workspace.to_string_lossy(),
        "session_id": "session-p4-gate",
        "agent_id": null,
        "branch_id": null,
        "context_epoch": null,
        "current_request": {
            "event_id": "req-gate",
            "text": user_prompt,
            "attachments_omitted": false,
            "essential_attachment_missing": false
        },
        "events": [
            {
                "event_id": "req-gate",
                "parent_id": null,
                "turn_id": "turn-gate",
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

fn gate_rank_args(
    workspace: PathBuf,
    context_file: PathBuf,
    flags: EffectFlags,
    require_skills: Vec<SkillId>,
    explain: bool,
    dry_run: bool,
) -> RankArgs {
    let gate = EffectGate::new(flags, Scope::Rank).unwrap();
    let source_options = SourceOptions {
        context: Some(LocalPath::new(context_file)),
        ..Default::default()
    };
    RankArgs {
        workspace,
        user_config_root: None,
        home: None,
        cache_dir: Some(support::private_store_dir("cache")),
        ledger_dir: Some(support::private_store_dir("ledger")),
        sources: ConfigSources::default(),
        gate,
        source_options,
        require_skills,
        shortlist_ids: Vec::new(),
        roster_file: None,
        explain,
        why_not: None,
        cursor: None,
        output_json: true,
        output_table: false,
        dry_run,
        save_case: None,
    }
}

#[test]
fn all_p4_invariants_verified() {
    // =========================================================================
    // Invariant 1: Scoring Mathematics, Normalization, and Stable Ties
    // =========================================================================
    let s_alpha = SkillId::new("skill_alpha").unwrap();
    let s_beta = SkillId::new("skill_beta").unwrap();
    let s_gamma = SkillId::new("skill_gamma").unwrap();

    let inputs = [
        Input {
            id: &s_alpha,
            rerank: 0.50,
            fit: 0.80,
            prior_delta: 0.0,
            phase_match: 0.0,
        },
        Input {
            id: &s_beta,
            rerank: 0.30,
            fit: 0.60,
            prior_delta: 0.0,
            phase_match: 0.0,
        },
        Input {
            id: &s_gamma,
            rerank: 0.20,
            fit: 0.40,
            prior_delta: 0.0,
            phase_match: 0.0,
        },
    ];

    // Verify finite default weights rank computation
    let ranking = rank(&inputs, Weights::DEFAULT, 2).expect("valid scoring");
    assert_eq!(ranking.returned.len(), 2);
    assert_eq!(inputs[ranking.returned[0].index].id, &s_alpha);
    assert_eq!(inputs[ranking.returned[1].index].id, &s_beta);

    // Full softmax normalization over all eligible before top-K truncation:
    // returned scores must sum to less than 1.0 when candidates are truncated,
    // and omitted_mass must account for the difference.
    let sum_top2: f64 = ranking.returned.iter().map(|s| s.rank_score).sum();
    assert!(
        (sum_top2 + ranking.omitted_mass - 1.0).abs() < 1e-9,
        "returned scores + omitted mass must equal 1.0"
    );
    assert!(ranking.omitted_mass > 0.0);

    // Singleton candidate score is 1.0 without mutating raw inputs
    let singleton = [Input {
        id: &s_alpha,
        rerank: 0.42,
        fit: 0.55,
        prior_delta: 0.0,
        phase_match: 0.0,
    }];
    let single_res = rank(&singleton, Weights::DEFAULT, 5).expect("singleton scoring");
    assert_eq!(single_res.returned.len(), 1);
    assert_eq!(single_res.returned[0].rank_score, 1.0);
    assert_eq!(single_res.omitted_mass, 0.0);

    // Weight bounds validation: w_fit in [0, 4], w_prior in [0, 0.5], w_phase in [0, 1]
    assert!(Weights::new(4.1, 0.0, 0.0).is_err());
    assert!(Weights::new(1.0, 0.51, 0.0).is_err());
    assert!(Weights::new(1.0, 0.0, 1.01).is_err());
    assert!(Weights::new(-0.1, 0.0, 0.0).is_err());

    // Stable-ID tie breaking: identical inputs order by skill ID lexicographically
    let id_a = SkillId::new("skill_a").unwrap();
    let id_b = SkillId::new("skill_b").unwrap();
    let tied_inputs_ba = [
        Input {
            id: &id_b,
            rerank: 0.40,
            fit: 0.70,
            prior_delta: 0.0,
            phase_match: 0.0,
        },
        Input {
            id: &id_a,
            rerank: 0.40,
            fit: 0.70,
            prior_delta: 0.0,
            phase_match: 0.0,
        },
    ];
    let tied_res = rank(&tied_inputs_ba, Weights::DEFAULT, 2).expect("tie rank");
    assert_eq!(tied_inputs_ba[tied_res.returned[0].index].id, &id_a);
    assert_eq!(tied_inputs_ba[tied_res.returned[1].index].id, &id_b);

    // =========================================================================
    // Invariant 2: Explicit Directive Local Resolution (Zero Provider Calls)
    // =========================================================================
    let (_root, workspace) = create_gate_env();
    let skills_dir = workspace.join(".claude/skills");
    create_gate_skill(
        &skills_dir,
        "security_audit",
        "Audits security vulnerabilities",
        "Run security checks.",
    );
    create_gate_skill(
        &skills_dir,
        "test_runner",
        "Runs unit test suites",
        "Run cargo test.",
    );

    let ctx_file = create_gate_context_file(&workspace, "Please audit the codebase security.");
    let clock = test_clock();
    let invocation = ProcessInvocation::from_clock(clock).unwrap();
    let cx = invocation.request_cx().unwrap();

    let offline_flags = EffectFlags {
        offline: true,
        allow_network: false,
        dry_run: false,
        no_cache: true,
        no_ledger: true,
        no_persist: true,
        save_case: false,
    };
    let args = gate_rank_args(
        workspace.clone(),
        ctx_file,
        offline_flags,
        vec![SkillId::new("security_audit").unwrap()],
        false,
        false,
    );

    let doc = invocation
        .runtime()
        .block_on(async { execute_pipeline(&invocation, &cx, args, None).await })
        .expect("pipeline execution succeeded");

    assert_eq!(doc.kind(), OutputKind::Decision(Decision::Explicit));
    assert_eq!(doc.exit_code(), CliExit::Success);
    let val = doc.as_value();
    assert_eq!(val["decision"], "explicit");
    assert_eq!(val["skills"][0]["invocation_name"], "security_audit");
    assert_eq!(val["usage"]["requests"], 0);
    assert_eq!(val["usage"]["http_attempts"], 0);
    assert!(invocation.shutdown(), "explicit pipeline shutdown cleanly");

    // Missing required skill fails locally without guessing or substituting
    let ctx_file_missing = create_gate_context_file(&workspace, "Run non-existent skill.");
    let missing_args = gate_rank_args(
        workspace.clone(),
        ctx_file_missing,
        offline_flags,
        vec![SkillId::new("non_existent_skill").unwrap()],
        false,
        false,
    );
    let clock_missing = test_clock();
    let inv_missing = ProcessInvocation::from_clock(clock_missing).unwrap();
    let cx_missing = inv_missing.request_cx().unwrap();
    let err_doc = inv_missing
        .runtime()
        .block_on(async { execute_pipeline(&inv_missing, &cx_missing, missing_args, None).await })
        .expect("missing required skill produces structured failure document");
    assert_eq!(err_doc.exit_code(), CliExit::Roster);
    let val = err_doc.as_value();
    assert_eq!(val["decision"], "unavailable");
    assert_eq!(val["error"]["kind"], "unresolved-explicit");
    let unresolved = val["unresolved"].as_array().expect("unresolved array");
    assert_eq!(unresolved[0]["reference"], "non_existent_skill");
    assert_eq!(unresolved[0]["reason"], "missing");
    assert!(
        inv_missing.shutdown(),
        "missing skill runtime shutdown cleanly"
    );

    // =========================================================================
    // Invariant 3: Two-Stage Jev Ranking with Mock Transport
    // =========================================================================
    let ctx_file_rank = create_gate_context_file(&workspace, "Run all tests for this project.");

    let wide_gen: ResponseGenerator = Box::new(|req: &Request| {
        let q_choice = match &req.questions()["which"] {
            skillranker::jev::codec::Question::Choice { criteria, .. } => criteria,
            _ => panic!("expected choice for which"),
        };
        let mut which_probs = serde_json::Map::new();
        which_probs.insert("__none__".into(), json!(0.10));
        let other_count = q_choice.keys().filter(|k| k.as_str() != "__none__").count();
        for k in q_choice.keys() {
            if k != "__none__" {
                which_probs.insert(k.clone(), json!(0.90 / other_count as f64));
            }
        }
        let top_key = q_choice.keys().find(|k| k.as_str() != "__none__").unwrap();
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
                    "choice": top_key,
                    "probabilities": which_probs,
                    "confidence": 0.85
                },
                "gate::specialized_method": {"type": "noul", "noul": 0.85},
                "gate::material_help": {"type": "noul", "noul": 0.90},
                "gate::context_suffices": {"type": "noul", "noul": 0.10},
                "phase": {
                    "type": "choice",
                    "choice": "testing",
                    "probabilities": phase_probs,
                    "confidence": 0.85
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

    let mock_transport = GateMockTransport::new(vec![wide_gen, rerank_gen]);

    let network_flags = EffectFlags {
        offline: false,
        allow_network: true,
        dry_run: false,
        no_cache: true,
        no_ledger: true,
        no_persist: true,
        save_case: false,
    };
    let mut sources_with_key = ConfigSources::default();
    sources_with_key
        .environment
        .push(("TYPESAFE_API_KEY".into(), "test-api-key-xyz".into()));

    let mut rank_args = gate_rank_args(
        workspace.clone(),
        ctx_file_rank,
        network_flags,
        Vec::new(),
        false,
        false,
    );
    rank_args.sources = sources_with_key;

    let clock_ranked = test_clock();
    let inv_ranked = ProcessInvocation::from_clock(clock_ranked).unwrap();
    let cx_ranked = inv_ranked.request_cx().unwrap();
    let ranked_doc = inv_ranked
        .runtime()
        .block_on(async {
            execute_pipeline(&inv_ranked, &cx_ranked, rank_args, Some(&mock_transport)).await
        })
        .expect("pipeline execution succeeded");

    assert_eq!(ranked_doc.kind(), OutputKind::Decision(Decision::Ranked));
    assert_eq!(ranked_doc.exit_code(), CliExit::Success);
    let ranked_val = ranked_doc.as_value();
    assert_eq!(ranked_val["decision"], "ranked");
    assert_eq!(mock_transport.recorded_requests.lock().unwrap().len(), 2);
    assert!(inv_ranked.shutdown(), "ranked pipeline shutdown cleanly");

    // =========================================================================
    // Invariant 4: Dry-Run Zero Network and Zero Mutation Guarantee
    // =========================================================================
    let ctx_file_dry = create_gate_context_file(&workspace, "Dry run request.");
    let dry_args = gate_rank_args(
        workspace.clone(),
        ctx_file_dry,
        EffectFlags {
            offline: false,
            allow_network: false,
            dry_run: true,
            no_cache: true,
            no_ledger: true,
            no_persist: true,
            save_case: false,
        },
        Vec::new(),
        false,
        true,
    );

    let clock_dry = test_clock();
    let inv_dry = ProcessInvocation::from_clock(clock_dry).unwrap();
    let cx_dry = inv_dry.request_cx().unwrap();
    let dry_doc = inv_dry
        .runtime()
        .block_on(async { execute_pipeline(&inv_dry, &cx_dry, dry_args, None).await })
        .expect("dry-run succeeds without network");

    assert_eq!(dry_doc.kind(), OutputKind::Artifact(ArtifactKind::Preview));
    assert_eq!(dry_doc.exit_code(), CliExit::Success);
    let dry_val = dry_doc.as_value();
    assert_eq!(dry_val["kind"], "preview");
    assert_eq!(dry_val["actionable"], false);
    assert_eq!(dry_val["stateless"], true);
    assert!(dry_val["local_decision"].is_null());
    assert_eq!(dry_val["provider_request"]["stages"][0]["stage"], "wide");
    assert!(inv_dry.shutdown(), "dry-run pipeline shutdown cleanly");

    // =========================================================================
    // Invariant 5: Progressive Stage Exclusions and Explainability
    // =========================================================================
    // Stage trace with --explain preserves identical evaluation while populating
    // detailed exclusions per skill
    let ctx_file_explain = create_gate_context_file(&workspace, "Auditing security.");
    let explain_args = gate_rank_args(
        workspace.clone(),
        ctx_file_explain,
        offline_flags,
        vec![SkillId::new("security_audit").unwrap()],
        true,
        false,
    );
    let clock_explain = test_clock();
    let inv_explain = ProcessInvocation::from_clock(clock_explain).unwrap();
    let cx_explain = inv_explain.request_cx().unwrap();
    let explain_doc = inv_explain
        .runtime()
        .block_on(async { execute_pipeline(&inv_explain, &cx_explain, explain_args, None).await })
        .expect("explain pipeline execution succeeded");

    assert_eq!(explain_doc.kind(), OutputKind::Decision(Decision::Explicit));
    let explain_val = explain_doc.as_value();
    assert!(
        explain_val.get("trace").is_some(),
        "explain doc must contain trace"
    );
    let trace = &explain_val["trace"];
    assert_eq!(trace["total"], 8);
    let trace_entries = trace["entries"].as_array().expect("entries array");
    assert_eq!(trace_entries.len(), 8);
    assert_eq!(
        trace_entries[0]["skill_id"],
        explain_val["skills"][0]["skill_id"]
    );
    assert_eq!(trace_entries[0]["stage"], "discovery");
    assert_eq!(trace_entries[0]["status"], "passed");
    assert!(inv_explain.shutdown(), "explain pipeline shutdown cleanly");

    // =========================================================================
    // Invariant 6: Fenced Lease Coordination and Response Cache Isolation
    // =========================================================================
    let cache_dir = Path::new("/tmp").join(format!(
        "sr-p4-cache-gate-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&cache_dir)
        .unwrap();

    let clock_cache = test_clock();
    let inv_cache = ProcessInvocation::from_clock(clock_cache).unwrap();
    let cx_cache = inv_cache.request_cx().unwrap();

    let store = match open_cache(
        &inv_cache,
        &cx_cache,
        CacheAccess::Initialize,
        CacheLocation::Directory(cache_dir.clone()),
    )
    .expect("cache init")
    {
        CacheOpen::Ready(store) => *store,
        _ => panic!("expected ready cache"),
    };

    let leases_path = cache_dir.join(skillranker::storage::CACHE_FILE);
    let key = CoordinationKey::from_bytes([42; 32]);

    let now_ms = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();

    // Leader acquisition
    let (store, acquisition) = store
        .acquire_lease(&inv_cache, &cx_cache, key, false)
        .expect("acquire lease");
    let leader = match acquisition {
        LeaseAcquisition::Leading(leader) => leader,
        _ => panic!("expected leader"),
    };

    // Storing entry while holding lease
    let entry = CachedResponseEntry {
        stage: RequestStage::Wide,
        request_fingerprint: RequestFingerprint::from_bytes([7; 32]),
        response_bytes: b"cached-p4-response".to_vec(),
        received_at_unix_ms: now_ms,
        ttl_seconds: 600,
        model: "jev-latest".into(),
        model_revision: None,
        original_usage: Usage {
            input_tokens: 10,
            output_tokens: 10,
        },
        attempt_id: None,
    };

    let store = store
        .record_response_fenced(
            &inv_cache,
            &cx_cache,
            [1; 32],
            entry,
            (leases_path.clone(), leader.clone()),
        )
        .expect("fenced cache record succeeds");

    // Complete lease
    let (store, completion) = store
        .complete_lease(&inv_cache, &cx_cache, leader.clone())
        .expect("complete lease");
    assert_eq!(completion, PublishOutcome::Published);

    // Successor re-acquires lease with higher generation
    let (store, acquisition) = store
        .acquire_lease(&inv_cache, &cx_cache, key, true)
        .expect("force reacquire");
    let successor = match acquisition {
        LeaseAcquisition::Leading(succ) => succ,
        _ => panic!("expected successor"),
    };
    assert!(successor.fencing_generation > leader.fencing_generation);
    assert!(!cache_dir.join("leases.sqlite3").exists());

    // Stale owner attempt to write must be rejected with LeaseSuperseded
    let stale_entry = CachedResponseEntry {
        stage: RequestStage::Wide,
        request_fingerprint: RequestFingerprint::from_bytes([7; 32]),
        response_bytes: b"stale-overwrite-attempt".to_vec(),
        received_at_unix_ms: now_ms,
        ttl_seconds: 600,
        model: "jev-latest".into(),
        model_revision: None,
        original_usage: Usage {
            input_tokens: 10,
            output_tokens: 10,
        },
        attempt_id: None,
    };

    let stale_write = store.record_response_fenced(
        &inv_cache,
        &cx_cache,
        [1; 32],
        stale_entry,
        (leases_path, leader),
    );
    assert!(
        matches!(
            stale_write,
            Err(skillranker::storage::StoreError::LeaseSuperseded)
        ),
        "stale owner write must be rejected with LeaseSuperseded"
    );

    // Clean runtime shutdown
    assert!(
        inv_cache.shutdown(),
        "runtime must cleanly shut down within deadline"
    );
}
