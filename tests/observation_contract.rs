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
    EventKind, LoadState, NormalizedEvent, PrivateText, Role, ToolEvent,
    ToolStatus,
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
use std::path::PathBuf;
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

fn init_test_ledger(dir: &PathBuf) {
    let (inv, cx) = test_invocation();
    init_ledger(
        &inv,
        &cx,
        LedgerLocation::Directory(dir.clone()),
    )
    .expect("init ledger");
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
        &[obs_2.clone()],
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
    assert_eq!(all_obs.len(), 1, "duplicate key was not duplicated in table");
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
    let late_obs = all_obs_2.iter().find(|o| o.observation_id == "obs-late").unwrap();
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

    let obs_failed = extract_load_observations(
        &[failed_event],
        &session_ident,
        &resolver,
        None,
    );
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

    let obs_err_line = extract_load_observations(
        &[error_line_event],
        &session_ident,
        &resolver,
        None,
    );
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

    let obs_succ = extract_load_observations(
        &[success_event],
        &session_ident,
        &resolver,
        None,
    );
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

    let obs = extract_load_observations(
        &[user_msg, unrelated_tool],
        &session_ident,
        &resolver,
        None,
    );
    assert_eq!(obs.len(), 0, "prose mentions and unrelated tool calls never produce observations");
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
                "parent_id": null,
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
    let out_json: serde_json::Value = serde_json::from_str(stdout_str.trim())
        .expect("output must be valid JSON");
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
    let (cur_gen, last_ev): (i64, String) = conn
        .query_row(
            "SELECT transcript_generation, last_complete_event_id FROM session_cursors WHERE session_id = 'session-cli-e2e'",
            [],
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
    let out_json_2: serde_json::Value = serde_json::from_str(stdout_2.trim())
        .expect("output 2 must be valid JSON");
    assert_eq!(
        out_json_2.get("cursor_generation").and_then(|v| v.as_u64()),
        Some(2),
        "second observe run advances cursor generation atomically to 2"
    );
}
