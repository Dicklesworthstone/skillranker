#![cfg(unix)]
//! Recorded-format tests and real subprocess boundary tests. The ignored test
//! separately exercises the actual installed cass binary against synthetic data.
use asupersync::{
    Budget, Cx,
    runtime::{Runtime, RuntimeBuilder},
};
use serde_json::{Value, json};
use skillranker::context::cass::*;
use skillranker::context::source::SourcePolicy;
use skillranker::identity::{ContentHash, SourceId, SourceProvenance};
use skillranker::runtime::EntryClock;
use skillranker::subprocess::TrustedExecutable;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
fn dir() -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "sr-cass-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&p).unwrap();
    p
}
fn caps() -> Value {
    json!({"crate_version":"0.8.0","api_version":1,"contract_version":"1","build_commit":"unknown","global_flags":[{"name":"db"}],"features":["json_output","export_command","self_describing_capabilities"],"commands":[{"name":"export","arguments":[{"name":"path"},{"name":"source"},{"name":"format","enum_values":["json"]},{"name":"include-tools"}]},{"name":"sessions","arguments":[{"name":"workspace"},{"name":"limit"},{"name":"json"}]}]})
}
fn producer() -> skillranker::adapter::CassProducer {
    let mut p = validate_capabilities(
        &serde_json::to_vec(&caps()).unwrap(),
        ContentHash::from_bytes(b"fixture-not-live-binary"),
    )
    .unwrap();
    p.source_id = Some(SourceId::new("local").unwrap());
    p
}
struct Run {
    runtime: Runtime,
    cx: Cx,
    clock: EntryClock,
}
impl Run {
    fn new() -> Self {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        let cx = runtime.request_cx_with_budget(Budget::new());
        Self {
            runtime,
            cx,
            clock: EntryClock::capture().unwrap(),
        }
    }
}
fn executable(root: &Path, body: &str) -> PathBuf {
    let path = root.join("cass-fixture");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    writeln!(file, "#!/bin/sh\n{body}").unwrap();
    file.set_permissions(fs::Permissions::from_mode(0o700))
        .unwrap();
    path
}
fn config(path: &Path, root: &Path) -> CassConfig {
    CassConfig {
        executable: TrustedExecutable::resolve(path, &[root.to_path_buf()]).unwrap(),
        directory: root.to_path_buf(),
        database: root.join("explicit.db"),
        binary_digest: ContentHash::from_bytes(&fs::read(path).unwrap()),
    }
}
fn script(export: &str) -> String {
    format!(
        "if [ \"$3\" = capabilities ]; then\ncat <<'CAPS'\n{}\nCAPS\nelse\n{export}\nfi",
        caps()
    )
    .replace("\ncat <<", "\n/bin/cat <<")
}
fn selection() -> ArchiveSelection {
    ArchiveSelection::local(
        PathBuf::from("/synthetic/path with spaces.jsonl"),
        SourceId::new("local").unwrap(),
    )
    .unwrap()
}

#[test]
fn qualified_capabilities_require_exact_version_and_commands() {
    assert!(producer().validate_support_claim().is_ok());
    for field in [
        "crate_version",
        "api_version",
        "contract_version",
        "features",
        "commands",
    ] {
        let mut value = caps();
        value.as_object_mut().unwrap().remove(field);
        assert_eq!(
            validate_capabilities(
                &serde_json::to_vec(&value).unwrap(),
                ContentHash::from_bytes(b"x")
            )
            .unwrap_err(),
            CassError::UnsupportedProducer
        );
    }
    let mut value = caps();
    value["crate_version"] = "0.9.0".into();
    assert_eq!(
        validate_capabilities(
            &serde_json::to_vec(&value).unwrap(),
            ContentHash::from_bytes(b"x")
        )
        .unwrap_err(),
        CassError::UnsupportedProducer
    );
}

#[test]
fn recorded_actual_cass_export_preserves_native_shapes_and_archive_namespace() {
    let bytes = include_bytes!("fixtures/cass-export.json");
    let export = decode_export(bytes, producer(), true).unwrap();
    assert_eq!(export.records.len(), 3);
    assert_eq!(
        export.records[1].native_value()["message"]["content"][1]["type"],
        "tool_use"
    );
    assert!(matches!(export.provenance(), SourceProvenance::Cass { .. }));
    assert!(export.skill_load_evidence_unknown);
    assert!(!export.producer.native_export_is_our_normalized_envelope());
    assert!(!format!("{export:?}").contains("Synthetic task"));
}

