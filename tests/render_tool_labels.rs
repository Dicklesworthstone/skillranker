//! Tool metadata must not bypass redaction or evict useful context. Local tool
//! dispatch keeps the original names; only the provider view is transformed.

use skillranker::context::render::{
    RenderContextOptions, RenderedLoadedReference, TOOL_LABEL_SCALARS, render_context,
    render_context_and_receipt,
};
use skillranker::context::{
    CurrentRequest, EventKind, NormalizedContext, NormalizedEvent, PrivateText, Role, ToolEvent,
    ToolStatus,
};
use skillranker::identity::{AgentId, EventId, HarnessId};
use skillranker::limits::NORMALIZED_CONTEXT_JSON_BYTES;
use skillranker::output::ContextQuality;
use skillranker::privacy::{ContextProfile, SourceCategory};

fn event(id: &str, parent: Option<&str>, text: &str) -> NormalizedEvent {
    NormalizedEvent {
        event_id: Some(EventId::new(id).unwrap()),
        parent_id: parent.map(|id| EventId::new(id).unwrap()),
        turn_id: None,
        agent_id: None,
        branch_id: None,
        role: Role::User,
        kind: EventKind::Message,
        timestamp_unix_ms: None,
        text: PrivateText::new(text),
        tool: None,
    }
}

fn context(name: &str) -> NormalizedContext {
    let mut call = event("call", Some("root"), "");
    call.role = Role::Assistant;
    call.kind = EventKind::ToolInvocation;
    call.tool = Some(ToolEvent {
        call_id: None,
        name: PrivateText::new(name),
        status: ToolStatus::Succeeded,
        arguments: Some(PrivateText::new("{}")),
        result: Some(PrivateText::new("ACTIVE_RESULT_CANARY")),
    });
    NormalizedContext {
        schema_version: 1,
        harness: HarnessId::new("claude_code").unwrap(),
        producer_id: None,
        workspace_root: PrivateText::new("/private-workspace"),
        session_id: None,
        agent_id: None,
        branch_id: None,
        context_epoch: None,
        current_request: CurrentRequest {
            event_id: Some(EventId::new("current").unwrap()),
            text: PrivateText::new("Explain the Rust compiler diagnostics"),
            attachments_omitted: false,
            essential_attachment_missing: false,
        },
        events: vec![
            event("root", None, "ACTIVE_TASK_CANARY"),
            call,
            event(
                "current",
                Some("call"),
                "Explain the Rust compiler diagnostics",
            ),
        ],
        explicit_skill_references: Vec::new(),
        supplied_loads: Vec::new(),
    }
}

#[test]
fn secret_tool_labels_are_redacted_and_receipted_without_changing_local_names() {
    let secret = "ghp_123456789012345678901234567890123456";
    let name = format!("loader-{secret}");
    let input = context(&name);
    let original = input.clone();
    let options = RenderContextOptions::default();
    let (payload, receipt) = render_context_and_receipt(&input, &options).unwrap();
    assert_eq!(payload, render_context(&input, &options).unwrap());
    let wire = String::from_utf8(payload.to_json_bytes().unwrap()).unwrap();
    assert!(!wire.contains(secret));
    assert!(wire.contains("loader-[REDACTED]"));
    assert!(wire.contains("ACTIVE_RESULT_CANARY"));
    let tools = receipt.category(SourceCategory::ToolEvents).unwrap();
    assert_eq!(tools.included_count, 1);
    assert_eq!(tools.redaction_count, 1);
    assert_eq!(tools.truncated_count, 0);
    assert_eq!(receipt.total_redactions, 1);
    receipt.verify_against_payload(&payload).unwrap();
    assert_eq!(input, original);
}

#[test]
fn an_oversized_unicode_label_cannot_evict_the_task_or_result() {
    let input = context(&format!("{}-END", "🦀".repeat(20_000)));
    let options = RenderContextOptions::default();
    let (payload, receipt) = render_context_and_receipt(&input, &options).unwrap();
    let tool = payload
        .recent_messages
        .iter()
        .find(|message| message.tool.is_some())
        .unwrap();
    let label = tool.tool.as_ref().unwrap();
    assert!(label.chars().count() <= TOOL_LABEL_SCALARS);
    assert!(label.contains("chars omitted]"));
    assert!(label.ends_with("-END"));
    let wire = String::from_utf8(payload.to_json_bytes().unwrap()).unwrap();
    assert!(wire.contains("ACTIVE_TASK_CANARY"));
    assert!(wire.contains("ACTIVE_RESULT_CANARY"));
    assert_eq!(payload.context_quality, ContextQuality::Partial);
    assert_eq!(
        receipt
            .category(SourceCategory::ToolEvents)
            .unwrap()
            .truncated_count,
        1
    );
    receipt.verify_against_payload(&payload).unwrap();
}

