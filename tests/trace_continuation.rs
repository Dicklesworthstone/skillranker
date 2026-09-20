//! Satisfies contract boundary `p4_explain_exclusion_stages` (sr-roadmap-l1i.5.14).

use asupersync::Cx;
use serde_json::{Value, json};
use skillranker::config::{ConfigSources, RawValue};
use skillranker::context::source::SourceOptions;
use skillranker::effects::{EffectGate, Scope};
use skillranker::identity::{ContentHash, SkillId};
use skillranker::jev::OriginScopedCredential;
use skillranker::jev::client::TransportError;
use skillranker::jev::codec::{Request, Response};
use skillranker::limits::DurationMillis;
use skillranker::output::{
    CliExit, Decision, OutputKind, TraceCursor, TraceQueryScope, TraceStage,
};
use skillranker::pipeline::{JevTransport, RankArgs, execute_pipeline};
use skillranker::privacy::{EffectFlags, NetworkConsent};
use skillranker::roster::LocalPath;
use skillranker::runtime::{EntryClock, ProcessInvocation};
use std::fs;
use std::os::unix::fs::DirBuilderExt;
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
    let root = std::env::temp_dir().join(format!(
        "sr-trace-continuation-{}-{}",
        std::process::id(),
        id
    ));
    let workspace = root.join("workspace");
    let skills_dir = workspace.join(".claude/skills");
    fs::create_dir_all(&skills_dir).unwrap();
    let workspace = fs::canonicalize(workspace).unwrap();
    (root, workspace)
}