#[test]
fn no_tools_projects_native_text_and_discards_unknown_payload_fields() {
    let mut data: Value =
        serde_json::from_slice(include_bytes!("fixtures/cass-export.json")).unwrap();
    data[1]["private_tool_payload"] = "secret-tool-canary".into();
    data[1]["message"]["content"][1]["input"]["secret"] = "secret-tool-canary".into();
    let export = decode_export(&serde_json::to_vec(&data).unwrap(), producer(), false).unwrap();
    assert_eq!(export.records.len(), 2);
    let text = serde_json::to_string(export.records[1].native_value()).unwrap();
    assert!(text.contains("I will inspect the test."));
    assert!(!text.contains("tool_use"));
    assert!(!text.contains("secret-tool-canary"));
    assert_eq!(export.records[1].native_value()["parentUuid"], "u1");
}

#[test]
fn no_tools_handles_flat_and_codex_payload_text_without_tool_roles() {
    let bytes = serde_json::to_vec(&json!([
        {"role":"assistant","content":"hello","tool_calls":[{"secret":"private"}]},
        {"role":"tool","content":"private"},
        {"type":"response_item","payload":{"role":"assistant","content":[{"type":"output_text","text":"answer"},{"type":"function_call","arguments":"private"}]}}
    ])).unwrap();
    let result = decode_export(&bytes, producer(), false).unwrap();
    assert_eq!(result.records.len(), 2);
    for row in &result.records {
        assert!(
            !serde_json::to_string(row.native_value())
                .unwrap()
                .contains("private")
        );
    }
    assert_eq!(
        result.records[1].native_value()["payload"]["content"][0]["text"],
        "answer"
    );
}

#[test]
fn listing_rejects_remote_and_neighbor_workspaces_and_reports_full_page() {
    let bytes = serde_json::to_vec(&json!({"sessions":[
        {"path":"/archive/a","workspace":"/work","agent":"claude","source_id":"local","origin_host":null},
        {"path":"/archive/b","workspace":"/work","agent":"claude","source_id":"other-host","origin_host":"host"},
        {"path":"/archive/c","workspace":"/work/subproject","agent":"codex","source_id":"local"}
    ]})).unwrap();
    let list = decode_listing(&bytes, Path::new("/work"), 3).unwrap();
    assert_eq!(list.sessions.len(), 1);
    assert_eq!(list.rejected_remote, 1);
    assert_eq!(list.rejected_workspace, 1);
    assert!(!list.complete);
    assert_eq!(list.sessions[0].selection.source().as_str(), "local");
    assert!(!format!("{list:?}").contains("/archive/a"));
    assert!(
        decode_listing(b"{\"sessions\":[]}", Path::new("/work"), 3)
            .unwrap()
            .complete
    );
}

#[test]
fn malformed_duplicate_deep_and_oversized_responses_do_not_echo_data() {
    for bytes in [
        b"{secret-canary}".as_slice(),
        b"[{\"a\":1,\"a\":2}]",
        b"[1]",
        b"{}",
    ] {
        let e = decode_export(bytes, producer(), true).unwrap_err();
        assert_eq!(e, CassError::InvalidResponse);
        assert!(!format!("{e:?} {e}").contains("secret-canary"));
    }
    assert_eq!(
        decode_export(&vec![b' '; EXPORT_BYTES + 1], producer(), true).unwrap_err(),
        CassError::LimitExceeded
    );
    let deep = format!("[{{\"x\":{}{}}}]", "[".repeat(70), "]".repeat(70));
    assert_eq!(
        decode_export(deep.as_bytes(), producer(), true).unwrap_err(),
        CassError::InvalidResponse
    );
    let too_many = serde_json::to_vec(&vec![json!({}); MAX_MESSAGES + 1]).unwrap();
    assert_eq!(
        decode_export(&too_many, producer(), true).unwrap_err(),
        CassError::LimitExceeded
    );
    assert_eq!(
        ArchiveSelection::local(PathBuf::from("/a"), SourceId::new("remote").unwrap()).unwrap_err(),
        CassError::RemoteSource
    );
}

#[test]
fn mode_restrictions_happen_before_even_capability_probe() {
    let root = dir();
    let marker = root.join("spawned");
    let path = executable(&root, &format!("printf x > '{}'", marker.display()));
    for policy in [
        SourcePolicy {
            offline: true,
            ..Default::default()
        },
        SourcePolicy {
            local_only: true,
            ..Default::default()
        },
        SourcePolicy {
            dry_run: true,
            ..Default::default()
        },
    ] {
        let r = Run::new();
        let result = r.runtime.block_on(CassAdapter::probe(
            config(&path, &root),
            policy,
            &r.cx,
            &r.clock,
        ));
        assert_eq!(result.unwrap_err(), CassError::UnsupportedSourceMode);
        assert!(!marker.exists());
    }
}

