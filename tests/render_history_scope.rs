//! Both public rendering APIs must exclude non-active prose before windowing
//! and preserve honest disclosure counts for the exact final payload.

use skillranker::context::{
    CurrentRequest, EventKind, NormalizedContext, NormalizedEvent, PrivateText,
    RenderContextOptions, Role, ToolEvent, ToolStatus, render_context,
    render_context_and_receipt,
};
use skillranker::identity::{AgentId, EventId, HarnessId};
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

fn tool(id: &str, parent: &str, text: &str) -> NormalizedEvent {
    let mut event = event(id, Some(parent), "");
    event.role = Role::Tool;
    event.kind = EventKind::ToolResult;
    event.tool = Some(ToolEvent {
        call_id: None,
        name: PrivateText::new("Read"),
        status: ToolStatus::Succeeded,
        arguments: None,
        result: Some(PrivateText::new(text)),
    });
    event
}

fn context(events: Vec<NormalizedEvent>) -> NormalizedContext {
    NormalizedContext {
        schema_version: 1,
        harness: HarnessId::new("claude_code").unwrap(),
        producer_id: None,
        workspace_root: PrivateText::new("/test-workspace"),
        session_id: None,
        agent_id: None,
        branch_id: None,
        context_epoch: None,
        current_request: CurrentRequest {
            event_id: Some(EventId::new("current").unwrap()),
            text: PrivateText::new("Explain Rust lifetimes"),
            attachments_omitted: false,
            essential_attachment_missing: false,
        },
        events,
        explicit_skill_references: Vec::new(),
        supplied_loads: Vec::new(),
    }
}

#[test]
fn sibling_tool_bodies_are_excluded_in_every_input_order_with_receipt_parity() {
    let events = [
        event("root", None, "Active history about the Rust compiler"),
        tool("active-result", "root", "Active compiler output"),
        tool("sibling-result", "root", "PRIVATE_SIBLING_BODY_CANARY"),
        event("current", Some("active-result"), "Explain Rust lifetimes"),
    ];
    let options = RenderContextOptions::default();
    let mut baseline = None;
    for order in [[0, 1, 2, 3], [3, 2, 1, 0], [2, 0, 3, 1], [0, 3, 1, 2]] {
        let input = context(order.iter().map(|&index| events[index].clone()).collect());
        let original = input.clone();
        let (payload, receipt) = render_context_and_receipt(&input, &options).unwrap();
        assert_eq!(input, original);
        assert_eq!(render_context(&input, &options).unwrap(), payload);
        let bytes = payload.to_json_bytes().unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(!text.contains("PRIVATE_SIBLING_BODY_CANARY"));
        assert!(text.contains("Active compiler output"));
        assert_eq!(text.matches("Explain Rust lifetimes").count(), 1);
        assert_eq!(payload.recent_messages.len(), 2);
        assert_eq!(receipt.disclosed_bytes, bytes.len());
        assert_eq!(receipt.disclosed_scalars, payload.total_message_scalars());
        assert_eq!(receipt.context_quality, payload.context_quality);
        let tools = receipt.categories.iter()
            .find(|category| category.category == SourceCategory::ToolEvents).unwrap();
        assert_eq!(tools.included_count, 1);
        assert_eq!(tools.omitted_count, 1);
        assert_eq!(receipt.total_omitted, 1);
        if let Some(baseline) = &baseline {
            assert_eq!(&payload, baseline);
        } else {
            baseline = Some(payload);
        }
    }
}

#[test]
fn foreign_reused_ids_cannot_disclose_bodies_or_hide_omissions() {
    let active = tool("read", "root", "Active result");
    let mut foreign = active.clone();
    foreign.agent_id = Some(AgentId::new("other-agent").unwrap());
    foreign.tool.as_mut().unwrap().result = Some(PrivateText::new("FOREIGN_BODY_CANARY"));
    let input = context(vec![
        event("root", None, "Active history"), active,
        event("current", Some("read"), "Explain Rust lifetimes"), foreign,
    ]);
    let (payload, receipt) = render_context_and_receipt(&input, &RenderContextOptions::default()).unwrap();
    assert!(!String::from_utf8(payload.to_json_bytes().unwrap()).unwrap().contains("FOREIGN_BODY_CANARY"));
    assert_eq!(receipt.total_omitted, 1);
}

