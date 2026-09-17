use skillranker::context::{
    ContextError, CurrentRequest, EventKind, EvidenceOrigin, NormalizedContext, NormalizedEvent,
    PrivateText, Role, SuppliedLoadClaim, parse_normalized_context,
};
use skillranker::identity::*;
use skillranker::roster::*;
use std::collections::BTreeSet;

fn identity() -> SessionIdentity {
    SessionIdentity {
        source: SourceProvenance::Native {
            adapter: AdapterId::new("claude_code").unwrap(),
            version: AdapterVersion::new("fixture-v1").unwrap(),
        },
        workspace: Some(WorkspaceId::new("workspace-a").unwrap()),
        session: Some(SessionId::new("session-a").unwrap()),
        agent: Some(AgentId::new("agent-a").unwrap()),
        branch: Some(BranchId::new("branch-a").unwrap()),
        epoch: Some(ContextEpoch::new("epoch-a").unwrap()),
    }
}

fn event(id: &str) -> NormalizedEvent {
    NormalizedEvent {
        event_id: Some(EventId::new(id).unwrap()),
        parent_id: None,
        turn_id: None,
        agent_id: None,
        branch_id: None,
        role: Role::User,
        kind: EventKind::Message,
        timestamp_unix_ms: None,
        text: PrivateText::new("continue"),
        tool: None,
    }
}

fn context() -> NormalizedContext {
    NormalizedContext {
        schema_version: 1,
        harness: HarnessId::new("claude_code").unwrap(),
        producer_id: Some(ProducerId::new("producer-a").unwrap()),
        workspace_root: PrivateText::new("/private/declared/path"),
        session_id: Some(SessionId::new("session-a").unwrap()),
        agent_id: Some(AgentId::new("agent-a").unwrap()),
        branch_id: Some(BranchId::new("branch-a").unwrap()),
        context_epoch: Some(ContextEpoch::new("epoch-a").unwrap()),
        current_request: CurrentRequest {
            event_id: Some(EventId::new("event-current").unwrap()),
            text: PrivateText::new("continue"),
            attachments_omitted: false,
            essential_attachment_missing: false,
        },
        events: vec![event("event-previous")],
        explicit_skill_references: vec![],
        supplied_loads: vec![],
    }
}

#[test]
fn documented_incomplete_context_round_trips_without_durable_authority() {
    let fixture = include_str!("fixtures/normalized-context.v1.json");
    let input = parse_normalized_context(fixture.as_bytes()).unwrap();
    assert_eq!(input.validate_definitions(), Ok(()));
    let decoded = parse_normalized_context(&serde_json::to_vec(&input).unwrap()).unwrap();
    assert_eq!(decoded, input);
    assert!(
        input
            .session_identity(identity().workspace)
            .unwrap()
            .durable_namespace()
            .is_err()
    );
}

#[test]
fn every_session_dimension_changes_the_durable_namespace() {
    let original = identity();
    let baseline = original.durable_namespace().unwrap();
    let mut variants = Vec::new();
    for n in 0..8 {
        let mut changed = original.clone();
        match n {
            0 => changed.workspace = Some(WorkspaceId::new("workspace-b").unwrap()),
            1 => changed.session = Some(SessionId::new("session-b").unwrap()),
            2 => changed.agent = Some(AgentId::new("agent-b").unwrap()),
            3 => changed.branch = Some(BranchId::new("branch-b").unwrap()),
            4 => changed.epoch = Some(ContextEpoch::new("epoch-b").unwrap()),
            5 => {
                changed.source = SourceProvenance::Native {
                    adapter: AdapterId::new("other-harness").unwrap(),
                    version: AdapterVersion::new("fixture-v1").unwrap(),
                }
            }
            6 => {
                changed.source = SourceProvenance::Native {
                    adapter: AdapterId::new("claude_code").unwrap(),
                    version: AdapterVersion::new("fixture-v2").unwrap(),
                }
            }
            _ => {
                changed.source = SourceProvenance::Cass {
                    source: Some(SourceId::new("archive-a").unwrap()),
                    version: AdapterVersion::new("fixture-v1").unwrap(),
                }
            }
        }
        variants.push(changed.durable_namespace().unwrap());
    }
    assert!(variants.iter().all(|key| key != &baseline));
    assert_eq!(variants.into_iter().collect::<BTreeSet<_>>().len(), 8);
    assert_eq!(original.durable_namespace().unwrap(), baseline);
}

