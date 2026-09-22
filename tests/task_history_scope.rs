//! Directives and continuation anchors must come from the selected history,
//! not a sibling, another agent, a future turn, or stale prompt text.

use skillranker::context::{
    AnchorProvenance, AnchorResolution, CurrentRequest, EventKind, NormalizedContext,
    NormalizedEvent, PrivateText, Role, resolve_task_anchor,
};
use skillranker::identity::{AgentId, BranchId, EventId, HarnessId, SessionId};
use skillranker::privacy::redaction::Redactor;

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

fn context(events: Vec<NormalizedEvent>, prompt: &str) -> NormalizedContext {
    NormalizedContext {
        schema_version: 1,
        harness: HarnessId::new("claude_code").unwrap(),
        producer_id: None,
        workspace_root: PrivateText::new("/test-workspace"),
        session_id: Some(SessionId::new("session").unwrap()),
        agent_id: None,
        branch_id: None,
        context_epoch: None,
        current_request: CurrentRequest {
            event_id: Some(EventId::new("current").unwrap()),
            text: PrivateText::new(prompt),
            attachments_omitted: false,
            essential_attachment_missing: false,
        },
        events,
        explicit_skill_references: Vec::new(),
        supplied_loads: Vec::new(),
    }
}

fn resolve(context: &NormalizedContext) -> AnchorResolution {
    resolve_task_anchor(context, None, &Redactor::default())
}

#[test]
fn sibling_directives_neither_require_a_skill_nor_create_a_conflict() {
    for sibling in ["/use-skill private-sibling", "/exclude-skill alpha"] {
        let root = event("root", None, "/use-skill alpha");
        let other = event("sibling", Some("root"), sibling);
        let current = event("current", Some("root"), "continue");
        for events in [
            vec![root.clone(), other.clone(), current.clone()],
            vec![current.clone(), other.clone(), root.clone()],
            vec![root.clone(), current.clone(), other.clone()],
        ] {
            let resolved = resolve(&context(events, "continue"));
            let anchor = resolved.anchor().expect("active instruction resolves");
            assert_eq!(anchor.text, "/use-skill alpha");
            assert_eq!(anchor.source_event_id.as_ref().unwrap().as_str(), "root");
            assert_eq!(anchor.directives.len(), 1);
            assert_eq!(anchor.directives[0].target, "alpha");
        }
    }
}

#[test]
fn a_real_active_conflict_is_not_discarded_with_siblings() {
    let input = context(vec![
        event("root", None, "/use-skill alpha"),
        event("sibling", Some("root"), "/use-skill private-sibling"),
        event("current", Some("root"), "/exclude-skill alpha"),
    ], "/exclude-skill alpha");
    assert!(matches!(resolve(&input), AnchorResolution::ConflictingDirectives { .. }));
}

#[test]
fn parent_order_not_input_order_selects_the_continuation_antecedent() {
    let mut old = event("old", None, "Review the old implementation");
    old.timestamp_unix_ms = Some(900);
    let mut next = event("next", Some("old"), "Debug the new implementation");
    next.timestamp_unix_ms = Some(100);
    let input = context(vec![
        next, event("current", Some("next"), "continue"), old,
    ], "continue");
    let resolved = resolve(&input);
    let anchor = resolved.anchor().unwrap();
    assert_eq!(anchor.text, "Debug the new implementation");
    assert_eq!(anchor.provenance, AnchorProvenance::HistoricalEvent {
        event_id: EventId::new("next").unwrap(),
    });
}

#[test]
fn task_boundaries_only_stop_recovery_on_the_active_lineage() {
    let mut boundary = event("boundary", Some("root"), "");
    boundary.kind = EventKind::TaskBoundary;
    let root = event("root", None, "Debug the compiler");
    let good = context(vec![
        root.clone(), event("current", Some("root"), "continue"), boundary.clone(),
    ], "continue");
    assert_eq!(resolve(&good).anchor().unwrap().text, "Debug the compiler");
    let blocked = context(vec![root, boundary,
        event("current", Some("boundary"), "continue"),
    ], "continue");
    assert!(matches!(resolve(&blocked), AnchorResolution::MissingTaskContext { .. }));
}

#[test]
fn authoritative_prompt_replaces_stale_same_event_but_not_a_distinct_equal_turn() {
    let input = context(vec![
        event("root", None, "Fix the compiler"),
        event("current", Some("root"), "/exclude-skill alpha"),
    ], "/use-skill alpha");
    let resolved = resolve(&input);
    assert_eq!(resolved.anchor().unwrap().directives.len(), 1);
    assert_eq!(resolved.anchor().unwrap().directives[0].target, "alpha");
    let repeat = context(vec![
        event("prior", None, "/use-skill alpha"),
        event("current", Some("prior"), "/use-skill alpha"),
    ], "/use-skill alpha");
    assert_eq!(resolve(&repeat).anchor().unwrap().directives.len(), 2);
}

