//! Adapter Conformance and Failure Handling Suite (P3)
//!
//! Verifies boundary `p3_conformance_matrix`:
//! - Unit Property Test: `tests/context_failures.rs::context_failure_cases`
//! - E2E Cases: `corrupted-jsonl`, `oversized-record`, `missing-prompt`
//! - Assertion IDs: `safe_context_rejection`, `context_accepted`
//!
//! Covers all three context acquisition boundaries:
//! 1. Native Claude Hook & JSONL Transcript Adapter
//! 2. Normalized Context Input Envelope Adapter
//! 3. Cass Archive Subprocess Adapter

use serde_json::json;
#[cfg(unix)]
use skillranker::adapter::CassProducer;
use skillranker::adapter::{ClaudeUserPromptSubmit, UnknownFieldPolicy};
#[cfg(unix)]
use skillranker::context::cass::{
    ArchiveSelection, CassError, QUALIFIED_VERSION, decode_export, validate_capabilities,
};
use skillranker::context::jsonl::{CursorKind, JsonlError, SkipKind, parse_line, snapshot_jsonl};
use skillranker::context::overlay::{
    ClaudeOverlayRequest, OverlayError, apply_claude_prompt_overlay,
};
use skillranker::context::source::SourcePolicy;
use skillranker::context::{ContextError, Role, parse_normalized_context};
use skillranker::identity::{
    AdapterId, AdapterVersion, ContentHash, HarnessId, ProducerId, SourceId, SourceProvenance,
    WorkspaceId,
};
use skillranker::limits::{NORMALIZED_CONTEXT_JSON_BYTES, ONE_TRANSCRIPT_RECORD_BYTES};
use skillranker::output::ContextQuality;
use skillranker::runtime::ProcessInvocation;
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

fn temp_test_dir(label: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "sr-ctx-fail-{}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed),
        label
    ));
    fs::create_dir_all(&path).expect("create temp test dir");
    path
}

fn parse_hook(json: &str) -> ClaudeUserPromptSubmit {
    ClaudeUserPromptSubmit::from_json(json.as_bytes(), UnknownFieldPolicy::RetainAdditive).unwrap()
}