#[test]
fn missing_attribution_is_invocation_local_and_cannot_update_durable_state() {
    for n in 0..7 {
        let mut value = identity();
        match n {
            0 => value.workspace = None,
            1 => value.session = None,
            2 => value.agent = None,
            3 => value.branch = None,
            4 => value.epoch = None,
            5 => {
                value.source = SourceProvenance::Normalized {
                    producer: None,
                    harness: HarnessId::new("claude_code").unwrap(),
                    schema_version: 1,
                }
            }
            _ => {
                value.source = SourceProvenance::Cass {
                    source: None,
                    version: AdapterVersion::new("fixture-v1").unwrap(),
                }
            }
        }
        assert_eq!(
            value.durable_namespace(),
            Err(IdentityError::UnknownAttribution)
        );
        let a = value.namespace(InvocationId::new("invocation-a").unwrap());
        let b = value.namespace(InvocationId::new("invocation-b").unwrap());
        assert!(matches!(a, SessionNamespace::InvocationLocal { .. }));
        assert_ne!(a, b);
    }
}

#[test]
fn normalized_producers_cannot_enter_native_or_each_others_namespace() {
    let a = context();
    let mut b = a.clone();
    b.producer_id = Some(ProducerId::new("producer-b").unwrap());
    let workspace = identity().workspace;
    let a_key = a
        .session_identity(workspace.clone())
        .unwrap()
        .durable_namespace()
        .unwrap();
    let b_key = b
        .session_identity(workspace)
        .unwrap()
        .durable_namespace()
        .unwrap();
    assert_ne!(a_key, identity().durable_namespace().unwrap());
    assert_ne!(a_key, b_key);
    b.producer_id = a.producer_id.clone();
    b.harness = HarnessId::new("other-harness").unwrap();
    assert_ne!(
        a_key,
        b.session_identity(identity().workspace)
            .unwrap()
            .durable_namespace()
            .unwrap()
    );
    // A declaration of a private path is never independently resolved workspace authority.
    assert!(
        a.session_identity(None)
            .unwrap()
            .durable_namespace()
            .is_err()
    );
}

#[test]
fn explicit_null_unknowns_round_trip_and_private_debug_stays_private() {
    let mut input = context();
    input.agent_id = None;
    input.branch_id = None;
    input.current_request.text = PrivateText::new("CANARY-PRIVATE-CONTENT");
    let json = serde_json::to_value(&input).unwrap();
    assert!(json["agent_id"].is_null());
    assert!(json["branch_id"].is_null());
    let decoded = parse_normalized_context(&serde_json::to_vec(&json).unwrap()).unwrap();
    assert_eq!(decoded, input);
    let debug = format!("{input:?}");
    assert!(!debug.contains("CANARY-PRIVATE-CONTENT"));
    assert!(!debug.contains("/private/declared/path"));
    assert!(!debug.contains("session-a"));
}

#[test]
fn supplied_success_and_rendered_hash_never_become_observed_evidence() {
    let claim = SuppliedLoadClaim {
        skill_id: SkillId::new("skill-a").unwrap(),
        source_content: Some(ContentHash::from_bytes(b"source")),
        rendered_content: Some(ContentHash::from_bytes(b"rendered")),
    };
    assert_eq!(claim.evidence_origin(), EvidenceOrigin::Supplied);
    let mut forged = serde_json::to_value(&claim).unwrap();
    forged["origin"] = serde_json::json!("observed");
    assert!(serde_json::from_value::<SuppliedLoadClaim>(forged).is_err());
    let mut input = serde_json::to_value(context()).unwrap();
    input["network"] = serde_json::json!({"enabled": true});
    assert!(parse_normalized_context(&serde_json::to_vec(&input).unwrap()).is_err());
}

