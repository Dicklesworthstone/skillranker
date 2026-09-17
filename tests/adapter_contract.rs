use skillranker::adapter::*;
use skillranker::identity::{AdapterId, AdapterVersion, ContentHash};
use skillranker::limits::HOOK_STDIN_BYTES;
use std::collections::BTreeMap;

fn tested_claude(version: &str) -> AdapterRecord {
    let installed = AdapterVersion::new(version).unwrap();
    let smoke = EvidenceRecord {
        class: EvidenceClass::RealHarnessSmoke,
        run_status: EvidenceRunStatus::Passed,
        digest: Some(ContentHash::from_bytes(b"synthetic-smoke")),
        observed_version: Some(installed.clone()),
    };
    let mut conformance = BTreeMap::new();
    for dimension in NATIVE_ADVICE_DIMENSIONS {
        conformance.insert(
            *dimension,
            ConformanceCell {
                status: ConformanceStatus::Pass,
                evidence: vec![smoke.clone()],
            },
        );
    }
    AdapterRecord {
        adapter_id: AdapterId::new(CLAUDE_CODE_ID).unwrap(),
        kind: AdapterKind::ClaudeHook,
        support: SupportClass::Tested,
        contract_version: CONTRACT_VERSION,
        on_default_hook_path: true,
        identity_semantics: SemanticsCompatibility::Compatible,
        visibility_semantics: SemanticsCompatibility::Compatible,
        tested_versions: vec![installed],
        unverified_versions: Vec::new(),
        conformance,
    }
}

#[test]
fn foundation_capabilities_match_the_checked_in_fixture_and_do_not_advertise_commands() {
    let fixture = include_str!("fixtures/capabilities.v1.json");
    let decoded = CapabilitiesDocument::from_json(fixture.as_bytes()).unwrap();
    assert_eq!(decoded, foundation_capabilities().unwrap());
    assert_eq!(
        decoded.implemented_cli,
        FOUNDATION_IMPLEMENTED_CLI
            .iter()
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>()
    );
    assert!(
        decoded
            .planned_cli
            .iter()
            .any(|command| command.name == "rank")
    );
    assert!(!decoded.implemented_cli.iter().any(|name| name == "rank"));
    assert!(!decoded.implemented_cli.iter().any(|name| name == "hook"));
    assert!(!decoded.implemented_cli.iter().any(|name| name == "tui"));
    let claude = decoded.implemented_adapter(CLAUDE_CODE_ID).unwrap();
    assert_eq!(claude.support, SupportClass::Unverified);
    assert!(claude.tested_versions.is_empty());
    assert_eq!(
        claude.advice(CompatibilityQuestion::EmitNativeAdvice, None),
        AdviceDisposition::Disabled(AdviceBlockReason::UnverifiedHarness)
    );
}

#[test]
fn foundation_claude_observation_is_not_native_advice() {
    let claude = foundation_capabilities()
        .unwrap()
        .implemented_adapter(CLAUDE_CODE_ID)
        .unwrap()
        .clone();
    let observed = AdapterVersion::new("2.1.274").unwrap();
    assert_eq!(
        claude.advice(CompatibilityQuestion::EmitNativeAdvice, Some(&observed)),
        AdviceDisposition::Disabled(AdviceBlockReason::UnverifiedHarness)
    );
    let fixture_only = EvidenceRecord {
        class: EvidenceClass::FixtureTest,
        run_status: EvidenceRunStatus::Passed,
        digest: Some(ContentHash::from_bytes(b"fixture")),
        observed_version: Some(observed.clone()),
    };
    assert!(!fixture_only.authorizes_installed_version(&observed));
    let official = EvidenceRecord {
        class: EvidenceClass::OfficialSchema,
        run_status: EvidenceRunStatus::Passed,
        digest: None,
        observed_version: None,
    };
    assert!(!official.authorizes_installed_version(&observed));
}

#[test]
fn unknown_capabilities_version_is_refused() {
    let json = br#"{"schema_version":2,"adapter_contract_version":1,"implemented_cli":["help","version"],"planned_cli":[],"adapters":[]}"#;
    assert_eq!(
        CapabilitiesDocument::from_json(json).unwrap_err(),
        AdapterError::UnsupportedVersion
    );
}

#[test]
fn capabilities_reject_unknown_keys_and_duplicate_keys() {
    assert_eq!(
        CapabilitiesDocument::from_json(
            br#"{"schema_version":1,"adapter_contract_version":1,"implemented_cli":["help","version"],"planned_cli":[],"adapters":[],"secret":"x"}"#
        )
        .unwrap_err(),
        AdapterError::InvalidField
    );
    assert_eq!(
        CapabilitiesDocument::from_json(
            br#"{"schema_version":1,"schema_version":1,"adapter_contract_version":1,"implemented_cli":["help","version"],"planned_cli":[],"adapters":[]}"#
        )
        .unwrap_err(),
        AdapterError::DuplicateKey
    );
}