#[test]
fn exact_export_argv_and_success_use_real_owned_child_runner() {
    let root = dir();
    let path = executable(
        &root,
        &script(
            "[ \"$3\" = export ] && [ \"$4\" = '/synthetic/path with spaces.jsonl' ] && [ \"$5\" = --source ] && [ \"$6\" = local ] && [ \"$7\" = --format ] && [ \"$8\" = json ] && [ \"$9\" = --include-tools ] || exit 9\nprintf '[{\"role\":\"user\",\"content\":\"success\"}]'",
        ),
    );
    let r = Run::new();
    let adapter = r
        .runtime
        .block_on(CassAdapter::probe(
            config(&path, &root),
            SourcePolicy::default(),
            &r.cx,
            &r.clock,
        ))
        .unwrap();
    let export = r
        .runtime
        .block_on(adapter.export(
            &selection(),
            false,
            SourcePolicy::default(),
            &r.cx,
            &r.clock,
        ))
        .unwrap();
    assert_eq!(export.records[0].native_value()["content"], "success");
    let stopped = r.runtime.block_on(adapter.export(
        &selection(),
        true,
        SourcePolicy {
            offline: true,
            ..Default::default()
        },
        &r.cx,
        &r.clock,
    ));
    assert_eq!(stopped.unwrap_err(), CassError::UnsupportedSourceMode);
}

#[test]
fn child_failure_and_eight_mib_overflow_are_sanitized() {
    for (body, expected) in [
        ("printf secret-canary >&2; exit 17", CassError::ChildFailed),
        (
            "exec /usr/bin/head -c 8388609 /dev/zero",
            CassError::LimitExceeded,
        ),
    ] {
        let root = dir();
        let path = executable(&root, &script(body));
        let r = Run::new();
        let adapter = r
            .runtime
            .block_on(CassAdapter::probe(
                config(&path, &root),
                SourcePolicy::default(),
                &r.cx,
                &r.clock,
            ))
            .unwrap();
        let error = r
            .runtime
            .block_on(adapter.export(&selection(), true, SourcePolicy::default(), &r.cx, &r.clock))
            .unwrap_err();
        assert_eq!(error, expected);
        assert!(!format!("{error:?} {error}").contains("secret-canary"));
    }
}

#[test]
fn provider_credentials_and_routing_environment_are_not_forwarded() {
    if std::env::var_os("SR_CASS_ENV_TEST_CHILD").is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "provider_credentials_and_routing_environment_are_not_forwarded",
                "--nocapture",
            ])
            .env_clear()
            .env("SR_CASS_ENV_TEST_CHILD", "1")
            .env("TYPESAFE_API_KEY", "synthetic-canary")
            .env("HTTPS_PROXY", "synthetic-proxy")
            .env("OPENAI_API_KEY", "synthetic-canary")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    let root = dir();
    let path = executable(
        &root,
        &script(
            "[ -z \"${TYPESAFE_API_KEY+x}${OPENAI_API_KEY+x}${HTTPS_PROXY+x}${HOME+x}\" ] || exit 19\nprintf '[{\"role\":\"user\",\"content\":\"scrubbed\"}]'",
        ),
    );
    let r = Run::new();
    let adapter = r
        .runtime
        .block_on(CassAdapter::probe(
            config(&path, &root),
            SourcePolicy::default(),
            &r.cx,
            &r.clock,
        ))
        .unwrap();
    let export = r
        .runtime
        .block_on(adapter.export(
            &selection(),
            false,
            SourcePolicy::default(),
            &r.cx,
            &r.clock,
        ))
        .unwrap();
    assert_eq!(export.records[0].native_value()["content"], "scrubbed");
}

#[test]
#[ignore = "requires SR_TEST_CASS pointing to the actual installed cass 0.8.0 binary"]
fn actual_installed_cass_exports_synthetic_session() {
    let path = PathBuf::from(std::env::var_os("SR_TEST_CASS").expect("explicit real cass path"))
        .canonicalize()
        .unwrap();
    let root = dir();
    let session = root.join("synthetic.jsonl");
    fs::write(&session, include_bytes!("fixtures/cass-session.jsonl")).unwrap();
    let config = CassConfig {
        executable: TrustedExecutable::resolve(&path, &[path.parent().unwrap().to_path_buf()])
            .unwrap(),
        directory: root.clone(),
        database: root.join("unused.db"),
        binary_digest: ContentHash::from_bytes(&fs::read(&path).unwrap()),
    };
    let r = Run::new();
    let adapter = r
        .runtime
        .block_on(CassAdapter::probe(
            config,
            SourcePolicy::default(),
            &r.cx,
            &r.clock,
        ))
        .unwrap();
    let selected = ArchiveSelection::local(session, SourceId::new("local").unwrap()).unwrap();
    let export = r
        .runtime
        .block_on(adapter.export(&selected, true, SourcePolicy::default(), &r.cx, &r.clock))
        .unwrap();
    let expected: Vec<Value> =
        serde_json::from_slice(include_bytes!("fixtures/cass-export.json")).unwrap();
    assert_eq!(export.records.len(), expected.len());
    for (actual, expected) in export.records.iter().zip(expected) {
        assert_eq!(actual.native_value(), &expected);
    }
    assert!(export.skill_load_evidence_unknown);
}