#[test]
fn duplicate_definitions_differ_from_valid_references_and_identical_turn_text() {
    let mut input = context();
    input.events.push(event("event-current"));
    // Current prompt references the already-present event. Equal text is not dedup evidence.
    assert_eq!(input.validate_definitions(), Ok(()));
    assert_eq!(input.events.len(), 2);
    assert_eq!(
        parse_normalized_context(&serde_json::to_vec(&input).unwrap()).unwrap(),
        input
    );
    input.events.push(event("event-current"));
    assert_eq!(
        input.validate_definitions(),
        Err(ContextError::DuplicateEvent)
    );
    assert_eq!(
        parse_normalized_context(&serde_json::to_vec(&input).unwrap()),
        Err(ContextError::DuplicateEvent)
    );

    let ids = vec![
        SkillId::new("skill-a").unwrap(),
        SkillId::new("skill-b").unwrap(),
    ];
    assert_eq!(validate_definition_ids(&ids), Ok(()));
    // The rerank collection may reference the same IDs as the wide collection.
    assert_eq!(validate_definition_ids(&ids), Ok(()));
    assert_eq!(
        validate_definition_ids([&ids[0], &ids[0]]),
        Err(IdentityError::DuplicateDefinition)
    );
}

#[test]
fn anonymous_events_are_context_but_not_durable_event_evidence() {
    let mut input = context();
    input.events[0].event_id = None;
    input.events.push(input.events[0].clone());
    input.current_request.event_id = None;
    assert_eq!(input.validate_definitions(), Ok(()));
    let round_trip = parse_normalized_context(&serde_json::to_vec(&input).unwrap()).unwrap();
    assert_eq!(round_trip, input);
    assert_eq!(
        identity().event_key(input.current_request.event_id.as_ref()),
        Err(IdentityError::UnknownAttribution)
    );
    for event in &input.events {
        assert_eq!(
            identity().event_key(event.event_id.as_ref()),
            Err(IdentityError::UnknownAttribution)
        );
    }
    let event = EventId::new("known-event").unwrap();
    assert!(identity().event_key(Some(&event)).is_ok());
    let mut incomplete = identity();
    incomplete.branch = None;
    assert_eq!(
        incomplete.event_key(Some(&event)),
        Err(IdentityError::UnknownAttribution)
    );
}

#[test]
fn duplicate_json_keys_and_unsupported_schema_are_not_silently_accepted() {
    let input = serde_json::to_string(&context()).unwrap();
    let duplicate = input.replacen(
        "\"schema_version\":1",
        "\"schema_version\":1,\"schema_version\":2",
        1,
    );
    assert_eq!(
        parse_normalized_context(duplicate.as_bytes()),
        Err(ContextError::DuplicateKey)
    );
    let mut future = context();
    future.schema_version = 2;
    assert_eq!(
        parse_normalized_context(&serde_json::to_vec(&future).unwrap()),
        Err(ContextError::UnsupportedSchema)
    );
    assert_eq!(
        future.validate_definitions(),
        Err(ContextError::UnsupportedSchema)
    );
    assert!(future.session_identity(identity().workspace).is_err());
}

#[test]
fn duplicate_fields_are_rejected_inside_every_normalized_record_type() {
    use skillranker::context::{ToolEvent, ToolStatus};
    let mut input = context();
    input.events[0].tool = Some(ToolEvent {
        call_id: Some(ToolCallId::new("call-a").unwrap()),
        name: PrivateText::new("read"),
        status: ToolStatus::Succeeded,
        arguments: None,
        result: Some(PrivateText::new("body")),
    });
    input.supplied_loads.push(SuppliedLoadClaim {
        skill_id: SkillId::new("skill-a").unwrap(),
        source_content: None,
        rendered_content: None,
    });
    let raw = serde_json::to_string(&input).unwrap();
    assert!(parse_normalized_context(raw.as_bytes()).is_ok());
    for (field, value) in [
        ("event_id", "null"),
        ("attachments_omitted", "false"),
        ("parent_id", "null"),
        ("call_id", "null"),
        ("arguments", "null"),
        ("skill_id", "\"skill-a\""),
        ("rendered_content", "null"),
    ] {
        let prefix = format!("\"{field}\":");
        let corrupt = raw.replacen(&prefix, &format!("{prefix}{value},{prefix}"), 1);
        assert!(
            parse_normalized_context(corrupt.as_bytes()).is_err(),
            "{field}"
        );
    }
}

