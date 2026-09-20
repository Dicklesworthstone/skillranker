//! Privacy, Trust, and Disclosure Profile Contract Tests
//!
//! Verifies boundary `p3_disclosure_profiles`:
//! - Unit Property Test: `tests/privacy_contract.rs::disclosure_profiles`
//! - Assertion ID: `profile_respected`
//! - E2E Case: `minimal-profile-drops-tools`
//!
//! Core requirements:
//! 1. Standard profile retains recent messages, tool bodies, and safe dirty paths.
//! 2. Minimal profile omits tool bodies, optional history, and dirty paths while retaining
//!    latest request, explicit constraints, and candidate material.
//! 3. `--no-tools` further restricts either profile, guaranteeing zero tool data disclosure.
//! 4. Missing essential attachment or essential tool result produces `ContextQuality::Insufficient`
//!    and fails strict rendering as `unavailable / unsupported-context`.
//! 5. Trust layers enforce non-widening: project cannot widen trusted user minimal profile.
//! 6. Disclosed bytes in Minimal profile are strictly less than in Standard profile.
//! 7. Local load observation evidence is invariant to the active disclosure profile.

use skillranker::context::branch::SkillUsageKind;
use skillranker::context::render::{
    RenderContextError, RenderContextOptions, RenderedLoadedReference, render_context,
    render_context_and_receipt,
};
use skillranker::context::signals::{DirtyPaths, ProjectSignals};
use skillranker::context::{
    CurrentRequest, EventKind, LoadState, NormalizedContext, NormalizedEvent, PrivateText, Role,
    SimpleSkillResolver, SkillMatch, ToolEvent, ToolStatus, extract_load_observations,
};
use skillranker::identity::{
    BranchId, ContextEpoch, EventId, HarnessId, SessionId, SessionIdentity, SkillId,
    SourceProvenance, WorkspaceId,
};
use skillranker::output::ContextQuality;
use skillranker::privacy::redaction::Redactor;
use skillranker::privacy::{
    ContextProfile, DisclosureReceipt, ProfileDisclosedFields, ProfileTrustError,
    ReceiptVerificationError, SourceCategory, count_disclosed_bytes, is_essential_tool_reference,
    resolve_context_profile, validate_project_profile,
};

/// Creates a standard test session context containing user messages, assistant messages,
/// tool invocations, and tool results.
fn make_test_context(request_text: &str, essential_attachment: bool) -> NormalizedContext {
    NormalizedContext {
        schema_version: 1,
        harness: HarnessId::new("claude_code").unwrap(),
        producer_id: None,
        workspace_root: PrivateText::new("/workspaces/demo"),
        session_id: Some(SessionId::new("sess-privacy-01").unwrap()),
        agent_id: None,
        branch_id: None,
        context_epoch: None,
        current_request: CurrentRequest {
            event_id: Some(EventId::new("ev-req-99").unwrap()),
            text: PrivateText::new(request_text),
            attachments_omitted: false,
            essential_attachment_missing: essential_attachment,
        },
        events: vec![
            NormalizedEvent {
                event_id: Some(EventId::new("ev-01").unwrap()),
                parent_id: None,
                turn_id: None,
                agent_id: None,
                branch_id: None,
                role: Role::User,
                kind: EventKind::Message,
                timestamp_unix_ms: Some(1000),
                text: PrivateText::new("Initial goal: refactor data models"),
                tool: None,
            },
            NormalizedEvent {
                event_id: Some(EventId::new("ev-02").unwrap()),
                parent_id: Some(EventId::new("ev-01").unwrap()),
                turn_id: None,
                agent_id: None,
                branch_id: None,
                role: Role::Assistant,
                kind: EventKind::Message,
                timestamp_unix_ms: Some(2000),
                text: PrivateText::new("I will inspect the workspace files."),
                tool: None,
            },
            NormalizedEvent {
                event_id: Some(EventId::new("ev-03").unwrap()),
                parent_id: Some(EventId::new("ev-02").unwrap()),
                turn_id: None,
                agent_id: None,
                branch_id: None,
                role: Role::Tool,
                kind: EventKind::ToolInvocation,
                timestamp_unix_ms: Some(3000),
                text: PrivateText::new(""),
                tool: Some(ToolEvent {
                    call_id: None,
                    name: PrivateText::new("bash"),
                    status: ToolStatus::Attempted,
                    arguments: Some(PrivateText::new("cargo check --workspace")),
                    result: None,
                }),
            },
            NormalizedEvent {
                event_id: Some(EventId::new("ev-04").unwrap()),
                parent_id: Some(EventId::new("ev-03").unwrap()),
                turn_id: None,
                agent_id: None,
                branch_id: None,
                role: Role::Tool,
                kind: EventKind::ToolResult,
                timestamp_unix_ms: Some(4000),
                text: PrivateText::new(""),
                tool: Some(ToolEvent {
                    call_id: None,
                    name: PrivateText::new("bash"),
                    status: ToolStatus::Succeeded,
                    arguments: None,
                    result: Some(PrivateText::new("Finished dev profile [unoptimized]")),
                }),
            },
        ],
        explicit_skill_references: vec![],
        supplied_loads: vec![],
    }
}

