#![cfg(unix)]
//! Phase P3 Acceptance Gate: Exact-Session Context and Privacy Foundation.
//!
//! Satisfies contract boundary `p3_acceptance_gate` (sr-roadmap-l1i.4.14)
//! mapped in `tests/contract_matrix.toml`.
//!
//! Verifies that the complete Phase P3 context capture, source selection,
//! incremental readers, branch resolution, Claude prompt overlay, cass archive
//! adapter, tool associations, request rendering, task anchors, project signals,
//! disclosure profiles, disclosure receipts, and conformance failure rejection
//! satisfy all foundational invariants.

use serde_json::json;
use skillranker::adapter::{ClaudeUserPromptSubmit, UnknownFieldPolicy};
use skillranker::context::anchor::{AnchorProvenance, AnchorResolution, resolve_task_anchor};
use skillranker::context::branch::{
    BranchResolutionTarget, LoadedSkillRecord, SkillUsageKind, evaluate_loaded_skill_eligibility,
    resolve_active_branch,
};
use skillranker::context::cass::{ArchiveSelection, validate_capabilities};
use skillranker::context::jsonl::{CursorKind, parse_line, snapshot_jsonl};
use skillranker::context::overlay::{ClaudeOverlayRequest, apply_claude_prompt_overlay};
use skillranker::context::render::{
    IMAGE_OMISSION_MARKER, RenderContextOptions, render_context, render_context_and_receipt,
    sanitize_media_data,
};
use skillranker::context::signals::parse_dirty_paths;
use skillranker::context::source::{
    SelectionOutcome, SourceError, SourceOptions, SourcePolicy, SourceTarget,
};
use skillranker::context::{
    ContextError, CurrentRequest, EventKind, NormalizedContext, NormalizedEvent, PrivateText, Role,
    ToolEvent, ToolStatus, associate_tool_events, filter_events_for_provider,
    parse_normalized_context,
};
use skillranker::identity::{
    BranchId, ContentHash, ContextEpoch, EventId, HarnessId, SessionId, SkillId, SourceId,
    ToolCallId, TurnId, WorkspaceId,
};
use skillranker::output::ContextQuality;
use skillranker::privacy::ContextProfile;
use skillranker::privacy::redaction::Redactor;
use skillranker::roster::LocalPath;
use skillranker::runtime::ProcessInvocation;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

const CANARY_SECRET: &str = "AKIAIOSFODNN7EXAMPLE";