fn create_cache_dir() -> PathBuf {
    let id = FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = Path::new("/tmp").join(format!("sr-trace-cache-{}-{}", std::process::id(), id));
    if !dir.exists() {
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
    }
    dir
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

#[test]
fn test_trace_cursor_token_roundtrip() {
    let snap = ContentHash::from_bytes(b"snapshot-test-hash");
    let query = ContentHash::from_bytes(b"query-test-hash");
    let cursor = TraceCursor {
        schema_version: 1,
        snapshot_id: snap.clone(),
        query_id: query.clone(),
        offset: 128,
    };

    let token = cursor.to_token();
    assert!(token.starts_with("t1."));
    let parsed = TraceCursor::from_token(&token).expect("token must parse");
    assert_eq!(parsed.schema_version, 1);
    assert_eq!(parsed.snapshot_id, snap);
    assert_eq!(parsed.query_id, query);
    assert_eq!(parsed.offset, 128);

    // Invalid prefix
    assert!(TraceCursor::from_token("r1.abc.def.0").is_err());
    assert!(TraceCursor::from_token("t2.abc.def.0").is_err());
    // Missing parts
    assert!(TraceCursor::from_token("t1.abc.128").is_err());
    // Non-hex hash
    assert!(TraceCursor::from_token("t1.not-hex.def.128").is_err());
    // Non-numeric offset
    assert!(
        TraceCursor::from_token(&format!("t1.{}.{}.abc", snap.as_str(), query.as_str())).is_err()
    );
}

#[test]
fn test_trace_query_scope_binds_frozen_parameters() {
    let req = "Diagnose the test failures";
    let target = SkillId::new("skill_01").unwrap();
    let requires = vec![SkillId::new("skill_req").unwrap()];
    let excludes = vec![SkillId::new("skill_excl").unwrap()];
    let ctx_hash = ContentHash::from_bytes(b"context 1");
    let eval_hash = ContentHash::from_bytes(b"eval 1");

    let base = TraceQueryScope {
        request_text: req,
        why_not: Some(&target),
        gate_threshold: 0.30,
        fits_threshold: 0.30,
        top: 5,
        shortlist: 8,
        require_skills: &requires,
        exclude_skills: &excludes,
        context_hash: Some(ctx_hash.clone()),
        model: Some("model-a"),
        endpoint: Some("https://api.example.com"),
        evaluation_hash: Some(eval_hash.clone()),
    };
    let base_id = base.compute_id();

    // Changed request text alters query_id
    let mut modified = base.clone();
    modified.request_text = "Different prompt";
    assert_ne!(base_id, modified.compute_id());

    // Changed why_not alters query_id
    let mut mod_target = base.clone();
    mod_target.why_not = None;
    assert_ne!(base_id, mod_target.compute_id());

    // Changed gate threshold alters query_id
    let mut mod_gate = base.clone();
    mod_gate.gate_threshold = 0.50;
    assert_ne!(base_id, mod_gate.compute_id());

    // Changed fits threshold alters query_id
    let mut mod_fits = base.clone();
    mod_fits.fits_threshold = 0.50;
    assert_ne!(base_id, mod_fits.compute_id());

    // Changed top K alters query_id
    let mut mod_top = base.clone();
    mod_top.top = 10;
    assert_ne!(base_id, mod_top.compute_id());

    // Changed shortlist M alters query_id
    let mut mod_shortlist = base.clone();
    mod_shortlist.shortlist = 16;
    assert_ne!(base_id, mod_shortlist.compute_id());

    // Changed require skills alters query_id
    let empty_requires: Vec<SkillId> = Vec::new();
    let mut mod_req = base.clone();
    mod_req.require_skills = &empty_requires;
    assert_ne!(base_id, mod_req.compute_id());

    // Changed exclude skills alters query_id
    let empty_excludes: Vec<SkillId> = Vec::new();
    let mut mod_excl = base.clone();
    mod_excl.exclude_skills = &empty_excludes;
    assert_ne!(base_id, mod_excl.compute_id());

    // Changed context hash alters query_id
    let mut mod_ctx = base.clone();
    mod_ctx.context_hash = Some(ContentHash::from_bytes(b"context 2"));
    assert_ne!(base_id, mod_ctx.compute_id());

    // Changed model alters query_id
    let mut mod_model = base.clone();
    mod_model.model = Some("model-b");
    assert_ne!(base_id, mod_model.compute_id());

    // Changed endpoint alters query_id
    let mut mod_ep = base.clone();
    mod_ep.endpoint = Some("https://other.example.com");
    assert_ne!(base_id, mod_ep.compute_id());

    // Changed evaluation hash alters query_id
    let mut mod_eval = base.clone();
    mod_eval.evaluation_hash = Some(ContentHash::from_bytes(b"eval 2"));
    assert_ne!(base_id, mod_eval.compute_id());

    // Identical scope reproduces exact same query_id
    let twin = base.clone();
    assert_eq!(base_id, twin.compute_id());
}

#[test]
fn test_trace_continuation_full_pagination() {
    let count = 17;
    let (_root, workspace) = create_test_env();
    for index in 0..count {
        create_skill(
            &workspace.join(".claude/skills"),
            &format!("skill_{index:02}"),
            "Help diagnose and fix Rust tests",
            "Inspect tests and correct the implementation.",
        );
    }
    let context_file = create_context_file(&workspace, "Diagnose the failing Rust tests.");
    let mut sources = ConfigSources::default();
    sources
        .environment
        .push(("TYPESAFE_API_KEY".into(), "test-key-123".into()));
    sources.cli = vec![
        ("ranking.top".into(), RawValue::Integer(count as i64)),
        ("ranking.shortlist".into(), RawValue::Integer(count as i64)),
    ];
    let cache_dir = create_cache_dir();
    let gate = EffectGate::new(
        EffectFlags {
            offline: false,
            allow_network: true,
            dry_run: false,
            no_cache: false,
            no_ledger: true,
            no_persist: false,
            save_case: false,
        },
        Scope::Rank,
    )
    .unwrap();

    let transport = MockJevTransport::new(vec![
        Box::new(all_eligible_response),
        Box::new(all_eligible_response),
    ]);

    let run_with_cursor = |cursor: Option<TraceCursor>| {
        let clock = test_clock();
        let invocation = ProcessInvocation::from_clock(clock).unwrap();
        let cx = invocation.request_cx().unwrap();
        let args = RankArgs {
            workspace: workspace.clone(),
            user_config_root: None,
            home: None,
            cache_dir: Some(cache_dir.clone()),
            sources: sources.clone(),
            gate,
            source_options: SourceOptions {
                context: Some(LocalPath::new(context_file.clone())),
                ..Default::default()
            },
            require_skills: Vec::new(),
            shortlist_ids: Vec::new(),
            roster_file: None,
            explain: true,
            why_not: None,
            cursor,
            output_json: true,
            output_table: false,
            dry_run: false,
            save_case: None,
            ledger_dir: None,
        };
        invocation
            .runtime()
            .block_on(execute_pipeline(&invocation, &cx, args, Some(&transport)))
    };

    // Page 1: Initial request without cursor
    let doc1 = run_with_cursor(None).expect("initial page must succeed");
    let val1 = doc1.as_value();
    let trace1 = &val1["trace"];
    assert_eq!(trace1["total"], json!(136));
    assert_eq!(trace1["cursor"]["offset"], json!(0));
    assert_eq!(trace1["next_offset"], json!(128));
    let entries1 = trace1["entries"].as_array().unwrap();
    assert_eq!(entries1.len(), 128);

    let next_cursor_token = trace1["next_cursor"]
        .as_str()
        .expect("next_cursor must be emitted");
    let next_cursor = TraceCursor::from_token(next_cursor_token).expect("token must parse");
    assert_eq!(next_cursor.offset, 128);

    // Page 2: Continuation request with cursor (serviced from cache with 0 new provider calls)
    let doc2 = run_with_cursor(Some(next_cursor)).expect("continuation page must succeed");
    let val2 = doc2.as_value();
    let trace2 = &val2["trace"];
    assert_eq!(trace2["total"], json!(136));
    assert_eq!(trace2["cursor"]["offset"], json!(128));
    assert!(trace2["next_offset"].is_null());
    assert!(trace2["next_cursor"].is_null());
    let entries2 = trace2["entries"].as_array().unwrap();
    assert_eq!(entries2.len(), 8);

    // Combined pages cover all 136 entries with complete coverage across all 8 stages
    let mut all_entries = Vec::new();
    all_entries.extend(entries1.clone());
    all_entries.extend(entries2.clone());
    assert_eq!(all_entries.len(), 136);

    // Verify exactly 2 provider requests occurred total (initial page only, continuation was 0)
    assert_eq!(
        transport.recorded_requests.lock().unwrap().len(),
        2,
        "continuation must make zero provider requests"
    );

    // Verify all 17 skills are present, each having 8 stages in proper order
    let returned_skills = doc1.as_value()["skills"].as_array().unwrap();
    assert_eq!(returned_skills.len(), count);
    for skill_val in returned_skills {
        let skill_id = skill_val["skill_id"].as_str().unwrap();
        let skill_entries: Vec<_> = all_entries
            .iter()
            .filter(|e| e["skill_id"] == skill_id)
            .collect();
        assert_eq!(
            skill_entries.len(),
            8,
            "skill {skill_id} must have 8 entries"
        );
        for (i, stage) in TraceStage::ALL.iter().enumerate() {
            assert_eq!(skill_entries[i]["stage"], stage.as_str());
            assert_eq!(skill_entries[i]["status"], "passed");
        }
    }
}

#[test]
fn test_trace_continuation_rejects_changed_snapshot() {
    let count = 17;
    let (_root, workspace) = create_test_env();
    for index in 0..count {
        create_skill(
            &workspace.join(".claude/skills"),
            &format!("skill_{index:02}"),
            "Help diagnose and fix Rust tests",
            "Inspect tests and correct the implementation.",
        );
    }
    let context_file = create_context_file(&workspace, "Diagnose the failing Rust tests.");
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
        cache_dir: None,
        sources: sources.clone(),
        gate,
        source_options: SourceOptions {
            context: Some(LocalPath::new(context_file.clone())),
            ..Default::default()
        },
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
        ledger_dir: None,
    };
    let doc = invocation
        .runtime()
        .block_on(execute_pipeline(&invocation, &cx, args, Some(&transport)))
        .expect("initial rank succeeds");

    let next_cursor_token = doc.as_value()["trace"]["next_cursor"].as_str().unwrap();
    let cursor = TraceCursor::from_token(next_cursor_token).unwrap();

    // Now alter the snapshot by adding a skill
    create_skill(
        &workspace.join(".claude/skills"),
        "skill_new",
        "Brand new skill",
        "Body of brand new skill",
    );

    // Attempting to resume with old cursor against changed snapshot must fail
    let clock2 = test_clock();
    let invocation2 = ProcessInvocation::from_clock(clock2).unwrap();
    let cx2 = invocation2.request_cx().unwrap();
    let transport2 = MockJevTransport::default();
    let args2 = RankArgs {
        workspace: workspace.clone(),
        user_config_root: None,
        home: None,
        cache_dir: None,
        sources: sources.clone(),
        gate,
        source_options: SourceOptions {
            context: Some(LocalPath::new(context_file)),
            ..Default::default()
        },
        require_skills: Vec::new(),
        shortlist_ids: Vec::new(),
        roster_file: None,
        explain: true,
        why_not: None,
        cursor: Some(cursor),
        output_json: true,
        output_table: false,
        dry_run: false,
        save_case: None,
        ledger_dir: None,
    };
    let doc = invocation2
        .runtime()
        .block_on(execute_pipeline(
            &invocation2,
            &cx2,
            args2,
            Some(&transport2),
        ))
        .expect("pipeline finishes with unavailable decision");
    assert_eq!(doc.kind(), OutputKind::Decision(Decision::Unavailable));
    assert_eq!(doc.exit_code(), CliExit::Roster);
    let val = doc.as_value();
    assert_eq!(val["error"]["code"], 5);
    assert_eq!(val["error"]["kind"], "roster-changed");
    assert!(
        val["error"]["message"]
            .as_str()
            .unwrap()
            .contains("roster or query changed")
    );
    assert_eq!(
        transport2.recorded_requests.lock().unwrap().len(),
        0,
        "continuation must make zero provider requests"
    );
}