#[test]
fn sibling_volume_cannot_evict_active_context_from_the_message_window() {
    let mut input = context(vec![
        event("root", None, "ACTIVE_ANTECEDENT_CANARY"),
        event("current", Some("root"), "Explain Rust lifetimes"),
    ]);
    for index in 0..100 {
        input.events.push(event(&format!("sibling-{index}"), Some("root"), "PRIVATE_WINDOW_CANARY"));
    }
    let (payload, receipt) = render_context_and_receipt(&input, &RenderContextOptions::default()).unwrap();
    assert_eq!(payload.recent_messages.len(), 1);
    assert_eq!(payload.recent_messages[0].text.as_deref(), Some("ACTIVE_ANTECEDENT_CANARY"));
    assert_eq!(receipt.total_omitted, 100);
    assert_eq!(payload.context_quality, ContextQuality::Complete);
}

#[test]
fn incomplete_lineage_is_partial_and_receipt_bytes_describe_the_final_payload() {
    let input = context(vec![
        event("known", Some("outside-window"), "Known active context"),
        event("current", Some("known"), "Explain Rust lifetimes"),
    ]);
    let options = RenderContextOptions::default();
    let (payload, receipt) = render_context_and_receipt(&input, &options).unwrap();
    assert_eq!(payload.context_quality, ContextQuality::Partial);
    assert_eq!(receipt.context_quality, ContextQuality::Partial);
    assert_eq!(receipt.disclosed_bytes, payload.to_json_bytes().unwrap().len());
    assert_eq!(render_context(&input, &options).unwrap(), payload);
}

#[test]
fn unresolved_graph_is_refused_in_both_profiles_and_both_entry_points() {
    let input = context(vec![
        event("root", None, "Known root"),
        event("left", Some("root"), "PRIVATE_LEFT_CANARY"),
        event("right", Some("root"), "PRIVATE_RIGHT_CANARY"),
    ]);
    for profile in [ContextProfile::Standard, ContextProfile::Minimal] {
        let options = RenderContextOptions { context_profile: profile, ..Default::default() };
        let error = render_context(&input, &options).unwrap_err();
        assert!(!error.to_string().contains("CANARY"));
        assert_eq!(render_context_and_receipt(&input, &options).unwrap_err(), error);
    }
}

#[test]
fn profile_omissions_and_scope_omissions_are_reconciled_without_double_counting() {
    let input = context(vec![
        event("root", None, "Active history"),
        tool("active-result", "root", "Active output"),
        event("sibling", Some("root"), "PRIVATE_HISTORY_CANARY"),
        tool("sibling-result", "root", "PRIVATE_TOOL_CANARY"),
        event("current", Some("active-result"), "Explain Rust lifetimes"),
    ]);
    let options = RenderContextOptions { context_profile: ContextProfile::Minimal, ..Default::default() };
    let (payload, receipt) = render_context_and_receipt(&input, &options).unwrap();
    assert!(payload.recent_messages.is_empty());
    assert_eq!(receipt.total_omitted, 4);
    assert_eq!(receipt.total_included, 1);
    assert_eq!(receipt.total_omitted, receipt.categories.iter().map(|c| c.omitted_count).sum::<usize>());
}

#[cfg(unix)]
#[test]
fn production_dry_run_contains_active_history_but_no_sibling_prose() {
    use std::fs;
    use std::os::unix::fs::DirBuilderExt;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    let root = std::env::temp_dir().join(format!(
        "sr-render-history-{}-{}", std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
    ));
    fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
    for path in ["workspace/.claude/skills/alpha", "home", "config/sr"] {
        fs::create_dir_all(root.join(path)).unwrap();
    }
    fs::write(root.join("workspace/.claude/skills/alpha/SKILL.md"),
        "---\nname: alpha\ndescription: Explain Rust lifetimes\n---\nReference text.\n").unwrap();
    let mut input = context(vec![
        event("root", None, "ACTIVE_CONTEXT_CANARY"),
        tool("sibling", "root", "PRIVATE_PREVIEW_CANARY"),
        event("current", Some("root"), "Explain Rust lifetimes"),
    ]);
    let workspace = root.join("workspace");
    input.workspace_root = PrivateText::new(workspace.to_str().unwrap());
    fs::write(workspace.join("context.json"), serde_json::to_vec(&input).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_sr"))
        .env_clear()
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_DATA_HOME", root.join("data"))
        .current_dir(&workspace)
        .args(["rank", "--context", "context.json", "--dry-run", "--json", "--timeout-ms", "10000"])
        .output().unwrap();
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{value}");
    assert_eq!(value["kind"], "preview");
    assert!(!value["provider_request"].is_null(), "{value}");
    let encoded = value.to_string();
    assert!(encoded.contains("ACTIVE_CONTEXT_CANARY"));
    assert!(!encoded.contains("PRIVATE_PREVIEW_CANARY"));
    // Retain fixture files for inspection.
}
