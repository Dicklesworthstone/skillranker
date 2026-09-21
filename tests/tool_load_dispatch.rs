//! The production tool resolver must not manufacture loads from a skill/tool
//! name collision, ambiguous JSON, or a path-only historical file read.

use serde_json::json;
use skillranker::context::tool::{
    SimpleSkillResolver, SkillEvidenceResolver, SkillMatch, extract_loaded_skill_records,
    filter_events_for_provider,
};
use skillranker::context::{
    EventKind, NormalizedEvent, PrivateText, Role, SkillUsageKind, ToolEvent, ToolStatus,
};
use skillranker::identity::{ContentHash, ContextEpoch, EventId, SkillId};
use skillranker::limits::NORMALIZED_CONTEXT_JSON_BYTES;

fn matched(id: &str) -> SkillMatch {
    SkillMatch {
        skill_id: SkillId::new(id).unwrap(),
        usage_kind: SkillUsageKind::Reference,
        source_content: Some(ContentHash::from_bytes(b"current source")),
        rendered_content: Some(ContentHash::from_bytes(b"known rendered reference")),
        has_dynamic_arguments: false,
        turn_scoped: false,
    }
}

fn resolver() -> SimpleSkillResolver {
    let mut resolver = SimpleSkillResolver::new();
    resolver.register_tool("alpha", matched("alpha-id"));
    resolver.register_tool("beta", matched("beta-id"));
    resolver.register_path("/skills/alpha/SKILL.md", matched("alpha-id"));
    resolver.register_path("/skills/beta/SKILL.md", matched("beta-id"));
    resolver
}

fn successful(tool: &str, arguments: &str) -> NormalizedEvent {
    NormalizedEvent {
        event_id: Some(EventId::new("load-1").unwrap()),
        parent_id: None,
        turn_id: None,
        agent_id: None,
        branch_id: None,
        role: Role::Assistant,
        kind: EventKind::ToolInvocation,
        timestamp_unix_ms: None,
        text: PrivateText::new(""),
        tool: Some(ToolEvent {
            call_id: None,
            name: PrivateText::new(tool),
            status: ToolStatus::Succeeded,
            arguments: Some(PrivateText::new(arguments)),
            result: Some(PrivateText::new("Loaded successfully")),
        }),
    }
}