#[test]
fn test_trace_continuation_rejects_changed_query() {
    let count = 17;
    let (_root, workspace) = create_test_env();
    for index in 0..count {
        create_skill(
            &workspace.join(".claude/skills"),
            &format!("skill_{index:02}"),
            "Help diagnose and fix Rust tests",
            "Inspect tests and correct the implementation.",
        );
    }
    let context_file = create_context_file(&workspace, "Diagnose the failing Rust tests.");
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
        cache_dir: None,
        sources: sources.clone(),
        gate,
        source_options: SourceOptions {
            context: Some(LocalPath::new(context_file)),
            ..Default::default()
        },
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
        ledger_dir: None,
    };
    let doc = invocation
        .runtime()
        .block_on(execute_pipeline(&invocation, &cx, args, Some(&transport)))
        .expect("initial rank succeeds");

    let next_cursor_token = doc.as_value()["trace"]["next_cursor"].as_str().unwrap();
    let cursor = TraceCursor::from_token(next_cursor_token).unwrap();

    // Changed query: different prompt text in context
    let changed_context = create_context_file(&workspace, "Completely different user prompt.");

    let clock2 = test_clock();
    let invocation2 = ProcessInvocation::from_clock(clock2).unwrap();
    let cx2 = invocation2.request_cx().unwrap();
    let transport2 = MockJevTransport::default();
    let args2 = RankArgs {
        workspace: workspace.clone(),
        user_config_root: None,
        home: None,
        cache_dir: None,
        sources: sources.clone(),
        gate,
        source_options: SourceOptions {
            context: Some(LocalPath::new(changed_context)),
            ..Default::default()
        },
        require_skills: Vec::new(),
        shortlist_ids: Vec::new(),
        roster_file: None,
        explain: true,
        why_not: None,
        cursor: Some(cursor.clone()),
        output_json: true,
        output_table: false,
        dry_run: false,
        save_case: None,
        ledger_dir: None,
    };
    let doc = invocation2
        .runtime()
        .block_on(execute_pipeline(
            &invocation2,
            &cx2,
            args2,
            Some(&transport2),
        ))
        .expect("pipeline finishes with unavailable decision");
    assert_eq!(doc.kind(), OutputKind::Decision(Decision::Unavailable));
    assert!(
        doc.exit_code() == CliExit::Roster || doc.exit_code() == CliExit::CacheMiss,
        "unexpected exit code: {:?}",
        doc.exit_code()
    );
    assert_eq!(
        transport2.recorded_requests.lock().unwrap().len(),
        0,
        "continuation must make zero provider requests"
    );
    let val = doc.as_value();
    assert!(
        val["error"]["kind"] == "roster-changed" || val["error"]["kind"] == "cache-miss",
        "unexpected error kind: {:?}",
        val["error"]
    );
}

