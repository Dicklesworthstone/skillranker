#![cfg(unix)]
//! Contract and verification tests for load attribution and atomic observation watermarks
//! (sr-roadmap-l1i.6.9 / Invariants I18, G39).
//!
//! Required behavior:
//! - CAS cursor generation conflict prevents stale overwrites (`StoreError::RecordConflict`).
//! - Atomic watermark advance, observations commit, and generation bump.
//! - Deduplication of `source_event_key` for idempotent repeated delivery.
//! - Attribution of loaded skills to the latest preceding emitted ranking event within 30 minutes.
//! - Earlier overlapping suggestions are superseded/censored rather than credited.
//! - Failed/attempted tools become `Attempted`, not `Loaded`.
//! - Prose mentions and arbitrary path reads never count as loads.
//! - Path-only reads without verified content hash never suppress references.
//! - Compaction invalidates prior-epoch suppression.
//! - Workflows remain repeatable across turns.
//! - `sr observe` requires explicit source: rejects missing source with exit code 2.
//! - `sr observe` rejects `--no-ledger` and `--no-persist` with exit code 2 (`invalid-usage`).
//! - `sr observe` requires SQLite ledger: fails with exit code 9 if missing.
//! - End-to-end CLI execution with context and transcript.

use asupersync::Cx;
use rusqlite::Connection;
use skillranker::context::branch::{
    ActiveBranch, LoadedSkillRecord, SkillUsageKind, evaluate_loaded_skill_eligibility,
};
use skillranker::context::tool::{SimpleSkillResolver, SkillMatch, extract_load_observations};
use skillranker::context::{
    EventKind, LoadState, NormalizedEvent, PrivateText, Role, ToolEvent, ToolStatus,
};
use skillranker::identity::{
    BranchId, ContentHash, ContextEpoch, EventId, HarnessId, SessionId, SessionIdentity, SkillId,
    SourceProvenance, ToolCallId, TurnId, WorkspaceId,
};
use skillranker::runtime::ProcessInvocation;
use skillranker::storage::ledger::*;
use skillranker::storage::{
    CandidateStage, CursorKind, DecisionKind, EvidenceState, ExposureState, LedgerAccess,
    LedgerLocation, NewObservation, NewRankingCandidate, NewRankingEvent, SessionCursor,
    StoreError, get_session_cursor, get_session_observations, record_observations_with_cursor,
    record_ranking,
};
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn test_invocation() -> (ProcessInvocation, Cx) {
    let inv = ProcessInvocation::enter().expect("process invocation");
    let cx = inv.request_cx().expect("request cx");
    (inv, cx)
}

fn temp_private_dir(prefix: &str) -> PathBuf {
    let dir = PathBuf::from("/tmp").join(format!(
        "sr-test-obs-{}-{}-{}",
        prefix,
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&dir)
        .expect("temp dir create");
    dir
}

/// Actual CLI and SQLite controls isolated from maintainer configuration.
struct ObserveBoundaryFixture {
    root: PathBuf,
}

impl ObserveBoundaryFixture {
    fn new() -> Self {
        let root = temp_private_dir("observe-boundary");
        for dir in ["workspace/.sr", "home/.config/sr", "ledger"] {
            fs::DirBuilder::new()
                .mode(0o700)
                .recursive(true)
                .create(root.join(dir))
                .unwrap();
        }
        let fixture = Self { root };
        let init = fixture
            .command()
            .args(["ledger", "init", "--dir"])
            .arg(fixture.root.join("ledger"))
            .arg("--json")
            .output()
            .unwrap();
        assert!(
            init.status.success(),
            "{}",
            String::from_utf8_lossy(&init.stderr)
        );
        fixture.context(&[]);
        fixture
    }

    fn command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_sr"));
        command
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("home/.config"))
            .current_dir(self.root.join("workspace"));
        command
    }

    fn context(&self, skills: &[&str]) {
        let mut events = vec![serde_json::json!({"event_id":"request", "branch_id":"main",
            "role":"user", "kind":"message", "text":"Review the code"})];
        events.extend(skills.iter().enumerate().map(|(index, skill)| {
            let parent = if index == 0 { "request".to_owned() } else { format!("load-{}", index - 1) };
            serde_json::json!({"event_id":format!("load-{index}"), "parent_id":parent, "branch_id":"main",
                "role":"tool", "kind":"tool_invocation", "text":"",
                "tool":{"call_id":format!("call-{index}"), "name":skill,
                    "status":"succeeded", "arguments":"{}", "result":"Skill loaded"}})
        }));
        fs::write(
            self.root.join("workspace/context.json"),
            serde_json::json!({
                "schema_version":1, "harness":"claude_code",
                "workspace_root":self.root.join("workspace"), "session_id":"observe-boundary",
                "branch_id":"main", "context_epoch":"epoch-0",
                "current_request":{"event_id":"request", "text":"Review the code",
                    "attachments_omitted":false, "essential_attachment_missing":false},
                "events":events, "explicit_skill_references":[], "supplied_loads":[]
            })
            .to_string(),
        )
        .unwrap();
    }

    fn observe(&self, path: &str, environment: &[(&str, &str)]) -> std::process::Output {
        let mut command = self.command();
        command
            .args(["observe", "--context", path, "--dir"])
            .arg(self.root.join("ledger"))
            .arg("--json");
        for (key, value) in environment {
            command.env(key, value);
        }
        command.output().unwrap()
    }

    fn fork_context(&self) -> serde_json::Value {
        self.skill("workspace/custom", "left-review");
        self.skill("workspace/custom", "right-review");
        fs::write(
            self.root.join("workspace/.sr/config.toml"),
            "[roster]\nroots=['custom']\n",
        )
        .unwrap();
        self.context(&["left-review", "right-review"]);
        let path = self.root.join("workspace/context.json");
        let mut context: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        context["branch_id"] = serde_json::Value::Null;
        context["events"][0]["branch_id"] = serde_json::Value::Null;
        for (index, branch) in [(1, "left"), (2, "right")] {
            context["events"][index]["parent_id"] = serde_json::json!("request");
            context["events"][index]["branch_id"] = serde_json::json!(branch);
        }
        fs::write(path, context.to_string()).unwrap();
        context
    }

    fn observe_branch(&self, branch: Option<&str>, path: &str) -> std::process::Output {
        let mut command = self.command();
        command
            .args(["observe", "--context", path, "--dir"])
            .arg(self.root.join("ledger"))
            .arg("--json");
        if let Some(branch) = branch {
            command.args(["--branch", branch]);
        }
        command.output().unwrap()
    }

    fn counts(&self) -> (i64, i64) {
        let connection = Connection::open(self.root.join("ledger").join(LEDGER_FILE)).unwrap();
        connection.query_row(
            "SELECT (SELECT count(*) FROM observations), (SELECT count(*) FROM session_cursors)",
            [], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap()
    }

    fn skill(&self, root: &str, name: &str) {
        let dir = self.root.join(root).join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), format!(
            "---\nname: {name}\ndescription: Review code carefully\n---\nInspect the implementation.\n"
        )).unwrap();
    }
}