#[test]
fn native_archive_normalizes_links_and_tool_calls_without_load_or_branch_claims() {
    use skillranker::context::{EventKind, ToolStatus};
    let archive = decode_export(
        include_bytes!("fixtures/cass-export.json"),
        producer(),
        true,
    )
    .unwrap();
    let normalized = archive.normalized_events().unwrap();
    assert_eq!(normalized.events.len(), 5);
    assert_eq!(
        normalized.events[1].parent_id.as_ref().unwrap().as_str(),
        "u1"
    );
    assert_eq!(normalized.events[2].kind, EventKind::ToolInvocation);
    assert_eq!(
        normalized.events[2]
            .tool
            .as_ref()
            .unwrap()
            .call_id
            .as_ref()
            .unwrap()
            .as_str(),
        "t1"
    );
    assert_eq!(normalized.events[4].kind, EventKind::ToolResult);
    assert_eq!(
        normalized.events[4].tool.as_ref().unwrap().status,
        ToolStatus::Unknown
    );
    assert!(normalized.events[2].event_id.is_none());
    assert!(normalized.active_branch_unverified);
    assert!(!format!("{normalized:?}").contains("Synthetic task"));
    let mut rows: Value =
        serde_json::from_slice(include_bytes!("fixtures/cass-export.json")).unwrap();
    rows[1]["uuid"] = "u1".into();
    let archive = decode_export(&serde_json::to_vec(&rows).unwrap(), producer(), true).unwrap();
    assert_eq!(
        archive.normalized_events().unwrap_err(),
        CassError::InvalidResponse
    );
}

#[test]
fn sessions_command_has_bounded_explicit_argv_and_policy_gate() {
    let root = dir();
    let path = executable(
        &root,
        &script(
            "[ \"$3\" = sessions ] && [ \"$4\" = --workspace ] && [ \"$5\" = /synthetic/work ] && [ \"$6\" = --limit ] && [ \"$7\" = 2 ] && [ \"$8\" = --json ] || exit 9\nprintf '{\"sessions\":[{\"path\":\"/archive/local\",\"workspace\":\"/synthetic/work\",\"agent\":\"claude\",\"source_id\":\"local\"}]}'",
        ),
    );
    let r = Run::new();
    let adapter = r
        .runtime
        .block_on(CassAdapter::probe(
            config(&path, &root),
            SourcePolicy::default(),
            &r.cx,
            &r.clock,
        ))
        .unwrap();
    let listing = r
        .runtime
        .block_on(adapter.sessions(
            Path::new("/synthetic/work"),
            2,
            SourcePolicy::default(),
            &r.cx,
            &r.clock,
        ))
        .unwrap();
    assert_eq!(listing.sessions.len(), 1);
    assert!(listing.complete);
    assert_eq!(
        r.runtime
            .block_on(adapter.sessions(
                Path::new("/synthetic/work"),
                2,
                SourcePolicy {
                    dry_run: true,
                    ..Default::default()
                },
                &r.cx,
                &r.clock
            ))
            .unwrap_err(),
        CassError::UnsupportedSourceMode
    );
}

#[test]
fn unsupported_capability_probe_never_runs_export() {
    let root = dir();
    let marker = root.join("exported");
    let mut data = caps();
    data["crate_version"] = "0.9.0".into();
    let path = executable(
        &root,
        &format!(
            "if [ \"$3\" = capabilities ]; then /bin/cat <<'CAPS'\n{data}\nCAPS\nelse printf x > '{}'; fi",
            marker.display()
        ),
    );
    let r = Run::new();
    assert_eq!(
        r.runtime
            .block_on(CassAdapter::probe(
                config(&path, &root),
                SourcePolicy::default(),
                &r.cx,
                &r.clock
            ))
            .unwrap_err(),
        CassError::UnsupportedProducer
    );
    assert!(!marker.exists());
}

#[test]
fn duplicate_listing_and_missing_json_format_support_are_refused() {
    let row = json!({"path":"/a","workspace":"/work","agent":"claude","source_id":"local"});
    let bytes = serde_json::to_vec(&json!({"sessions":[row.clone(),row]})).unwrap();
    assert_eq!(
        decode_listing(&bytes, Path::new("/work"), 3).unwrap_err(),
        CassError::InvalidResponse
    );
    let mut data = caps();
    data["commands"][0]["arguments"][2]["enum_values"] = json!(["markdown"]);
    assert_eq!(
        validate_capabilities(
            &serde_json::to_vec(&data).unwrap(),
            ContentHash::from_bytes(b"fixture")
        )
        .unwrap_err(),
        CassError::UnsupportedProducer
    );
}