#[test]
fn test_trace_continuation_rejects_out_of_bounds_offset() {
    let count = 17;
    let (_root, workspace) = create_test_env();
    for index in 0..count {
        create_skill(
            &workspace.join(".claude/skills"),
            &format!("skill_{index:02}"),
            "Help diagnose and fix Rust tests",
            "Inspect tests and correct the implementation.",
        );
    }
    let context_file = create_context_file(&workspace, "Diagnose the failing Rust tests.");
    let mut sources = ConfigSources::default();
    sources
        .environment
        .push(("TYPESAFE_API_KEY".into(), "test-key-123".into()));
    sources.cli = vec![
        ("ranking.top".into(), RawValue::Integer(count as i64)),
        ("ranking.shortlist".into(), RawValue::Integer(count as i64)),
    ];
    let cache_dir = create_cache_dir();
    let gate = EffectGate::new(
        EffectFlags {
            offline: false,
            allow_network: true,
            dry_run: false,
            no_cache: false,
            no_ledger: true,
            no_persist: false,
            save_case: false,
        },
        Scope::Rank,
    )
    .unwrap();

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
        cache_dir: Some(cache_dir.clone()),
        sources: sources.clone(),
        gate,
        source_options: SourceOptions {
            context: Some(LocalPath::new(context_file.clone())),
            ..Default::default()
        },
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
        ledger_dir: None,
    };
    let doc = invocation
        .runtime()
        .block_on(execute_pipeline(&invocation, &cx, args, Some(&transport)))
        .expect("initial rank succeeds");

    let next_cursor_token = doc.as_value()["trace"]["next_cursor"].as_str().unwrap();
    let mut cursor = TraceCursor::from_token(next_cursor_token).unwrap();
    // Set offset past total (total is 136)
    cursor.offset = 9999;

    let clock2 = test_clock();
    let invocation2 = ProcessInvocation::from_clock(clock2).unwrap();
    let cx2 = invocation2.request_cx().unwrap();
    let transport2 = MockJevTransport::default();
    let args2 = RankArgs {
        workspace: workspace.clone(),
        user_config_root: None,
        home: None,
        cache_dir: Some(cache_dir),
        sources: sources.clone(),
        gate,
        source_options: SourceOptions {
            context: Some(LocalPath::new(context_file)),
            ..Default::default()
        },
        require_skills: Vec::new(),
        shortlist_ids: Vec::new(),
        roster_file: None,
        explain: true,
        why_not: None,
        cursor: Some(cursor),
        output_json: true,
        output_table: false,
        dry_run: false,
        save_case: None,
        ledger_dir: None,
    };
    let doc = invocation2
        .runtime()
        .block_on(execute_pipeline(
            &invocation2,
            &cx2,
            args2,
            Some(&transport2),
        ))
        .expect("pipeline finishes with unavailable decision");
    assert_eq!(doc.kind(), OutputKind::Decision(Decision::Unavailable));
    assert_eq!(doc.exit_code(), CliExit::Usage);
    let val = doc.as_value();
    assert_eq!(val["error"]["code"], 2);
    assert_eq!(val["error"]["kind"], "invalid-usage");
    assert!(
        val["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Cursor offset exceeds total")
    );
    assert_eq!(
        transport2.recorded_requests.lock().unwrap().len(),
        0,
        "continuation must make zero provider requests"
    );
}