#[test]
fn claude_user_prompt_submit_fixture_retains_additive_fields_and_event_identity() {
    let bytes = include_bytes!("fixtures/adapter-claude-user-prompt-submit.v1.json");
    let envelope =
        ClaudeUserPromptSubmit::from_json(bytes, UnknownFieldPolicy::RetainAdditive).unwrap();
    assert_eq!(
        envelope.current_request_event().unwrap().as_str(),
        "prompt-2"
    );
    assert_eq!(
        envelope.additive_keys().collect::<Vec<_>>(),
        ["permission_mode"]
    );
    let debug = format!("{envelope:?}");
    assert!(!debug.contains("Continue investigating"));
    assert!(!debug.contains("/synthetic/"));
    assert_eq!(
        ClaudeUserPromptSubmit::transcript_state(false, false).unwrap(),
        HookTranscriptState::Missing
    );
    assert_eq!(
        ClaudeUserPromptSubmit::transcript_state(true, false).unwrap(),
        HookTranscriptState::Present
    );
    assert_eq!(
        ClaudeUserPromptSubmit::transcript_state(true, true).unwrap_err(),
        AdapterError::InvalidField
    );
}

#[test]
fn unknown_and_unsupported_hook_events_disable_advice() {
    assert_eq!(
        ClaudeHookEvent::parse(USER_PROMPT_EXPANSION).unwrap_err(),
        AdapterError::UnsupportedEvent
    );
    assert_eq!(
        ClaudeHookEvent::parse("Stop").unwrap_err(),
        AdapterError::UnknownEvent
    );
    assert_eq!(
        ClaudeHookEvent::parse(USER_PROMPT_SUBMIT).unwrap().advice(),
        AdviceDisposition::Eligible
    );
    let expansion = br#"{"hook_event_name":"UserPromptExpansion","prompt":"secret-canary"}"#;
    let error = ClaudeUserPromptSubmit::from_json(expansion, UnknownFieldPolicy::RetainAdditive)
        .unwrap_err();
    assert_eq!(error, AdapterError::UnsupportedEvent);
    assert!(!error.to_string().contains("secret-canary"));
}

#[test]
fn duplicate_hook_keys_fail_before_last_key_wins() {
    let json = br#"{"hook_event_name":"UserPromptSubmit","prompt":"first-canary","prompt":"second-canary"}"#;
    let error =
        ClaudeUserPromptSubmit::from_json(json, UnknownFieldPolicy::RetainAdditive).unwrap_err();
    assert_eq!(error, AdapterError::DuplicateKey);
    assert!(!error.to_string().contains("canary"));
}

#[test]
fn owned_schema_policy_rejects_unknown_hook_fields() {
    let bytes = include_bytes!("fixtures/adapter-claude-user-prompt-submit.v1.json");
    assert_eq!(
        ClaudeUserPromptSubmit::from_json(bytes, UnknownFieldPolicy::RejectUnknown).unwrap_err(),
        AdapterError::InvalidField
    );
}

#[test]
fn hook_output_budget_and_control_characters() {
    additional_context_allowed("use rust-test-triage", 1).unwrap();
    additional_context_allowed(&"a".repeat(ADDITIONAL_CONTEXT_MAX_CHARS), 0).unwrap();
    assert_eq!(
        additional_context_allowed(&"a".repeat(ADDITIONAL_CONTEXT_MAX_CHARS + 1), 0).unwrap_err(),
        AdapterError::HookOutputLimit
    );
    assert_eq!(
        additional_context_allowed("ok", 2).unwrap_err(),
        AdapterError::HookOutputLimit
    );
    assert_eq!(
        additional_context_allowed("bad\u{0007}bell", 1).unwrap_err(),
        AdapterError::UnsafeHookText
    );
}

#[test]
fn cass_producer_fixture_is_archive_identity_not_normalized_or_support() {
    let producer =
        CassProducer::from_json(include_bytes!("fixtures/adapter-cass-producer.v1.json")).unwrap();
    producer.validate_archive_identity().unwrap();
    assert!(producer.export_omits_skills_by_default);
    assert!(producer.export_retains_native_shapes);
    assert!(!producer.native_export_is_our_normalized_envelope());
    assert_eq!(
        producer.validate_support_claim().unwrap_err(),
        AdapterError::MissingCassProvenance
    );
    let mut remote = producer.clone();
    remote.remote_source = true;
    assert_eq!(
        remote.validate_archive_identity().unwrap_err(),
        AdapterError::RemoteCassSourceRejected
    );
    let mut claimed = producer;
    claimed.binary_digest = Some(ContentHash::from_bytes(b"cass-binary"));
    claimed.validate_support_claim().unwrap();
}

#[test]
fn unverified_versions_cannot_inherit_tested_support() {
    let source = tested_claude("2.1.274");
    let other = AdapterId::new("codex").unwrap();
    let same_version = AdapterVersion::new("2.1.274").unwrap();
    let other_version = AdapterVersion::new("2.1.275").unwrap();
    assert_eq!(
        transfer_tested_support(&source, &other, &same_version).unwrap_err(),
        AdapterError::SupportInheritanceForbidden
    );
    assert_eq!(
        transfer_tested_support(&source, &source.adapter_id, &other_version).unwrap_err(),
        AdapterError::SupportInheritanceForbidden
    );
    transfer_tested_support(&source, &source.adapter_id, &same_version).unwrap();
}