fn make_test_signals() -> ProjectSignals {
    let dirty = DirtyPaths {
        paths: vec![
            PrivateText::new("src/models/user.rs"),
            PrivateText::new("src/models/auth.rs"),
        ],
        omitted_non_utf8: 0,
        omitted_unsafe: 0,
        truncated: false,
    };

    ProjectSignals {
        filenames: vec!["Cargo.toml", "src/lib.rs"],
        tools_on_path: vec!["cargo", "git"],
        dirty_paths: Some(dirty),
        git_omission: None,
        inventory_partial: false,
    }
}

#[test]
fn disclosure_profiles() {
    let signals = make_test_signals();
    let loaded_refs = vec![RenderedLoadedReference {
        name: "git-commit-helper".to_string(),
        summary: "Git commit message generator".to_string(),
    }];
    let exclusions = vec!["deprecated-skill".to_string()];

    // -------------------------------------------------------------------------
    // Sub-case 1: Standard profile retains full context (recent messages, tools, dirty paths)
    // -------------------------------------------------------------------------
    let context = make_test_context("Please create a unit test for the auth model", false);
    let standard_options = RenderContextOptions {
        context_profile: ContextProfile::Standard,
        no_tools: false,
        project_signals: Some(&signals),
        loaded_references: loaded_refs.clone(),
        explicit_exclusions: exclusions.clone(),
        ..RenderContextOptions::default()
    };

    let standard_payload = render_context(&context, &standard_options)
        .expect("rendering standard context should succeed");

    assert_eq!(standard_payload.context_profile, ContextProfile::Standard);
    assert_eq!(standard_payload.context_quality, ContextQuality::Complete);
    assert_eq!(
        standard_payload.latest_user_request,
        "Please create a unit test for the auth model"
    );
    // Recent messages must contain messages AND tool summary
    assert!(
        !standard_payload.recent_messages.is_empty(),
        "standard profile must retain recent messages"
    );
    let has_tool = standard_payload
        .recent_messages
        .iter()
        .any(|m| m.role == "tool");
    assert!(
        has_tool,
        "standard profile with no_tools=false must include tool summary"
    );

    // Dirty paths must be included in project signals
    assert_eq!(
        standard_payload.project_signals.dirty_paths.len(),
        2,
        "standard profile must disclose dirty paths"
    );
    assert_eq!(
        standard_payload.project_signals.dirty_paths[0],
        "src/models/user.rs"
    );

    // Explicit exclusions and candidate material must be preserved
    assert_eq!(
        standard_payload.session_state.explicit_exclusions,
        exclusions
    );
    assert_eq!(
        standard_payload.session_state.loaded_references,
        loaded_refs
    );

    // ProfileDisclosedFields check for Standard
    let std_fields = ContextProfile::Standard.disclosed_fields(false);
    assert_eq!(
        std_fields,
        ProfileDisclosedFields {
            current_request: true,
            task_anchor: true,
            explicit_constraints: true,
            candidate_material: true,
            recent_messages: true,
            tool_bodies: true,
            dirty_paths: true,
        }
    );

    // -------------------------------------------------------------------------
    // Sub-case 2: Minimal profile omits tool bodies, optional history, and dirty paths
    //             while retaining latest request, explicit constraints, and candidate material
    // -------------------------------------------------------------------------
    let minimal_options = RenderContextOptions {
        context_profile: ContextProfile::Minimal,
        no_tools: false,
        project_signals: Some(&signals),
        loaded_references: loaded_refs.clone(),
        explicit_exclusions: exclusions.clone(),
        ..RenderContextOptions::default()
    };

    let minimal_payload = render_context(&context, &minimal_options)
        .expect("rendering minimal context should succeed");

    assert_eq!(minimal_payload.context_profile, ContextProfile::Minimal);
    assert_eq!(minimal_payload.context_quality, ContextQuality::Complete);
    // Latest user request is strictly preserved
    assert_eq!(
        minimal_payload.latest_user_request,
        "Please create a unit test for the auth model"
    );
    // Optional history is completely omitted
    assert!(
        minimal_payload.recent_messages.is_empty(),
        "minimal profile must omit all optional message history and tool bodies"
    );
    // Dirty paths must be empty
    assert!(
        minimal_payload.project_signals.dirty_paths.is_empty(),
        "minimal profile must omit dirty paths"
    );
    assert!(
        !minimal_payload.project_signals.dirty_paths_truncated,
        "dirty paths truncation flag must be false when omitted"
    );
    // Safe language markers and tools on path remain
    assert!(
        minimal_payload
            .project_signals
            .languages
            .contains(&"rust".to_string())
    );

    // Candidate material and explicit exclusions are strictly preserved
    assert_eq!(
        minimal_payload.session_state.explicit_exclusions,
        exclusions
    );
    assert_eq!(minimal_payload.session_state.loaded_references, loaded_refs);

    // ProfileDisclosedFields check for Minimal
    let min_fields = ContextProfile::Minimal.disclosed_fields(false);
    assert_eq!(
        min_fields,
        ProfileDisclosedFields {
            current_request: true,
            task_anchor: true,
            explicit_constraints: true,
            candidate_material: true,
            recent_messages: false,
            tool_bodies: false,
            dirty_paths: false,
        }
    );

    // -------------------------------------------------------------------------
    // Sub-case 3: --no-tools with Standard profile drops tools while keeping recent messages
    // -------------------------------------------------------------------------
    let std_no_tools_options = RenderContextOptions {
        context_profile: ContextProfile::Standard,
        no_tools: true,
        project_signals: Some(&signals),
        ..RenderContextOptions::default()
    };

    let std_no_tools_payload = render_context(&context, &std_no_tools_options)
        .expect("rendering standard context with no-tools should succeed");

    assert!(
        !std_no_tools_payload.recent_messages.is_empty(),
        "recent messages should be kept"
    );
    let has_any_tools = std_no_tools_payload
        .recent_messages
        .iter()
        .any(|m| m.role == "tool" || m.tool.is_some());
    assert!(
        !has_any_tools,
        "--no-tools must drop all tool events from recent_messages"
    );
    // Dirty paths still retained under Standard with --no-tools
    assert_eq!(
        std_no_tools_payload.project_signals.dirty_paths.len(),
        2,
        "dirty paths remain disclosed under Standard even with no-tools"
    );

    let std_no_tools_fields = ContextProfile::Standard.disclosed_fields(true);
    assert_eq!(
        std_no_tools_fields,
        ProfileDisclosedFields {
            current_request: true,
            task_anchor: true,
            explicit_constraints: true,
            candidate_material: true,
            recent_messages: true,
            tool_bodies: false,
            dirty_paths: true,
        }
    );

    // -------------------------------------------------------------------------
    // Sub-case 4: --no-tools with Minimal profile preserves zero-tool invariant
    // -------------------------------------------------------------------------
    let min_no_tools_options = RenderContextOptions {
        context_profile: ContextProfile::Minimal,
        no_tools: true,
        project_signals: Some(&signals),
        ..RenderContextOptions::default()
    };

    let min_no_tools_payload = render_context(&context, &min_no_tools_options)
        .expect("rendering minimal context with no-tools should succeed");

    assert!(
        min_no_tools_payload.recent_messages.is_empty(),
        "minimal with no-tools must have empty messages"
    );
    assert!(
        min_no_tools_payload.project_signals.dirty_paths.is_empty(),
        "minimal with no-tools must omit dirty paths"
    );

    let min_no_tools_fields = ContextProfile::Minimal.disclosed_fields(true);
    assert_eq!(
        min_no_tools_fields,
        ProfileDisclosedFields {
            current_request: true,
            task_anchor: true,
            explicit_constraints: true,
            candidate_material: true,
            recent_messages: false,
            tool_bodies: false,
            dirty_paths: false,
        }
    );

    // -------------------------------------------------------------------------
    // Sub-case 5: Missing essential evidence (attachment or essential tool result)
    //             produces Insufficient context / fails strict rendering
    // -------------------------------------------------------------------------
    // 5a: Essential attachment missing
    let missing_att_context =
        make_test_context("Please analyze the attached architectural diagram", true);
    let payload_missing_att = render_context(&missing_att_context, &minimal_options)
        .expect("non-strict render of missing attachment yields Insufficient quality");
    assert_eq!(
        payload_missing_att.context_quality,
        ContextQuality::Insufficient
    );
    assert!(payload_missing_att.is_unsupported_context());

    let strict_missing_att_options = RenderContextOptions {
        fail_on_unsupported_context: true,
        ..minimal_options.clone()
    };
    let err_att = render_context(&missing_att_context, &strict_missing_att_options)
        .expect_err("strict mode must fail on essential attachment missing");
    assert!(
        matches!(err_att, RenderContextError::UnsupportedContext(_)),
        "error must be UnsupportedContext"
    );

    // 5b: Essential tool result missing under Minimal profile
    let tool_dep_context =
        make_test_context("Why did the command fail? Fix the error above.", false);
    assert!(is_essential_tool_reference(
        tool_dep_context.current_request.text.as_str()
    ));

    let payload_essential_tool = render_context(&tool_dep_context, &minimal_options)
        .expect("render of essential tool reference under minimal yields Insufficient quality");
    assert_eq!(
        payload_essential_tool.context_quality,
        ContextQuality::Insufficient,
        "essential tool result omitted under Minimal profile must yield Insufficient quality"
    );

    let strict_tool_options = RenderContextOptions {
        fail_on_unsupported_context: true,
        ..minimal_options.clone()
    };
    let err_tool = render_context(&tool_dep_context, &strict_tool_options)
        .expect_err("strict mode must fail on essential tool result missing");
    assert!(
        matches!(err_tool, RenderContextError::UnsupportedContext(ref msg) if msg.contains("tool result")),
        "error message must cite omitted tool result"
    );

    // 5c: Essential tool result present under Standard profile (with tools enabled)
    let payload_std_tool = render_context(&tool_dep_context, &standard_options)
        .expect("render of essential tool reference under standard profile must succeed");
    assert_eq!(
        payload_std_tool.context_quality,
        ContextQuality::Complete,
        "when tools are disclosed under Standard profile, context is Complete"
    );

    // -------------------------------------------------------------------------
    // Sub-case 6: Trust layer hierarchy: project cannot widen user Minimal
    // -------------------------------------------------------------------------
    let widen_err = validate_project_profile(ContextProfile::Minimal, ContextProfile::Standard)
        .expect_err("project Standard must not widen user Minimal");
    assert_eq!(
        widen_err,
        ProfileTrustError::ProjectCannotWidenProfile {
            user: ContextProfile::Minimal,
            project: ContextProfile::Standard,
        }
    );

    let resolve_widen_err = resolve_context_profile(
        Some(ContextProfile::Minimal),
        Some(ContextProfile::Standard),
        None,
    )
    .expect_err("resolve_context_profile must reject project widening");
    assert_eq!(resolve_widen_err, widen_err);

    // -------------------------------------------------------------------------
    // Sub-case 7: Trust layer hierarchy: project can restrict user Standard to Minimal
    // -------------------------------------------------------------------------
    let narrowed = validate_project_profile(ContextProfile::Standard, ContextProfile::Minimal)
        .expect("project Minimal can narrow user Standard");
    assert_eq!(narrowed, ContextProfile::Minimal);

    let resolved_narrowed = resolve_context_profile(
        Some(ContextProfile::Standard),
        Some(ContextProfile::Minimal),
        None,
    )
    .expect("resolve_context_profile allows project narrowing");
    assert_eq!(resolved_narrowed, ContextProfile::Minimal);

    // -------------------------------------------------------------------------
    // Sub-case 8: Trust layer hierarchy: explicit CLI flag is authoritative
    // -------------------------------------------------------------------------
    let cli_override_std = resolve_context_profile(
        Some(ContextProfile::Minimal),
        Some(ContextProfile::Minimal),
        Some(ContextProfile::Standard),
    )
    .expect("explicit CLI flag overrides config layers");
    assert_eq!(cli_override_std, ContextProfile::Standard);

    let cli_override_min = resolve_context_profile(
        Some(ContextProfile::Standard),
        Some(ContextProfile::Standard),
        Some(ContextProfile::Minimal),
    )
    .expect("explicit CLI flag overrides config layers");
    assert_eq!(cli_override_min, ContextProfile::Minimal);

    // -------------------------------------------------------------------------
    // Sub-case 9: Disclosed bytes comparison
    // -------------------------------------------------------------------------
    let std_bytes = count_disclosed_bytes(&standard_payload);
    let min_bytes = count_disclosed_bytes(&minimal_payload);

    assert_eq!(std_bytes, standard_payload.disclosed_bytes());
    assert_eq!(min_bytes, minimal_payload.disclosed_bytes());
    assert!(
        min_bytes < std_bytes,
        "disclosed bytes for Minimal ({min_bytes}) must be strictly less than Standard ({std_bytes})"
    );

    // -------------------------------------------------------------------------
    // Sub-case 10: Local load observation evidence is invariant to disclosure profile
    // -------------------------------------------------------------------------
    let mut resolver = SimpleSkillResolver::new();
    let skill_bash = SkillId::new("bash-skill").unwrap();
    resolver.register_tool(
        "bash",
        SkillMatch {
            skill_id: skill_bash.clone(),
            usage_kind: SkillUsageKind::Workflow,
            source_content: None,
            rendered_content: None,
            has_dynamic_arguments: false,
            turn_scoped: false,
        },
    );

    let session_id = SessionIdentity {
        source: SourceProvenance::Normalized {
            producer: None,
            harness: HarnessId::new("claude_code").unwrap(),
            schema_version: 1,
        },
        workspace: Some(WorkspaceId::new("ws-01").unwrap()),
        session: Some(SessionId::new("sess-privacy-01").unwrap()),
        agent: None,
        branch: Some(BranchId::new("main").unwrap()),
        epoch: Some(ContextEpoch::new("epoch-0").unwrap()),
    };

    let observations_from_events =
        extract_load_observations(&context.events, &session_id, &resolver, None);

    assert_eq!(
        observations_from_events.len(),
        1,
        "local observation must detect the bash tool invocation/result"
    );
    assert_eq!(observations_from_events[0].skill_id.as_str(), "bash-skill");
    assert_eq!(observations_from_events[0].state, LoadState::ObservedLoaded);

    // Local observation does not inspect standard_payload or minimal_payload;
    // it reads the authoritative normalized events, proving total independence.

    // -------------------------------------------------------------------------
    // Sub-case 11: E2E Case & Assertion ID: profile_respected
    //              Inspect serialized JSON fields and secret boundary
    // -------------------------------------------------------------------------
    let std_json = standard_payload
        .to_json_bytes()
        .expect("standard serialization");
    let min_json = minimal_payload
        .to_json_bytes()
        .expect("minimal serialization");

    let std_value: serde_json::Value =
        serde_json::from_slice(&std_json).expect("valid standard JSON");
    let min_value: serde_json::Value =
        serde_json::from_slice(&min_json).expect("valid minimal JSON");

    // Standard JSON checks
    assert_eq!(std_value["context_profile"], "standard");
    assert!(
        std_value["recent_messages"].as_array().unwrap().len() >= 2,
        "standard JSON must contain recent_messages"
    );
    assert_eq!(
        std_value["project_signals"]["dirty_paths"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // Minimal JSON checks (Assertion ID: profile_respected, Case: minimal-profile-drops-tools)
    assert_eq!(min_value["context_profile"], "minimal");
    assert!(
        min_value["recent_messages"].as_array().unwrap().is_empty(),
        "minimal JSON recent_messages must be empty"
    );
    assert!(
        min_value["project_signals"]["dirty_paths"]
            .as_array()
            .unwrap()
            .is_empty(),
        "minimal JSON dirty_paths must be empty"
    );
    assert_eq!(
        min_value["latest_user_request"],
        "Please create a unit test for the auth model"
    );
    assert_eq!(
        min_value["session_state"]["explicit_exclusions"][0],
        "deprecated-skill"
    );
    assert_eq!(
        min_value["session_state"]["loaded_references"][0]["name"],
        "git-commit-helper"
    );

    // Verify payload inspection passes secret boundary check
    let redactor = Redactor::default();
    redactor
        .inspect_payload(&std_json)
        .expect("standard payload contains no unredacted secrets");
    redactor
        .inspect_payload(&min_json)
        .expect("minimal payload contains no unredacted secrets");
}

#[test]
fn disclosure_receipts() {
    let signals = make_test_signals();
    let loaded_refs = vec![RenderedLoadedReference {
        name: "git-commit-helper".to_string(),
        summary: "Git commit message generator".to_string(),
    }];
    let exclusions = vec!["deprecated-skill".to_string()];

    // -------------------------------------------------------------------------
    // Sub-case 1: Standard profile receipt generation and exact payload parity
    // -------------------------------------------------------------------------
    let context = make_test_context("Please create a unit test for the auth model", false);
    let standard_options = RenderContextOptions {
        context_profile: ContextProfile::Standard,
        no_tools: false,
        project_signals: Some(&signals),
        loaded_references: loaded_refs.clone(),
        explicit_exclusions: exclusions.clone(),
        ..RenderContextOptions::default()
    };

    let (standard_payload, standard_receipt) =
        render_context_and_receipt(&context, &standard_options)
            .expect("rendering standard context with receipt should succeed");

    // Exact parity check (assertion_id: receipt_accurate)
    standard_receipt
        .verify_against_payload(&standard_payload)
        .expect("receipt must verify against payload");

    assert_eq!(standard_receipt.schema_version, 1);
    assert_eq!(standard_receipt.context_profile, ContextProfile::Standard);
    assert_eq!(standard_receipt.context_quality, ContextQuality::Complete);
    assert!(!standard_receipt.no_tools);

    // Check categories
    let req_cat = standard_receipt
        .category(SourceCategory::UserRequest)
        .unwrap();
    assert_eq!(req_cat.included_count, 1);
    assert_eq!(req_cat.omitted_count, 0);

    let hist_cat = standard_receipt
        .category(SourceCategory::MessageHistory)
        .unwrap();
    assert_eq!(hist_cat.included_count, 2); // 2 non-tool messages (ev-01, ev-02)
    assert_eq!(hist_cat.omitted_count, 0);

    let tool_cat = standard_receipt
        .category(SourceCategory::ToolEvents)
        .unwrap();
    assert_eq!(tool_cat.included_count, 2); // 2 tool message candidates (ev-03, ev-04)
    assert_eq!(tool_cat.omitted_count, 0);

    let sig_cat = standard_receipt
        .category(SourceCategory::ProjectSignals)
        .unwrap();
    assert_eq!(
        sig_cat.included_count,
        standard_payload.project_signals.languages.len()
            + standard_payload.project_signals.tools_on_path.len()
            + standard_payload.project_signals.dirty_paths.len()
    );
    assert_eq!(sig_cat.omitted_count, 0);

    let sess_cat = standard_receipt
        .category(SourceCategory::SessionState)
        .unwrap();
    assert_eq!(sess_cat.included_count, 2); // 1 loaded reference + 1 exclusion
    assert_eq!(sess_cat.omitted_count, 0);

    // Verify totals
    assert_eq!(
        standard_receipt.disclosed_bytes,
        standard_payload.disclosed_bytes()
    );
    assert_eq!(
        standard_receipt.disclosed_scalars,
        standard_payload.total_message_scalars()
    );

    // -------------------------------------------------------------------------
    // Sub-case 2: Minimal profile receipt reflects omissions
    // -------------------------------------------------------------------------
    let minimal_options = RenderContextOptions {
        context_profile: ContextProfile::Minimal,
        no_tools: false,
        project_signals: Some(&signals),
        loaded_references: loaded_refs.clone(),
        explicit_exclusions: exclusions.clone(),
        ..RenderContextOptions::default()
    };

    let (minimal_payload, minimal_receipt) = render_context_and_receipt(&context, &minimal_options)
        .expect("rendering minimal context with receipt should succeed");

    minimal_receipt
        .verify_against_payload(&minimal_payload)
        .expect("minimal receipt must verify against payload");

    assert_eq!(minimal_receipt.context_profile, ContextProfile::Minimal);
    let min_hist = minimal_receipt
        .category(SourceCategory::MessageHistory)
        .unwrap();
    assert_eq!(min_hist.included_count, 0);
    assert_eq!(min_hist.omitted_count, 2); // 2 messages omitted

    let min_tools = minimal_receipt
        .category(SourceCategory::ToolEvents)
        .unwrap();
    assert_eq!(min_tools.included_count, 0);
    assert_eq!(min_tools.omitted_count, 2); // 2 tool events omitted

    let min_signals = minimal_receipt
        .category(SourceCategory::ProjectSignals)
        .unwrap();
    assert_eq!(min_signals.omitted_count, 2); // 2 dirty paths omitted
    assert_eq!(minimal_payload.project_signals.dirty_paths.len(), 0);

    // -------------------------------------------------------------------------
    // Sub-case 3: --no-tools receipt reflection across Standard profile
    // -------------------------------------------------------------------------
    let no_tools_options = RenderContextOptions {
        context_profile: ContextProfile::Standard,
        no_tools: true,
        project_signals: Some(&signals),
        ..RenderContextOptions::default()
    };

    let (no_tools_payload, no_tools_receipt) =
        render_context_and_receipt(&context, &no_tools_options)
            .expect("rendering no-tools context with receipt should succeed");

    no_tools_receipt
        .verify_against_payload(&no_tools_payload)
        .expect("no-tools receipt must verify against payload");

    assert!(no_tools_receipt.no_tools);
    let nt_tools = no_tools_receipt
        .category(SourceCategory::ToolEvents)
        .unwrap();
    assert_eq!(nt_tools.included_count, 0);
    assert_eq!(nt_tools.omitted_count, 2); // all tool events omitted

    let nt_hist = no_tools_receipt
        .category(SourceCategory::MessageHistory)
        .unwrap();
    assert_eq!(nt_hist.included_count, 2); // non-tool messages still included

    // -------------------------------------------------------------------------
    // Sub-case 4: Redaction tracking in receipt and zero secret fragments
    // -------------------------------------------------------------------------
    let mut secret_context = context.clone();
    secret_context.current_request.text =
        PrivateText::new("Connect with ghp_123456789012345678901234567890123456");
    secret_context.events[0].text =
        PrivateText::new("Authorization: Bearer my-secret-token-abcdef123456");
    secret_context.events[2].tool = Some(ToolEvent {
        call_id: None,
        name: PrivateText::new("bash"),
        status: ToolStatus::Attempted,
        arguments: Some(PrivateText::new(r#"api_key="AKIAIOSFODNN7EXAMPLE1""#)),
        result: None,
    });
    secret_context.events[3].tool = Some(ToolEvent {
        call_id: None,
        name: PrivateText::new("bash"),
        status: ToolStatus::Succeeded,
        arguments: None,
        result: Some(PrivateText::new(
            r#"token="ghp_abcdefghijklmnopqrstuvwxyz1234567890""#,
        )),
    });

    let (secret_payload, secret_receipt) =
        render_context_and_receipt(&secret_context, &standard_options)
            .expect("rendering secret context with receipt should succeed");

    secret_receipt
        .verify_against_payload(&secret_payload)
        .expect("secret receipt must verify against payload");

    assert!(
        secret_receipt.total_redactions >= 4,
        "receipt must track all redactions across fields (actual: {})",
        secret_receipt.total_redactions
    );
    assert!(
        secret_receipt
            .category(SourceCategory::UserRequest)
            .unwrap()
            .redaction_count
            >= 1
    );
    assert!(
        secret_receipt
            .category(SourceCategory::MessageHistory)
            .unwrap()
            .redaction_count
            >= 1
    );
    assert!(
        secret_receipt
            .category(SourceCategory::ToolEvents)
            .unwrap()
            .redaction_count
            >= 2
    );

    // Invariant: receipt JSON serialization contains ZERO secret fragments
    let receipt_json = serde_json::to_string(&secret_receipt).expect("serialize receipt to JSON");
    assert!(!receipt_json.contains("ghp_"));
    assert!(!receipt_json.contains("AKIA"));
    assert!(!receipt_json.contains("my-secret-token"));
    assert!(!receipt_json.contains("abcdefghijklmnopqrstuvwxyz"));

    // -------------------------------------------------------------------------
    // Sub-case 5: Truncation tracking in receipt
    // -------------------------------------------------------------------------
    let budget_options = RenderContextOptions {
        max_total_scalars: 90, // tight budget forces truncation (overhead 13 + min excerpt 30 = 43)
        ..standard_options.clone()
    };

    let (truncated_payload, truncated_receipt) =
        render_context_and_receipt(&context, &budget_options)
            .expect("rendering with tight budget should succeed");

    truncated_receipt
        .verify_against_payload(&truncated_payload)
        .expect("truncated receipt must verify against payload");

    assert!(
        truncated_receipt.total_truncated > 0,
        "receipt must track truncation events under tight scalar budget"
    );

    // -------------------------------------------------------------------------
    // Sub-case 6: Category totals consistency and tampering detection
    // -------------------------------------------------------------------------
    let mut tampered_receipt = standard_receipt.clone();
    tampered_receipt.total_included += 1;
    let err_totals = tampered_receipt
        .verify_against_payload(&standard_payload)
        .expect_err("tampered totals must be detected");
    assert!(matches!(
        err_totals,
        ReceiptVerificationError::TotalsInconsistent { .. }
    ));

    let mut tampered_item = standard_receipt.clone();
    tampered_item.categories[0].included_count += 5;
    tampered_item.total_included += 5;
    let err_item = tampered_item
        .verify_against_payload(&standard_payload)
        .expect_err("tampered item count must be detected");
    assert!(matches!(
        err_item,
        ReceiptVerificationError::ItemCountMismatch { .. }
    ));

    // -------------------------------------------------------------------------
    // Sub-case 7: E2E Case & Assertion ID: receipt_accurate
    //              Round-trip serialization & schema conformance
    // -------------------------------------------------------------------------
    let serialized_receipt = serde_json::to_string(&standard_receipt).expect("serialize receipt");
    let deserialized: DisclosureReceipt =
        serde_json::from_str(&serialized_receipt).expect("deserialize receipt");
    assert_eq!(standard_receipt, deserialized);
}