#[test]
fn test_trace_continuation_rejects_changed_history() {
    let count = 17;
    let (_root, workspace) = create_test_env();
    for index in 0..count {
        create_skill(
            &workspace.join(".claude/skills"),
            &format!("skill_{index:02}"),
            "Help diagnose and fix Rust tests",
            "Inspect tests and correct the implementation.",
        );
    }
    let context_file = create_context_file(&workspace, "Diagnose the failing Rust tests.");
    let mut sources = ConfigSources::default();
    sources
        .environment
        .push(("TYPESAFE_API_KEY".into(), "test-key-123".into()));
    sources.cli = vec![
        ("ranking.top".into(), RawValue::Integer(count as i64)),
        ("ranking.shortlist".into(), RawValue::Integer(count as i64)),
    ];
    let cache_dir = create_cache_dir();
    let gate = EffectGate::new(
        EffectFlags {
            offline: false,
            allow_network: true,
            dry_run: false,
            no_cache: false,
            no_ledger: true,
            no_persist: false,
            save_case: false,
        },
        Scope::Rank,
    )
    .unwrap();
    let transport = MockJevTransport::new(vec![
        Box::new(all_eligible_response),
        Box::new(all_eligible_response),
    ]);

    let clock = test_clock();
    let invocation = ProcessInvocation::from_clock(clock).unwrap();
    let cx = invocation.request_cx().unwrap();
    let args = RankArgs {
        workspace: workspace.clone(),
        user_config_root: None,
        home: None,
        cache_dir: Some(cache_dir.clone()),
        sources: sources.clone(),
        gate,
        source_options: SourceOptions {
            context: Some(LocalPath::new(context_file.clone())),
            ..Default::default()
        },
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
        ledger_dir: None,
    };
    let doc = invocation
        .runtime()
        .block_on(execute_pipeline(&invocation, &cx, args, Some(&transport)))
        .expect("initial rank succeeds");

    let next_cursor_token = doc.as_value()["trace"]["next_cursor"].as_str().unwrap();
    let cursor = TraceCursor::from_token(next_cursor_token).unwrap();

    // Changed history: add an event to context.json with same prompt text
    let mut ctx_val: Value = serde_json::from_slice(&fs::read(&context_file).unwrap()).unwrap();
    let events = ctx_val["events"].as_array_mut().unwrap();
    events.push(json!({
        "event_id": "reply-1",
        "parent_id": "request-1",
        "turn_id": "turn-1",
        "agent_id": null,
        "branch_id": null,
        "role": "assistant",
        "kind": "message",
        "timestamp_unix_ms": null,
        "text": "Looking at the tests now.",
        "tool": null
    }));
    fs::write(&context_file, serde_json::to_vec(&ctx_val).unwrap()).unwrap();

    let transport2 = MockJevTransport::default();
    let clock2 = test_clock();
    let invocation2 = ProcessInvocation::from_clock(clock2).unwrap();
    let cx2 = invocation2.request_cx().unwrap();
    let args2 = RankArgs {
        workspace: workspace.clone(),
        user_config_root: None,
        home: None,
        cache_dir: Some(cache_dir),
        sources: sources.clone(),
        gate,
        source_options: SourceOptions {
            context: Some(LocalPath::new(context_file)),
            ..Default::default()
        },
        require_skills: Vec::new(),
        shortlist_ids: Vec::new(),
        roster_file: None,
        explain: true,
        why_not: None,
        cursor: Some(cursor),
        output_json: true,
        output_table: false,
        dry_run: false,
        save_case: None,
        ledger_dir: None,
    };
    let doc = invocation2
        .runtime()
        .block_on(execute_pipeline(
            &invocation2,
            &cx2,
            args2,
            Some(&transport2),
        ))
        .expect("pipeline finishes with unavailable decision");
    assert_eq!(doc.kind(), OutputKind::Decision(Decision::Unavailable));
    assert!(
        doc.exit_code() == CliExit::Roster || doc.exit_code() == CliExit::CacheMiss,
        "unexpected exit code: {:?}",
        doc.exit_code()
    );
    assert_eq!(
        transport2.recorded_requests.lock().unwrap().len(),
        0,
        "continuation must make zero provider requests"
    );
}