#[test]
fn normalized_wire_bounds_reject_before_schema_conversion() {
    use skillranker::limits::NORMALIZED_CONTEXT_JSON_BYTES;
    let mut bytes = serde_json::to_vec(&context()).unwrap();
    bytes.resize(NORMALIZED_CONTEXT_JSON_BYTES.max(), b' ');
    assert_eq!(parse_normalized_context(&bytes).unwrap(), context());
    bytes.push(b' ');
    assert_eq!(
        parse_normalized_context(&bytes),
        Err(ContextError::LimitExceeded)
    );

    // Root depth is zero. Unknown fields still undergo the depth check before
    // schema rejection, so a hostile ignored subtree cannot bypass the bound.
    let at_limit = format!("{}0{}", "[".repeat(64), "]".repeat(64));
    assert_eq!(
        parse_normalized_context(at_limit.as_bytes()),
        Err(ContextError::InvalidField)
    );
    let too_deep = format!("[{at_limit}]");
    assert_eq!(
        parse_normalized_context(too_deep.as_bytes()),
        Err(ContextError::LimitExceeded)
    );
    assert_eq!(
        parse_normalized_context(b"{} {}"),
        Err(ContextError::InvalidJson)
    );
    assert_eq!(
        parse_normalized_context(b"\xff"),
        Err(ContextError::InvalidJson)
    );
}

#[test]
fn normalized_load_definitions_do_not_deduplicate_skill_references() {
    let mut input = context();
    let skill = SkillId::new("skill-a").unwrap();
    input.explicit_skill_references = vec![skill.clone(), skill.clone()];
    input.supplied_loads.push(SuppliedLoadClaim {
        skill_id: skill,
        source_content: None,
        rendered_content: None,
    });
    let decoded = parse_normalized_context(&serde_json::to_vec(&input).unwrap()).unwrap();
    assert_eq!(
        decoded.explicit_skill_references,
        input.explicit_skill_references
    );
    assert_eq!(
        decoded.supplied_loads[0].evidence_origin(),
        EvidenceOrigin::Supplied
    );
    input.supplied_loads.push(input.supplied_loads[0].clone());
    assert_eq!(
        parse_normalized_context(&serde_json::to_vec(&input).unwrap()),
        Err(ContextError::DuplicateLoadDefinition)
    );
}

#[test]
fn opaque_ids_are_bounded_and_diagnostics_do_not_echo_bad_input() {
    for bad in [
        "",
        " ",
        "two words",
        "secret\nline",
        "\u{202e}secret",
        "\u{1b}[31m",
    ] {
        let error = SessionId::new(bad).unwrap_err();
        // A space is valid punctuation in the fixed diagnostic; its presence
        // is not evidence that the rejected input was echoed. Exact messages
        // enforce the privacy contract even for empty/whitespace-only input.
        let expected = if bad.is_empty() {
            "identity is empty"
        } else {
            "identity contains a forbidden character"
        };
        assert_eq!(error.to_string(), expected);
        if !bad.trim().is_empty() {
            assert!(!error.to_string().contains(bad));
        }
    }
    assert!(SessionId::new("a".repeat(MAX_ID_BYTES)).is_ok());
    assert_eq!(
        SessionId::new("a".repeat(MAX_ID_BYTES + 1)),
        Err(IdentityError::TooLong)
    );
    assert!(SessionId::new("é".repeat(MAX_ID_BYTES / 2)).is_ok());
    assert!(SessionId::new("é".repeat(MAX_ID_BYTES / 2 + 1)).is_err());
    assert!(serde_json::from_str::<SessionId>("\"\"").is_err());
    assert_eq!(
        OptionId::new("__none__"),
        Err(IdentityError::ReservedOption)
    );
    assert_eq!(SkillId::new("__none__"), Err(IdentityError::ReservedOption));
    assert!(OptionId::new("candidate-0").is_ok());
}