#[test]
fn label_redaction_happens_before_head_tail_truncation() {
    let secret = "ghp_123456789012345678901234567890123456";
    let input = context(&format!("{} {secret} END", "x".repeat(200)));
    let options = RenderContextOptions::default();
    let (payload, receipt) = render_context_and_receipt(&input, &options).unwrap();
    let wire = String::from_utf8(payload.to_json_bytes().unwrap()).unwrap();
    assert!(!wire.contains(secret));
    assert!(!wire.contains("1234567890"));
    assert!(wire.contains("[REDACTED]"));
    let tools = receipt.category(SourceCategory::ToolEvents).unwrap();
    assert_eq!(tools.redaction_count, 1);
    assert_eq!(tools.truncated_count, 1);
    receipt.verify_against_payload(&payload).unwrap();
}

#[test]
fn omitted_labels_do_not_trigger_scanning_or_claim_redactions() {
    let input = context(&"x".repeat(NORMALIZED_CONTEXT_JSON_BYTES.max() + 1));
    let normal = RenderContextOptions::default();
    assert!(render_context(&input, &normal).is_err());
    for options in [
        RenderContextOptions {
            no_tools: true,
            ..Default::default()
        },
        RenderContextOptions {
            context_profile: ContextProfile::Minimal,
            ..Default::default()
        },
    ] {
        let (payload, receipt) = render_context_and_receipt(&input, &options).unwrap();
        assert!(
            payload
                .recent_messages
                .iter()
                .all(|message| message.tool.is_none())
        );
        let tools = receipt.category(SourceCategory::ToolEvents).unwrap();
        assert_eq!(tools.included_count, 0);
        assert_eq!(tools.omitted_count, 1);
        assert_eq!(tools.redaction_count, 0);
        assert_eq!(tools.truncated_count, 0);
        receipt.verify_against_payload(&payload).unwrap();
    }
}

#[test]
fn sibling_labels_are_excluded_before_the_label_input_limit() {
    let mut input = context("Read");
    let mut sibling = input.events[1].clone();
    sibling.agent_id = Some(AgentId::new("foreign").unwrap());
    sibling.tool.as_mut().unwrap().name =
        PrivateText::new("q".repeat(NORMALIZED_CONTEXT_JSON_BYTES.max() + 1));
    input.events.push(sibling);
    let (payload, receipt) = render_context_and_receipt(&input, &Default::default()).unwrap();
    assert_eq!(payload.context_quality, ContextQuality::Complete);
    let tools = receipt.category(SourceCategory::ToolEvents).unwrap();
    assert_eq!(tools.included_count, 1);
    assert_eq!(tools.omitted_count, 1);
    assert_eq!(tools.truncated_count, 0);
    receipt.verify_against_payload(&payload).unwrap();
}

#[test]
fn session_and_label_receipts_combine_and_insufficient_context_stays_insufficient() {
    let mut input = context(&"a".repeat(300));
    input.current_request.essential_attachment_missing = true;
    let options = RenderContextOptions {
        loaded_references: vec![RenderedLoadedReference {
            name: "docs".to_owned(),
            summary: "b".repeat(1000),
        }],
        ..Default::default()
    };
    let (payload, receipt) = render_context_and_receipt(&input, &options).unwrap();
    assert_eq!(payload.context_quality, ContextQuality::Insufficient);
    assert_eq!(receipt.total_truncated, 2);
    assert_eq!(payload, render_context(&input, &options).unwrap());
    receipt.verify_against_payload(&payload).unwrap();
}

#[cfg(unix)]
#[test]
fn production_dry_run_redacts_tool_labels_and_no_tools_omits_them() {
    use std::fs;
    use std::os::unix::fs::DirBuilderExt;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    let root = fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(format!(
            "sr-tool-labels-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
    fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
    for path in ["workspace/.claude/skills/alpha", "home", "config/sr"] {
        fs::create_dir_all(root.join(path)).unwrap();
    }
    let workspace = root.join("workspace");
    fs::write(
        workspace.join(".claude/skills/alpha/SKILL.md"),
        "---\nname: alpha\ndescription: Explain Rust compiler diagnostics\n---\nReference text.\n",
    )
    .unwrap();
    let secret = "ghp_123456789012345678901234567890123456";
    let mut input = context(secret);
    input.workspace_root = PrivateText::new(workspace.to_str().unwrap());
    fs::write(
        workspace.join("context.json"),
        serde_json::to_vec(&input).unwrap(),
    )
    .unwrap();
    for no_tools in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sr"));
        command
            .env_clear()
            .env("HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_DATA_HOME", root.join("data"))
            .current_dir(&workspace)
            .args([
                "rank",
                "--context",
                "context.json",
                "--dry-run",
                "--json",
                "--timeout-ms",
                "10000",
            ]);
        if no_tools {
            command.arg("--no-tools");
        }
        let output = command.output().unwrap();
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(0), "{value}");
        assert_eq!(value["kind"], "preview");
        assert!(!value["provider_request"].is_null(), "{value}");
        let request = value["provider_request"].to_string();
        assert!(!value.to_string().contains(secret));
        assert!(request.contains("ACTIVE_TASK_CANARY"));
        assert_eq!(request.contains("ACTIVE_RESULT_CANARY"), !no_tools);
        assert_eq!(request.contains("[REDACTED]"), !no_tools);
        assert!(!root.join("cache").exists());
        assert!(!root.join("data").exists());
    }
    // Retain the fixture, including original local tool names, for inspection.
}