#[test]
fn test_trace_continuation_rejects_changed_provider_answer() {
    let count = 17;
    let (_root, workspace) = create_test_env();
    for index in 0..count {
        create_skill(
            &workspace.join(".claude/skills"),
            &format!("skill_{index:02}"),
            "Help diagnose and fix Rust tests",
            "Inspect tests and correct the implementation.",
        );
    }
    let context_file = create_context_file(&workspace, "Diagnose the failing Rust tests.");
    let mut sources = ConfigSources::default();
    sources
        .environment
        .push(("TYPESAFE_API_KEY".into(), "test-key-123".into()));
    sources.cli = vec![
        ("ranking.top".into(), RawValue::Integer(count as i64)),
        ("ranking.shortlist".into(), RawValue::Integer(count as i64)),
    ];
    let cache_dir = create_cache_dir();
    let gate = EffectGate::new(
        EffectFlags {
            offline: false,
            allow_network: true,
            dry_run: false,
            no_cache: false,
            no_ledger: true,
            no_persist: false,
            save_case: false,
        },
        Scope::Rank,
    )
    .unwrap();
    let transport = MockJevTransport::new(vec![
        Box::new(all_eligible_response),
        Box::new(all_eligible_response),
    ]);

    let clock = test_clock();
    let invocation = ProcessInvocation::from_clock(clock).unwrap();
    let cx = invocation.request_cx().unwrap();
    let args = RankArgs {
        workspace: workspace.clone(),
        user_config_root: None,
        home: None,
        cache_dir: Some(cache_dir.clone()),
        sources: sources.clone(),
        gate,
        source_options: SourceOptions {
            context: Some(LocalPath::new(context_file.clone())),
            ..Default::default()
        },
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
        ledger_dir: None,
    };
    let doc = invocation
        .runtime()
        .block_on(execute_pipeline(&invocation, &cx, args, Some(&transport)))
        .expect("initial rank succeeds");

    let next_cursor_token = doc.as_value()["trace"]["next_cursor"].as_str().unwrap();
    let cursor = TraceCursor::from_token(next_cursor_token).unwrap();

    // Modify the cached response in SQLite cache database to simulate a changed evaluation answer
    {
        let conn = rusqlite::Connection::open(cache_dir.join("cache.sqlite3")).unwrap();
        let low_need_bytes = serde_json::to_vec(&json!({
            "model": "jev-test",
            "answers": {
                "gate::context_suffices": {"type": "noul", "noul": 1.0},
                "gate::material_help": {"type": "noul", "noul": 0.0},
                "gate::specialized_method": {"type": "noul", "noul": 0.0}
            },
            "usage": {"input_tokens": 100, "output_tokens": 25}
        }))
        .unwrap();
        conn.execute(
            "UPDATE sr_cache_response SET response = ?1 WHERE stage = 'wide'",
            [&low_need_bytes],
        )
        .unwrap();
    }

    let transport2 = MockJevTransport::default();
    let clock2 = test_clock();
    let invocation2 = ProcessInvocation::from_clock(clock2).unwrap();
    let cx2 = invocation2.request_cx().unwrap();
    let args2 = RankArgs {
        workspace: workspace.clone(),
        user_config_root: None,
        home: None,
        cache_dir: Some(cache_dir),
        sources: sources.clone(),
        gate,
        source_options: SourceOptions {
            context: Some(LocalPath::new(context_file)),
            ..Default::default()
        },
        require_skills: Vec::new(),
        shortlist_ids: Vec::new(),
        roster_file: None,
        explain: true,
        why_not: None,
        cursor: Some(cursor),
        output_json: true,
        output_table: false,
        dry_run: false,
        save_case: None,
        ledger_dir: None,
    };
    let doc = invocation2
        .runtime()
        .block_on(execute_pipeline(
            &invocation2,
            &cx2,
            args2,
            Some(&transport2),
        ))
        .expect("pipeline finishes with unavailable decision");
    assert_eq!(doc.kind(), OutputKind::Decision(Decision::Unavailable));
    assert!(
        doc.exit_code() == CliExit::Roster || doc.exit_code() == CliExit::CacheMiss,
        "unexpected exit code: {:?}",
        doc.exit_code()
    );
    let val = doc.as_value();
    let err_kind = val["error"]["kind"].as_str().unwrap_or_default();
    assert!(
        err_kind == "roster-changed" || err_kind == "cache-miss",
        "unexpected error kind: {err_kind}"
    );
    let err_code = val["error"]["code"].as_i64().unwrap_or_default();
    assert!(
        err_code == 5 || err_code == 11,
        "unexpected error code: {err_code}"
    );
    assert_eq!(
        transport2.recorded_requests.lock().unwrap().len(),
        0,
        "continuation must make zero provider requests"
    );
}