#[test]
fn flat_history_is_scoped_and_stops_at_the_current_event() {
    let agent = AgentId::new("active-agent").unwrap();
    let branch = BranchId::new("active-branch").unwrap();
    let mut input = context(vec![
        event("prior", None, "Debug this compiler"),
        event("current", None, "continue"),
        event("future", None, "/use-skill future-skill"),
    ], "continue");
    input.agent_id = Some(agent.clone());
    input.branch_id = Some(branch.clone());
    for e in &mut input.events {
        e.agent_id = Some(agent.clone());
        e.branch_id = Some(branch.clone());
    }
    input.events.insert(1, event("unassigned", None, "/use-skill unrelated"));
    let mut other = input.events[0].clone();
    other.branch_id = Some(BranchId::new("sibling").unwrap());
    other.text = PrivateText::new("/use-skill sibling-skill");
    input.events.push(other);
    let resolved = resolve(&input);
    assert_eq!(resolved.anchor().unwrap().text, "Debug this compiler");
    assert!(resolved.anchor().unwrap().directives.is_empty());
}

#[test]
fn a_pending_prompt_uses_only_a_unique_scoped_graph_leaf() {
    let mut input = context(vec![
        event("root", None, "Fix the compiler"),
        event("leaf", Some("root"), "continue"),
    ], "continue");
    assert_eq!(resolve(&input).anchor().unwrap().text, "Fix the compiler");
    input.events.push(event("other-leaf", Some("root"), "/use-skill beta"));
    assert!(matches!(resolve(&input), AnchorResolution::MissingTaskContext { .. }));
}

#[test]
fn cycles_conflicts_and_foreign_current_ids_do_not_fall_back_to_flat_history() {
    let cyclic = context(vec![
        event("root", Some("current"), "/use-skill alpha"),
        event("current", Some("root"), "continue"),
    ], "continue");
    assert!(matches!(resolve(&cyclic), AnchorResolution::MissingTaskContext { .. }));
    let conflict = context(vec![
        event("prior", None, "/use-skill alpha"),
        event("prior", None, "/use-skill beta"),
    ], "continue");
    assert!(matches!(resolve(&conflict), AnchorResolution::MissingTaskContext { .. }));
    let mut foreign = event("current", None, "/use-skill beta");
    foreign.agent_id = Some(AgentId::new("other").unwrap());
    let input = context(vec![event("prior", None, "Fix the compiler"), foreign], "continue");
    assert!(matches!(resolve(&input), AnchorResolution::MissingTaskContext { .. }));
}

#[test]
fn flat_pending_prompt_and_empty_prompt_only_context_keep_working() {
    let input = context(vec![event("prior", None, "/use-skill alpha")], "continue");
    assert_eq!(resolve(&input).anchor().unwrap().directives[0].target, "alpha");
    let input = context(Vec::new(), "Explain Rust lifetimes");
    assert_eq!(resolve(&input).anchor().unwrap().text, "Explain Rust lifetimes");
    assert!(matches!(resolve(&context(Vec::new(), "continue")),
        AnchorResolution::MissingTaskContext { .. }));
}

#[cfg(unix)]
#[test]
fn offline_cli_never_turns_a_sibling_directive_into_an_explicit_request() {
    use std::fs;
    use std::os::unix::fs::DirBuilderExt;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    let root = std::env::temp_dir().join(format!(
        "sr-task-history-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
    ));
    fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
    for directory in ["workspace/.claude/skills/alpha", "home", "config/sr"] {
        fs::create_dir_all(root.join(directory)).unwrap();
    }
    fs::write(root.join("workspace/.claude/skills/alpha/SKILL.md"),
        "---\nname: alpha\ndescription: Debug Rust tests\n---\nUse cargo test.\n").unwrap();
    let workspace = root.join("workspace");
    for (prior, expected_exit, expected_decision) in [
        ("Debug failing Rust tests", 11, "unavailable"),
        ("/use-skill alpha", 0, "explicit"),
    ] {
        let mut input = context(vec![
            event("root", None, prior),
            event("sibling", Some("root"), "/use-skill nonexistent-sibling-skill"),
            event("current", Some("root"), "continue"),
        ], "continue");
        input.workspace_root = PrivateText::new(workspace.to_str().unwrap());
        fs::write(workspace.join("context.json"), serde_json::to_vec(&input).unwrap()).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_sr"))
            .env_clear()
            .env("HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_DATA_HOME", root.join("data"))
            .current_dir(&workspace)
            .args(["rank", "--context", "context.json", "--offline", "--no-persist",
                "--json", "--timeout-ms", "10000"])
            .output().unwrap();
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(expected_exit), "{value}");
        assert_eq!(value["decision"], expected_decision, "{value}");
        assert_eq!(value["usage"]["http_attempts"], 0);
        if expected_exit == 11 {
            assert_eq!(value["error"]["kind"], "cache-miss");
        } else {
            assert_eq!(value["skills"][0]["invocation_name"], "alpha");
        }
    }
    // Retain the private fixture, as required by the repository's no-deletion rule.
}