#[test]
fn incompatible_or_unverified_semantics_disable_native_advice() {
    let mut record = tested_claude("2.1.274");
    let installed = AdapterVersion::new("2.1.274").unwrap();
    record.identity_semantics = SemanticsCompatibility::Incompatible;
    assert_eq!(
        record.advice(CompatibilityQuestion::EmitNativeAdvice, Some(&installed)),
        AdviceDisposition::Disabled(AdviceBlockReason::IncompatibleIdentitySemantics)
    );
    record.identity_semantics = SemanticsCompatibility::Compatible;
    record.visibility_semantics = SemanticsCompatibility::Unverified;
    assert_eq!(
        record.advice(CompatibilityQuestion::EmitNativeAdvice, Some(&installed)),
        AdviceDisposition::Disabled(AdviceBlockReason::UnverifiedHarness)
    );
}

#[test]
fn tested_matching_version_is_eligible_native_advice() {
    let record = tested_claude("2.1.274");
    let installed = AdapterVersion::new("2.1.274").unwrap();
    assert_eq!(
        record.advice(CompatibilityQuestion::EmitNativeAdvice, Some(&installed)),
        AdviceDisposition::Eligible
    );
    let other = AdapterVersion::new("9.9.9").unwrap();
    assert_eq!(
        record.advice(CompatibilityQuestion::EmitNativeAdvice, Some(&other)),
        AdviceDisposition::Disabled(AdviceBlockReason::InstalledVersionNotTested)
    );
}

#[test]
fn fixture_pass_without_smoke_cannot_authorize_installed_advice() {
    let mut record = tested_claude("2.1.274");
    let installed = AdapterVersion::new("2.1.274").unwrap();
    for cell in record.conformance.values_mut() {
        cell.evidence = vec![EvidenceRecord {
            class: EvidenceClass::FixtureTest,
            run_status: EvidenceRunStatus::Passed,
            digest: Some(ContentHash::from_bytes(b"fixture")),
            observed_version: Some(installed.clone()),
        }];
    }
    assert_eq!(
        record.advice(CompatibilityQuestion::EmitNativeAdvice, Some(&installed)),
        AdviceDisposition::Disabled(AdviceBlockReason::FixtureDigestIsNotInstalledProof)
    );
}

#[test]
fn source_flags_are_mutually_exclusive_and_stdin_is_explicit() {
    assert_eq!(
        select_source(SourceRequest::default()).unwrap(),
        SelectedSource::Discovery
    );
    assert_eq!(
        select_source(SourceRequest {
            claude_hook: true,
            context_file: true,
            ..SourceRequest::default()
        })
        .unwrap_err(),
        AdapterError::ConflictingSourceFlags
    );
    assert_eq!(
        select_source(SourceRequest {
            stdin_present: true,
            stdin_mode_explicit: false,
            ..SourceRequest::default()
        })
        .unwrap_err(),
        AdapterError::MissingExplicitStdinMode
    );
    assert_eq!(
        select_source(SourceRequest {
            context_file: true,
            stdin_present: true,
            stdin_mode_explicit: true,
            ..SourceRequest::default()
        })
        .unwrap(),
        SelectedSource::NormalizedContext { stdin: true }
    );
}

#[test]
fn oversized_hook_stdin_is_a_limit_error_without_echoing_bytes() {
    let mut bytes = br#"{"hook_event_name":"UserPromptSubmit","prompt":""}"#.to_vec();
    bytes.resize(HOOK_STDIN_BYTES.max() + 1, b'x');
    let error =
        ClaudeUserPromptSubmit::from_json(&bytes, UnknownFieldPolicy::RetainAdditive).unwrap_err();
    assert_eq!(error, AdapterError::LimitExceeded);
    assert!(!error.to_string().contains("prompt"));
}

#[test]
fn normalized_input_acceptance_does_not_emit_native_advice() {
    let normalized = foundation_capabilities()
        .unwrap()
        .implemented_adapter(NORMALIZED_ID)
        .unwrap()
        .clone();
    assert_eq!(
        normalized.advice(CompatibilityQuestion::AcceptInput, None),
        AdviceDisposition::Eligible
    );
    assert_eq!(
        normalized.advice(CompatibilityQuestion::EmitNativeAdvice, None),
        AdviceDisposition::Disabled(AdviceBlockReason::MissingRequiredEvidence)
    );
    let cass = foundation_capabilities()
        .unwrap()
        .implemented_adapter(CASS_ID)
        .unwrap()
        .clone();
    assert!(!cass.on_default_hook_path);
    assert_eq!(
        cass.advice(CompatibilityQuestion::EmitNativeAdvice, None),
        AdviceDisposition::Disabled(AdviceBlockReason::UnverifiedHarness)
    );
}