#[test]
fn context_failure_cases() {
    let base_dir = temp_test_dir("suite");

    // =========================================================================
    // SECTION 1: Native Claude Hook & JSONL Transcript Conformance
    // =========================================================================

    // -------------------------------------------------------------------------
    // Sub-case 1.1 (E2E: corrupted-jsonl):
    // Syntactically invalid JSON in transcript file rejected safely.
    // Assertion: safe_context_rejection
    // -------------------------------------------------------------------------
    {
        let corrupt_file = base_dir.join("corrupt.jsonl");
        let mut f = File::create(&corrupt_file).expect("create corrupt file");
        writeln!(f, r#"{{"role":"user","kind":"message","text":"Hello"}}"#).unwrap();
        writeln!(f, r#"{{NOT_VALID_JSON_AT_ALL"#).unwrap(); // corrupt line
        f.flush().unwrap();

        let hook = parse_hook(&format!(
            r#"{{
                "hook_event_name": "UserPromptSubmit",
                "session_id": "sess-corrupt-01",
                "transcript_path": "{}",
                "prompt": "How do I fix this?",
                "prompt_id": "ev-corrupt-user"
            }}"#,
            corrupt_file.display()
        ));
        let req = ClaudeOverlayRequest {
            hook_input: hook,
            transcript_path: None,
            authorized_root: Some(base_dir.clone()),
        };

        let err = apply_claude_prompt_overlay(&req)
            .expect_err("corrupted transcript line must be safely rejected");

        // Assertion: safe_context_rejection
        assert!(
            matches!(err, OverlayError::MalformedTranscript(ref msg) if msg.contains("corrupt")),
            "expected MalformedTranscript with corrupt detail, got: {:?}",
            err
        );

        // Also verify direct parse_line behavior
        let parse_err = parse_line(b"{not json}").expect_err("parse_line must reject invalid JSON");
        assert_eq!(parse_err, SkipKind::Corrupt);
    }

    // -------------------------------------------------------------------------
    // Sub-case 1.2 (E2E: oversized-record):
    // Single JSONL record exceeding ONE_TRANSCRIPT_RECORD_BYTES (256 KiB) is never parsed:
    // its content is dropped and the context reports partial coverage (sr-xy2q). Being over
    // the limit is not malformation, so it no longer withholds the whole prompt.
    // Assertion: safe_context_rejection
    // -------------------------------------------------------------------------
    {
        let oversize_file = base_dir.join("oversize.jsonl");
        let huge_padding = "x".repeat(ONE_TRANSCRIPT_RECORD_BYTES.max() + 100);
        let mut f = File::create(&oversize_file).expect("create oversize file");
        writeln!(
            f,
            r#"{{"role":"user","kind":"message","text":"{}"}}"#,
            huge_padding
        )
        .unwrap();
        f.flush().unwrap();

        let hook = parse_hook(&format!(
            r#"{{
                "hook_event_name": "UserPromptSubmit",
                "session_id": "sess-oversize-01",
                "transcript_path": "{}",
                "prompt": "Review this code",
                "prompt_id": "ev-oversize-user"
            }}"#,
            oversize_file.display()
        ));
        let req = ClaudeOverlayRequest {
            hook_input: hook,
            transcript_path: None,
            authorized_root: Some(base_dir.clone()),
        };

        let result = apply_claude_prompt_overlay(&req)
            .expect("an oversized transcript line is dropped, not fatal");

        // Assertion: safe_context_rejection
        assert_eq!(result.context_quality, ContextQuality::Partial);
        assert!(
            result
                .events
                .iter()
                .all(|event| !event.text.as_str().contains("xxxx")),
            "no oversized content may reach the context"
        );

        // Also verify direct parse_line behavior
        let huge_line = vec![b'a'; ONE_TRANSCRIPT_RECORD_BYTES.max() + 1];
        let parse_err = parse_line(&huge_line).expect_err("parse_line must reject oversized line");
        assert_eq!(parse_err, SkipKind::Oversize);
    }

    // -------------------------------------------------------------------------
    // Sub-case 1.3 (E2E: missing-prompt):
    // Empty or whitespace-only prompt in hook stdin rejected safely.
    // Assertion: safe_context_rejection
    // -------------------------------------------------------------------------
    {
        for empty_text in ["", "   ", "\n\t  \n"] {
            let hook = parse_hook(&format!(
                r#"{{
                    "hook_event_name": "UserPromptSubmit",
                    "session_id": "sess-noprompt-01",
                    "prompt": "{}",
                    "prompt_id": "ev-noprompt"
                }}"#,
                empty_text.replace('\n', "\\n").replace('\t', "\\t")
            ));
            let req = ClaudeOverlayRequest {
                hook_input: hook,
                transcript_path: None,
                authorized_root: Some(base_dir.clone()),
            };

            let err = apply_claude_prompt_overlay(&req)
                .expect_err("empty prompt must be safely rejected");

            // Assertion: safe_context_rejection
            assert_eq!(err, OverlayError::MissingPrompt);
        }
    }

    // -------------------------------------------------------------------------
    // Sub-case 1.4: Duplicate key in transcript record rejected safely.
    // Assertion: safe_context_rejection
    // -------------------------------------------------------------------------
    {
        let dupkey_file = base_dir.join("dupkey.jsonl");
        let mut f = File::create(&dupkey_file).expect("create dupkey file");
        writeln!(
            f,
            r#"{{"role":"user","role":"assistant","kind":"message","text":"Hello"}}"#
        )
        .unwrap();
        f.flush().unwrap();

        let hook = parse_hook(&format!(
            r#"{{
                "hook_event_name": "UserPromptSubmit",
                "session_id": "sess-dupkey-01",
                "transcript_path": "{}",
                "prompt": "Prompt text",
                "prompt_id": "ev-dupkey-01"
            }}"#,
            dupkey_file.display()
        ));
        let req = ClaudeOverlayRequest {
            hook_input: hook,
            transcript_path: None,
            authorized_root: Some(base_dir.clone()),
        };

        let err = apply_claude_prompt_overlay(&req)
            .expect_err("duplicate key in transcript record must be rejected");

        // Assertion: safe_context_rejection
        assert!(
            matches!(err, OverlayError::MalformedTranscript(ref msg) if msg.contains("duplicate key")),
            "expected MalformedTranscript duplicate key, got: {:?}",
            err
        );

        let parse_err =
            parse_line(br#"{"role":"user","role":"assistant","kind":"message","text":"Hello"}"#)
                .expect_err("parse_line must reject duplicate key");
        assert_eq!(parse_err, SkipKind::DuplicateKey);
    }

    // -------------------------------------------------------------------------
    // Sub-case 1.5: Incomplete trailing line deferred without corrupting history.
    // -------------------------------------------------------------------------
    {
        let tail_file = base_dir.join("tail.jsonl");
        let mut f = File::create(&tail_file).expect("create tail file");
        writeln!(
            f,
            r#"{{"role":"user","kind":"message","text":"Complete turn 1"}}"#
        )
        .unwrap();
        // Incomplete line lacking newline
        write!(f, r#"{{"role":"assistant","kind":"message","text":"Part"#).unwrap();
        f.flush().unwrap();

        let hook = parse_hook(&format!(
            r#"{{
                "hook_event_name": "UserPromptSubmit",
                "session_id": "sess-tail-01",
                "transcript_path": "{}",
                "prompt": "New prompt",
                "prompt_id": "ev-tail-new"
            }}"#,
            tail_file.display()
        ));
        let req = ClaudeOverlayRequest {
            hook_input: hook,
            transcript_path: None,
            authorized_root: Some(base_dir.clone()),
        };

        let res = apply_claude_prompt_overlay(&req)
            .expect("incomplete trailing line should be cleanly deferred");
        assert_eq!(res.events.len(), 2);
        assert_eq!(res.events[0].text.as_str(), "Complete turn 1");
        assert_eq!(res.events[1].text.as_str(), "New prompt");
    }

    // -------------------------------------------------------------------------
    // Sub-case 1.6: Filesystem path safety and boundary confinement.
    // Assertion: safe_context_rejection
    // -------------------------------------------------------------------------
    {
        // 1. Directory as transcript
        let sub_dir = base_dir.join("sub_dir_target");
        fs::create_dir_all(&sub_dir).unwrap();
        let hook_dir = parse_hook(&format!(
            r#"{{
                "hook_event_name": "UserPromptSubmit",
                "session_id": "sess-dir-01",
                "transcript_path": "{}",
                "prompt": "Prompt text",
                "prompt_id": "ev-dir-01"
            }}"#,
            sub_dir.display()
        ));
        let req_dir = ClaudeOverlayRequest {
            hook_input: hook_dir,
            transcript_path: None,
            authorized_root: Some(base_dir.clone()),
        };
        let err_dir =
            apply_claude_prompt_overlay(&req_dir).expect_err("directory must be rejected");
        assert!(matches!(err_dir, OverlayError::TranscriptIsDirectory(_)));

        // 2. Traversal escaping authorized_root
        let outside_dir = temp_test_dir("outside");
        let outside_file = outside_dir.join("outside.jsonl");
        fs::write(
            &outside_file,
            b"{\"role\":\"user\",\"kind\":\"message\",\"text\":\"hi\"}\n",
        )
        .unwrap();

        let hook_outside = parse_hook(&format!(
            r#"{{
                "hook_event_name": "UserPromptSubmit",
                "session_id": "sess-outside-01",
                "transcript_path": "{}",
                "prompt": "Prompt text",
                "prompt_id": "ev-outside-01"
            }}"#,
            outside_file.display()
        ));
        let req_outside = ClaudeOverlayRequest {
            hook_input: hook_outside,
            transcript_path: None,
            authorized_root: Some(base_dir.clone()), // outside_file is not inside base_dir
        };
        let err_outside = apply_claude_prompt_overlay(&req_outside)
            .expect_err("path outside authorized root must be rejected");
        assert_eq!(err_outside, OverlayError::CrossSessionReadForbidden);

        // 3. snapshot_jsonl on symlink rejected
        #[cfg(unix)]
        {
            let real_file = base_dir.join("real_target.jsonl");
            fs::write(
                &real_file,
                b"{\"role\":\"user\",\"kind\":\"message\",\"text\":\"hi\"}\n",
            )
            .unwrap();
            let symlink_file = base_dir.join("symlink_target.jsonl");
            let _ = std::os::unix::fs::symlink(&real_file, &symlink_file);

            let invocation = ProcessInvocation::enter().unwrap();
            let cx = invocation.request_cx().unwrap();
            let symlink_err =
                snapshot_jsonl(&invocation, &cx, &symlink_file, None, CursorKind::Ranking)
                    .expect_err("symlink must be rejected by snapshot_jsonl");
            assert_eq!(symlink_err, JsonlError::UnsafePath);
            let _ = invocation.shutdown();
        }
    }

    // -------------------------------------------------------------------------
    // Sub-case 1.7: Honest success counterpart for Native adapter.
    // Assertion: context_accepted
    // -------------------------------------------------------------------------
    {
        // Part A: First prompt of a session when transcript file does not exist yet
        let nonexistent_file = base_dir.join("nonexistent_session.jsonl");
        let first_turn_hook = parse_hook(&format!(
            r#"{{
                "hook_event_name": "UserPromptSubmit",
                "session_id": "sess-first-turn-01",
                "transcript_path": "{}",
                "prompt": "Starting a fresh session",
                "prompt_id": "ev-first-01"
            }}"#,
            nonexistent_file.display()
        ));
        let first_turn_req = ClaudeOverlayRequest {
            hook_input: first_turn_hook,
            transcript_path: None,
            authorized_root: Some(base_dir.clone()),
        };
        let first_turn_res = apply_claude_prompt_overlay(&first_turn_req)
            .expect("first turn with non-existent transcript file must succeed");
        assert_eq!(first_turn_res.context_quality, ContextQuality::PromptOnly);
        assert_eq!(first_turn_res.events.len(), 1);
        assert_eq!(first_turn_res.events[0].role, Role::User);
        assert_eq!(
            first_turn_res.events[0].text.as_str(),
            "Starting a fresh session"
        );
        assert!(first_turn_res.prompt_overlaid);

        // Part B: Multi-turn existing transcript file with prompt overlay
        let valid_file = base_dir.join("valid_multiturn.jsonl");
        let mut f = File::create(&valid_file).expect("create valid file");
        writeln!(
            f,
            r#"{{"event_id":"ev-01","role":"user","kind":"message","text":"Can you help me?"}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"event_id":"ev-02","role":"assistant","kind":"message","text":"Sure, what do you need?"}}"#
        )
        .unwrap();
        f.flush().unwrap();

        let multi_hook = parse_hook(&format!(
            r#"{{
                "hook_event_name": "UserPromptSubmit",
                "session_id": "sess-valid-01",
                "transcript_path": "{}",
                "prompt": "Fix the compiler errors",
                "prompt_id": "ev-03"
            }}"#,
            valid_file.display()
        ));
        let multi_req = ClaudeOverlayRequest {
            hook_input: multi_hook,
            transcript_path: None,
            authorized_root: Some(base_dir.clone()),
        };
        let multi_res = apply_claude_prompt_overlay(&multi_req)
            .expect("valid multi-turn transcript overlay must succeed");
        assert_eq!(multi_res.context_quality, ContextQuality::Complete);
        assert_eq!(multi_res.events.len(), 3);
        assert_eq!(multi_res.events[0].text.as_str(), "Can you help me?");
        assert_eq!(multi_res.events[1].text.as_str(), "Sure, what do you need?");
        assert_eq!(multi_res.events[2].text.as_str(), "Fix the compiler errors");
    }

    // =========================================================================
    // SECTION 2: Normalized Context Input Envelope Conformance
    // =========================================================================

    // -------------------------------------------------------------------------
    // Sub-case 2.1: Invalid JSON syntax in normalized context rejected.
    // Assertion: safe_context_rejection
    // -------------------------------------------------------------------------
    {
        let corrupt_envelope = b"{\"schema_version\": 1, \"harness\": \"claude_code\", TRUNCATED";
        let err = parse_normalized_context(corrupt_envelope)
            .expect_err("corrupted normalized envelope must be rejected");
        assert_eq!(err, ContextError::InvalidJson);
    }

    // -------------------------------------------------------------------------
    // Sub-case 2.2: Duplicate keys in normalized context rejected.
    // Assertion: safe_context_rejection
    // -------------------------------------------------------------------------
    {
        let dupkey_envelope = br#"{
            "schema_version": 1,
            "schema_version": 1,
            "harness": "claude_code",
            "workspace_root": "/workspace",
            "current_request": {
                "event_id": "ev-01",
                "text": "test",
                "attachments_omitted": false,
                "essential_attachment_missing": false
            },
            "events": [],
            "supplied_loads": []
        }"#;
        let err = parse_normalized_context(dupkey_envelope)
            .expect_err("duplicate keys in normalized envelope must be rejected");
        assert_eq!(err, ContextError::DuplicateKey);
    }

    // -------------------------------------------------------------------------
    // Sub-case 2.3: Unsupported schema version rejected.
    // Assertion: safe_context_rejection
    // -------------------------------------------------------------------------
    {
        for bad_version in [0, 2, 99] {
            let bad_ver_json = json!({
                "schema_version": bad_version,
                "harness": "claude_code",
                "workspace_root": "/workspace",
                "current_request": {
                    "event_id": "ev-01",
                    "text": "test",
                    "attachments_omitted": false,
                    "essential_attachment_missing": false
                },
                "events": [],
                "supplied_loads": []
            });
            let bytes = serde_json::to_vec(&bad_ver_json).unwrap();
            let err = parse_normalized_context(&bytes)
                .expect_err("unsupported schema version must be rejected");
            assert_eq!(err, ContextError::UnsupportedSchema);
        }
    }

    // -------------------------------------------------------------------------
    // Sub-case 2.4: Duplicate event IDs rejected.
    // Assertion: safe_context_rejection
    // -------------------------------------------------------------------------
    {
        let dup_event_json = json!({
            "schema_version": 1,
            "harness": "claude_code",
            "workspace_root": "/workspace",
            "current_request": {
                "event_id": "ev-req",
                "text": "test",
                "attachments_omitted": false,
                "essential_attachment_missing": false
            },
            "events": [
                {
                    "event_id": "ev-dup-01",
                    "parent_id": null,
                    "turn_id": null,
                    "agent_id": null,
                    "branch_id": null,
                    "role": "user",
                    "kind": "message",
                    "timestamp_unix_ms": null,
                    "text": "First message",
                    "tool": null
                },
                {
                    "event_id": "ev-dup-01", // duplicate ID
                    "parent_id": null,
                    "turn_id": null,
                    "agent_id": null,
                    "branch_id": null,
                    "role": "assistant",
                    "kind": "message",
                    "timestamp_unix_ms": null,
                    "text": "Second message",
                    "tool": null
                }
            ],
            "explicit_skill_references": [],
            "supplied_loads": []
        });
        let bytes = serde_json::to_vec(&dup_event_json).unwrap();
        let err = parse_normalized_context(&bytes)
            .expect_err("duplicate event ID in normalized envelope must be rejected");
        assert_eq!(err, ContextError::DuplicateEvent);
    }

    // -------------------------------------------------------------------------
    // Sub-case 2.5: Duplicate load definitions rejected.
    // Assertion: safe_context_rejection
    // -------------------------------------------------------------------------
    {
        let dup_load_json = json!({
            "schema_version": 1,
            "harness": "claude_code",
            "workspace_root": "/workspace",
            "current_request": {
                "event_id": "ev-req",
                "text": "test",
                "attachments_omitted": false,
                "essential_attachment_missing": false
            },
            "events": [],
            "explicit_skill_references": [],
            "supplied_loads": [
                {
                    "skill_id": "skill-rust-basics",
                    "source_content": null,
                    "rendered_content": null
                },
                {
                    "skill_id": "skill-rust-basics", // duplicate skill ID
                    "source_content": null,
                    "rendered_content": null
                }
            ]
        });
        let bytes = serde_json::to_vec(&dup_load_json).unwrap();
        let err = parse_normalized_context(&bytes)
            .expect_err("duplicate load definitions must be rejected");
        assert_eq!(err, ContextError::DuplicateLoadDefinition);
    }

    // -------------------------------------------------------------------------
    // Sub-case 2.6: Limit exceeded for normalized envelope rejected.
    // Assertion: safe_context_rejection
    // -------------------------------------------------------------------------
    {
        let huge_bytes = vec![b' '; NORMALIZED_CONTEXT_JSON_BYTES.max() + 10];
        let err = parse_normalized_context(&huge_bytes)
            .expect_err("oversized normalized context must be rejected");
        assert_eq!(err, ContextError::LimitExceeded);
    }

    // -------------------------------------------------------------------------
    // Sub-case 2.7: Missing required fields rejected.
    // Assertion: safe_context_rejection
    // -------------------------------------------------------------------------
    {
        let missing_field = json!({
            "schema_version": 1,
            "harness": "claude_code",
            "events": []
        });
        let bytes = serde_json::to_vec(&missing_field).unwrap();
        let err = parse_normalized_context(&bytes)
            .expect_err("missing required field in normalized context must be rejected");
        assert_eq!(err, ContextError::InvalidField);
    }

    // -------------------------------------------------------------------------
    // Sub-case 2.8: Honest success counterpart for Normalized envelope.
    // Assertion: context_accepted
    // -------------------------------------------------------------------------
    {
        let valid_envelope = json!({
            "schema_version": 1,
            "harness": "claude_code",
            "producer_id": "cli-v1",
            "workspace_root": "/data/projects/demo",
            "session_id": "sess-norm-01",
            "agent_id": "agent-alpha",
            "branch_id": "feature-x",
            "context_epoch": "epoch-1",
            "current_request": {
                "event_id": "ev-norm-02",
                "text": "Run unit tests",
                "attachments_omitted": false,
                "essential_attachment_missing": false
            },
            "events": [
                {
                    "event_id": "ev-norm-01",
                    "parent_id": null,
                    "turn_id": null,
                    "agent_id": null,
                    "branch_id": null,
                    "role": "user",
                    "kind": "message",
                    "timestamp_unix_ms": null,
                    "text": "Hello assistant",
                    "tool": null
                }
            ],
            "explicit_skill_references": [],
            "supplied_loads": [
                {
                    "skill_id": "skill-cargo-test",
                    "source_content": null,
                    "rendered_content": null
                }
            ]
        });
        let bytes = serde_json::to_vec(&valid_envelope).unwrap();
        let norm_ctx =
            parse_normalized_context(&bytes).expect("valid normalized envelope must parse");
        assert_eq!(norm_ctx.schema_version, 1);
        assert_eq!(norm_ctx.harness.as_str(), "claude_code");
        assert_eq!(norm_ctx.events.len(), 1);
        assert_eq!(norm_ctx.supplied_loads.len(), 1);

        let ws = WorkspaceId::new("/data/projects/demo").unwrap();
        let session_id = norm_ctx
            .session_identity(Some(ws.clone()))
            .expect("session identity generation must succeed");
        assert!(matches!(
            session_id.source,
            SourceProvenance::Normalized {
                schema_version: 1,
                ..
            }
        ));
        assert_eq!(session_id.workspace, Some(ws));
    }

    // =========================================================================
    // SECTION 3: Cass Archive Adapter Conformance & Policy Gates
    // =========================================================================
    #[cfg(unix)]
    {
        // ---------------------------------------------------------------------
        // Sub-case 3.1: Policy gating rejects cass under offline, dry-run, local-only
        // Assertion: safe_context_rejection
        // ---------------------------------------------------------------------
        {
            // Offline policy
            let offline_policy = SourcePolicy {
                offline: true,
                dry_run: false,
                local_only: false,
                allow_network: false,
            };
            assert!(!offline_policy.cass_allowed());

            // Dry-run policy
            let dry_run_policy = SourcePolicy {
                offline: false,
                dry_run: true,
                local_only: false,
                allow_network: false,
            };
            assert!(!dry_run_policy.cass_allowed());

            // Local-only policy
            let local_only_policy = SourcePolicy {
                offline: false,
                dry_run: false,
                local_only: true,
                allow_network: false,
            };
            assert!(!local_only_policy.cass_allowed());

            // Allowed policy
            let allowed_policy = SourcePolicy {
                offline: false,
                dry_run: false,
                local_only: false,
                allow_network: false,
            };
            assert!(allowed_policy.cass_allowed());
        }

        // ---------------------------------------------------------------------
        // Sub-case 3.2: Remote source rejected by ArchiveSelection::local
        // Assertion: safe_context_rejection
        // -------------------------------------------------------------------------
        {
            let p = PathBuf::from("/var/cass/sessions/demo.json");
            let remote_source = SourceId::new("remote_cluster").unwrap();
            let err = ArchiveSelection::local(p, remote_source)
                .expect_err("remote source ID must be rejected by ArchiveSelection::local");
            assert_eq!(err, CassError::RemoteSource);

            // Valid local source ID accepted
            let local_source = SourceId::new("local").unwrap();
            let ok_sel = ArchiveSelection::local(
                PathBuf::from("/var/cass/sessions/demo.json"),
                local_source,
            )
            .expect("local source ID must be accepted");
            assert_eq!(ok_sel.source().as_str(), "local");
        }

        // ---------------------------------------------------------------------
        // Sub-case 3.3: Unsupported cass producer / version mismatch rejected
        // Assertion: safe_context_rejection
        // ---------------------------------------------------------------------
        {
            let bad_version_caps = json!({
                "crate_version": "0.7.9", // qualified is 0.8.0
                "api_version": 1,
                "contract_version": "1",
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
            let bytes = serde_json::to_vec(&bad_version_caps).unwrap();
            let digest = ContentHash::from_bytes(b"cass_digest_01");
            let err = validate_capabilities(&bytes, digest)
                .expect_err("cass version != 0.8.0 must be rejected");
            assert_eq!(err, CassError::UnsupportedProducer);
        }

        // ---------------------------------------------------------------------
        // Sub-case 3.4: Corrupted / invalid export JSON rejected
        // Assertion: safe_context_rejection
        // ---------------------------------------------------------------------
        {
            let valid_producer = CassProducer {
                version: AdapterVersion::new(QUALIFIED_VERSION).unwrap(),
                api_version: 1,
                contract_version: 1,
                build_commit: None,
                binary_digest: Some(ContentHash::from_bytes(b"digest")),
                source_id: Some(SourceId::new("local").unwrap()),
                remote_source: false,
                export_omits_skills_by_default: true,
                export_retains_native_shapes: true,
            };

            let invalid_export_bytes = b"NOT_VALID_JSON_EXPORT";
            let err = decode_export(invalid_export_bytes, valid_producer.clone(), true)
                .expect_err("invalid export JSON must be rejected");
            assert_eq!(err, CassError::InvalidResponse);
        }

        // ---------------------------------------------------------------------
        // Sub-case 3.5: Honest success counterpart for Cass adapter
        // Assertion: context_accepted
        // ---------------------------------------------------------------------
        {
            let valid_caps = json!({
                "crate_version": QUALIFIED_VERSION,
                "api_version": 1,
                "contract_version": "1",
                "build_commit": "unknown",
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
            let digest = ContentHash::from_bytes(b"cass_digest_qualified");
            let mut producer = validate_capabilities(&caps_bytes, digest)
                .expect("valid capabilities JSON must parse");
            assert_eq!(producer.version.as_str(), QUALIFIED_VERSION);
            producer.source_id = Some(SourceId::new("local").unwrap());

            // Test valid export JSON decoding with tool filtering
            let export_json = json!([
                {
                    "role": "user",
                    "message": {
                        "role": "user",
                        "content": "Can you analyze this project?"
                    }
                },
                {
                    "role": "assistant",
                    "message": {
                        "role": "assistant",
                        "content": "Sure, I will inspect the cargo files."
                    }
                },
                {
                    "role": "tool",
                    "message": {
                        "role": "tool",
                        "content": "Running cargo check..."
                    }
                }
            ]);
            let export_bytes = serde_json::to_vec(&export_json).unwrap();

            // When include_tools = true, all 3 records preserved
            let export_with_tools = decode_export(&export_bytes, producer.clone(), true)
                .expect("decode_export with tools should succeed");
            assert_eq!(export_with_tools.records.len(), 3);
            assert!(export_with_tools.tool_content_included);
            assert_eq!(
                export_with_tools.provenance(),
                SourceProvenance::Cass {
                    source: Some(SourceId::new("local").unwrap()),
                    version: AdapterVersion::new(QUALIFIED_VERSION).unwrap(),
                }
            );

            // When include_tools = false, tool record dropped by text_only projection
            let export_text_only = decode_export(&export_bytes, producer.clone(), false)
                .expect("decode_export text-only should succeed");
            assert_eq!(export_text_only.records.len(), 2);
            assert!(!export_text_only.tool_content_included);
        }
    }

    // =========================================================================
    // SECTION 4: Cross-Adapter Namespace and Provenance Invariants
    // =========================================================================
    {
        let prov_native = SourceProvenance::Native {
            adapter: AdapterId::new("claude_hook").unwrap(),
            version: AdapterVersion::new("1.0.0").unwrap(),
        };
        let prov_normalized = SourceProvenance::Normalized {
            producer: Some(ProducerId::new("prod-01").unwrap()),
            harness: HarnessId::new("claude_code").unwrap(),
            schema_version: 1,
        };
        let prov_cass = SourceProvenance::Cass {
            source: Some(SourceId::new("local").unwrap()),
            version: AdapterVersion::new("0.8.0").unwrap(),
        };

        // All 3 provenances are distinct
        assert_ne!(prov_native, prov_normalized);
        assert_ne!(prov_native, prov_cass);
        assert_ne!(prov_normalized, prov_cass);
    }
}