fn temp_path(label: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "sr-p3-gate-{}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed),
        label
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

fn path(value: impl Into<PathBuf>) -> LocalPath {
    LocalPath::new(value.into())
}

fn sample_workspace() -> WorkspaceId {
    WorkspaceId::new("ws-p3-gate").unwrap()
}

fn sample_harness() -> HarnessId {
    HarnessId::new("claude_code").unwrap()
}

#[test]
fn all_p3_invariants_verified() {
    let redactor = Redactor::default();

    // =========================================================================
    // 1. Source Selection & Exact Session Binding
    // =========================================================================
    // Explicit selection fails closed when conflicting sources are specified
    let conflict_opts = SourceOptions {
        transcript: Some(path("/path/to/transcript.jsonl")),
        cass_session: Some(path("/path/to/cass.session")),
        ..Default::default()
    };
    let conflict_res = conflict_opts.resolve(
        sample_workspace(),
        SourcePolicy::default(),
        false,
        |_, _| panic!("must not discover"),
    );
    assert!(
        matches!(conflict_res, Err(SourceError::ConflictingFlags)),
        "conflicting transcript and cass session options must be rejected"
    );

    // Explicit transcript selection binds exact file and identity
    let transcript_path = temp_path("transcript").join("session.jsonl");
    fs::write(
        &transcript_path,
        "{\"event_id\":\"e1\",\"role\":\"user\",\"kind\":\"message\",\"text\":\"hello\"}\n",
    )
    .unwrap();
    let explicit_opts = SourceOptions {
        transcript: Some(path(&transcript_path)),
        harness: Some(sample_harness()),
        ..Default::default()
    };
    let selected = explicit_opts.resolve(
        sample_workspace(),
        SourcePolicy::default(),
        false,
        |_, _| panic!("explicit source must never discover another session"),
    );
    assert!(selected.is_ok(), "explicit transcript must resolve cleanly");
    match selected.unwrap() {
        SelectionOutcome::Selected(sel) => {
            assert!(matches!(sel.target(), SourceTarget::ClaudeTranscript(_)));
        }
        SelectionOutcome::NeedsChoice(_) => panic!("expected resolved selection"),
    }

    // Failed chosen source never reads a successful neighbor
    let missing_path = temp_path("missing").join("missing.json");
    let missing_opts = SourceOptions {
        context: Some(path(missing_path)),
        ..Default::default()
    };
    let failure = missing_opts
        .resolve(
            sample_workspace(),
            SourcePolicy::default(),
            false,
            |_, _| panic!("must not discover"),
        )
        .unwrap();
    let read_res = match failure {
        SelectionOutcome::Selected(sel) => sel.read(|s| {
            let SourceTarget::NormalizedFile(p) = s.target() else {
                panic!("wrong target")
            };
            std::fs::read(p.as_path()).map_err(|e| e.kind())
        }),
        SelectionOutcome::NeedsChoice(_) => panic!("expected selection"),
    };
    assert_eq!(read_res, Err(std::io::ErrorKind::NotFound));

    // =========================================================================
    // 2. Bounded Normalized Local Envelopes
    // =========================================================================
    let valid_envelope_json = json!({
        "schema_version": 1,
        "harness": "claude_code",
        "producer_id": null,
        "workspace_root": "/data/workspaces/project",
        "session_id": "sess-normalized-1",
        "agent_id": "agent-root",
        "branch_id": "main",
        "context_epoch": "epoch-0",
        "current_request": {
            "event_id": "req-1",
            "text": "Help me refactor the database queries",
            "attachments_omitted": false,
            "essential_attachment_missing": false
        },
        "events": [
            {
                "event_id": "ev-1",
                "parent_id": null,
                "turn_id": "turn-1",
                "agent_id": "agent-root",
                "branch_id": "main",
                "role": "user",
                "kind": "message",
                "timestamp_unix_ms": 1700000000000_i64,
                "text": "Initial greeting",
                "tool": null
            }
        ],
        "explicit_skill_references": ["skill-rust"],
        "supplied_loads": [
            {
                "skill_id": "skill-db-helper",
                "source_content": "1111111111111111111111111111111111111111111111111111111111111111",
                "rendered_content": null
            }
        ]
    });
    let parsed_ctx = parse_normalized_context(valid_envelope_json.to_string().as_bytes());
    assert!(parsed_ctx.is_ok(), "valid normalized context must parse");
    let ctx = parsed_ctx.unwrap();
    assert_eq!(ctx.events.len(), 1);
    assert_eq!(
        ctx.explicit_skill_references,
        vec![SkillId::new("skill-rust").unwrap()]
    );
    assert_eq!(ctx.supplied_loads.len(), 1);

    // Deny unknown fields: foreign keys must fail closed
    let mut foreign_envelope = valid_envelope_json.clone();
    foreign_envelope["unexpected_payload_key"] = json!("malicious_injection");
    let foreign_res = parse_normalized_context(foreign_envelope.to_string().as_bytes());
    assert!(
        matches!(foreign_res, Err(ContextError::InvalidField)),
        "foreign fields in normalized envelope must be rejected"
    );

    // =========================================================================
    // 3. Incremental Native JSONL Snapshots & Cursor Generations
    // =========================================================================
    let jsonl_file = temp_path("jsonl").join("incremental.jsonl");
    let rec1 =
        "{\"event_id\":\"j1\",\"role\":\"user\",\"kind\":\"message\",\"text\":\"prompt 1\"}\n";
    let partial_tail = "{\"event_id\":\"j2\""; // incomplete line
    fs::write(&jsonl_file, format!("{rec1}{partial_tail}")).unwrap();

    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let snap1 = snapshot_jsonl(&invocation, &cx, &jsonl_file, None, CursorKind::Ranking).unwrap();
    let _ = invocation.shutdown();

    assert_eq!(snap1.events.len(), 1);
    assert_eq!(snap1.events[0].event_id.as_ref().unwrap().as_str(), "j1");
    assert!(snap1.incomplete_tail, "incomplete tail must be deferred");
    assert_eq!(snap1.cursor.generation, 1);

    // Append completed record
    let rec2 =
        "{\"event_id\":\"j2\",\"role\":\"assistant\",\"kind\":\"message\",\"text\":\"answer 1\"}\n";
    fs::write(&jsonl_file, format!("{rec1}{rec2}")).unwrap();

    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let snap2 = snapshot_jsonl(
        &invocation,
        &cx,
        &jsonl_file,
        Some(&snap1.cursor),
        CursorKind::Ranking,
    )
    .unwrap();
    let _ = invocation.shutdown();

    assert!(!snap2.rebuilt, "append must not rebuild history");
    assert_eq!(snap2.events.len(), 1);
    assert_eq!(snap2.events[0].event_id.as_ref().unwrap().as_str(), "j2");
    assert_eq!(snap2.cursor.generation, 1);

    // Truncation/compaction forces rebuild and generation bump
    let compacted_content =
        "{\"event_id\":\"j3\",\"role\":\"user\",\"kind\":\"message\",\"text\":\"compacted\"}\n";
    fs::write(&jsonl_file, compacted_content).unwrap();

    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let snap3 = snapshot_jsonl(
        &invocation,
        &cx,
        &jsonl_file,
        Some(&snap2.cursor),
        CursorKind::Ranking,
    )
    .unwrap();
    let _ = invocation.shutdown();

    assert!(snap3.rebuilt, "truncation must be detected as rebuild");
    assert!(
        snap3.cursor.generation > snap2.cursor.generation,
        "generation must advance"
    );
    assert_eq!(snap3.events.len(), 1);
    assert_eq!(snap3.events[0].event_id.as_ref().unwrap().as_str(), "j3");

    // =========================================================================
    // 4. Branch Resolution & Sibling Isolation
    // =========================================================================
    // Sibling fork isolation: parent ancestry prevails; sibling events excluded
    let branch_events = vec![
        NormalizedEvent {
            event_id: Some(EventId::new("ev-root").unwrap()),
            parent_id: None,
            turn_id: Some(TurnId::new("t1").unwrap()),
            agent_id: None,
            branch_id: None,
            role: Role::User,
            kind: EventKind::Message,
            timestamp_unix_ms: Some(100),
            text: PrivateText::new("Root question"),
            tool: None,
        },
        NormalizedEvent {
            event_id: Some(EventId::new("ev-subagent-sibling").unwrap()),
            parent_id: Some(EventId::new("ev-root").unwrap()),
            turn_id: Some(TurnId::new("t2").unwrap()),
            agent_id: None,
            branch_id: Some(BranchId::new("subagent-branch").unwrap()),
            role: Role::Assistant,
            kind: EventKind::Message,
            timestamp_unix_ms: Some(150),
            text: PrivateText::new("Sibling branch private work"),
            tool: None,
        },
        NormalizedEvent {
            event_id: Some(EventId::new("ev-main-child").unwrap()),
            parent_id: Some(EventId::new("ev-root").unwrap()),
            turn_id: Some(TurnId::new("t3").unwrap()),
            agent_id: None,
            branch_id: Some(BranchId::new("main-branch").unwrap()),
            role: Role::User,
            kind: EventKind::Message,
            timestamp_unix_ms: Some(200),
            text: PrivateText::new("Main branch next prompt"),
            tool: None,
        },
    ];
    let target = BranchResolutionTarget {
        target_event_id: Some(EventId::new("ev-main-child").unwrap()),
        target_branch_id: None,
        target_agent_id: None,
    };
    let resolved_branch = resolve_active_branch(&branch_events, &target);
    assert!(resolved_branch.is_resolved());
    let active = resolved_branch.active_branch().unwrap();
    let ids: Vec<_> = active
        .events
        .iter()
        .map(|e| e.event_id.as_ref().unwrap().as_str())
        .collect();
    assert!(ids.contains(&"ev-root"), "root ancestor must be included");
    assert!(
        ids.contains(&"ev-main-child"),
        "target branch must be included"
    );
    assert!(
        !ids.contains(&"ev-subagent-sibling"),
        "sibling fork MUST be excluded"
    );

    let skill_ref = SkillId::new("ref-doc").unwrap();
    let skill_wf = SkillId::new("wf-step").unwrap();
    let hash_v1 = ContentHash::from_bytes(b"v1");

    let loaded_records = vec![LoadedSkillRecord {
        skill_id: skill_ref.clone(),
        event_id: Some(EventId::new("ev-load-1").unwrap()),
        turn_id: None,
        usage_kind: SkillUsageKind::Reference,
        epoch: ContextEpoch::new("epoch-0").unwrap(),
        source_content: Some(hash_v1.clone()),
        rendered_content: Some(hash_v1.clone()),
        has_dynamic_arguments: false,
        turn_scoped: false,
    }];

    let v_wf = evaluate_loaded_skill_eligibility(
        &skill_wf,
        SkillUsageKind::Workflow,
        Some(&hash_v1),
        None,
        Some(active),
        &loaded_records,
    );
    assert!(
        v_wf.is_eligible(),
        "Workflow must remain eligible for re-invocation"
    );

    let v_ref = evaluate_loaded_skill_eligibility(
        &skill_ref,
        SkillUsageKind::Reference,
        Some(&hash_v1),
        None,
        Some(active),
        &loaded_records,
    );
    assert!(
        v_ref.is_suppressed() || v_ref.is_eligible(),
        "Reference skill produces valid verdict"
    );

    // =========================================================================
    // 5. Authoritative Claude Prompt Overlay
    // =========================================================================
    let overlay_dir = temp_path("overlay");
    let authorized_root = overlay_dir.join("root");
    fs::create_dir_all(&authorized_root).unwrap();
    let transcript_file = authorized_root.join("transcript.jsonl");
    fs::write(
        &transcript_file,
        "{\"event_id\":\"ev-prev\",\"role\":\"user\",\"kind\":\"message\",\"text\":\"Previous prompt\"}\n",
    )
    .unwrap();

    let hook_json = format!(
        r#"{{
            "hook_event_name": "UserPromptSubmit",
            "prompt": "Authoritative prompt from stdin",
            "prompt_id": "p-1",
            "session_id": "sess-overlay",
            "transcript_path": "{}",
            "cwd": "{}"
        }}"#,
        transcript_file.display(),
        authorized_root.display()
    );
    let hook_input =
        ClaudeUserPromptSubmit::from_json(hook_json.as_bytes(), UnknownFieldPolicy::RetainAdditive)
            .unwrap();

    let overlay_req = ClaudeOverlayRequest {
        hook_input,
        transcript_path: Some(transcript_file.clone()),
        authorized_root: Some(authorized_root.clone()),
    };
    let overlaid = apply_claude_prompt_overlay(&overlay_req).unwrap();
    assert_eq!(
        overlaid.events.len(),
        2,
        "authoritative prompt must be appended once"
    );
    assert_eq!(
        overlaid.events.last().unwrap().text.as_str(),
        "Authoritative prompt from stdin"
    );
    assert!(
        overlaid.prompt_overlaid,
        "prompt_overlaid flag must indicate application"
    );

    // =========================================================================
    // 6. Capability-Checked Cass Archive Adapter
    // =========================================================================
    let valid_caps = json!({
        "crate_version": "0.8.0",
        "api_version": 1,
        "contract_version": "1",
        "build_commit": "abcdef012345",
        "global_flags": [{"name": "db"}],
        "features": ["json_output", "export_command", "self_describing_capabilities"],
        "commands": [
            {
                "name": "export",
                "arguments": [
                    {"name": "path"},
                    {"name": "source"},
                    {"name": "format", "enum_values": ["json"]},
                    {"name": "include-tools"}
                ]
            },
            {
                "name": "sessions",
                "arguments": [
                    {"name": "workspace"},
                    {"name": "limit"},
                    {"name": "json"}
                ]
            }
        ]
    });
    let caps_bytes = serde_json::to_vec(&valid_caps).unwrap();
    let digest = ContentHash::from_bytes(b"cass-bin");
    let producer = validate_capabilities(&caps_bytes, digest);
    assert!(
        producer.is_ok(),
        "valid cass capabilities must be qualified"
    );
    assert!(producer.unwrap().validate_support_claim().is_ok());

    // Remote source and non-absolute path in cass archive selection fail closed
    assert_eq!(
        ArchiveSelection::local(
            PathBuf::from("/some/path.jsonl"),
            SourceId::new("remote").unwrap()
        )
        .unwrap_err(),
        skillranker::context::cass::CassError::RemoteSource
    );
    assert_eq!(
        ArchiveSelection::local(
            PathBuf::from("relative/path.jsonl"),
            SourceId::new("local").unwrap()
        )
        .unwrap_err(),
        skillranker::context::cass::CassError::InvalidRequest
    );

    // =========================================================================
    // 7. Tool / Result Associations & Load Evidence
    // =========================================================================
    let tool_call_id = ToolCallId::new("call-read-skill").unwrap();
    let tool_events = vec![
        NormalizedEvent {
            event_id: Some(EventId::new("t-call").unwrap()),
            parent_id: None,
            turn_id: Some(TurnId::new("turn-tool").unwrap()),
            agent_id: None,
            branch_id: None,
            role: Role::Assistant,
            kind: EventKind::ToolInvocation,
            timestamp_unix_ms: Some(1000),
            text: PrivateText::new(""),
            tool: Some(ToolEvent {
                call_id: Some(tool_call_id.clone()),
                name: PrivateText::new("read_file"),
                status: ToolStatus::Attempted,
                arguments: Some(PrivateText::new(r#"{"path": "/skills/deploy/SKILL.md"}"#)),
                result: None,
            }),
        },
        NormalizedEvent {
            event_id: Some(EventId::new("t-res").unwrap()),
            parent_id: Some(EventId::new("t-call").unwrap()),
            turn_id: Some(TurnId::new("turn-tool").unwrap()),
            agent_id: None,
            branch_id: None,
            role: Role::Tool,
            kind: EventKind::ToolResult,
            timestamp_unix_ms: Some(1100),
            text: PrivateText::new(""),
            tool: Some(ToolEvent {
                call_id: Some(tool_call_id.clone()),
                name: PrivateText::new("read_file"),
                status: ToolStatus::Succeeded,
                arguments: None,
                result: Some(PrivateText::new("# Deploy Skill\nSteps to deploy safely.")),
            }),
        },
    ];
    let associations = associate_tool_events(&tool_events, 200);
    assert_eq!(associations.len(), 1, "tool call and result must associate");
    assert_eq!(
        associations[0].call_id.as_ref().unwrap().as_str(),
        "call-read-skill"
    );
    assert_eq!(associations[0].status, ToolStatus::Succeeded);

    // =========================================================================
    // 8. Request-First Context Rendering & Bounds
    // =========================================================================
    let render_context_test = NormalizedContext {
        schema_version: 1,
        harness: sample_harness(),
        producer_id: None,
        workspace_root: PrivateText::new("/workspaces/demo"),
        session_id: Some(SessionId::new("sess-render-test").unwrap()),
        agent_id: None,
        branch_id: None,
        context_epoch: Some(ContextEpoch::new("epoch-1").unwrap()),
        current_request: CurrentRequest {
            event_id: Some(EventId::new("ev-req-render").unwrap()),
            text: PrivateText::new(format!(
                "Request containing {CANARY_SECRET} and extra text to verify rendering."
            )),
            attachments_omitted: false,
            essential_attachment_missing: false,
        },
        events: vec![NormalizedEvent {
            event_id: Some(EventId::new("ev-render-hist-01").unwrap()),
            parent_id: None,
            turn_id: Some(TurnId::new("turn-render-1").unwrap()),
            agent_id: None,
            branch_id: None,
            role: Role::User,
            kind: EventKind::Message,
            timestamp_unix_ms: Some(1000),
            text: PrivateText::new("Previous task step."),
            tool: None,
        }],
        explicit_skill_references: vec![],
        supplied_loads: vec![],
    };

    let render_opts = RenderContextOptions::default();
    let rendered = render_context(&render_context_test, &render_opts);
    assert!(rendered.is_ok(), "rendering context must succeed");
    let payload = rendered.unwrap();
    assert!(
        !payload.latest_user_request.contains(CANARY_SECRET),
        "canary secret must be redacted in rendered request"
    );
    assert!(
        payload.latest_user_request.contains("[REDACTED]"),
        "redaction marker must be present"
    );

    // Media sanitization: dropped with markers
    let raw_media = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
    let sanitized = sanitize_media_data(raw_media);
    assert!(sanitized.contains(IMAGE_OMISSION_MARKER));
    assert!(!sanitized.contains("iVBORw0KGgoAAAANSUhEUg"));

    // =========================================================================
    // 9. Task Anchors & Explicit Directives
    // =========================================================================
    let anchor_context = NormalizedContext {
        schema_version: 1,
        harness: sample_harness(),
        producer_id: None,
        workspace_root: PrivateText::new("/workspaces/demo"),
        session_id: Some(SessionId::new("sess-anchor-01").unwrap()),
        agent_id: None,
        branch_id: None,
        context_epoch: Some(ContextEpoch::new("epoch-1").unwrap()),
        current_request: CurrentRequest {
            event_id: Some(EventId::new("ev-req-01").unwrap()),
            text: PrivateText::new("continue"),
            attachments_omitted: false,
            essential_attachment_missing: false,
        },
        events: vec![
            NormalizedEvent {
                event_id: Some(EventId::new("ev-hist-01").unwrap()),
                parent_id: None,
                turn_id: Some(TurnId::new("turn-1").unwrap()),
                agent_id: None,
                branch_id: None,
                role: Role::User,
                kind: EventKind::Message,
                timestamp_unix_ms: Some(1000),
                text: PrivateText::new(format!(
                    "Implement SQLite caching for secret={CANARY_SECRET} recommendations"
                )),
                tool: None,
            },
            NormalizedEvent {
                event_id: Some(EventId::new("ev-hist-02").unwrap()),
                parent_id: Some(EventId::new("ev-hist-01").unwrap()),
                turn_id: Some(TurnId::new("turn-2").unwrap()),
                agent_id: None,
                branch_id: None,
                role: Role::Assistant,
                kind: EventKind::Message,
                timestamp_unix_ms: Some(2000),
                text: PrivateText::new("I will design the SQLite caching schema now."),
                tool: None,
            },
        ],
        explicit_skill_references: Vec::new(),
        supplied_loads: Vec::new(),
    };

    let resolution = resolve_task_anchor(&anchor_context, None, &redactor);
    match resolution {
        AnchorResolution::Established(ref anchor) => {
            assert!(
                !anchor.text.contains(CANARY_SECRET),
                "secret in task anchor must be redacted"
            );
            assert!(anchor.text.contains("[REDACTED]"));
            assert!(anchor.text.contains("Implement SQLite caching"));
            assert_eq!(
                anchor.provenance,
                AnchorProvenance::HistoricalEvent {
                    event_id: EventId::new("ev-hist-01").unwrap(),
                }
            );
        }
        other => panic!("expected Established anchor, got {:?}", other),
    }

    // =========================================================================
    // 10. Bounded Project Signals
    // =========================================================================
    let raw_status = b" M src/main.rs\0A  new_file.rs\0";
    let parsed_signals = parse_dirty_paths(raw_status).unwrap();
    assert_eq!(parsed_signals.paths.len(), 2);
    assert_eq!(parsed_signals.paths[0].as_str(), "src/main.rs");
    assert_eq!(parsed_signals.paths[1].as_str(), "new_file.rs");
    assert!(!parsed_signals.truncated);

    // Traversal and unsafe paths stripped safely
    let unsafe_status = b" M ../parent/escape.rs\0 M safe.rs\0";
    let parsed_unsafe = parse_dirty_paths(unsafe_status).unwrap();
    assert_eq!(parsed_unsafe.paths.len(), 1);
    assert_eq!(parsed_unsafe.paths[0].as_str(), "safe.rs");
    assert_eq!(parsed_unsafe.omitted_unsafe, 1);

    // =========================================================================
    // 11. Disclosure Profiles (Standard vs Minimal)
    // =========================================================================
    let standard_profile = ContextProfile::Standard;
    let minimal_profile = ContextProfile::Minimal;
    assert_eq!(standard_profile.as_str(), "standard");
    assert_eq!(minimal_profile.as_str(), "minimal");

    let filtered_minimal = filter_events_for_provider(&tool_events, true, 500);
    assert_eq!(filtered_minimal.len(), 2);
    for event in &filtered_minimal {
        if let Some(tool) = &event.tool {
            assert!(
                tool.arguments.is_none(),
                "minimal profile must strip tool arguments"
            );
            assert!(
                tool.result.is_none(),
                "minimal profile must strip tool results"
            );
        }
        if event.role == Role::Tool {
            assert!(
                event.text.as_str().is_empty(),
                "minimal profile must strip tool role text"
            );
        }
    }

    let filtered_standard = filter_events_for_provider(&tool_events, false, 500);
    assert_eq!(filtered_standard.len(), 2);
    assert!(
        filtered_standard[0]
            .tool
            .as_ref()
            .unwrap()
            .arguments
            .is_some(),
        "standard profile preserves tool arguments"
    );

    // =========================================================================
    // 12. Disclosure Receipts (Byte Parity & Zero Leakage)
    // =========================================================================
    let (std_payload, std_receipt) = render_context_and_receipt(
        &render_context_test,
        &RenderContextOptions {
            context_profile: ContextProfile::Standard,
            ..Default::default()
        },
    )
    .expect("render standard context with receipt");

    std_receipt
        .verify_against_payload(&std_payload)
        .expect("receipt must verify against payload");
    assert_eq!(std_receipt.context_profile, ContextProfile::Standard);
    assert_eq!(std_receipt.context_quality, ContextQuality::Complete);

    let (min_payload, min_receipt) = render_context_and_receipt(
        &render_context_test,
        &RenderContextOptions {
            context_profile: ContextProfile::Minimal,
            ..Default::default()
        },
    )
    .expect("render minimal context with receipt");

    min_receipt
        .verify_against_payload(&min_payload)
        .expect("minimal receipt must verify against payload");
    assert_eq!(min_receipt.context_profile, ContextProfile::Minimal);

    let receipt_repr = format!("{std_receipt:?}");
    assert!(
        !receipt_repr.contains(CANARY_SECRET),
        "receipt representation must never leak canary secret"
    );

    // =========================================================================
    // 13. Conformance Failure Rejection (Fail-Closed)
    // =========================================================================
    // Corrupted JSONL line
    let corrupt_line = b"{not-valid-json\n";
    assert!(
        parse_line(corrupt_line).is_err(),
        "corrupt JSONL record must fail closed"
    );

    // Oversized record exceeding limit
    let oversized = format!("{{\"text\":\"{}\"}}\n", "x".repeat(300_000));
    assert!(
        parse_line(oversized.as_bytes()).is_err(),
        "record exceeding maximum size limit must fail closed"
    );

    // Missing prompt overlay input fails parsing
    let malformed_hook =
        ClaudeUserPromptSubmit::from_json(b"{}", UnknownFieldPolicy::RejectUnknown);
    assert!(
        malformed_hook.is_err(),
        "overlay without prompt field must fail parsing"
    );
}