#[test]
fn test_trace_continuation_rejects_changed_model_or_policy() {
    let count = 17;
    let (_root, workspace) = create_test_env();
    for index in 0..count {
        create_skill(
            &workspace.join(".claude/skills"),
            &format!("skill_{index:02}"),
            "Help diagnose and fix Rust tests",
            "Inspect tests and correct the implementation.",
        );
    }
    let context_file = create_context_file(&workspace, "Diagnose the failing Rust tests.");
    let mut sources = ConfigSources::default();
    sources
        .environment
        .push(("TYPESAFE_API_KEY".into(), "test-key-123".into()));
    sources.cli = vec![
        ("ranking.top".into(), RawValue::Integer(count as i64)),
        ("ranking.shortlist".into(), RawValue::Integer(count as i64)),
        ("ranking.gate".into(), RawValue::Float(0.30)),
    ];
    let cache_dir = create_cache_dir();
    let gate = EffectGate::new(
        EffectFlags {
            offline: false,
            allow_network: true,
            dry_run: false,
            no_cache: false,
            no_ledger: true,
            no_persist: false,
            save_case: false,
        },
        Scope::Rank,
    )
    .unwrap();
    let transport = MockJevTransport::new(vec![
        Box::new(all_eligible_response),
        Box::new(all_eligible_response),
    ]);

    let clock = test_clock();
    let invocation = ProcessInvocation::from_clock(clock).unwrap();
    let cx = invocation.request_cx().unwrap();
    let args = RankArgs {
        workspace: workspace.clone(),
        user_config_root: None,
        home: None,
        cache_dir: Some(cache_dir.clone()),
        sources: sources.clone(),
        gate,
        source_options: SourceOptions {
            context: Some(LocalPath::new(context_file.clone())),
            ..Default::default()
        },
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
        ledger_dir: None,
    };
    let doc = invocation
        .runtime()
        .block_on(execute_pipeline(&invocation, &cx, args, Some(&transport)))
        .expect("initial rank succeeds");

    let next_cursor_token = doc.as_value()["trace"]["next_cursor"].as_str().unwrap();
    let cursor = TraceCursor::from_token(next_cursor_token).unwrap();

    // Changed policy: gate threshold changes to 0.70
    let mut sources2 = sources.clone();
    sources2.cli = vec![
        ("ranking.top".into(), RawValue::Integer(count as i64)),
        ("ranking.shortlist".into(), RawValue::Integer(count as i64)),
        ("ranking.gate".into(), RawValue::Float(0.70)),
    ];

    let transport2 = MockJevTransport::default();
    let clock2 = test_clock();
    let invocation2 = ProcessInvocation::from_clock(clock2).unwrap();
    let cx2 = invocation2.request_cx().unwrap();
    let args2 = RankArgs {
        workspace: workspace.clone(),
        user_config_root: None,
        home: None,
        cache_dir: Some(cache_dir),
        sources: sources2,
        gate,
        source_options: SourceOptions {
            context: Some(LocalPath::new(context_file)),
            ..Default::default()
        },
        require_skills: Vec::new(),
        shortlist_ids: Vec::new(),
        roster_file: None,
        explain: true,
        why_not: None,
        cursor: Some(cursor),
        output_json: true,
        output_table: false,
        dry_run: false,
        save_case: None,
        ledger_dir: None,
    };
    let doc = invocation2
        .runtime()
        .block_on(execute_pipeline(
            &invocation2,
            &cx2,
            args2,
            Some(&transport2),
        ))
        .expect("pipeline finishes with unavailable decision");
    assert_eq!(doc.kind(), OutputKind::Decision(Decision::Unavailable));
    assert!(
        doc.exit_code() == CliExit::Roster || doc.exit_code() == CliExit::CacheMiss,
        "unexpected exit code: {:?}",
        doc.exit_code()
    );
    assert_eq!(
        transport2.recorded_requests.lock().unwrap().len(),
        0,
        "continuation must make zero provider requests"
    );
}

#[test]
fn test_trace_continuation_refuses_when_no_cache() {
    let count = 17;
    let (_root, workspace) = create_test_env();
    for index in 0..count {
        create_skill(
            &workspace.join(".claude/skills"),
            &format!("skill_{index:02}"),
            "Help diagnose and fix Rust tests",
            "Inspect tests and correct the implementation.",
        );
    }
    let context_file = create_context_file(&workspace, "Diagnose the failing Rust tests.");
    let mut sources = ConfigSources::default();
    sources
        .environment
        .push(("TYPESAFE_API_KEY".into(), "test-key-123".into()));
    sources.cli = vec![
        ("ranking.top".into(), RawValue::Integer(count as i64)),
        ("ranking.shortlist".into(), RawValue::Integer(count as i64)),
    ];
    // No-cache gate
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
    let transport = MockJevTransport::new(vec![
        Box::new(all_eligible_response),
        Box::new(all_eligible_response),
    ]);

    let clock = test_clock();
    let invocation = ProcessInvocation::from_clock(clock).unwrap();
    let cx = invocation.request_cx().unwrap();
    let args = RankArgs {
        workspace: workspace.clone(),
        user_config_root: None,
        home: None,
        cache_dir: None,
        sources: sources.clone(),
        gate,
        source_options: SourceOptions {
            context: Some(LocalPath::new(context_file.clone())),
            ..Default::default()
        },
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
        ledger_dir: None,
    };
    let doc = invocation
        .runtime()
        .block_on(execute_pipeline(&invocation, &cx, args, Some(&transport)))
        .expect("initial rank succeeds");

    let next_cursor_token = doc.as_value()["trace"]["next_cursor"].as_str().unwrap();
    let cursor = TraceCursor::from_token(next_cursor_token).unwrap();

    // Now resume with cursor: since cache is disabled/empty, it must refuse with cache-miss (exit 11)
    // without making ANY provider requests.
    let transport2 = MockJevTransport::default();
    let clock2 = test_clock();
    let invocation2 = ProcessInvocation::from_clock(clock2).unwrap();
    let cx2 = invocation2.request_cx().unwrap();
    let args2 = RankArgs {
        workspace: workspace.clone(),
        user_config_root: None,
        home: None,
        cache_dir: None,
        sources,
        gate,
        source_options: SourceOptions {
            context: Some(LocalPath::new(context_file)),
            ..Default::default()
        },
        require_skills: Vec::new(),
        shortlist_ids: Vec::new(),
        roster_file: None,
        explain: true,
        why_not: None,
        cursor: Some(cursor),
        output_json: true,
        output_table: false,
        dry_run: false,
        save_case: None,
        ledger_dir: None,
    };
    let doc = invocation2
        .runtime()
        .block_on(execute_pipeline(
            &invocation2,
            &cx2,
            args2,
            Some(&transport2),
        ))
        .expect("pipeline finishes with unavailable decision");
    assert_eq!(doc.kind(), OutputKind::Decision(Decision::Unavailable));
    assert_eq!(doc.exit_code(), CliExit::CacheMiss);
    let val = doc.as_value();
    assert_eq!(val["error"]["code"], 11);
    assert_eq!(val["error"]["kind"], "cache-miss");
    assert_eq!(
        transport2.recorded_requests.lock().unwrap().len(),
        0,
        "continuation must make zero provider requests"
    );
}