#[test]
fn stable_skill_ids_are_source_scoped_framed_and_content_independent() {
    let mut ids = BTreeSet::new();
    for source in ["a", "ab", "source-α"] {
        for key in ["bc", "c", "name"] {
            let id = SkillId::from_source(
                &SourceId::new(source).unwrap(),
                &LogicalSkillKey::new(key).unwrap(),
            );
            assert!(ids.insert(id.clone()));
            assert_eq!(
                id,
                SkillId::from_source(
                    &SourceId::new(source).unwrap(),
                    &LogicalSkillKey::new(key).unwrap()
                )
            );
        }
    }
    assert_ne!(
        ContentHash::from_bytes(b"old"),
        ContentHash::from_bytes(b"new")
    );
    let digest = ContentHash::from_bytes(b"exact bytes");
    assert_eq!(ContentHash::parse(digest.as_str()).unwrap(), digest);
    assert!(ContentHash::parse("f".repeat(63)).is_err());
    assert!(ContentHash::parse("G".repeat(64)).is_err());
}

#[test]
fn display_names_are_not_invocations_and_manual_only_stays_manual() {
    assert_eq!(
        DisplayName::from_text("hello\u{1b}\n\u{202e}world").as_str(),
        "helloworld"
    );
    assert!(InvocationName::new("display name").is_err());
    assert_eq!(
        InvocationName::new("plugin:skill-name").unwrap().as_str(),
        "plugin:skill-name"
    );
    assert_eq!(
        InvocationRestrictions {
            agent_invocable: true,
            user_invocable: false
        }
        .explicit_kind(),
        InvocationKind::Agent
    );
    assert_eq!(
        InvocationRestrictions {
            agent_invocable: false,
            user_invocable: true
        }
        .explicit_kind(),
        InvocationKind::ManualOnly
    );
    assert_eq!(
        InvocationRestrictions {
            agent_invocable: false,
            user_invocable: false
        }
        .explicit_kind(),
        InvocationKind::Forbidden
    );
    let aliases = [
        SkillAlias {
            source: SourceId::new("user-root").unwrap(),
            invocation: InvocationName::new("skill").unwrap(),
        },
        SkillAlias {
            source: SourceId::new("plugin-root").unwrap(),
            invocation: InvocationName::new("plugin:skill").unwrap(),
        },
    ];
    assert_ne!(aliases[0], aliases[1]);
    assert_eq!(aliases.len(), 2);
}

#[test]
fn untrusted_option_and_roster_names_do_not_leak_through_debug() {
    let canary = "PRIVATE-CANARY";
    let option = OptionId::new(canary).unwrap();
    let invocation = InvocationName::new(canary).unwrap();
    let display = DisplayName::from_text(canary);
    assert_eq!(format!("{option:?}"), "OptionId(<request-id>)");
    assert_eq!(format!("{invocation:?}"), "InvocationName(<private>)");
    assert_eq!(format!("{display:?}"), "DisplayName(<private>)");
    assert!(!format!("{:?}", ChoiceTarget::Skill(option.clone())).contains(canary));
    // Explicit data access/serialization remains lossless; Debug is not output.
    assert_eq!(option.as_str(), canary);
    assert_eq!(invocation.as_str(), canary);
    assert_eq!(display.as_str(), canary);
    assert_eq!(
        serde_json::to_string(&option).unwrap(),
        "\"PRIVATE-CANARY\""
    );
    assert_eq!(
        serde_json::to_string(&invocation).unwrap(),
        "\"PRIVATE-CANARY\""
    );
    assert_eq!(
        serde_json::to_string(&display).unwrap(),
        "\"PRIVATE-CANARY\""
    );
}

#[cfg(unix)]
#[test]
fn local_paths_preserve_native_bytes_without_leaking_in_debug() {
    use std::ffi::OsString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let bytes = b"/private/\xff-skill".to_vec();
    let local = LocalPath::new(OsString::from_vec(bytes.clone()).into());
    assert_eq!(local.as_path().as_os_str().as_bytes(), bytes);
    assert_eq!(format!("{local:?}"), "LocalPath(<private>)");
}