#[test]
fn observe_boundary_unresolved_lineage_cannot_record_or_advance_cursor() {
    for case in [
        "siblings",
        "missing-branch",
        "cycle",
        "conflicting-ancestor",
        "disconnected-context",
    ] {
        let fixture = ObserveBoundaryFixture::new();
        let mut context = fixture.fork_context();
        let branch = match case {
            "missing-branch" => Some("absent"),
            "cycle" => {
                context["events"][0]["parent_id"] = serde_json::json!("load-0");
                Some("left")
            }
            "conflicting-ancestor" => {
                context["events"][0]["branch_id"] = serde_json::json!("right");
                Some("left")
            }
            "disconnected-context" => {
                context["events"].as_array_mut().unwrap().truncate(2);
                context["events"][0]["branch_id"] = serde_json::json!("left");
                context["events"][1]["parent_id"] = serde_json::Value::Null;
                Some("left")
            }
            _ => None,
        };
        fs::write(
            fixture.root.join("workspace/context.json"),
            context.to_string(),
        )
        .unwrap();
        let output = fixture.observe_branch(branch, "context.json");
        assert_eq!(
            output.status.code(),
            Some(3),
            "{case}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["error"]["kind"], "ambiguous-branch", "{case}");
        assert_eq!(fixture.counts(), (0, 0), "{case}");
        let connection = Connection::open(fixture.root.join("ledger").join(LEDGER_FILE)).unwrap();
        let snapshots: i64 = connection
            .query_row("SELECT count(*) FROM roster_snapshots", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            snapshots, 0,
            "{case}: refusal must precede roster persistence"
        );
    }
}

#[test]
fn observe_boundary_selected_fork_records_only_its_load_idempotently() {
    for (branch, last_event) in [("left", "load-0"), ("right", "load-1")] {
        let fixture = ObserveBoundaryFixture::new();
        fixture.fork_context();
        for generation in 1..=2 {
            let output = fixture.observe_branch(Some(branch), "context.json");
            assert!(
                output.status.success(),
                "{} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["agent_branch"], branch);
            // The watermark names the last inspected source record, while
            // persisted observations must contain only the selected lineage.
            assert_eq!(value["last_event_id"], "load-1");
            assert_eq!(value["observations_recorded"], 1);
            assert_eq!(value["cursor_generation"], generation);
            assert_eq!(fixture.counts(), (1, 1));
        }
        let connection = Connection::open(fixture.root.join("ledger").join(LEDGER_FILE)).unwrap();
        let observation: (String, String, String) = connection
            .query_row(
                "SELECT agent_branch, evidence_state, source_event_key FROM observations",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(observation.0, branch);
        assert_eq!(observation.1, "loaded");
        assert!(observation.2.contains(last_event));
        let cursor_branch: String = connection
            .query_row("SELECT agent_branch FROM session_cursors", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(cursor_branch, branch);
    }
}

#[test]
fn observe_boundary_invalid_branch_is_rejected_before_source_reads() {
    let too_long = "a".repeat(skillranker::identity::MAX_ID_BYTES + 1);
    for branch in [
        "",
        "two branches",
        "private\nbranch",
        "\u{202e}branch",
        &too_long,
    ] {
        let fixture = ObserveBoundaryFixture::new();
        let output = fixture.observe_branch(Some(branch), "nonexistent.json");
        assert_eq!(output.status.code(), Some(2));
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            value["error"]["message"],
            "Invalid observation branch identity"
        );
        assert_eq!(fixture.counts(), (0, 0));
    }
}

#[test]
fn observe_boundary_empty_or_unlabelled_history_preserves_safe_observation() {
    for empty in [true, false] {
        let fixture = ObserveBoundaryFixture::new();
        fixture.skill("workspace/.claude/skills", "review");
        fixture.context(&["review"]);
        let path = fixture.root.join("workspace/context.json");
        let mut context: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        context["branch_id"] = serde_json::Value::Null;
        if empty {
            context["events"] = serde_json::json!([]);
        } else {
            for event in context["events"].as_array_mut().unwrap() {
                event["branch_id"] = serde_json::Value::Null;
            }
        }
        fs::write(path, context.to_string()).unwrap();
        let output = fixture.observe_branch(empty.then_some("selected"), "context.json");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            value["agent_branch"],
            if empty { "selected" } else { "main" }
        );
        assert_eq!(value["observations_recorded"], if empty { 0 } else { 1 });
        assert_eq!(fixture.counts(), (if empty { 0 } else { 1 }, 1));
    }
}

#[test]
fn observe_boundary_producer_namespaces_preserve_distinct_loads_and_idempotence() {
    let fixture = ObserveBoundaryFixture::new();
    fixture.skill("workspace/.claude/skills", "review");
    fixture.context(&["review"]);
    let path = fixture.root.join("workspace/context.json");
    let mut context: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for (index, (producer, session, agent)) in [
        ("producer-a", "same-session", None),
        ("producer-b", "same-session", None),
        ("a", "b:c", None),
        ("a:b", "c", None),
        ("producer-a", "same-session", Some("agent-a")),
        ("producer-a", "same-session", Some("agent-b")),
    ]
    .into_iter()
    .enumerate()
    {
        context["producer_id"] = serde_json::json!(producer);
        context["session_id"] = serde_json::json!(session);
        context["agent_id"] = serde_json::json!(agent);
        for event in context["events"].as_array_mut().unwrap() {
            event["agent_id"] = serde_json::json!(agent);
        }
        fs::write(&path, context.to_string()).unwrap();
        for generation in [1, 2] {
            let output = fixture.observe("context.json", &[]);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["cursor_generation"], generation);
            assert_eq!(value["observations_recorded"], 1);
            assert_eq!(fixture.counts(), ((index + 1) as i64, (index + 1) as i64));
        }
    }
    let connection = Connection::open(fixture.root.join("ledger").join(LEDGER_FILE)).unwrap();
    let loaded: i64 = connection.query_row(
        "SELECT count(*) FROM observations WHERE evidence_state='loaded' AND source_event_key LIKE 'normalized-v3:%'",
        [], |row| row.get(0),
    ).unwrap();
    assert_eq!(loaded, 6);
}

#[test]
fn observe_boundary_producer_keys_do_not_reassign_legacy_rows() {
    let fixture = ObserveBoundaryFixture::new();
    fixture.skill("workspace/.claude/skills", "review");
    fixture.context(&["review"]);
    let path = fixture.root.join("workspace/context.json");
    let mut context: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let listing = fixture
        .command()
        .args(["roster", "--json"])
        .output()
        .unwrap();
    assert!(listing.status.success());
    let listing: serde_json::Value = serde_json::from_slice(&listing.stdout).unwrap();
    let skill_id = listing["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["name"] == "review")
        .unwrap()["skill_id"]
        .as_str()
        .unwrap();
    let workspace = fixture
        .root
        .join("workspace")
        .to_string_lossy()
        .into_owned();
    let location = LedgerLocation::Directory(fixture.root.join("ledger"));
    let (invocation, cx) = test_invocation();
    // Seed the actual historical formats through the storage API. The new CLI
    // deliberately cannot create these incompletely scoped identities.
    let historical = [
        (
            "observe-boundary".to_owned(),
            format!("normalized:observe-boundary:main:load-0:{skill_id}"),
        ),
        (
            format!(
                "producer-v2:{}",
                serde_json::json!(["claude_code", "producer-a", null, "observe-boundary"])
            ),
            format!(
                "normalized-v2:{}",
                serde_json::json!([
                    workspace,
                    "claude_code",
                    "producer-a",
                    null,
                    "observe-boundary",
                    "main",
                    "load-0",
                    skill_id
                ])
            ),
        ),
    ]
    .map(|(session_key, source_event_key)| {
        let observation = NewObservation {
            observation_id: format!("obs-{source_event_key}"),
            source_event_key,
            workspace_root: workspace.clone(),
            session_id: "observe-boundary".into(),
            agent_branch: "main".into(),
            attributed_event_id: None,
            skill_id: skill_id.into(),
            evidence_state: EvidenceState::Loaded,
            observed_at_unix_ms: 1234,
        };
        let cursor = SessionCursor {
            workspace_root: workspace.clone(),
            session_id: session_key,
            agent_branch: "main".into(),
            cursor_kind: CursorKind::Observation,
            transcript_generation: 1,
            last_complete_event_id: "load-0".into(),
            last_offset_bytes: 12,
            updated_at_unix_ms: 1234,
        };
        assert_eq!(
            record_observations_with_cursor(
                &invocation,
                &cx,
                LedgerAccess::ExistingOnly,
                location.clone(),
                std::slice::from_ref(&observation),
                &cursor,
                Some(0)
            ),
            Ok(true)
        );
        (observation, cursor)
    });
    for producer in [None, Some("producer-a"), Some("producer-b")] {
        context["producer_id"] = serde_json::json!(producer);
        fs::write(&path, context.to_string()).unwrap();
        let output = fixture.observe("context.json", &[]);
        assert!(output.status.success());
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["cursor_generation"], 1);
    }
    assert_eq!(fixture.counts(), (5, 5));
    let (invocation, cx) = test_invocation();
    let observations = get_session_observations(
        &invocation,
        &cx,
        LedgerAccess::ExistingOnly,
        location.clone(),
        &workspace,
        "observe-boundary",
    )
    .unwrap();
    for (observation, cursor) in historical {
        assert_eq!(
            observations
                .iter()
                .find(|row| row.observation_id == observation.observation_id),
            Some(&observation)
        );
        assert_eq!(
            get_session_cursor(
                &invocation,
                &cx,
                LedgerAccess::ExistingOnly,
                location.clone(),
                &workspace,
                &cursor.session_id,
                "main",
                CursorKind::Observation
            )
            .unwrap(),
            Some(cursor)
        );
    }
}

#[test]
fn observe_boundary_nullable_producer_scopes_include_agent_harness_and_epoch() {
    let fixture = ObserveBoundaryFixture::new();
    fixture.skill("workspace/custom", "review");
    fs::write(
        fixture.root.join("workspace/.sr/config.toml"),
        "[roster]\nroots=['custom']\n",
    )
    .unwrap();
    fixture.context(&["review"]);
    let path = fixture.root.join("workspace/context.json");
    let mut context: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for (index, (producer, harness, agent, epoch)) in [
        (None, "claude_code", None, None),
        (None, "claude_code", None, Some("epoch-0")),
        (None, "claude_code", Some("agent-a"), Some("epoch-0")),
        (None, "claude_code", Some("agent-b"), Some("epoch-0")),
        (None, "external-harness", Some("agent-b"), Some("epoch-0")),
        (None, "external-harness", Some("agent-b"), Some("epoch-1")),
        (
            Some("producer-a"),
            "external-harness",
            Some("agent-b"),
            Some("epoch-1"),
        ),
        (
            Some("producer-a"),
            "external-harness",
            Some("agent-b"),
            Some("epoch-0"),
        ),
        (
            Some("producer-a"),
            "external-harness",
            Some("agent-b"),
            None,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        context["producer_id"] = serde_json::json!(producer);
        context["harness"] = serde_json::json!(harness);
        context["agent_id"] = serde_json::json!(agent);
        context["context_epoch"] = serde_json::json!(epoch);
        for event in context["events"].as_array_mut().unwrap() {
            event["agent_id"] = serde_json::json!(agent);
        }
        fs::write(&path, context.to_string()).unwrap();
        for generation in [1, 2] {
            let output = fixture.observe("context.json", &[]);
            assert!(
                output.status.success(),
                "{}; {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["cursor_generation"], generation);
            assert_eq!(value["observations_recorded"], 1);
            assert_eq!(fixture.counts(), ((index + 1) as i64, (index + 1) as i64));
        }
    }
    let connection = Connection::open(fixture.root.join("ledger").join(LEDGER_FILE)).unwrap();
    let loaded: (i64, i64, i64) = connection.query_row(
        "SELECT count(*),count(DISTINCT skill_id),count(attributed_event_id) FROM observations WHERE evidence_state='loaded' AND source_event_key LIKE 'normalized-v3:%'",
        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).unwrap();
    assert_eq!(loaded, (9, 1, 0));
}

#[test]
fn observe_boundary_unqualified_imports_in_shared_ledger_keep_actual_workspaces_separate() {
    let fixture = ObserveBoundaryFixture::new();
    fixture.skill("home/.claude/skills", "review");
    fixture.context(&["review"]);
    let context: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.root.join("workspace/context.json")).unwrap())
            .unwrap();
    let other = fixture.root.join("other-workspace");
    fs::DirBuilder::new().mode(0o700).create(&other).unwrap();
    // Both inputs claim the first workspace; only the invocation workspace has
    // authority. The same home skill identity is visible in both workspaces.
    fs::write(other.join("context.json"), context.to_string()).unwrap();
    for (index, workspace) in [fixture.root.join("workspace"), other]
        .into_iter()
        .enumerate()
    {
        for generation in [1, 2] {
            let output = fixture
                .command()
                .current_dir(&workspace)
                .args(["observe", "--context", "context.json", "--dir"])
                .arg(fixture.root.join("ledger"))
                .arg("--json")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}; {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["cursor_generation"], generation);
            assert_eq!(value["observations_recorded"], 1);
            assert_eq!(
                value["workspace_root"],
                workspace.to_string_lossy().as_ref()
            );
            assert_eq!(fixture.counts(), ((index + 1) as i64, (index + 1) as i64));
        }
    }
    let connection = Connection::open(fixture.root.join("ledger").join(LEDGER_FILE)).unwrap();
    let scopes: (i64, i64) = connection
        .query_row(
            "SELECT count(DISTINCT workspace_root),count(DISTINCT skill_id) FROM observations",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(scopes, (2, 1));
}

#[test]
fn observe_boundary_imported_loads_cannot_claim_native_exposure_by_repeating_ids() {
    for producer in [None, Some("external-producer")] {
        let fixture = ObserveBoundaryFixture::new();
        fixture.skill("workspace/.claude/skills", "review");
        fixture.context(&["review"]);
        let path = fixture.root.join("workspace/context.json");
        let mut context: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        context["producer_id"] = serde_json::json!(producer);
        fs::write(&path, context.to_string()).unwrap();
        let (invocation, cx) = test_invocation();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let mut event = make_test_ranking_event("native-exposure", now, ExposureState::Emitted);
        event.workspace_root = fixture
            .root
            .join("workspace")
            .to_string_lossy()
            .into_owned();
        event.session_id = "observe-boundary".into();
        event.agent_branch = "main".into();
        assert!(
            record_ranking(
                &invocation,
                &cx,
                LedgerAccess::ExistingOnly,
                LedgerLocation::Directory(fixture.root.join("ledger")),
                &event,
                &[],
                None
            )
            .unwrap()
        );
        for _ in 0..2 {
            let output = fixture.observe("context.json", &[]);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            assert_eq!(fixture.counts(), (1, 1));
        }
        let connection = Connection::open(fixture.root.join("ledger").join(LEDGER_FILE)).unwrap();
        let observation: (String, Option<String>) = connection
            .query_row(
                "SELECT evidence_state,attributed_event_id FROM observations",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(observation.0, "loaded");
        assert_eq!(
            observation.1, None,
            "repeated IDs cannot prove native exposure attribution"
        );
        let exposure: String = connection
            .query_row(
                "SELECT exposure_state FROM ranking_events WHERE event_id='native-exposure'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exposure, "emitted");
    }
}

#[test]
fn observe_boundary_native_namespace_cannot_authorize_an_unresolved_fork() {
    let fixture = ObserveBoundaryFixture::new();
    fixture.skill("workspace/.claude/skills", "review");
    let path = fixture.root.join("workspace/native-branch.jsonl");
    let records = [
        serde_json::json!({"cwd":fixture.root.join("workspace"),"sessionId":"native-branch"}),
        serde_json::json!({"type":"user","uuid":"request","sessionId":"native-branch",
            "message":{"role":"user","content":"Review code"}}),
        serde_json::json!({"type":"assistant","uuid":"call","parentUuid":"request",
            "sessionId":"native-branch","message":{"role":"assistant","content":[
                {"type":"tool_use","id":"review-call","name":"review","input":{}}]}}),
        serde_json::json!({"type":"tool_result","uuid":"result","parentUuid":"call",
            "sessionId":"native-branch","tool_use_id":"review-call",
            "content":"Skill loaded","is_error":false}),
    ];
    let lines = records.iter().map(ToString::to_string).collect::<Vec<_>>();
    fs::write(&path, lines.join("\n") + "\n").unwrap();
    let observe = || {
        fixture
            .command()
            .args(["observe", "--transcript"])
            .arg(&path)
            .args(["--harness", "claude_code", "--branch", "feature-x", "--dir"])
            .arg(fixture.root.join("ledger"))
            .arg("--json")
            .output()
            .unwrap()
    };
    let output = observe();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["agent_branch"], "feature-x");
    assert_eq!(value["observations_recorded"], 1);
    assert_eq!(fixture.counts(), (1, 1));
    let connection = Connection::open(fixture.root.join("ledger").join(LEDGER_FILE)).unwrap();
    let cursor = || {
        connection.query_row(
            "SELECT agent_branch, transcript_generation, last_offset_bytes, last_complete_event_id FROM session_cursors",
            [], |row| Ok((row.get::<_,String>(0)?, row.get::<_,i64>(1)?, row.get::<_,i64>(2)?, row.get::<_,String>(3)?)),
        ).unwrap()
    };
    let before = cursor();
    assert_eq!(before.0, "feature-x");
    assert_eq!(before.3, "result");
    for case in ["siblings", "missing-link"] {
        let mut broken = lines.clone();
        for leaf in if case == "siblings" {
            &["left", "right"][..]
        } else {
            &["continued"][..]
        } {
            broken.push(
                serde_json::json!({"type":"user","uuid":leaf,
                "parentUuid":if case == "siblings" { "result" } else { "missing-id" },
                "sessionId":"native-branch","message":{"role":"user","content":"Continue"}})
                .to_string(),
            );
        }
        fs::write(&path, broken.join("\n") + "\n").unwrap();
        let output = observe();
        assert_eq!(output.status.code(), Some(3), "{case}");
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["error"]["kind"], "ambiguous-branch", "{case}");
        assert_eq!(fixture.counts(), (1, 1), "{case}");
        assert_eq!(
            cursor(),
            before,
            "{case}: refusal must preserve the existing byte watermark"
        );
    }
}

#[test]
fn observe_boundary_invalid_configuration_cannot_advance_the_cursor() {
    let mut results = Vec::new();
    for (config, environment) in [
        ("", vec![("SR_UNKNOWN_OBSERVE_SETTING", "1")]),
        ("", vec![("SR_TIMEOUT_MS", "200")]),
        ("[network]\nenabled=true\n", vec![]),
        ("[ranking]\ntop=2\ntop=3\n", vec![]),
        ("[roster\n", vec![]),
        ("[roster]\nroots=['../external']\n", vec![]),
    ] {
        let fixture = ObserveBoundaryFixture::new();
        fs::write(fixture.root.join("workspace/.sr/config.toml"), config).unwrap();
        let output = fixture.observe("context.json", &environment);
        results.push((output, fixture.counts()));
    }
    for (output, counts) in results {
        assert_eq!(
            output.status.code(),
            Some(2),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(counts, (0, 0));
    }
    let fixture = ObserveBoundaryFixture::new();
    let output = fixture.observe("context.json", &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.counts(), (0, 1));
}

#[test]
fn observe_boundary_configured_roots_record_successful_loads_idempotently() {
    let fixture = ObserveBoundaryFixture::new();
    fixture.skill("workspace/custom", "project-review");
    fixture.skill("external", "personal-review");
    fs::write(
        fixture.root.join("workspace/.sr/config.toml"),
        "[roster]\nroots=['custom']\n",
    )
    .unwrap();
    fs::write(
        fixture.root.join("home/.config/sr/config.toml"),
        format!(
            "[roster]\nroots=['{}']\n",
            fixture.root.join("external").display()
        ),
    )
    .unwrap();
    fixture.context(&["project-review", "personal-review"]);
    for generation in 1..=2 {
        let output = fixture.observe("context.json", &[]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["observations_recorded"], 2);
        assert_eq!(value["cursor_generation"], generation);
        assert_eq!(fixture.counts(), (2, 1));
    }
    let connection = Connection::open(fixture.root.join("ledger").join(LEDGER_FILE)).unwrap();
    let loaded: i64 = connection
        .query_row(
            "SELECT count(*) FROM observations WHERE evidence_state='loaded'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(loaded, 2);
}

#[test]
fn observe_boundary_other_harness_requires_its_own_inventory() {
    let fixture = ObserveBoundaryFixture::new();
    fixture.skill("workspace/.claude/skills", "claude-only");
    fixture.context(&["claude-only"]);
    let path = fixture.root.join("workspace/context.json");
    let mut context: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    context["harness"] = serde_json::json!("external-harness");
    fs::write(&path, context.to_string()).unwrap();
    let output = fixture.observe("context.json", &[]);
    assert_eq!(output.status.code(), Some(5));
    assert_eq!(fixture.counts(), (0, 0));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["error"]["kind"], "unusable-roster");

    context["harness"] = serde_json::json!("claude_code");
    fs::write(&path, context.to_string()).unwrap();
    assert!(fixture.observe("context.json", &[]).status.success());
    assert_eq!(fixture.counts(), (1, 1));
}

#[test]
fn observe_boundary_other_harness_uses_only_configured_inventory() {
    let fixture = ObserveBoundaryFixture::new();
    fixture.skill("workspace/.claude/skills", "claude-only");
    fixture.skill("workspace/custom", "configured-only");
    fs::write(
        fixture.root.join("workspace/.sr/config.toml"),
        "[roster]\nroots=['custom']\n",
    )
    .unwrap();
    fixture.context(&["claude-only", "configured-only"]);
    let path = fixture.root.join("workspace/context.json");
    let mut context: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    context["harness"] = serde_json::json!("external-harness");
    fs::write(&path, context.to_string()).unwrap();
    let listing = fixture
        .command()
        .args(["roster", "--json"])
        .output()
        .unwrap();
    assert!(listing.status.success());
    let listing: serde_json::Value = serde_json::from_slice(&listing.stdout).unwrap();
    let expected = listing["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["name"] == "configured-only")
        .unwrap();
    assert!(
        expected["source"]
            .as_str()
            .unwrap()
            .starts_with("configured.")
    );
    for _ in 0..2 {
        let output = fixture.observe("context.json", &[]);
        assert!(
            output.status.success(),
            "stdout: {}; stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fixture.counts(), (1, 1));
        let connection = Connection::open(fixture.root.join("ledger").join(LEDGER_FILE)).unwrap();
        let actual: String = connection
            .query_row("SELECT skill_id FROM observations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(actual, expected["skill_id"].as_str().unwrap());
    }
}

#[test]
fn observe_boundary_refuses_escaping_symlinks_before_storage() {
    let fixture = ObserveBoundaryFixture::new();
    fs::copy(
        fixture.root.join("workspace/context.json"),
        fixture.root.join("outside.json"),
    )
    .unwrap();
    std::os::unix::fs::symlink(
        fixture.root.join("outside.json"),
        fixture.root.join("workspace/escape.json"),
    )
    .unwrap();
    let output = fixture.observe("escape.json", &[]);
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(fixture.counts(), (0, 0));
    std::os::unix::fs::symlink("context.json", fixture.root.join("workspace/alias.json")).unwrap();
    assert!(fixture.observe("alias.json", &[]).status.success());
    assert_eq!(fixture.counts(), (0, 1));
    let output = fixture.observe("context.json", &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.counts(), (0, 1));
}

#[test]
fn observe_boundary_refuses_oversized_regular_files_before_storage() {
    let fixture = ObserveBoundaryFixture::new();
    let file = fs::File::create(fixture.root.join("workspace/oversized.json")).unwrap();
    file.set_len((skillranker::limits::NORMALIZED_CONTEXT_JSON_BYTES.max() + 1) as u64)
        .unwrap();
    let output = fixture.observe("oversized.json", &[]);
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(fixture.counts(), (0, 0));
    assert!(fixture.observe("context.json", &[]).status.success());
    assert_eq!(fixture.counts(), (0, 1));
}

#[test]
fn observe_boundary_explicit_inventory_replaces_configured_discovery() {
    use std::os::unix::ffi::OsStrExt;
    let fixture = ObserveBoundaryFixture::new();
    fixture.skill("workspace/custom", "chosen");
    fixture.skill("workspace/custom", "omitted");
    fixture.context(&["chosen", "omitted"]);
    fs::write(
        fixture.root.join("workspace/.sr/config.toml"),
        "[roster]\nroots=['custom']\n",
    )
    .unwrap();
    let declared = fixture.root.join("workspace/custom");
    let source = format!(
        "configured.{}",
        blake3::hash(declared.as_os_str().as_bytes()).to_hex()
    );
    fs::write(
        fixture.root.join("workspace/roster.json"),
        serde_json::json!({
            "schema":"sr.roster.v1", "harness":"claude_code", "mode":"authorized_files",
            "skills":[{"source":source, "path":"chosen/SKILL.md"}]
        })
        .to_string(),
    )
    .unwrap();
    let output = fixture
        .command()
        .args([
            "observe",
            "--context",
            "context.json",
            "--roster",
            "roster.json",
            "--dir",
        ])
        .arg(fixture.root.join("ledger"))
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout: {}; stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["observations_recorded"], 1);
    assert_eq!(fixture.counts(), (1, 1));
}

#[test]
fn observe_boundary_refuses_fifo_and_device_inputs_without_blocking() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let fixture = ObserveBoundaryFixture::new();
    let fifo = fixture.root.join("workspace/input.fifo");
    nix::unistd::mkfifo(
        &fifo,
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .unwrap();
    let mut results = Vec::new();
    for path in [fifo.as_path(), Path::new("/dev/null")] {
        let mut child = fixture
            .command()
            .args(["observe", "--context"])
            .arg(path)
            .arg("--dir")
            .arg(fixture.root.join("ledger"))
            .arg("--json")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let started = Instant::now();
        let mut timed_out = false;
        while child.try_wait().unwrap().is_none() {
            if started.elapsed() >= Duration::from_secs(4) {
                child.kill().unwrap();
                timed_out = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // Always reap this exact owned child, including the planted stall.
        results.push((timed_out, child.wait_with_output().unwrap()));
    }
    let counts = fixture.counts();
    // Check the ordinary positive too, even when the old FIFO path stalls.
    let positive = fixture.observe("context.json", &[]);
    assert!(
        positive.status.success(),
        "{}",
        String::from_utf8_lossy(&positive.stderr)
    );
    assert_eq!(fixture.counts(), (0, 1));
    assert_eq!(counts, (0, 0));
    for (timed_out, output) in results {
        assert!(
            !timed_out,
            "nonregular input stalled until the supervisor killed and reaped it"
        );
        assert_eq!(output.status.code(), Some(7));
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["error"]["kind"], "unsupported-input");
    }
}

fn init_test_ledger(dir: &Path) {
    let (inv, cx) = test_invocation();
    init_ledger(&inv, &cx, LedgerLocation::Directory(dir.to_path_buf())).expect("init ledger");
}

fn make_test_ranking_event(
    event_id: &str,
    created_at_unix_ms: u64,
    exposure: ExposureState,
) -> NewRankingEvent {
    NewRankingEvent {
        event_id: event_id.into(),
        verified_delivery_key: None,
        workspace_root: "/data/workspace".into(),
        session_id: "test-session-obs".into(),
        agent_branch: "main".into(),
        mode_channel: "cli".into(),
        policy_version: "v1".into(),
        schema_version: 1,
        decision: DecisionKind::Ranked,
        reason: "eligible".into(),
        exposure_state: exposure,
        elapsed_ms: 15,
        created_at_unix_ms,
        input_tokens: Some(100),
        output_tokens: Some(25),
        snapshot_id: None,
    }
}

fn make_test_candidate(event_id: &str, skill_id: &str) -> NewRankingCandidate {
    NewRankingCandidate {
        event_id: event_id.into(),
        stage: CandidateStage::Rerank,
        skill_id: skill_id.into(),
        skill_version: "1.0.0".into(),
        raw_probability: Some(0.85),
        normalized_probability: Some(0.85),
        fit_score: Some(0.9),
        rank_score: Some(0.9),
        rank_position: Some(1),
        excluded: false,
        exclusion_reason: None,
    }
}

#[test]
fn test_cas_cursor_generation_conflict_prevents_stale_overwrites() {
    let dir = temp_private_dir("cas-conflict");
    init_test_ledger(&dir);
    let (inv, cx) = test_invocation();

    let cursor_1 = SessionCursor {
        workspace_root: "/data/workspace".into(),
        session_id: "session-1".into(),
        agent_branch: "main".into(),
        cursor_kind: CursorKind::Observation,
        transcript_generation: 1,
        last_complete_event_id: "ev-1".into(),
        last_offset_bytes: 100,
        updated_at_unix_ms: 1000,
    };

    let obs_1 = NewObservation {
        observation_id: "obs-1".into(),
        source_event_key: "key-1".into(),
        workspace_root: "/data/workspace".into(),
        session_id: "session-1".into(),
        agent_branch: "main".into(),
        attributed_event_id: None,
        skill_id: "skill-alpha".into(),
        evidence_state: EvidenceState::Loaded,
        observed_at_unix_ms: 1000,
    };

    // First commit with expected_cursor_gen: Some(0) succeeds
    let res1 = record_observations_with_cursor(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        &[obs_1],
        &cursor_1,
        Some(0),
    );
    assert_eq!(res1, Ok(true));

    // Stale overwrite attempt: passing expected_cursor_gen: Some(0) when db is already at generation 1
    let obs_2 = NewObservation {
        observation_id: "obs-2".into(),
        source_event_key: "key-2".into(),
        workspace_root: "/data/workspace".into(),
        session_id: "session-1".into(),
        agent_branch: "main".into(),
        attributed_event_id: None,
        skill_id: "skill-beta".into(),
        evidence_state: EvidenceState::Loaded,
        observed_at_unix_ms: 1100,
    };

    let cursor_stale = SessionCursor {
        workspace_root: "/data/workspace".into(),
        session_id: "session-1".into(),
        agent_branch: "main".into(),
        cursor_kind: CursorKind::Observation,
        transcript_generation: 1,
        last_complete_event_id: "ev-2-stale".into(),
        last_offset_bytes: 200,
        updated_at_unix_ms: 1100,
    };

    let res_stale = record_observations_with_cursor(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        std::slice::from_ref(&obs_2),
        &cursor_stale,
        Some(0),
    );
    assert_eq!(res_stale, Err(StoreError::RecordConflict));

    // Next commit with expected_cursor_gen: Some(1) and generation 2 succeeds
    let cursor_2 = SessionCursor {
        workspace_root: "/data/workspace".into(),
        session_id: "session-1".into(),
        agent_branch: "main".into(),
        cursor_kind: CursorKind::Observation,
        transcript_generation: 2,
        last_complete_event_id: "ev-2".into(),
        last_offset_bytes: 200,
        updated_at_unix_ms: 1200,
    };

    let res2 = record_observations_with_cursor(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        &[obs_2],
        &cursor_2,
        Some(1),
    );
    assert_eq!(res2, Ok(true));

    // Verify cursor generation in database
    let cur = get_session_cursor(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        "/data/workspace",
        "session-1",
        "main",
        CursorKind::Observation,
    )
    .expect("get cursor")
    .expect("cursor exists");
    assert_eq!(cur.transcript_generation, 2);
    assert_eq!(cur.last_complete_event_id, "ev-2");
}

#[test]
fn test_atomic_watermark_advance_and_generation_bump() {
    let dir = temp_private_dir("atomic-advance");
    init_test_ledger(&dir);
    let (inv, cx) = test_invocation();

    let db_path = dir.join(LEDGER_FILE);
    let conn = Connection::open(&db_path).expect("open db");

    let initial_gen: i64 = conn
        .query_row(
            "SELECT data_generation FROM store_meta WHERE singleton = 1",
            [],
            |r| r.get(0),
        )
        .expect("query initial gen");

    let cursor = SessionCursor {
        workspace_root: "/data/workspace".into(),
        session_id: "session-atomic".into(),
        agent_branch: "main".into(),
        cursor_kind: CursorKind::Observation,
        transcript_generation: 1,
        last_complete_event_id: "ev-100".into(),
        last_offset_bytes: 500,
        updated_at_unix_ms: 2000,
    };

    let obs = NewObservation {
        observation_id: "obs-100".into(),
        source_event_key: "key-atomic-1".into(),
        workspace_root: "/data/workspace".into(),
        session_id: "session-atomic".into(),
        agent_branch: "main".into(),
        attributed_event_id: None,
        skill_id: "skill-gamma".into(),
        evidence_state: EvidenceState::Loaded,
        observed_at_unix_ms: 2000,
    };

    record_observations_with_cursor(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        &[obs],
        &cursor,
        Some(0),
    )
    .expect("record observations");

    let new_gen: i64 = conn
        .query_row(
            "SELECT data_generation FROM store_meta WHERE singleton = 1",
            [],
            |r| r.get(0),
        )
        .expect("query new gen");
    assert_eq!(new_gen, initial_gen + 1);

    let obs_count: i64 = conn
        .query_row(
            "SELECT count(*) FROM observations WHERE session_id = 'session-atomic'",
            [],
            |r| r.get(0),
        )
        .expect("query obs count");
    assert_eq!(obs_count, 1);
}

#[test]
fn test_deduplication_of_source_event_key() {
    let dir = temp_private_dir("dedup-source-key");
    init_test_ledger(&dir);
    let (inv, cx) = test_invocation();

    let cursor_1 = SessionCursor {
        workspace_root: "/data/workspace".into(),
        session_id: "session-dedup".into(),
        agent_branch: "main".into(),
        cursor_kind: CursorKind::Observation,
        transcript_generation: 1,
        last_complete_event_id: "ev-1".into(),
        last_offset_bytes: 100,
        updated_at_unix_ms: 1000,
    };

    let obs = NewObservation {
        observation_id: "obs-orig".into(),
        source_event_key: "shared-event-key".into(),
        workspace_root: "/data/workspace".into(),
        session_id: "session-dedup".into(),
        agent_branch: "main".into(),
        attributed_event_id: None,
        skill_id: "skill-alpha".into(),
        evidence_state: EvidenceState::Loaded,
        observed_at_unix_ms: 1000,
    };

    record_observations_with_cursor(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        &[obs],
        &cursor_1,
        Some(0),
    )
    .expect("first record");

    // Re-delivering the same source_event_key in next batch (e.g. repeated hook delivery)
    let cursor_2 = SessionCursor {
        workspace_root: "/data/workspace".into(),
        session_id: "session-dedup".into(),
        agent_branch: "main".into(),
        cursor_kind: CursorKind::Observation,
        transcript_generation: 2,
        last_complete_event_id: "ev-2".into(),
        last_offset_bytes: 200,
        updated_at_unix_ms: 1100,
    };

    let obs_dup = NewObservation {
        observation_id: "obs-dup".into(),
        source_event_key: "shared-event-key".into(),
        workspace_root: "/data/workspace".into(),
        session_id: "session-dedup".into(),
        agent_branch: "main".into(),
        attributed_event_id: None,
        skill_id: "skill-alpha".into(),
        evidence_state: EvidenceState::Loaded,
        observed_at_unix_ms: 1100,
    };

    record_observations_with_cursor(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        &[obs_dup],
        &cursor_2,
        Some(1),
    )
    .expect("second record with duplicate key succeeds via ON CONFLICT DO NOTHING");

    let all_obs = get_session_observations(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        "/data/workspace",
        "session-dedup",
    )
    .expect("get observations");
    assert_eq!(
        all_obs.len(),
        1,
        "duplicate key was not duplicated in table"
    );
    assert_eq!(all_obs[0].observation_id, "obs-orig");
}

#[test]
fn test_attribution_to_latest_preceding_emission_within_30_min() {
    let dir = temp_private_dir("attribution-window");
    init_test_ledger(&dir);
    let (inv, cx) = test_invocation();

    // 1. Ranking event A emitted at t = 1_000_000
    let ev_a = make_test_ranking_event("rank-ev-A", 1_000_000, ExposureState::Emitted);
    let cand_a = make_test_candidate("rank-ev-A", "skill-test");
    record_ranking(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        &ev_a,
        &[cand_a],
        None,
    )
    .expect("record ranking A");

    // 2. Ranking event B emitted at t = 1_500_000 (supersedes A)
    let ev_b = make_test_ranking_event("rank-ev-B", 1_500_000, ExposureState::Emitted);
    let cand_b = make_test_candidate("rank-ev-B", "skill-test");
    record_ranking(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        &ev_b,
        &[cand_b],
        None,
    )
    .expect("record ranking B");

    // 3. Observation at t = 1_600_000: within 30 min (1,800,000 ms) of B
    let cursor = SessionCursor {
        workspace_root: "/data/workspace".into(),
        session_id: "test-session-obs".into(),
        agent_branch: "main".into(),
        cursor_kind: CursorKind::Observation,
        transcript_generation: 1,
        last_complete_event_id: "tool-1".into(),
        last_offset_bytes: 300,
        updated_at_unix_ms: 1_600_000,
    };

    let obs_recent = NewObservation {
        observation_id: "obs-recent".into(),
        source_event_key: "key-recent".into(),
        workspace_root: "/data/workspace".into(),
        session_id: "test-session-obs".into(),
        agent_branch: "main".into(),
        attributed_event_id: None, // Triggers auto-attribution
        skill_id: "skill-test".into(),
        evidence_state: EvidenceState::Loaded,
        observed_at_unix_ms: 1_600_000,
    };

    record_observations_with_cursor(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        &[obs_recent],
        &cursor,
        Some(0),
    )
    .expect("record observation");

    let all_obs = get_session_observations(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        "/data/workspace",
        "test-session-obs",
    )
    .expect("get observations");
    assert_eq!(all_obs.len(), 1);
    assert_eq!(
        all_obs[0].attributed_event_id.as_deref(),
        Some("rank-ev-B"),
        "must be attributed to latest preceding emitted event (B supersedes A)"
    );

    // 4. Observation at t = 5_000_000: > 30 minutes after B (3.5M ms > 1.8M ms)
    let cursor_late = SessionCursor {
        workspace_root: "/data/workspace".into(),
        session_id: "test-session-obs".into(),
        agent_branch: "main".into(),
        cursor_kind: CursorKind::Observation,
        transcript_generation: 2,
        last_complete_event_id: "tool-2".into(),
        last_offset_bytes: 600,
        updated_at_unix_ms: 5_000_000,
    };

    let obs_late = NewObservation {
        observation_id: "obs-late".into(),
        source_event_key: "key-late".into(),
        workspace_root: "/data/workspace".into(),
        session_id: "test-session-obs".into(),
        agent_branch: "main".into(),
        attributed_event_id: None,
        skill_id: "skill-test".into(),
        evidence_state: EvidenceState::Loaded,
        observed_at_unix_ms: 5_000_000,
    };

    record_observations_with_cursor(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        &[obs_late],
        &cursor_late,
        Some(1),
    )
    .expect("record late observation");

    let all_obs_2 = get_session_observations(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        "/data/workspace",
        "test-session-obs",
    )
    .expect("get observations");
    assert_eq!(all_obs_2.len(), 2);
    let late_obs = all_obs_2
        .iter()
        .find(|o| o.observation_id == "obs-late")
        .unwrap();
    assert_eq!(
        late_obs.attributed_event_id, None,
        "observation outside 30-min window must have None attribution"
    );
}

#[test]
fn test_failed_or_attempted_tools_become_attempted() {
    let mut resolver = SimpleSkillResolver::new();
    let hash = ContentHash::from_bytes(b"skill-content");
    resolver.register_tool(
        "run-analysis",
        SkillMatch {
            skill_id: SkillId::new("data-analysis").unwrap(),
            usage_kind: SkillUsageKind::Workflow,
            source_content: Some(hash.clone()),
            rendered_content: Some(hash),
            has_dynamic_arguments: false,
            turn_scoped: false,
        },
    );

    let session_ident = SessionIdentity {
        source: SourceProvenance::Normalized {
            producer: None,
            harness: HarnessId::new("claude_code").unwrap(),
            schema_version: 1,
        },
        workspace: Some(WorkspaceId::new("/data/ws").unwrap()),
        session: Some(SessionId::new("sess-1").unwrap()),
        agent: None,
        branch: Some(BranchId::new("main").unwrap()),
        epoch: Some(ContextEpoch::new("epoch-0").unwrap()),
    };

    // Case 1: Failed tool invocation
    let failed_event = NormalizedEvent {
        event_id: Some(EventId::new("ev-fail").unwrap()),
        parent_id: None,
        turn_id: Some(TurnId::new("turn-1").unwrap()),
        agent_id: None,
        branch_id: Some(BranchId::new("main").unwrap()),
        role: Role::Tool,
        kind: EventKind::ToolResult,
        timestamp_unix_ms: Some(1000),
        text: PrivateText::new("error: command not found"),
        tool: Some(ToolEvent {
            call_id: Some(ToolCallId::new("call-1").unwrap()),
            name: PrivateText::new("run-analysis"),
            status: ToolStatus::Failed,
            arguments: None,
            result: Some(PrivateText::new("error: command not found")),
        }),
    };

    let obs_failed = extract_load_observations(&[failed_event], &session_ident, &resolver, None);
    assert_eq!(obs_failed.len(), 1);
    assert_eq!(obs_failed[0].state, LoadState::Attempted);

    // Case 2: Succeeded tool with error lines indicator
    let error_line_event = NormalizedEvent {
        event_id: Some(EventId::new("ev-err-line").unwrap()),
        parent_id: None,
        turn_id: Some(TurnId::new("turn-2").unwrap()),
        agent_id: None,
        branch_id: Some(BranchId::new("main").unwrap()),
        role: Role::Tool,
        kind: EventKind::ToolResult,
        timestamp_unix_ms: Some(2000),
        text: PrivateText::new("Output: fatal: failed to parse argument"),
        tool: Some(ToolEvent {
            call_id: Some(ToolCallId::new("call-2").unwrap()),
            name: PrivateText::new("run-analysis"),
            status: ToolStatus::Succeeded,
            arguments: None,
            result: Some(PrivateText::new("fatal: failed to parse argument")),
        }),
    };

    let obs_err_line =
        extract_load_observations(&[error_line_event], &session_ident, &resolver, None);
    assert_eq!(obs_err_line.len(), 1);
    assert_eq!(obs_err_line[0].state, LoadState::Attempted);

    // Case 3: Clean successful invocation
    let success_event = NormalizedEvent {
        event_id: Some(EventId::new("ev-succ").unwrap()),
        parent_id: None,
        turn_id: Some(TurnId::new("turn-3").unwrap()),
        agent_id: None,
        branch_id: Some(BranchId::new("main").unwrap()),
        role: Role::Tool,
        kind: EventKind::ToolResult,
        timestamp_unix_ms: Some(3000),
        text: PrivateText::new("Analysis completed successfully"),
        tool: Some(ToolEvent {
            call_id: Some(ToolCallId::new("call-3").unwrap()),
            name: PrivateText::new("run-analysis"),
            status: ToolStatus::Succeeded,
            arguments: None,
            result: Some(PrivateText::new("Analysis completed successfully")),
        }),
    };

    let obs_succ = extract_load_observations(&[success_event], &session_ident, &resolver, None);
    assert_eq!(obs_succ.len(), 1);
    assert_eq!(obs_succ[0].state, LoadState::ObservedLoaded);
}

#[test]
fn test_prose_mentions_and_arbitrary_paths_never_count_as_loads() {
    let mut resolver = SimpleSkillResolver::new();
    let hash = ContentHash::from_bytes(b"target-content");
    resolver.register_tool(
        "target-skill",
        SkillMatch {
            skill_id: SkillId::new("target-skill").unwrap(),
            usage_kind: SkillUsageKind::Reference,
            source_content: Some(hash.clone()),
            rendered_content: Some(hash),
            has_dynamic_arguments: false,
            turn_scoped: false,
        },
    );

    let session_ident = SessionIdentity {
        source: SourceProvenance::Normalized {
            producer: None,
            harness: HarnessId::new("claude_code").unwrap(),
            schema_version: 1,
        },
        workspace: Some(WorkspaceId::new("/data/ws").unwrap()),
        session: Some(SessionId::new("sess-1").unwrap()),
        agent: None,
        branch: Some(BranchId::new("main").unwrap()),
        epoch: Some(ContextEpoch::new("epoch-0").unwrap()),
    };

    // User message mentioning the skill
    let user_msg = NormalizedEvent {
        event_id: Some(EventId::new("ev-user").unwrap()),
        parent_id: None,
        turn_id: Some(TurnId::new("turn-1").unwrap()),
        agent_id: None,
        branch_id: Some(BranchId::new("main").unwrap()),
        role: Role::User,
        kind: EventKind::Message,
        timestamp_unix_ms: Some(1000),
        text: PrivateText::new("Can you please use target-skill for this task?"),
        tool: None,
    };

    // Unrelated tool invocation
    let unrelated_tool = NormalizedEvent {
        event_id: Some(EventId::new("ev-tool-unrelated").unwrap()),
        parent_id: None,
        turn_id: Some(TurnId::new("turn-1").unwrap()),
        agent_id: None,
        branch_id: Some(BranchId::new("main").unwrap()),
        role: Role::Tool,
        kind: EventKind::ToolResult,
        timestamp_unix_ms: Some(1500),
        text: PrivateText::new("file contents"),
        tool: Some(ToolEvent {
            call_id: Some(ToolCallId::new("call-other").unwrap()),
            name: PrivateText::new("view_file"),
            status: ToolStatus::Succeeded,
            arguments: Some(PrivateText::new(r#"{"path": "src/main.rs"}"#)),
            result: Some(PrivateText::new("fn main() {}")),
        }),
    };

    let obs =
        extract_load_observations(&[user_msg, unrelated_tool], &session_ident, &resolver, None);
    assert_eq!(
        obs.len(),
        0,
        "prose mentions and unrelated tool calls never produce observations"
    );
}

#[test]
fn test_path_only_reads_without_content_hash_never_suppress_references() {
    let skill_id = SkillId::new("doc-reference").unwrap();
    let hash = ContentHash::from_bytes(b"immutable-doc-content");

    let branch = ActiveBranch {
        branch_id: Some(BranchId::new("main").unwrap()),
        leaf_event_id: None,
        events: Vec::new(),
        current_epoch: ContextEpoch::new("epoch-0").unwrap(),
        compaction_count: 0,
        task_boundary_count: 0,
        ancestor_chain_truncated: false,
    };

    // Record from a path-only read has source_content: None
    let path_only_record = LoadedSkillRecord {
        skill_id: skill_id.clone(),
        event_id: Some(EventId::new("ev-read").unwrap()),
        turn_id: Some(TurnId::new("turn-1").unwrap()),
        usage_kind: SkillUsageKind::Reference,
        epoch: ContextEpoch::new("epoch-0").unwrap(),
        source_content: None, // Content hash unknown from simple path read
        rendered_content: None,
        has_dynamic_arguments: false,
        turn_scoped: false,
    };

    let verdict = evaluate_loaded_skill_eligibility(
        &skill_id,
        SkillUsageKind::Reference,
        Some(&hash),
        Some(&hash),
        Some(&branch),
        &[path_only_record],
    );
    assert!(
        verdict.is_eligible(),
        "path-only read with unknown content hash must NEVER suppress reference"
    );

    // Conversely, when direct tool load proves matching content hash, it suppresses
    let verified_record = LoadedSkillRecord {
        skill_id: skill_id.clone(),
        event_id: Some(EventId::new("ev-direct").unwrap()),
        turn_id: Some(TurnId::new("turn-1").unwrap()),
        usage_kind: SkillUsageKind::Reference,
        epoch: ContextEpoch::new("epoch-0").unwrap(),
        source_content: Some(hash.clone()),
        rendered_content: Some(hash.clone()),
        has_dynamic_arguments: false,
        turn_scoped: false,
    };

    let mut active_branch_with_event = branch.clone();
    active_branch_with_event.events.push(NormalizedEvent {
        event_id: Some(EventId::new("ev-direct").unwrap()),
        parent_id: None,
        turn_id: Some(TurnId::new("turn-1").unwrap()),
        agent_id: None,
        branch_id: Some(BranchId::new("main").unwrap()),
        role: Role::Tool,
        kind: EventKind::ToolResult,
        timestamp_unix_ms: Some(1000),
        text: PrivateText::new(""),
        tool: None,
    });

    let verdict_suppressed = evaluate_loaded_skill_eligibility(
        &skill_id,
        SkillUsageKind::Reference,
        Some(&hash),
        Some(&hash),
        Some(&active_branch_with_event),
        &[verified_record],
    );
    assert!(
        verdict_suppressed.is_suppressed(),
        "matching content hash on active branch in current epoch must suppress reference"
    );
}

#[test]
fn test_compaction_invalidates_prior_epoch_suppression() {
    let skill_id = SkillId::new("ref-skill").unwrap();
    let hash = ContentHash::from_bytes(b"content-v1");

    // Active branch is currently in epoch-1 (compaction occurred)
    let branch = ActiveBranch {
        branch_id: Some(BranchId::new("main").unwrap()),
        leaf_event_id: None,
        events: vec![NormalizedEvent {
            event_id: Some(EventId::new("ev-old-load").unwrap()),
            parent_id: None,
            turn_id: Some(TurnId::new("turn-1").unwrap()),
            agent_id: None,
            branch_id: Some(BranchId::new("main").unwrap()),
            role: Role::Tool,
            kind: EventKind::ToolResult,
            timestamp_unix_ms: Some(1000),
            text: PrivateText::new(""),
            tool: None,
        }],
        current_epoch: ContextEpoch::new("epoch-1").unwrap(),
        compaction_count: 1,
        task_boundary_count: 0,
        ancestor_chain_truncated: false,
    };

    // The load occurred in epoch-0
    let pre_compaction_record = LoadedSkillRecord {
        skill_id: skill_id.clone(),
        event_id: Some(EventId::new("ev-old-load").unwrap()),
        turn_id: Some(TurnId::new("turn-1").unwrap()),
        usage_kind: SkillUsageKind::Reference,
        epoch: ContextEpoch::new("epoch-0").unwrap(),
        source_content: Some(hash.clone()),
        rendered_content: Some(hash.clone()),
        has_dynamic_arguments: false,
        turn_scoped: false,
    };

    let verdict = evaluate_loaded_skill_eligibility(
        &skill_id,
        SkillUsageKind::Reference,
        Some(&hash),
        Some(&hash),
        Some(&branch),
        &[pre_compaction_record],
    );
    assert!(
        verdict.is_eligible(),
        "pre-compaction load from prior epoch must not suppress in current epoch"
    );
}

#[test]
fn test_workflows_remain_repeatable() {
    let skill_id = SkillId::new("workflow-deploy").unwrap();
    let hash = ContentHash::from_bytes(b"workflow-script");

    let branch = ActiveBranch {
        branch_id: Some(BranchId::new("main").unwrap()),
        leaf_event_id: None,
        events: vec![NormalizedEvent {
            event_id: Some(EventId::new("ev-wf").unwrap()),
            parent_id: None,
            turn_id: Some(TurnId::new("turn-1").unwrap()),
            agent_id: None,
            branch_id: Some(BranchId::new("main").unwrap()),
            role: Role::Tool,
            kind: EventKind::ToolResult,
            timestamp_unix_ms: Some(1000),
            text: PrivateText::new(""),
            tool: None,
        }],
        current_epoch: ContextEpoch::new("epoch-0").unwrap(),
        compaction_count: 0,
        task_boundary_count: 0,
        ancestor_chain_truncated: false,
    };

    let wf_record = LoadedSkillRecord {
        skill_id: skill_id.clone(),
        event_id: Some(EventId::new("ev-wf").unwrap()),
        turn_id: Some(TurnId::new("turn-1").unwrap()),
        usage_kind: SkillUsageKind::Workflow,
        epoch: ContextEpoch::new("epoch-0").unwrap(),
        source_content: Some(hash.clone()),
        rendered_content: Some(hash.clone()),
        has_dynamic_arguments: false,
        turn_scoped: false,
    };

    let verdict = evaluate_loaded_skill_eligibility(
        &skill_id,
        SkillUsageKind::Workflow,
        Some(&hash),
        Some(&hash),
        Some(&branch),
        &[wf_record],
    );
    assert!(
        verdict.is_eligible(),
        "workflows must remain eligible for re-invocation even when previously loaded"
    );
}

#[test]
fn test_observe_cli_refuses_no_ledger_and_no_persist() {
    let bin = env!("CARGO_BIN_EXE_sr");

    // Rejects --no-ledger with exit code 2
    let out_no_ledger = std::process::Command::new(bin)
        .args(["observe", "--context", "/tmp/dummy.json", "--no-ledger"])
        .output()
        .expect("run sr observe --no-ledger");
    assert_eq!(
        out_no_ledger.status.code(),
        Some(2),
        "--no-ledger must be rejected with exit code 2 (invalid-usage)"
    );

    // Rejects --no-persist with exit code 2
    let out_no_persist = std::process::Command::new(bin)
        .args(["observe", "--context", "/tmp/dummy.json", "--no-persist"])
        .output()
        .expect("run sr observe --no-persist");
    assert_eq!(
        out_no_persist.status.code(),
        Some(2),
        "--no-persist must be rejected with exit code 2 (invalid-usage)"
    );

    // Rejects missing explicit source with exit code 2
    let out_no_source = std::process::Command::new(bin)
        .args(["observe"])
        .output()
        .expect("run sr observe without source");
    assert_eq!(
        out_no_source.status.code(),
        Some(2),
        "missing explicit source must be rejected with exit code 2"
    );
}

#[test]
fn test_observe_cli_requires_ledger_existence() {
    let bin = env!("CARGO_BIN_EXE_sr");
    let dir = temp_private_dir("uninit-ledger");
    let dir_str = dir.to_str().unwrap();

    // Create a dummy context file in a separate directory
    let ctx_dir = temp_private_dir("uninit-ctx");
    let ctx_path = ctx_dir.join("context.json");
    let dummy_ctx = serde_json::json!({
        "schema_version": 1,
        "harness": "claude_code",
        "producer_id": null,
        "workspace_root": "/data/workspaces/project",
        "session_id": "sess-test",
        "agent_id": null,
        "branch_id": null,
        "context_epoch": null,
        "current_request": {
            "event_id": "req-test",
            "text": "do something",
            "attachments_omitted": false,
            "essential_attachment_missing": false
        },
        "events": [],
        "explicit_skill_references": [],
        "supplied_loads": []
    });
    fs::write(&ctx_path, dummy_ctx.to_string()).expect("write dummy ctx");

    // Run sr observe pointing to uninitialized ledger dir -> exit code 9 (storage-failure)
    let out = std::process::Command::new(bin)
        .args([
            "observe",
            "--context",
            ctx_path.to_str().unwrap(),
            "--dir",
            dir_str,
            "--json",
        ])
        .output()
        .expect("run sr observe on uninitialized ledger");

    assert_eq!(
        out.status.code(),
        Some(9),
        "uninitialized ledger must fail with exit code 9 (storage-failure); got stdout: {}, stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn test_observe_cli_e2e_with_context() {
    let bin = env!("CARGO_BIN_EXE_sr");
    let dir = temp_private_dir("e2e-observe");
    let dir_str = dir.to_str().unwrap();

    // Initialize ledger via CLI
    let init_out = std::process::Command::new(bin)
        .args(["ledger", "init", "--dir", dir_str, "--json"])
        .output()
        .expect("run ledger init");
    assert_eq!(init_out.status.code(), Some(0));

    // Create a valid normalized context with a tool result in a separate directory
    let ctx_dir = temp_private_dir("e2e-ctx");
    let ctx_path = ctx_dir.join("context.json");
    let ctx_val = serde_json::json!({
        "schema_version": 1,
        "harness": "claude_code",
        "producer_id": null,
        "workspace_root": "/data/workspaces/project",
        "session_id": "session-cli-e2e",
        "agent_id": null,
        "branch_id": "main",
        "context_epoch": "epoch-0",
        "current_request": {
            "event_id": "req-1",
            "text": "Analyze the codebase",
            "attachments_omitted": false,
            "essential_attachment_missing": false
        },
        "events": [
            {
                "event_id": "req-1",
                "parent_id": null,
                "turn_id": null,
                "agent_id": null,
                "branch_id": "main",
                "role": "user",
                "kind": "message",
                "timestamp_unix_ms": 1700000000000_i64,
                "text": "Analyze the codebase",
                "tool": null
            },
            {
                "event_id": "tool-call-1",
                "parent_id": "req-1",
                "turn_id": "turn-1",
                "agent_id": null,
                "branch_id": "main",
                "role": "tool",
                "kind": "tool_invocation",
                "timestamp_unix_ms": 1700000001000_i64,
                "text": "",
                "tool": {
                    "call_id": "call-1",
                    "name": "view_file",
                    "status": "attempted",
                    "arguments": "{\"path\": \"src/lib.rs\"}",
                    "result": null
                }
            },
            {
                "event_id": "tool-res-1",
                "parent_id": "tool-call-1",
                "turn_id": "turn-1",
                "agent_id": null,
                "branch_id": "main",
                "role": "tool",
                "kind": "tool_result",
                "timestamp_unix_ms": 1700000002000_i64,
                "text": "pub fn hello() {}",
                "tool": {
                    "call_id": "call-1",
                    "name": "view_file",
                    "status": "succeeded",
                    "arguments": null,
                    "result": "pub fn hello() {}"
                }
            }
        ],
        "explicit_skill_references": [],
        "supplied_loads": []
    });
    fs::write(&ctx_path, ctx_val.to_string()).expect("write ctx");

    // Run sr observe --context <ctx> --dir <dir> --json
    let out = std::process::Command::new(bin)
        .args([
            "observe",
            "--context",
            ctx_path.to_str().unwrap(),
            "--dir",
            dir_str,
            "--json",
        ])
        .output()
        .expect("run sr observe");

    assert_eq!(
        out.status.code(),
        Some(0),
        "observe should succeed with exit code 0; got stdout: {}, stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout_str = String::from_utf8_lossy(&out.stdout);
    let out_json: serde_json::Value =
        serde_json::from_str(stdout_str.trim()).expect("output must be valid JSON");
    assert_eq!(out_json.get("status").and_then(|v| v.as_str()), Some("ok"));
    assert_eq!(
        out_json.get("session_id").and_then(|v| v.as_str()),
        Some("session-cli-e2e")
    );
    assert_eq!(
        out_json.get("cursor_generation").and_then(|v| v.as_u64()),
        Some(1)
    );

    // Verify database has the session cursor recorded at generation 1
    let db_path = dir.join(LEDGER_FILE);
    let conn = Connection::open(&db_path).expect("open db");
    let cursor_key = format!(
        "producer-v3:{}",
        serde_json::json!(["claude_code", null, null, "session-cli-e2e", "epoch-0"])
    );
    let (cur_gen, last_ev): (i64, String) = conn
        .query_row(
            "SELECT transcript_generation, last_complete_event_id FROM session_cursors WHERE session_id = ?1",
            [&cursor_key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("query cursor");
    assert_eq!(cur_gen, 1);
    assert_eq!(last_ev, "tool-res-1");

    // Run sr observe a second time with the same context -> advances cursor to generation 2
    let out_2 = std::process::Command::new(bin)
        .args([
            "observe",
            "--context",
            ctx_path.to_str().unwrap(),
            "--dir",
            dir_str,
            "--json",
        ])
        .output()
        .expect("run sr observe 2");

    assert_eq!(out_2.status.code(), Some(0));
    let stdout_2 = String::from_utf8_lossy(&out_2.stdout);
    let out_json_2: serde_json::Value =
        serde_json::from_str(stdout_2.trim()).expect("output 2 must be valid JSON");
    assert_eq!(
        out_json_2.get("cursor_generation").and_then(|v| v.as_u64()),
        Some(2),
        "second observe run advances cursor generation atomically to 2"
    );
}

fn native_observe_journey(split: bool, failed: bool) {
    let bin = env!("CARGO_BIN_EXE_sr");
    let ledger_dir = temp_private_dir("native-obs-ledger");
    let ledger_dir_str = ledger_dir.to_str().unwrap();

    // Initialize ledger via CLI
    let init_out = std::process::Command::new(bin)
        .args(["ledger", "init", "--dir", ledger_dir_str, "--json"])
        .output()
        .expect("run ledger init");
    assert_eq!(init_out.status.code(), Some(0));

    // Create a mock workspace
    let ws_dir = temp_private_dir("native-obs-ws");
    let ws_path_str = ws_dir.to_str().unwrap();

    // Create a skill in the workspace's .claude/skills directory
    let skill_dir = ws_dir.join(".claude").join("skills").join("code-review");
    fs::create_dir_all(&skill_dir).expect("create skill dir");
    let skill_content = "---\nname: code-review\ndescription: Review pull requests and code changes\n---\n# code-review\nInspect diffs.\n";
    fs::write(skill_dir.join("SKILL.md"), skill_content).expect("write SKILL.md");

    // Pre-record an emitted ranking event for this session so we test attribution
    let (inv, cx) = test_invocation();
    let ranking_ev = NewRankingEvent {
        event_id: "rank-ev-1".into(),
        verified_delivery_key: None,
        workspace_root: ws_path_str.into(),
        session_id: "native-sess-1".into(),
        agent_branch: "main".into(),
        mode_channel: "hook".into(),
        policy_version: "v1".into(),
        schema_version: 1,
        decision: DecisionKind::Ranked,
        reason: "eligible".into(),
        exposure_state: ExposureState::Emitted,
        elapsed_ms: 20,
        created_at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64,
        input_tokens: Some(50),
        output_tokens: Some(10),
        snapshot_id: None,
    };
    record_ranking(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(ledger_dir.clone()),
        &ranking_ev,
        &[],
        None,
    )
    .expect("record ranking event");

    // Create skill in workspace roster
    let skill_dir = ws_dir.join(".claude/skills/code-review");
    fs::create_dir_all(&skill_dir).expect("create skill dir");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: code-review\ndescription: Code review skill.\n---\n# code-review\n",
    )
    .expect("write SKILL.md");

    // Create a native Claude transcript
    let transcript_path = ws_dir.join("native-sess-1.jsonl");
    let lines = [
        format!("{{\"cwd\":\"{}\",\"sessionId\":\"native-sess-1\"}}", ws_path_str),
        "{\"type\":\"user\",\"uuid\":\"msg-u1\",\"sessionId\":\"native-sess-1\",\"message\":{\"role\":\"user\",\"content\":\"Please review code\"}}".to_string(),
        "{\"type\":\"assistant\",\"uuid\":\"msg-a1\",\"sessionId\":\"native-sess-1\",\"parentUuid\":\"msg-u1\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"call-cr-1\",\"name\":\"code-review\",\"input\":{\"path\":\"src/lib.rs\"}}]}}".to_string(),
        "{\"type\":\"tool_result\",\"uuid\":\"msg-r1\",\"sessionId\":\"native-sess-1\",\"parentUuid\":\"msg-a1\",\"tool_use_id\":\"call-cr-1\",\"content\":\"Review passed\",\"is_error\":false}".to_string(),
    ];
    let mut original_id = None;
    if split {
        fs::write(&transcript_path, lines[..3].join("\n") + "\n").unwrap();
        let out = std::process::Command::new(bin)
            .current_dir(&ws_dir)
            .args([
                "observe",
                "--transcript",
                transcript_path.to_str().unwrap(),
                "--harness",
                "claude_code",
                "--dir",
                ledger_dir_str,
                "--json",
            ])
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        let conn = Connection::open(ledger_dir.join(LEDGER_FILE)).unwrap();
        let (id, state): (String, String) = conn
            .query_row(
                "SELECT observation_id, evidence_state FROM observations",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "attempted");
        original_id = Some(id);
        // A later emission must not steal attribution when confirmation arrives.
        let mut later = ranking_ev.clone();
        later.event_id = "rank-ev-later".into();
        later.created_at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let (inv, cx) = test_invocation();
        record_ranking(
            &inv,
            &cx,
            LedgerAccess::ExistingOnly,
            LedgerLocation::Directory(ledger_dir.clone()),
            &later,
            &[],
            None,
        )
        .unwrap();
    }
    let mut lines = lines;
    if failed {
        lines[3] = lines[3].replace("false", "true");
    }
    fs::write(&transcript_path, lines.join("\n") + "\n").expect("write transcript");

    // Run sr observe --transcript ... --harness claude_code --dir ... --json
    let out = std::process::Command::new(bin)
        .current_dir(&ws_dir)
        .args([
            "observe",
            "--transcript",
            transcript_path.to_str().unwrap(),
            "--harness",
            "claude_code",
            "--dir",
            ledger_dir_str,
            "--json",
        ])
        .output()
        .expect("run sr observe");

    assert_eq!(
        out.status.code(),
        Some(0),
        "observe should succeed; stdout: {}, stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let out_json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(out_json.get("status").and_then(|v| v.as_str()), Some("ok"));
    assert_eq!(
        out_json.get("session_id").and_then(|v| v.as_str()),
        Some("native-sess-1")
    );
    assert_eq!(
        out_json
            .get("observations_recorded")
            .and_then(|v| v.as_u64()),
        Some(1)
    );
    assert_eq!(
        out_json.get("cursor_generation").and_then(|v| v.as_u64()),
        Some(if split { 2 } else { 1 })
    );

    // Verify DB records
    let db_path = ledger_dir.join(LEDGER_FILE);
    let conn = Connection::open(&db_path).expect("open db");

    // Verify native session cursor is isolated under native namespace
    let (cur_gen, last_ev): (i64, String) = conn
        .query_row(
            "SELECT transcript_generation, last_complete_event_id FROM session_cursors WHERE session_id = 'native:claude_code:native-sess-1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("query native cursor");
    assert_eq!(cur_gen, if split { 2 } else { 1 });
    assert_eq!(last_ev, "msg-r1");

    // Verify observation is recorded and attributed to the preceding ranking event
    let (skill_id, ev_state, attr_id): (String, String, Option<String>) = conn
        .query_row(
            "SELECT skill_id, evidence_state, attributed_event_id FROM observations WHERE session_id = 'native-sess-1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("query observation");
    assert!(skill_id.starts_with("s_"));
    assert_eq!(ev_state, if failed { "attempted" } else { "loaded" });
    if let Some(id) = original_id {
        let stored: String = conn
            .query_row("SELECT observation_id FROM observations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, id);
    }
    assert_eq!(attr_id, Some("rank-ev-1".to_string()));

    // Run sr observe a second time: idempotent, generation advances to 2, no duplicate observation
    let out_2 = std::process::Command::new(bin)
        .current_dir(&ws_dir)
        .args([
            "observe",
            "--transcript",
            transcript_path.to_str().unwrap(),
            "--harness",
            "claude_code",
            "--dir",
            ledger_dir_str,
            "--json",
        ])
        .output()
        .expect("run sr observe second time");

    assert_eq!(out_2.status.code(), Some(0));
    let out_json_2: serde_json::Value = serde_json::from_slice(&out_2.stdout).unwrap();
    assert_eq!(
        out_json_2.get("cursor_generation").and_then(|v| v.as_u64()),
        Some(if split { 3 } else { 2 })
    );

    let obs_count: i64 = conn
        .query_row(
            "SELECT count(*) FROM observations WHERE session_id = 'native-sess-1'",
            [],
            |r| r.get(0),
        )
        .expect("query count");
    assert_eq!(
        obs_count, 1,
        "observation row must not be duplicated on repeated run"
    );
    let stats = std::process::Command::new(bin)
        .current_dir(&ws_dir)
        .args(["stats", "--dir", ledger_dir_str, "--json", "--by-skill"])
        .output()
        .unwrap();
    assert_eq!(
        stats.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&stats.stdout)
    );
    let stats: serde_json::Value = serde_json::from_slice(&stats.stdout).unwrap();
    assert_eq!(
        stats["observations"]["observed_loads"],
        if failed { 0 } else { 1 }
    );
    assert_eq!(
        stats["observations"]["attempted_loads"],
        if failed { 1 } else { 0 }
    );
    if !failed {
        assert!(
            stats["observations"]["suggestion_adoption_rate"]
                .as_f64()
                .unwrap()
                > 0.0
        );
    }
}

#[test]
fn test_observe_cli_native_final_turn_success_and_idempotency() {
    native_observe_journey(false, false);
}

#[test]
fn split_observation_confirmation_preserves_identity_and_attribution() {
    native_observe_journey(true, false);
}

#[test]
fn split_observation_failure_does_not_claim_a_load() {
    native_observe_journey(true, true);
}

#[test]
fn test_observe_cli_cass_and_normalized_same_ids_cannot_move_native_cursor() {
    let bin = env!("CARGO_BIN_EXE_sr");
    let ledger_dir = temp_private_dir("ns-isolation-ledger");
    let ledger_dir_str = ledger_dir.to_str().unwrap();

    let init_out = std::process::Command::new(bin)
        .args(["ledger", "init", "--dir", ledger_dir_str, "--json"])
        .output()
        .expect("run ledger init");
    assert_eq!(init_out.status.code(), Some(0));

    let ws_dir = temp_private_dir("ns-isolation-ws");
    let ws_path_str = ws_dir.to_str().unwrap();

    // 1. Setup native transcript and run observe to advance native cursor to generation 1
    let transcript_path = ws_dir.join("shared-session.jsonl");
    let lines = [
        format!("{{\"cwd\":\"{}\",\"sessionId\":\"shared-session\"}}", ws_path_str),
        "{\"type\":\"user\",\"uuid\":\"msg-u1\",\"sessionId\":\"shared-session\",\"text\":\"Hello\"}".to_string(),
    ];
    fs::write(&transcript_path, lines.join("\n") + "\n").expect("write transcript");

    let native_out = std::process::Command::new(bin)
        .current_dir(&ws_dir)
        .args([
            "observe",
            "--transcript",
            transcript_path.to_str().unwrap(),
            "--harness",
            "claude_code",
            "--dir",
            ledger_dir_str,
            "--json",
        ])
        .output()
        .expect("run native observe");
    assert_eq!(native_out.status.code(), Some(0));

    let db_path = ledger_dir.join(LEDGER_FILE);
    let conn = Connection::open(&db_path).expect("open db");

    let cur_gen_native: i64 = conn
        .query_row(
            "SELECT transcript_generation FROM session_cursors WHERE session_id = 'native:claude_code:shared-session'",
            [],
            |r| r.get(0),
        )
        .expect("query native cursor");
    assert_eq!(cur_gen_native, 1);

    // 2. Cass session source must fail with exit code 3 (missing-session) and cannot move native cursor
    let cass_out = std::process::Command::new(bin)
        .current_dir(&ws_dir)
        .args([
            "observe",
            "--session",
            "/data/cass/shared-session.json",
            "--dir",
            ledger_dir_str,
            "--json",
        ])
        .output()
        .expect("run cass observe");
    assert_eq!(
        cass_out.status.code(),
        Some(3),
        "cass session must fail with exit code 3 (missing-session)"
    );

    let cur_gen_native_after_cass: i64 = conn
        .query_row(
            "SELECT transcript_generation FROM session_cursors WHERE session_id = 'native:claude_code:shared-session'",
            [],
            |r| r.get(0),
        )
        .expect("query native cursor");
    assert_eq!(
        cur_gen_native_after_cass, 1,
        "cass must not move native cursor"
    );

    // 3. Normalized context with the SAME session ID ("shared-session")
    let ctx_path = ws_dir.join("context.json");
    let ctx_val = serde_json::json!({
        "schema_version": 1,
        "harness": "claude_code",
        "producer_id": null,
        "workspace_root": ws_path_str,
        "session_id": "shared-session",
        "agent_id": null,
        "branch_id": "main",
        "context_epoch": "epoch-0",
        "current_request": {
            "event_id": "req-1",
            "text": "Do work",
            "attachments_omitted": false,
            "essential_attachment_missing": false
        },
        "events": [
            {
                "event_id": "req-1",
                "parent_id": null,
                "turn_id": null,
                "agent_id": null,
                "branch_id": "main",
                "role": "user",
                "kind": "message",
                "timestamp_unix_ms": 1700000000000_i64,
                "text": "Do work",
                "tool": null
            }
        ],
        "explicit_skill_references": [],
        "supplied_loads": []
    });
    fs::write(&ctx_path, ctx_val.to_string()).expect("write ctx");

    let norm_out = std::process::Command::new(bin)
        .current_dir(&ws_dir)
        .args([
            "observe",
            "--context",
            ctx_path.to_str().unwrap(),
            "--dir",
            ledger_dir_str,
            "--json",
        ])
        .output()
        .expect("run normalized observe");
    assert_eq!(norm_out.status.code(), Some(0));

    // Even an unqualified normalized import uses its framed epoch-aware key.
    let normalized_key = format!(
        "producer-v3:{}",
        serde_json::json!(["claude_code", null, null, "shared-session", "epoch-0"])
    );
    let cur_gen_norm: i64 = conn
        .query_row(
            "SELECT transcript_generation FROM session_cursors WHERE session_id = ?1",
            [&normalized_key],
            |r| r.get(0),
        )
        .expect("query normalized cursor");
    assert_eq!(cur_gen_norm, 1);

    // Native cursor at "native:claude_code:shared-session" STILL IS AT GENERATION 1!
    let cur_gen_native_after_norm: i64 = conn
        .query_row(
            "SELECT transcript_generation FROM session_cursors WHERE session_id = 'native:claude_code:shared-session'",
            [],
            |r| r.get(0),
        )
        .expect("query native cursor");
    assert_eq!(
        cur_gen_native_after_norm, 1,
        "normalized context with same session ID must not move native cursor"
    );

    // An import can supply a complete native cursor string as its opaque ID.
    // It must still start its own generation, never join the native namespace.
    let mut spoof = ctx_val;
    spoof["session_id"] = serde_json::json!("native:claude_code:shared-session");
    fs::write(&ctx_path, spoof.to_string()).expect("write spoofed cursor ID");
    for expected in [1, 2] {
        let output = std::process::Command::new(bin)
            .current_dir(&ws_dir)
            .args(["observe", "--context"])
            .arg(&ctx_path)
            .args(["--dir", ledger_dir_str, "--json"])
            .output()
            .expect("run normalized reserved-namespace import");
        assert_eq!(output.status.code(), Some(0));
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["cursor_generation"], expected);
        let native_generation: i64 = conn.query_row(
            "SELECT transcript_generation FROM session_cursors WHERE session_id='native:claude_code:shared-session'",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(native_generation, 1);
    }
}

#[test]
fn test_observe_cli_refuses_unsupported_harness_and_missing_session() {
    let bin = env!("CARGO_BIN_EXE_sr");
    let ws_dir = temp_private_dir("err-obs-ws");

    // Unsupported harness rejected with exit code 2
    let dummy_transcript = ws_dir.join("dummy.jsonl");
    fs::write(&dummy_transcript, "{}\n").unwrap();

    let out_bad_harness = std::process::Command::new(bin)
        .current_dir(&ws_dir)
        .args([
            "observe",
            "--transcript",
            dummy_transcript.to_str().unwrap(),
            "--harness",
            "codex_cli",
        ])
        .output()
        .expect("run bad harness");
    assert_eq!(
        out_bad_harness.status.code(),
        Some(2),
        "unsupported harness must be rejected with exit code 2 (invalid-usage)"
    );

    // Missing session ID in normalized context rejected with exit code 3
    let ctx_path = ws_dir.join("no_session_ctx.json");
    let ctx_val = serde_json::json!({
        "schema_version": 1,
        "harness": "claude_code",
        "producer_id": null,
        "workspace_root": ws_dir.to_str().unwrap(),
        "session_id": null,
        "agent_id": null,
        "branch_id": null,
        "context_epoch": null,
        "current_request": {
            "event_id": "req-1",
            "text": "hello",
            "attachments_omitted": false,
            "essential_attachment_missing": false
        },
        "events": [],
        "explicit_skill_references": [],
        "supplied_loads": []
    });
    fs::write(&ctx_path, ctx_val.to_string()).unwrap();

    let out_no_sess = std::process::Command::new(bin)
        .current_dir(&ws_dir)
        .args([
            "observe",
            "--context",
            ctx_path.to_str().unwrap(),
            "--dir",
            ws_dir.to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("run no session");
    assert_eq!(
        out_no_sess.status.code(),
        Some(3),
        "missing durable session identity must be rejected with exit code 3 (missing-session)"
    );
}

#[test]
fn observation_completion_is_identity_bound_monotone_and_cursor_atomic() {
    let dir = temp_private_dir("completion-fences");
    init_test_ledger(&dir);
    let mut cursor = SessionCursor {
        workspace_root: "/data/workspace".into(),
        session_id: "session-complete".into(),
        agent_branch: "main".into(),
        cursor_kind: CursorKind::Observation,
        transcript_generation: 1,
        last_complete_event_id: "call".into(),
        last_offset_bytes: 100,
        updated_at_unix_ms: 1000,
    };
    let original = NewObservation {
        observation_id: "observation".into(),
        source_event_key: "source-call".into(),
        workspace_root: cursor.workspace_root.clone(),
        session_id: cursor.session_id.clone(),
        agent_branch: cursor.agent_branch.clone(),
        attributed_event_id: None,
        skill_id: "skill-alpha".into(),
        evidence_state: EvidenceState::Attempted,
        observed_at_unix_ms: 1000,
    };
    let write = |obs: NewObservation, cursor: &SessionCursor, expected| {
        let (inv, cx) = test_invocation();
        record_observations_with_cursor(
            &inv,
            &cx,
            LedgerAccess::ExistingOnly,
            LedgerLocation::Directory(dir.clone()),
            &[obs],
            cursor,
            Some(expected),
        )
    };
    write(original.clone(), &cursor, 0).unwrap();
    let conn = Connection::open(dir.join(LEDGER_FILE)).unwrap();
    let read = || {
        conn.query_row(
            "SELECT evidence_state, observed_at_unix_ms FROM observations",
            [],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .unwrap()
    };
    let mut confirmed = original.clone();
    confirmed.evidence_state = EvidenceState::Loaded;
    confirmed.observed_at_unix_ms = 2000;
    cursor.transcript_generation = 2;
    assert!(matches!(
        write(confirmed.clone(), &cursor, 9),
        Err(StoreError::RecordConflict)
    ));
    assert_eq!(read(), ("attempted".into(), 1000));
    // A matching source key alone cannot authorize an update across identities.
    for field in 0..6 {
        let mut foreign = confirmed.clone();
        match field {
            0 => foreign.observation_id = "another".into(),
            1 => foreign.workspace_root = "/another".into(),
            2 => foreign.session_id = "another".into(),
            3 => foreign.agent_branch = "another".into(),
            4 => foreign.skill_id = "another".into(),
            _ => foreign.observed_at_unix_ms = 999,
        }
        write(foreign, &cursor, cursor.transcript_generation - 1).unwrap();
        assert_eq!(read(), ("attempted".into(), 1000));
        cursor.transcript_generation += 1;
    }
    write(confirmed.clone(), &cursor, cursor.transcript_generation - 1).unwrap();
    assert_eq!(read(), ("loaded".into(), 2000));
    // Neither a duplicate completion nor newer incomplete input rewrites history.
    for state in [
        EvidenceState::Loaded,
        EvidenceState::Attempted,
        EvidenceState::Censored,
    ] {
        cursor.transcript_generation += 1;
        let mut replayed = confirmed.clone();
        replayed.evidence_state = state;
        replayed.observed_at_unix_ms = 3000;
        write(replayed, &cursor, cursor.transcript_generation - 1).unwrap();
        assert_eq!(read(), ("loaded".into(), 2000));
    }
    let count: i64 = conn
        .query_row("SELECT count(*) FROM observations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

/// A uniqueness clash is a conflict, not corruption (sr-mdng).
///
/// Every unmapped SQLite error used to become `StoreError::Corrupt`, so a constraint
/// violation — an ordinary outcome of a rule this schema states on purpose — reached a person
/// as "cache database could not be read safely", with a hint pointing at their configuration.
/// Nothing was corrupt. This pins the honest mapping, and the planted negative is the old one:
/// with `ConstraintViolation` falling through to the catch-all, this asserts Corrupt instead.
#[test]
fn a_uniqueness_clash_is_reported_as_a_conflict_not_as_corruption() {
    let dir = temp_private_dir("constraint-not-corrupt");
    init_test_ledger(&dir);
    let (inv, cx) = test_invocation();

    // Two observations that share a primary key while differing in the unique key. The store
    // must refuse the second, and must say why in terms a reader can act on.
    let shared_id = "obs-clash";
    let first = NewObservation {
        observation_id: shared_id.into(),
        source_event_key: "native:sess:main:ev-1:skill-a".into(),
        workspace_root: "/data/workspace".into(),
        session_id: "clash-session".into(),
        agent_branch: "main".into(),
        attributed_event_id: None,
        skill_id: "skill-a".into(),
        evidence_state: EvidenceState::Loaded,
        observed_at_unix_ms: 1_000,
    };
    let second = NewObservation {
        source_event_key: "native:sess:other:ev-1:skill-a".into(),
        agent_branch: "other".into(),
        ..first.clone()
    };
    let cursor = SessionCursor {
        workspace_root: "/data/workspace".into(),
        session_id: "clash-session".into(),
        agent_branch: "main".into(),
        cursor_kind: CursorKind::Observation,
        transcript_generation: 1,
        last_complete_event_id: "ev-1".into(),
        last_offset_bytes: 10,
        updated_at_unix_ms: 1_000,
    };

    assert_eq!(
        record_observations_with_cursor(
            &inv,
            &cx,
            LedgerAccess::ExistingOnly,
            LedgerLocation::Directory(dir.clone()),
            &[first],
            &cursor,
            Some(0),
        ),
        Ok(true)
    );
    let refused = record_observations_with_cursor(
        &inv,
        &cx,
        LedgerAccess::ExistingOnly,
        LedgerLocation::Directory(dir.clone()),
        &[second],
        &cursor,
        None,
    );
    assert_eq!(
        refused,
        Err(StoreError::RecordConflict),
        "a row already claims that identity; that is a conflict, and calling it corruption \
         sends a reader looking for damage that is not there"
    );
    assert!(inv.shutdown());
}