#[test]
fn reserved_loader_names_cannot_intercept_other_skills() {
    for loader in ["Skill", "skill", "load_skill", "RUN_SKILL"] {
        let mut resolver = resolver();
        resolver.register_tool(loader, matched("reserved-name-id"));
        let actual = resolver
            .resolve_tool(loader, Some(r#"{"skill":"alpha"}"#))
            .unwrap();
        assert_eq!(actual.skill_id.as_str(), "alpha-id");
        assert!(resolver.resolve_tool(loader, None).is_none());
        assert!(
            resolver
                .resolve_tool(loader, Some(r#"{"skill":"unknown"}"#))
                .is_none()
        );
        // The reserved word is still a legitimate explicit target.
        let arguments = json!({"skill": loader}).to_string();
        assert_eq!(
            resolver
                .resolve_tool("Skill", Some(&arguments))
                .unwrap()
                .skill_id
                .as_str(),
            "reserved-name-id"
        );
    }
}

#[test]
fn reserved_read_names_cannot_manufacture_a_skill_load() {
    for reader in ["Read", "read_file", "VIEW_FILE", "cat"] {
        let mut resolver = resolver();
        resolver.register_tool(reader, matched("reserved-name-id"));
        assert!(
            resolver
                .resolve_tool(reader, Some(r#"{"path":"/unrelated.txt"}"#))
                .is_none()
        );
        let actual = resolver
            .resolve_tool(reader, Some(r#"{"file_path":"/skills/alpha/SKILL.md"}"#))
            .unwrap();
        assert_eq!(actual.skill_id.as_str(), "alpha-id");
        assert!(actual.source_content.is_none());
        assert!(actual.rendered_content.is_none());
    }
}

#[test]
fn all_present_target_fields_must_identify_one_valid_target() {
    let resolver = resolver();
    for bad in [
        r#"{"skill":"alpha","name":"beta"}"#,
        r#"{"name":"beta","skill":"alpha"}"#,
        r#"{"skill":"unknown","skill_name":"alpha"}"#,
        r#"{"skill":null,"name":"alpha"}"#,
        r#"{"skill":"alpha","name":17}"#,
        r#"{"skill":"","name":"alpha"}"#,
        r#"{"skill":false}"#,
    ] {
        assert!(resolver.resolve_tool("Skill", Some(bad)).is_none(), "{bad}");
    }
    for good in [
        r#"{"skill":"alpha"}"#,
        r#"{"skill_name":"alpha"}"#,
        r#"{"name":"alpha"}"#,
        r#"{"skill":"alpha","skill_name":"alpha","name":"alpha"}"#,
    ] {
        assert_eq!(
            resolver.resolve_tool("Skill", Some(good)).unwrap(),
            matched("alpha-id")
        );
    }
    for bad in [
        r#"{"path":"/skills/alpha/SKILL.md","file_path":"/skills/beta/SKILL.md"}"#,
        r#"{"path":"/unknown","target":"/skills/alpha/SKILL.md"}"#,
        r#"{"path":null,"target":"/skills/alpha/SKILL.md"}"#,
    ] {
        assert!(resolver.resolve_tool("Read", Some(bad)).is_none(), "{bad}");
    }
}

#[test]
fn duplicate_keys_and_malformed_documents_never_become_load_evidence() {
    let resolver = resolver();
    for bad in [
        r#"{"skill":"beta","skill":"alpha"}"#,
        r#"{"skill":"alpha","skill":"alpha"}"#,
        r#"{"skill":"beta","\u0073kill":"alpha"}"#,
        r#"{"skill":"alpha","extra":{"x":1,"x":2}}"#,
        r#"{"skill":"alpha"} {"skill":"beta"}"#,
        r#"{"skill":"alpha""#,
        r#"["alpha"]"#,
        "null",
        "",
    ] {
        assert!(resolver.resolve_tool("Skill", Some(bad)).is_none(), "{bad}");
    }
    assert!(
        resolver
            .resolve_tool(
                "Read",
                Some(r#"{"path":"/skills/beta/SKILL.md","path":"/skills/alpha/SKILL.md"}"#)
            )
            .is_none()
    );
}

#[test]
fn argument_documents_obey_the_context_byte_and_depth_bounds() {
    let resolver = resolver();
    let huge = format!(
        "{{\"skill\":\"alpha\",\"extra\":\"{}\"}}",
        "x".repeat(NORMALIZED_CONTEXT_JSON_BYTES.max())
    );
    assert!(resolver.resolve_tool("Skill", Some(&huge)).is_none());
    let deep = format!(
        "{{\"skill\":\"alpha\",\"extra\":{}0{}}}",
        "[".repeat(100),
        "]".repeat(100)
    );
    assert!(resolver.resolve_tool("Skill", Some(&deep)).is_none());
    assert!(
        resolver
            .resolve_tool("Skill", Some(r#"{"skill":"alpha","extra":{"x":1}}"#))
            .is_some()
    );
}

#[test]
fn path_only_evidence_cannot_inherit_registered_current_hashes() {
    let resolver = resolver();
    let actual = resolver.resolve_file_read("/skills/alpha/SKILL.md").unwrap();
    assert_eq!(actual.skill_id.as_str(), "alpha-id");
    assert!(actual.source_content.is_none());
    assert!(actual.rendered_content.is_none());
    // Resolution must not mutate the registration or a direct-tool contract.
    assert_eq!(
        resolver.resolve_tool("alpha", None),
        Some(matched("alpha-id"))
    );
    assert!(resolver.resolve_file_read("/unknown").is_none());
}

#[test]
fn parameterized_loads_do_not_reuse_parameterless_rendered_evidence() {
    let resolver = resolver();
    for args in [
        json!("production"),
        json!({"target": "production"}),
        json!(["x"]),
    ] {
        let arguments = json!({"skill":"alpha", "args":args}).to_string();
        let actual = resolver.resolve_tool("Skill", Some(&arguments)).unwrap();
        assert_eq!(actual.skill_id.as_str(), "alpha-id");
        assert!(actual.has_dynamic_arguments);
        assert!(actual.rendered_content.is_none());
    }
    for good in [
        r#"{"skill":"alpha","args":""}"#,
        r#"{"skill":"alpha"}"#,
    ] {
        assert_eq!(
            resolver.resolve_tool("Skill", Some(good)),
            Some(matched("alpha-id"))
        );
    }
}

#[test]
fn production_extraction_keeps_load_identity_and_unknown_versions() {
    let mut resolver = resolver();
    resolver.register_tool("Read", matched("read-name-id"));
    resolver.register_tool("Skill", matched("skill-name-id"));
    let epoch = ContextEpoch::new("epoch-1").unwrap();
    let unrelated = successful("Read", r#"{"path":"/unrelated.txt"}"#);
    assert!(extract_loaded_skill_records(&[unrelated], &resolver, None, &epoch).is_empty());
    let ambiguous = successful("Skill", r#"{"skill":"alpha","name":"beta"}"#);
    assert!(extract_loaded_skill_records(&[ambiguous], &resolver, None, &epoch).is_empty());

    let event = successful("Read", r#"{"path":"/skills/alpha/SKILL.md"}"#);
    let records =
        extract_loaded_skill_records(std::slice::from_ref(&event), &resolver, None, &epoch);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].skill_id.as_str(), "alpha-id");
    assert!(records[0].source_content.is_none());
    assert!(records[0].rendered_content.is_none());
    // Provider omission is independent of retained local evidence.
    let filtered = filter_events_for_provider(std::slice::from_ref(&event), true, 200);
    assert!(filtered[0].tool.as_ref().unwrap().arguments.is_none());
    assert!(event.tool.as_ref().unwrap().arguments.is_some());
    assert_eq!(
        extract_loaded_skill_records(&[event], &resolver, None, &epoch),
        records
    );
}