#[test]
fn test_trace_continuation_explicit_local_success() {
    let count = 17;
    let (_root, workspace) = create_test_env();
    let mut skill_ids = Vec::new();
    for index in 0..count {
        let name = format!("skill_{index:02}");
        create_skill(
            &workspace.join(".claude/skills"),
            &name,
            "Help diagnose and fix Rust tests",
            "Inspect tests and correct the implementation.",
        );
        skill_ids.push(SkillId::new(&name).unwrap());
    }
    let context_file = create_context_file(&workspace, "Diagnose the failing Rust tests.");
    let sources = ConfigSources::default();
    let gate = EffectGate::new(
        EffectFlags {
            offline: false,
            allow_network: false,
            dry_run: false,
            no_cache: true,
            no_ledger: true,
            no_persist: true,
            save_case: false,
        },
        Scope::Rank,
    )
    .unwrap();
    let transport = MockJevTransport::default();

    let clock = test_clock();
    let invocation = ProcessInvocation::from_clock(clock).unwrap();
    let cx = invocation.request_cx().unwrap();
    let args = RankArgs {
        workspace: workspace.clone(),
        user_config_root: None,
        home: None,
        cache_dir: None,
        sources: sources.clone(),
        gate,
        source_options: SourceOptions {
            context: Some(LocalPath::new(context_file.clone())),
            ..Default::default()
        },
        require_skills: skill_ids.clone(),
        shortlist_ids: Vec::new(),
        roster_file: None,
        explain: true,
        why_not: None,
        cursor: None,
        output_json: true,
        output_table: false,
        dry_run: false,
        save_case: None,
        ledger_dir: None,
    };
    let doc = invocation
        .runtime()
        .block_on(execute_pipeline(&invocation, &cx, args, Some(&transport)))
        .expect("explicit rank succeeds");
    assert_eq!(doc.kind(), OutputKind::Decision(Decision::Explicit));
    assert_eq!(doc.exit_code(), CliExit::Success);
    assert_eq!(
        transport.recorded_requests.lock().unwrap().len(),
        0,
        "explicit local resolution must make zero provider requests"
    );

    let trace1 = &doc.as_value()["trace"];
    assert_eq!(trace1["total"], 136); // 17 skills * 8 stages
    let entries1 = trace1["entries"].as_array().unwrap();
    assert_eq!(entries1.len(), 128);

    let next_cursor_token = trace1["next_cursor"].as_str().unwrap();
    let cursor = TraceCursor::from_token(next_cursor_token).unwrap();
    assert_eq!(cursor.offset, 128);

    // Second page with cursor
    let transport2 = MockJevTransport::default();
    let clock2 = test_clock();
    let invocation2 = ProcessInvocation::from_clock(clock2).unwrap();
    let cx2 = invocation2.request_cx().unwrap();
    let args2 = RankArgs {
        workspace: workspace.clone(),
        user_config_root: None,
        home: None,
        cache_dir: None,
        sources,
        gate,
        source_options: SourceOptions {
            context: Some(LocalPath::new(context_file)),
            ..Default::default()
        },
        require_skills: skill_ids,
        shortlist_ids: Vec::new(),
        roster_file: None,
        explain: true,
        why_not: None,
        cursor: Some(cursor),
        output_json: true,
        output_table: false,
        dry_run: false,
        save_case: None,
        ledger_dir: None,
    };
    let doc2 = invocation2
        .runtime()
        .block_on(execute_pipeline(
            &invocation2,
            &cx2,
            args2,
            Some(&transport2),
        ))
        .expect("explicit rank page 2 succeeds");
    assert_eq!(doc2.kind(), OutputKind::Decision(Decision::Explicit));
    assert_eq!(doc2.exit_code(), CliExit::Success);
    assert_eq!(
        transport2.recorded_requests.lock().unwrap().len(),
        0,
        "continuation of explicit local resolution must make zero provider requests"
    );

    let trace2 = &doc2.as_value()["trace"];
    assert_eq!(trace2["total"], 136);
    let entries2 = trace2["entries"].as_array().unwrap();
    assert_eq!(entries2.len(), 8);
    assert!(trace2["next_offset"].is_null());
    assert!(trace2["next_cursor"].is_null());
}
