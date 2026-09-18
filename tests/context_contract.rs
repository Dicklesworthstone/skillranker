//! Context and Branch Resolution Contract Tests
//!
//! Verifies boundary `p3_branch_resolution`:
//! - Unit Property Test: `tests/context_contract.rs::branch_and_worktree`
//! - Parent/subagent forks isolation: sibling events are strictly excluded.
//! - Out-of-order timestamps: parent links prevail over timestamp sorting.
//! - Compaction and resumed epochs: epochs advance on compaction; task boundaries tracked.
//! - Compacted reference versus workflow: workflows always eligible; references suppressed
//!   only when proven available in current epoch with matching version.
//! - Unresolved branch withholds session-specific advice.
//! - Worktree identity: distinct canonical worktree IDs for linked worktrees; non-Git and detached HEAD.

use serde_json::json;
use skillranker::adapter::{ClaudeUserPromptSubmit, UnknownFieldPolicy};
use skillranker::context::anchor::{
    AnchorDirectiveKind, AnchorError, AnchorProvenance, AnchorResolution, is_terse_continuation,
    resolve_task_anchor,
};
use skillranker::context::branch::{
    ActiveBranch, BranchAdvice, BranchResolutionTarget, LoadedSkillRecord, SkillUsageKind,
    UnresolvedBranchReason, evaluate_loaded_skill_eligibility, resolve_active_branch,
    resolve_worktree,
};
use skillranker::context::jsonl::{CursorKind, snapshot_jsonl};
use skillranker::context::overlay::{
    ClaudeOverlayRequest, OverlayError, apply_claude_prompt_overlay,
};
use skillranker::context::render::{
    IMAGE_OMISSION_MARKER, MEDIA_OMISSION_MARKER, RenderContextError, RenderContextOptions,
    RenderedLoadedReference, render_context, sanitize_media_data,
};
use skillranker::context::signals::{
    DIRTY_PATH_LIMIT, DirtyPaths, ProjectSignals, parse_dirty_paths,
};
use skillranker::context::{
    ContextError, CurrentRequest, EventKind, LoadState, NormalizedContext, NormalizedEvent,
    PrivateText, Role, SimpleSkillResolver, SkillMatch, ToolEvent, ToolStatus,
    associate_tool_events, extract_load_observations, extract_loaded_skill_records,
    filter_events_for_provider, parse_normalized_context,
};
use skillranker::identity::{
    BranchId, ContentHash, ContextEpoch, EventId, HarnessId, SessionId, SessionIdentity, SkillId,
    SourceProvenance, ToolCallId, TurnId, WorkspaceId,
};
use skillranker::limits::HOOK_STDIN_BYTES;
use skillranker::output::ContextQuality;
use skillranker::privacy::redaction::Redactor;
use skillranker::runtime::ProcessInvocation;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

fn temp_dir(label: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "sr-ctx-test-{}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed),
        label
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.com")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.com")
        .output()
        .expect("run git command");
    assert!(
        output.status.success(),
        "git command failed: {:?} (stderr: {})",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

struct EventBuilder<'a> {
    id: &'a str,
    parent: Option<&'a str>,
    role: Role,
    kind: EventKind,
    text: &'a str,
    ts: Option<i64>,
    branch: Option<&'a str>,
}

impl<'a> EventBuilder<'a> {
    fn new(id: &'a str) -> Self {
        Self {
            id,
            parent: None,
            role: Role::Assistant,
            kind: EventKind::Message,
            text: "",
            ts: None,
            branch: None,
        }
    }

    fn parent(mut self, p: &'a str) -> Self {
        self.parent = Some(p);
        self
    }

    fn role(mut self, r: Role) -> Self {
        self.role = r;
        self
    }

    fn kind(mut self, k: EventKind) -> Self {
        self.kind = k;
        self
    }

    fn text(mut self, t: &'a str) -> Self {
        self.text = t;
        self
    }

    fn ts(mut self, ts: i64) -> Self {
        self.ts = Some(ts);
        self
    }

    fn branch(mut self, b: &'a str) -> Self {
        self.branch = Some(b);
        self
    }

    fn build(self) -> NormalizedEvent {
        NormalizedEvent {
            event_id: Some(EventId::new(self.id).unwrap()),
            parent_id: self.parent.map(|p| EventId::new(p).unwrap()),
            turn_id: Some(TurnId::new(format!("turn-{}", self.id)).unwrap()),
            agent_id: None,
            branch_id: self.branch.map(|b| BranchId::new(b).unwrap()),
            role: self.role,
            kind: self.kind,
            timestamp_unix_ms: self.ts,
            text: PrivateText::new(self.text),
            tool: None,
        }
    }
}

// ==============================================================================
// Unit Property Test: tests/context_contract.rs::branch_and_worktree
// ==============================================================================

#[test]
fn branch_and_worktree() {
    // 1. Sibling Branch Isolation and Parent-Link Lineage
    // Root R forks into Branch A (A1 -> A2) and Branch B (B1 -> B2)
    let root = EventBuilder::new("root")
        .role(Role::User)
        .kind(EventKind::Message)
        .text("start")
        .ts(100)
        .branch("main")
        .build();
    let a1 = EventBuilder::new("a1")
        .parent("root")
        .kind(EventKind::Message)
        .text("a1")
        .ts(110)
        .branch("feat-a")
        .build();
    let a2 = EventBuilder::new("a2")
        .parent("a1")
        .kind(EventKind::Message)
        .text("a2")
        .ts(120)
        .branch("feat-a")
        .build();
    let b1 = EventBuilder::new("b1")
        .parent("root")
        .kind(EventKind::Message)
        .text("b1")
        .ts(115)
        .branch("feat-b")
        .build();
    let b2 = EventBuilder::new("b2")
        .parent("b1")
        .kind(EventKind::Message)
        .text("b2")
        .ts(125)
        .branch("feat-b")
        .build();

    let all_events = vec![root.clone(), a1.clone(), b1.clone(), a2.clone(), b2.clone()];

    // Resolving for target leaf A2
    let target_a = BranchResolutionTarget {
        target_event_id: Some(EventId::new("a2").unwrap()),
        target_branch_id: None,
        target_agent_id: None,
    };
    let res_a = resolve_active_branch(&all_events, &target_a);
    assert!(res_a.is_resolved());
    let active_a = res_a.active_branch().unwrap();
    assert_eq!(active_a.events.len(), 3);
    assert_eq!(
        active_a.events[0].event_id.as_ref().unwrap().as_str(),
        "root"
    );
    assert_eq!(active_a.events[1].event_id.as_ref().unwrap().as_str(), "a1");
    assert_eq!(active_a.events[2].event_id.as_ref().unwrap().as_str(), "a2");
    // Assert sibling branch isolation: no B events on A's active branch
    assert!(!active_a.contains_event(&EventId::new("b1").unwrap()));
    assert!(!active_a.contains_event(&EventId::new("b2").unwrap()));

    // Resolving for target leaf B2
    let target_b = BranchResolutionTarget {
        target_event_id: Some(EventId::new("b2").unwrap()),
        target_branch_id: None,
        target_agent_id: None,
    };
    let res_b = resolve_active_branch(&all_events, &target_b);
    assert!(res_b.is_resolved());
    let active_b = res_b.active_branch().unwrap();
    assert_eq!(active_b.events.len(), 3);
    assert_eq!(
        active_b.events[0].event_id.as_ref().unwrap().as_str(),
        "root"
    );
    assert_eq!(active_b.events[1].event_id.as_ref().unwrap().as_str(), "b1");
    assert_eq!(active_b.events[2].event_id.as_ref().unwrap().as_str(), "b2");
    // Assert sibling branch isolation: no A events on B's active branch
    assert!(!active_b.contains_event(&EventId::new("a1").unwrap()));
    assert!(!active_a.contains_event(&EventId::new("b2").unwrap()));

    // 2. Out-of-Order Timestamps vs Parent-Link Order
    // Events on Branch B have skewed timestamps (B2 earlier than B1 in timestamp),
    // but parent links strictly define lineage.
    let skew_root = EventBuilder::new("s_root")
        .role(Role::User)
        .text("root")
        .ts(500)
        .build();
    let skew_b1 = EventBuilder::new("s_b1")
        .parent("s_root")
        .text("b1")
        .ts(900)
        .build();
    // B2 has timestamp 600, which is LESS than B1 (900)!
    let skew_b2 = EventBuilder::new("s_b2")
        .parent("s_b1")
        .text("b2")
        .ts(600)
        .build();
    let skew_events = vec![skew_b2.clone(), skew_root.clone(), skew_b1.clone()];

    let target_skew = BranchResolutionTarget {
        target_event_id: Some(EventId::new("s_b2").unwrap()),
        target_branch_id: None,
        target_agent_id: None,
    };
    let res_skew = resolve_active_branch(&skew_events, &target_skew);
    assert!(res_skew.is_resolved());
    let active_skew = res_skew.active_branch().unwrap();
    // Lineage must be strictly s_root -> s_b1 -> s_b2, ignoring timestamp ordering!
    assert_eq!(
        active_skew.events[0].event_id.as_ref().unwrap().as_str(),
        "s_root"
    );
    assert_eq!(
        active_skew.events[1].event_id.as_ref().unwrap().as_str(),
        "s_b1"
    );
    assert_eq!(
        active_skew.events[2].event_id.as_ref().unwrap().as_str(),
        "s_b2"
    );

    // 3. Compaction, Resumed Epochs, and Task Boundaries
    let c_root = EventBuilder::new("c_root")
        .role(Role::User)
        .text("prompt")
        .ts(100)
        .build();
    let c_task = EventBuilder::new("c_task")
        .parent("c_root")
        .role(Role::System)
        .kind(EventKind::TaskBoundary)
        .text("tb1")
        .ts(110)
        .build();
    let c_pre = EventBuilder::new("c_pre")
        .parent("c_task")
        .text("pre")
        .ts(120)
        .build();
    let c_compact = EventBuilder::new("c_comp")
        .parent("c_pre")
        .role(Role::System)
        .kind(EventKind::Compaction)
        .text("summary")
        .ts(130)
        .build();
    let c_post = EventBuilder::new("c_post")
        .parent("c_comp")
        .text("post")
        .ts(140)
        .build();
    let c_events = vec![c_root, c_task, c_pre, c_compact, c_post];

    let target_comp = BranchResolutionTarget {
        target_event_id: Some(EventId::new("c_post").unwrap()),
        target_branch_id: None,
        target_agent_id: None,
    };
    let res_comp = resolve_active_branch(&c_events, &target_comp);
    assert!(res_comp.is_resolved());
    let active_comp = res_comp.active_branch().unwrap();
    assert_eq!(active_comp.compaction_count, 1);
    assert_eq!(active_comp.task_boundary_count, 1);
    assert_eq!(active_comp.current_epoch.as_str(), "epoch-1");

    // 4. Compacted Reference versus Workflow Eligibility
    let skill_ref = SkillId::new("cargo-docs").unwrap();
    let skill_wf = SkillId::new("deploy-release").unwrap();
    let hash_v1 = ContentHash::from_bytes(b"v1-content");
    let hash_v2 = ContentHash::from_bytes(b"v2-content");

    // Loaded records: skill_ref loaded in epoch-0, skill_wf loaded in epoch-0
    let loaded_ref_epoch0 = LoadedSkillRecord {
        skill_id: skill_ref.clone(),
        event_id: Some(EventId::new("c_pre").unwrap()),
        turn_id: Some(TurnId::new("turn-c_pre").unwrap()),
        usage_kind: SkillUsageKind::Reference,
        epoch: ContextEpoch::new("epoch-0").unwrap(),
        source_content: Some(hash_v1.clone()),
        rendered_content: None,
        has_dynamic_arguments: false,
        turn_scoped: false,
    };
    let loaded_wf_epoch0 = LoadedSkillRecord {
        skill_id: skill_wf.clone(),
        event_id: Some(EventId::new("c_pre").unwrap()),
        turn_id: Some(TurnId::new("turn-c_pre").unwrap()),
        usage_kind: SkillUsageKind::Workflow,
        epoch: ContextEpoch::new("epoch-0").unwrap(),
        source_content: Some(hash_v1.clone()),
        rendered_content: None,
        has_dynamic_arguments: false,
        turn_scoped: false,
    };

    // Case A: Workflow is ALWAYS eligible, even before compaction
    let pre_branch = ActiveBranch {
        branch_id: None,
        leaf_event_id: Some(EventId::new("c_pre").unwrap()),
        events: vec![],
        current_epoch: ContextEpoch::new("epoch-0").unwrap(),
        compaction_count: 0,
        task_boundary_count: 0,
        ancestor_chain_truncated: false,
    };
    let mut pre_branch_with_events = pre_branch.clone();
    pre_branch_with_events.events = vec![EventBuilder::new("c_pre").text("pre").build()];

    let v_wf = evaluate_loaded_skill_eligibility(
        &skill_wf,
        SkillUsageKind::Workflow,
        Some(&hash_v1),
        None,
        Some(&pre_branch_with_events),
        &[loaded_ref_epoch0.clone(), loaded_wf_epoch0.clone()],
    );
    assert!(
        v_wf.is_eligible(),
        "Workflow must remain eligible for re-invocation"
    );

    // Case B: Reference in current epoch (epoch-0) with matching hash is SUPPRESSED
    let v_ref_pre = evaluate_loaded_skill_eligibility(
        &skill_ref,
        SkillUsageKind::Reference,
        Some(&hash_v1),
        None,
        Some(&pre_branch_with_events),
        &[loaded_ref_epoch0.clone(), loaded_wf_epoch0.clone()],
    );
    assert!(
        v_ref_pre.is_suppressed(),
        "Reference proven present in current epoch must be suppressed"
    );

    // Case C: Reference after compaction (now in epoch-1) is ELIGIBLE
    // active_comp is in epoch-1
    let v_ref_post = evaluate_loaded_skill_eligibility(
        &skill_ref,
        SkillUsageKind::Reference,
        Some(&hash_v1),
        None,
        Some(active_comp),
        &[loaded_ref_epoch0.clone(), loaded_wf_epoch0],
    );
    assert!(
        v_ref_post.is_eligible(),
        "Compacted reference whose presence is not proven in epoch-1 must be eligible"
    );

    // Case D: Reference version mismatch is ELIGIBLE
    let v_ref_v2 = evaluate_loaded_skill_eligibility(
        &skill_ref,
        SkillUsageKind::Reference,
        Some(&hash_v2),
        None,
        Some(&pre_branch_with_events),
        std::slice::from_ref(&loaded_ref_epoch0),
    );
    assert!(
        v_ref_v2.is_eligible(),
        "Reference with new version must be eligible"
    );

    // Case E: Reference loaded on a sibling branch is NOT suppressed on active branch
    let sibling_loaded_ref = LoadedSkillRecord {
        skill_id: skill_ref.clone(),
        event_id: Some(EventId::new("b1").unwrap()), // on branch B
        turn_id: Some(TurnId::new("turn-b1").unwrap()),
        usage_kind: SkillUsageKind::Reference,
        epoch: ContextEpoch::new("epoch-0").unwrap(),
        source_content: Some(hash_v1.clone()),
        rendered_content: None,
        has_dynamic_arguments: false,
        turn_scoped: false,
    };
    let v_ref_sibling = evaluate_loaded_skill_eligibility(
        &skill_ref,
        SkillUsageKind::Reference,
        Some(&hash_v1),
        None,
        Some(active_a), // active branch is A, does not contain b1
        &[sibling_loaded_ref],
    );
    assert!(
        v_ref_sibling.is_eligible(),
        "Skill loaded only on a sibling branch must NOT be suppressed on active branch"
    );

    // 5. Unresolved Ambiguous Sibling Forks Withholds Advice
    // When no target is specified and multiple sibling leaves exist
    let target_unspecified = BranchResolutionTarget::default();
    let res_ambig = resolve_active_branch(&all_events, &target_unspecified);
    assert!(!res_ambig.is_resolved());
    let reason = res_ambig.unresolved_reason().unwrap();
    assert!(matches!(
        reason,
        UnresolvedBranchReason::AmbiguousSiblingForks { .. }
    ));

    let advice: BranchAdvice<&str> = BranchAdvice::Withheld {
        reason: reason.clone(),
    };
    assert!(!advice.is_available());
    assert_eq!(advice.withheld_reason(), Some(reason));

    // When branch is unresolved, loaded-state suppression is withheld
    let v_unresolved = evaluate_loaded_skill_eligibility(
        &skill_ref,
        SkillUsageKind::Reference,
        Some(&hash_v1),
        None,
        None, // unresolved branch
        &[loaded_ref_epoch0],
    );
    assert!(
        v_unresolved.is_eligible(),
        "When active branch is unresolved, session-specific suppression is withheld"
    );

    // 6. Worktree Resolution (Main Git, Linked Worktree, Non-Git, Detached HEAD)
    let temp_root = temp_dir("worktree");
    let main_repo = temp_root.join("main");
    let linked_repo = temp_root.join("linked");
    let non_git = temp_root.join("nongit");
    fs::create_dir_all(&main_repo).unwrap();
    fs::create_dir_all(&non_git).unwrap();

    // Setup main git repo
    git(
        &main_repo,
        &["init", "--quiet", "--template=", "--initial-branch=main"],
    );
    git(
        &main_repo,
        &["commit", "--quiet", "--allow-empty", "-m", "init"],
    );

    // Main repo worktree
    let wt_main = resolve_worktree(&main_repo).unwrap();
    assert!(wt_main.is_git);
    assert!(!wt_main.is_linked_worktree);
    assert_eq!(wt_main.git_branch.as_ref().unwrap().as_str(), "main");
    assert!(!wt_main.is_detached_head);

    // Linked worktree
    git(
        &main_repo,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "feature-linked",
            linked_repo.to_str().unwrap(),
        ],
    );
    let wt_linked = resolve_worktree(&linked_repo).unwrap();
    assert!(wt_linked.is_git);
    assert!(wt_linked.is_linked_worktree);
    assert_eq!(
        wt_linked.git_branch.as_ref().unwrap().as_str(),
        "feature-linked"
    );
    assert!(!wt_linked.is_detached_head);
    // Distinct WorkspaceId despite sharing common Git object directory
    assert_ne!(
        wt_main.workspace_id, wt_linked.workspace_id,
        "Linked worktrees must receive distinct WorkspaceIds"
    );

    // Detached HEAD
    let head_commit = git(&main_repo, &["rev-parse", "HEAD"]);
    git(&main_repo, &["checkout", "--quiet", &head_commit]);
    let wt_detached = resolve_worktree(&main_repo).unwrap();
    assert!(wt_detached.is_git);
    assert!(wt_detached.is_detached_head);
    assert_eq!(wt_detached.git_branch, None);

    // Non-Git Directory
    let wt_nongit = resolve_worktree(&non_git).unwrap();
    assert!(!wt_nongit.is_git);
    assert!(!wt_nongit.is_linked_worktree);
    assert_eq!(wt_nongit.git_branch, None);
    assert!(!wt_nongit.is_detached_head);
    assert_ne!(wt_nongit.workspace_id, wt_main.workspace_id);

    // Cleanup
    let _ = fs::remove_dir_all(&temp_root);
}

// ==============================================================================
// Unit Property Test: tests/context_contract.rs::claude_prompt_overlay
// ==============================================================================

#[test]
fn claude_prompt_overlay() {
    let test_dir = temp_dir("claude-overlay");
    let authorized_root = test_dir.join("authorized");
    fs::create_dir_all(&authorized_root).unwrap();

    let parse_hook = |json: &str| -> ClaudeUserPromptSubmit {
        ClaudeUserPromptSubmit::from_json(json.as_bytes(), UnknownFieldPolicy::RetainAdditive)
            .unwrap()
    };

    let write_lines = |path: &Path, lines: &[&str]| {
        let mut file = File::create(path).unwrap();
        for l in lines {
            writeln!(file, "{l}").unwrap();
        }
        file.sync_all().unwrap();
    };

    // 1. First File Missing -> PromptOnly Context Quality
    let absent_transcript = authorized_root.join("absent_transcript.jsonl");
    let hook_p1 = parse_hook(&format!(
        r#"{{
            "hook_event_name": "UserPromptSubmit",
            "prompt": "initial requirement",
            "prompt_id": "p1",
            "session_id": "sess-alpha",
            "transcript_path": "{}",
            "cwd": "{}"
        }}"#,
        absent_transcript.display(),
        authorized_root.display()
    ));

    let req_p1 = ClaudeOverlayRequest {
        hook_input: hook_p1,
        transcript_path: Some(absent_transcript.clone()),
        authorized_root: Some(authorized_root.clone()),
    };
    let res_p1 = apply_claude_prompt_overlay(&req_p1)
        .expect("absent first file must succeed as prompt_only");
    assert_eq!(res_p1.context_quality, ContextQuality::PromptOnly);
    assert!(res_p1.prompt_overlaid);
    assert!(!res_p1.deduplicated_by_event_id);
    assert_eq!(res_p1.events.len(), 1);
    assert_eq!(res_p1.events[0].event_id.as_ref().unwrap().as_str(), "p1");
    assert_eq!(res_p1.current_request.text.as_str(), "initial requirement");
    assert_eq!(res_p1.session_id.as_ref().unwrap().as_str(), "sess-alpha");

    // 2. Prompt Absent from Existing Transcript -> Appended Authoritatively Once
    let existing_transcript = authorized_root.join("existing_transcript.jsonl");
    write_lines(
        &existing_transcript,
        &[
            r#"{"event_id":"e1","role":"user","kind":"message","text":"turn 1 prompt"}"#,
            r#"{"event_id":"e2","parent_id":"e1","role":"assistant","kind":"message","text":"turn 1 answer"}"#,
        ],
    );

    let hook_p2 = parse_hook(&format!(
        r#"{{
            "hook_event_name": "UserPromptSubmit",
            "prompt": "second request",
            "prompt_id": "p2",
            "session_id": "sess-alpha",
            "transcript_path": "{}",
            "cwd": "{}"
        }}"#,
        existing_transcript.display(),
        authorized_root.display()
    ));
    let req_p2 = ClaudeOverlayRequest {
        hook_input: hook_p2,
        transcript_path: Some(existing_transcript.clone()),
        authorized_root: Some(authorized_root.clone()),
    };
    let res_p2 = apply_claude_prompt_overlay(&req_p2).expect("absent prompt must overlay once");
    assert_eq!(res_p2.context_quality, ContextQuality::Complete);
    assert!(res_p2.prompt_overlaid);
    assert!(!res_p2.deduplicated_by_event_id);
    assert_eq!(res_p2.events.len(), 3);
    assert_eq!(res_p2.events[0].event_id.as_ref().unwrap().as_str(), "e1");
    assert_eq!(res_p2.events[1].event_id.as_ref().unwrap().as_str(), "e2");
    assert_eq!(res_p2.events[2].event_id.as_ref().unwrap().as_str(), "p2");
    assert_eq!(res_p2.events[2].parent_id.as_ref().unwrap().as_str(), "e2");
    assert_eq!(res_p2.current_request.text.as_str(), "second request");

    // 3. Prompt Already Present in Transcript -> Deduplicated by Event ID, Never Duplicated
    let flushed_transcript = authorized_root.join("flushed_transcript.jsonl");
    write_lines(
        &flushed_transcript,
        &[
            r#"{"event_id":"e1","role":"user","kind":"message","text":"turn 1 prompt"}"#,
            r#"{"event_id":"e2","parent_id":"e1","role":"assistant","kind":"message","text":"turn 1 answer"}"#,
            r#"{"event_id":"p2","parent_id":"e2","role":"user","kind":"message","text":"old partial text"}"#,
        ],
    );
    let hook_flushed = parse_hook(&format!(
        r#"{{
            "hook_event_name": "UserPromptSubmit",
            "prompt": "authoritative second request",
            "prompt_id": "p2",
            "session_id": "sess-alpha",
            "transcript_path": "{}",
            "cwd": "{}"
        }}"#,
        flushed_transcript.display(),
        authorized_root.display()
    ));
    let req_flushed = ClaudeOverlayRequest {
        hook_input: hook_flushed,
        transcript_path: Some(flushed_transcript.clone()),
        authorized_root: Some(authorized_root.clone()),
    };
    let res_flushed = apply_claude_prompt_overlay(&req_flushed)
        .expect("present prompt must deduplicate by event ID");
    assert_eq!(res_flushed.context_quality, ContextQuality::Complete);
    assert!(res_flushed.prompt_overlaid);
    assert!(res_flushed.deduplicated_by_event_id);
    // Overlaid in-place at index 2, NOT duplicated to length 4!
    assert_eq!(res_flushed.events.len(), 3);
    assert_eq!(
        res_flushed.events[2].event_id.as_ref().unwrap().as_str(),
        "p2"
    );
    assert_eq!(
        res_flushed.events[2].text.as_str(),
        "authoritative second request"
    );
    assert_eq!(
        res_flushed.current_request.text.as_str(),
        "authoritative second request"
    );

    // 4. Equal Text on Distinct Turns -> NEVER Deduplicated by Text Equality
    let repeat_transcript = authorized_root.join("repeat_transcript.jsonl");
    write_lines(
        &repeat_transcript,
        &[
            r#"{"event_id":"p1","role":"user","kind":"message","text":"run tests"}"#,
            r#"{"event_id":"a1","parent_id":"p1","role":"assistant","kind":"message","text":"tests passed"}"#,
        ],
    );
    // User types the EXACT SAME prompt text again ("run tests") with a new prompt_id ("p3")
    let hook_repeat = parse_hook(&format!(
        r#"{{
            "hook_event_name": "UserPromptSubmit",
            "prompt": "run tests",
            "prompt_id": "p3",
            "session_id": "sess-alpha",
            "transcript_path": "{}",
            "cwd": "{}"
        }}"#,
        repeat_transcript.display(),
        authorized_root.display()
    ));
    let req_repeat = ClaudeOverlayRequest {
        hook_input: hook_repeat,
        transcript_path: Some(repeat_transcript.clone()),
        authorized_root: Some(authorized_root.clone()),
    };
    let res_repeat = apply_claude_prompt_overlay(&req_repeat)
        .expect("repeated prompt text must be distinct turn");
    assert_eq!(res_repeat.events.len(), 3);
    assert_eq!(
        res_repeat.events[0].event_id.as_ref().unwrap().as_str(),
        "p1"
    );
    assert_eq!(res_repeat.events[0].text.as_str(), "run tests");
    assert_eq!(
        res_repeat.events[2].event_id.as_ref().unwrap().as_str(),
        "p3"
    );
    assert_eq!(res_repeat.events[2].text.as_str(), "run tests");
    assert!(!res_repeat.deduplicated_by_event_id);
    assert_eq!(
        res_repeat
            .current_request
            .event_id
            .as_ref()
            .unwrap()
            .as_str(),
        "p3"
    );

    // 5. Malformed Existing File -> Rejected with MalformedTranscript
    let malformed_transcript = authorized_root.join("malformed_transcript.jsonl");
    write_lines(
        &malformed_transcript,
        &[
            r#"{"event_id":"e1","role":"user","kind":"message","text":"valid"}"#,
            r#"{"broken-json-record"#,
        ],
    );
    let hook_malformed = parse_hook(&format!(
        r#"{{
            "hook_event_name": "UserPromptSubmit",
            "prompt": "some prompt",
            "prompt_id": "p_bad",
            "session_id": "sess-alpha",
            "transcript_path": "{}",
            "cwd": "{}"
        }}"#,
        malformed_transcript.display(),
        authorized_root.display()
    ));
    let req_malformed = ClaudeOverlayRequest {
        hook_input: hook_malformed,
        transcript_path: Some(malformed_transcript),
        authorized_root: Some(authorized_root.clone()),
    };
    let err_malformed = apply_claude_prompt_overlay(&req_malformed)
        .expect_err("malformed transcript must be rejected");
    assert!(matches!(
        err_malformed,
        OverlayError::MalformedTranscript(_)
    ));

    // 6. FIFO Rejected
    let fifo_path = authorized_root.join("named_fifo.jsonl");
    nix::unistd::mkfifo(&fifo_path, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
    let hook_fifo = parse_hook(&format!(
        r#"{{
            "hook_event_name": "UserPromptSubmit",
            "prompt": "some prompt",
            "prompt_id": "p_fifo",
            "session_id": "sess-alpha",
            "transcript_path": "{}",
            "cwd": "{}"
        }}"#,
        fifo_path.display(),
        authorized_root.display()
    ));
    let req_fifo = ClaudeOverlayRequest {
        hook_input: hook_fifo,
        transcript_path: Some(fifo_path),
        authorized_root: Some(authorized_root.clone()),
    };
    let err_fifo = apply_claude_prompt_overlay(&req_fifo).expect_err("FIFO must be rejected");
    assert!(matches!(
        err_fifo,
        OverlayError::TranscriptIsDeviceOrFifo(_)
    ));

    // 7. Directory Rejected
    let dir_path = authorized_root.join("transcript_dir");
    fs::create_dir_all(&dir_path).unwrap();
    let hook_dir = parse_hook(&format!(
        r#"{{
            "hook_event_name": "UserPromptSubmit",
            "prompt": "some prompt",
            "prompt_id": "p_dir",
            "session_id": "sess-alpha",
            "transcript_path": "{}",
            "cwd": "{}"
        }}"#,
        dir_path.display(),
        authorized_root.display()
    ));
    let req_dir = ClaudeOverlayRequest {
        hook_input: hook_dir,
        transcript_path: Some(dir_path),
        authorized_root: Some(authorized_root.clone()),
    };
    let err_dir =
        apply_claude_prompt_overlay(&req_dir).expect_err("directory transcript must be rejected");
    assert!(matches!(err_dir, OverlayError::TranscriptIsDirectory(_)));

    // 8. Cross-Session Read Outside Authorized Root Rejected
    let outside_dir = test_dir.join("outside");
    fs::create_dir_all(&outside_dir).unwrap();
    let outside_transcript = outside_dir.join("other_session.jsonl");
    write_lines(
        &outside_transcript,
        &[r#"{"event_id":"x1","role":"user","kind":"message","text":"foreign"}"#],
    );

    let hook_outside = parse_hook(&format!(
        r#"{{
            "hook_event_name": "UserPromptSubmit",
            "prompt": "some prompt",
            "prompt_id": "p_out",
            "session_id": "sess-alpha",
            "transcript_path": "{}",
            "cwd": "{}"
        }}"#,
        outside_transcript.display(),
        authorized_root.display()
    ));
    let req_outside = ClaudeOverlayRequest {
        hook_input: hook_outside,
        transcript_path: Some(outside_transcript),
        authorized_root: Some(authorized_root.clone()),
    };
    let err_outside =
        apply_claude_prompt_overlay(&req_outside).expect_err("outside transcript must be rejected");
    assert!(matches!(
        err_outside,
        OverlayError::CrossSessionReadForbidden
    ));

    // 9. Valid Symlink Inside Authorized Root Allowed; Broken Symlink Rejected
    let symlink_target = authorized_root.join("real_target.jsonl");
    write_lines(
        &symlink_target,
        &[r#"{"event_id":"s1","role":"user","kind":"message","text":"real target"}"#],
    );
    let valid_symlink = authorized_root.join("valid_symlink.jsonl");
    std::os::unix::fs::symlink(&symlink_target, &valid_symlink).unwrap();

    let hook_symlink = parse_hook(&format!(
        r#"{{
            "hook_event_name": "UserPromptSubmit",
            "prompt": "prompt via symlink",
            "prompt_id": "p_sym",
            "session_id": "sess-alpha",
            "transcript_path": "{}",
            "cwd": "{}"
        }}"#,
        valid_symlink.display(),
        authorized_root.display()
    ));
    let req_symlink = ClaudeOverlayRequest {
        hook_input: hook_symlink,
        transcript_path: Some(valid_symlink),
        authorized_root: Some(authorized_root),
    };
    let res_symlink =
        apply_claude_prompt_overlay(&req_symlink).expect("valid symlink inside root must succeed");
    assert_eq!(res_symlink.events.len(), 2);
    assert_eq!(
        res_symlink.events[0].event_id.as_ref().unwrap().as_str(),
        "s1"
    );
    assert_eq!(
        res_symlink.events[1].event_id.as_ref().unwrap().as_str(),
        "p_sym"
    );

    // Cleanup
    let _ = fs::remove_dir_all(&test_dir);
}

#[test]
fn normalized_envelopes() {
    // 1. Valid normalized context parses successfully
    let valid_json = r#"{
        "schema_version": 1,
        "harness": "claude_code",
        "producer_id": "adapter-v1",
        "workspace_root": "/data/workspaces/project",
        "session_id": "sess-normalized-1",
        "agent_id": "agent-root",
        "branch_id": "main",
        "context_epoch": "epoch-0",
        "current_request": {
            "event_id": "req-1",
            "text": "Help me refactor the database queries",
            "attachments_omitted": false,
            "essential_attachment_missing": false
        },
        "events": [
            {
                "event_id": "ev-1",
                "parent_id": null,
                "turn_id": "turn-1",
                "agent_id": "agent-root",
                "branch_id": "main",
                "role": "user",
                "kind": "message",
                "timestamp_unix_ms": 1700000000000,
                "text": "Initial greeting",
                "tool": null
            }
        ],
        "explicit_skill_references": ["skill-rust", "skill-rust"],
        "supplied_loads": [
            {
                "skill_id": "skill-db-helper",
                "source_content": "1111111111111111111111111111111111111111111111111111111111111111",
                "rendered_content": null
            }
        ]
    }"#;

    let parsed = parse_normalized_context(valid_json.as_bytes())
        .expect("valid normalized context must parse");
    assert_eq!(parsed.schema_version, 1);
    assert_eq!(parsed.harness.as_str(), "claude_code");
    assert_eq!(parsed.events.len(), 1);
    assert_eq!(parsed.explicit_skill_references.len(), 2);
    // Duplicate references across stages are valid and not rejected
    assert_eq!(
        parsed.explicit_skill_references[0],
        parsed.explicit_skill_references[1]
    );

    // 2. Session identity retains SourceProvenance::Normalized, preventing native hook hijacking
    let session_id = parsed
        .session_identity(Some(WorkspaceId::new("ws-project").unwrap()))
        .unwrap();
    match session_id.source {
        SourceProvenance::Normalized {
            producer,
            harness,
            schema_version,
        } => {
            assert_eq!(producer.as_ref().map(|p| p.as_str()), Some("adapter-v1"));
            assert_eq!(harness.as_str(), "claude_code");
            assert_eq!(schema_version, 1);
        }
        _ => panic!("normalized session must have SourceProvenance::Normalized"),
    }

    // 3. Schema version != 1 rejected with UnsupportedSchema
    let mut bad_version: serde_json::Value = serde_json::from_str(valid_json).unwrap();
    bad_version["schema_version"] = serde_json::json!(2);
    let bad_ver_bytes = serde_json::to_vec(&bad_version).unwrap();
    assert_eq!(
        parse_normalized_context(&bad_ver_bytes),
        Err(ContextError::UnsupportedSchema)
    );

    // 4. Duplicate JSON object keys rejected with DuplicateKey
    let dup_key_json = r#"{
        "schema_version": 1,
        "schema_version": 1,
        "harness": "claude_code",
        "workspace_root": "/data/workspaces/project",
        "current_request": {
            "text": "hi",
            "attachments_omitted": false,
            "essential_attachment_missing": false
        },
        "events": []
    }"#;
    assert_eq!(
        parse_normalized_context(dup_key_json.as_bytes()),
        Err(ContextError::DuplicateKey)
    );

    // 5. Duplicate event IDs rejected with DuplicateEvent
    let mut dup_events: serde_json::Value = serde_json::from_str(valid_json).unwrap();
    dup_events["events"] = serde_json::json!([
        {
            "event_id": "ev-same",
            "role": "user",
            "kind": "message",
            "text": "first"
        },
        {
            "event_id": "ev-same",
            "role": "assistant",
            "kind": "message",
            "text": "second"
        }
    ]);
    let dup_events_bytes = serde_json::to_vec(&dup_events).unwrap();
    assert_eq!(
        parse_normalized_context(&dup_events_bytes),
        Err(ContextError::DuplicateEvent)
    );

    // 6. Duplicate load definitions rejected with DuplicateLoadDefinition
    let mut dup_loads: serde_json::Value = serde_json::from_str(valid_json).unwrap();
    dup_loads["supplied_loads"] = serde_json::json!([
        {
            "skill_id": "skill-duplicate",
            "source_content": null,
            "rendered_content": null
        },
        {
            "skill_id": "skill-duplicate",
            "source_content": null,
            "rendered_content": null
        }
    ]);
    let dup_loads_bytes = serde_json::to_vec(&dup_loads).unwrap();
    assert_eq!(
        parse_normalized_context(&dup_loads_bytes),
        Err(ContextError::DuplicateLoadDefinition)
    );

    // 7. Byte limit (1 MiB) rejected with LimitExceeded
    let huge_bytes = vec![b' '; 1024 * 1024 + 1];
    assert_eq!(
        parse_normalized_context(&huge_bytes),
        Err(ContextError::LimitExceeded)
    );

    // 8. Nesting depth limit (> 64) rejected with LimitExceeded
    let mut deep_json = String::new();
    for _ in 0..65 {
        deep_json.push_str("{\"nested\":");
    }
    deep_json.push('1');
    for _ in 0..65 {
        deep_json.push('}');
    }
    assert_eq!(
        parse_normalized_context(deep_json.as_bytes()),
        Err(ContextError::LimitExceeded)
    );

    // 9. Malicious path / workspace_root confers zero authority and redacts in debug
    let malicious_json = r#"{
        "schema_version": 1,
        "harness": "claude_code",
        "workspace_root": "/etc/shadow",
        "current_request": {
            "text": "secret api key sk-test-1234567890abcdef",
            "attachments_omitted": false,
            "essential_attachment_missing": false
        },
        "events": []
    }"#;
    let mal_parsed = parse_normalized_context(malicious_json.as_bytes()).unwrap();
    let debug_repr = format!("{:?}", mal_parsed.current_request.text);
    assert!(
        !debug_repr.contains("sk-test"),
        "PrivateText must redact contents in Debug"
    );
    assert!(
        debug_repr.contains("PrivateText(<"),
        "PrivateText Debug must show length only"
    );
}

#[test]
fn tool_associations() {
    let base_session = SessionIdentity {
        source: SourceProvenance::Normalized {
            producer: None,
            harness: HarnessId::new("claude_code").unwrap(),
            schema_version: 1,
        },
        workspace: Some(WorkspaceId::new("ws-test").unwrap()),
        session: Some(SessionId::new("sess-1").unwrap()),
        agent: None,
        branch: Some(BranchId::new("main").unwrap()),
        epoch: Some(ContextEpoch::new("epoch-0").unwrap()),
    };

    let mut resolver = SimpleSkillResolver::new();
    let skill_cargo = SkillId::new("skill-cargo-test").unwrap();
    let skill_deploy = SkillId::new("skill-deploy").unwrap();
    let skill_refactor = SkillId::new("skill-refactor").unwrap();
    let skill_forked = SkillId::new("skill-git-rebase").unwrap();
    let skill_path_only = SkillId::new("skill-file-read").unwrap();

    let known_digest =
        ContentHash::parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .unwrap();

    resolver.register_tool(
        "cargo_test",
        SkillMatch {
            skill_id: skill_cargo.clone(),
            usage_kind: SkillUsageKind::Reference,
            source_content: Some(known_digest.clone()),
            rendered_content: Some(known_digest.clone()),
            has_dynamic_arguments: false,
            turn_scoped: false,
        },
    );
    resolver.register_tool(
        "deploy_tool",
        SkillMatch {
            skill_id: skill_deploy.clone(),
            usage_kind: SkillUsageKind::Workflow,
            source_content: None,
            rendered_content: None,
            has_dynamic_arguments: true,
            turn_scoped: false,
        },
    );
    resolver.register_tool(
        "refactor_tool",
        SkillMatch {
            skill_id: skill_refactor.clone(),
            usage_kind: SkillUsageKind::Workflow,
            source_content: None,
            rendered_content: None,
            has_dynamic_arguments: false,
            turn_scoped: false,
        },
    );
    resolver.register_tool(
        "fork_tool",
        SkillMatch {
            skill_id: skill_forked.clone(),
            usage_kind: SkillUsageKind::Workflow,
            source_content: None,
            rendered_content: None,
            has_dynamic_arguments: false,
            turn_scoped: false,
        },
    );

    let test_dir = temp_dir("tool-assoc");
    let skill_file_path = test_dir.join("SKILL.md");
    fs::write(&skill_file_path, "skill documentation content on disk").unwrap();
    let path_str = skill_file_path.to_str().unwrap().to_string();

    resolver.register_path(
        &path_str,
        SkillMatch {
            skill_id: skill_path_only.clone(),
            usage_kind: SkillUsageKind::Reference,
            source_content: None, // Path-only: unknown version
            rendered_content: None,
            has_dynamic_arguments: false,
            turn_scoped: false,
        },
    );

    // Build event stream
    let branch_main = BranchId::new("main").unwrap();
    let branch_fork = BranchId::new("fork-feature").unwrap();

    let events = vec![
        // 1. Matched tool event (Success)
        NormalizedEvent {
            event_id: Some(EventId::new("ev_call_1").unwrap()),
            parent_id: None,
            turn_id: Some(TurnId::new("turn_1").unwrap()),
            agent_id: None,
            branch_id: Some(branch_main.clone()),
            role: Role::Assistant,
            kind: EventKind::ToolInvocation,
            timestamp_unix_ms: Some(100),
            text: PrivateText::new("Running cargo test"),
            tool: Some(ToolEvent {
                call_id: Some(ToolCallId::new("call_match").unwrap()),
                name: PrivateText::new("cargo_test"),
                status: ToolStatus::Attempted,
                arguments: Some(PrivateText::new(
                    r#"{"command": "test", "secret_key": "sk-hidden"}"#,
                )),
                result: None,
            }),
        },
        NormalizedEvent {
            event_id: Some(EventId::new("ev_res_1").unwrap()),
            parent_id: Some(EventId::new("ev_call_1").unwrap()),
            turn_id: Some(TurnId::new("turn_1").unwrap()),
            agent_id: None,
            branch_id: Some(branch_main.clone()),
            role: Role::Tool,
            kind: EventKind::ToolResult,
            timestamp_unix_ms: Some(105),
            text: PrivateText::new("cargo test: ok"),
            tool: Some(ToolEvent {
                call_id: Some(ToolCallId::new("call_match").unwrap()),
                name: PrivateText::new("cargo_test"),
                status: ToolStatus::Succeeded,
                arguments: None,
                result: Some(PrivateText::new("test result: ok. 42 passed; 0 failed")),
            }),
        },
        // 2. Failed tool event
        NormalizedEvent {
            event_id: Some(EventId::new("ev_call_2").unwrap()),
            parent_id: Some(EventId::new("ev_res_1").unwrap()),
            turn_id: Some(TurnId::new("turn_2").unwrap()),
            agent_id: None,
            branch_id: Some(branch_main.clone()),
            role: Role::Assistant,
            kind: EventKind::ToolInvocation,
            timestamp_unix_ms: Some(200),
            text: PrivateText::new("Deploying to staging"),
            tool: Some(ToolEvent {
                call_id: Some(ToolCallId::new("call_fail").unwrap()),
                name: PrivateText::new("deploy_tool"),
                status: ToolStatus::Attempted,
                arguments: Some(PrivateText::new(r#"{"target": "staging"}"#)),
                result: None,
            }),
        },
        NormalizedEvent {
            event_id: Some(EventId::new("ev_res_2").unwrap()),
            parent_id: Some(EventId::new("ev_call_2").unwrap()),
            turn_id: Some(TurnId::new("turn_2").unwrap()),
            agent_id: None,
            branch_id: Some(branch_main.clone()),
            role: Role::Tool,
            kind: EventKind::ToolResult,
            timestamp_unix_ms: Some(205),
            text: PrivateText::new("deploy failed"),
            tool: Some(ToolEvent {
                call_id: Some(ToolCallId::new("call_fail").unwrap()),
                name: PrivateText::new("deploy_tool"),
                status: ToolStatus::Failed,
                arguments: None,
                result: Some(PrivateText::new(
                    "fatal: connection refused\nerror: deploy aborted",
                )),
            }),
        },
        // 3. Missing-result tool event
        NormalizedEvent {
            event_id: Some(EventId::new("ev_call_3").unwrap()),
            parent_id: Some(EventId::new("ev_res_2").unwrap()),
            turn_id: Some(TurnId::new("turn_3").unwrap()),
            agent_id: None,
            branch_id: Some(branch_main.clone()),
            role: Role::Assistant,
            kind: EventKind::ToolInvocation,
            timestamp_unix_ms: Some(300),
            text: PrivateText::new("Starting refactor tool"),
            tool: Some(ToolEvent {
                call_id: Some(ToolCallId::new("call_missing").unwrap()),
                name: PrivateText::new("refactor_tool"),
                status: ToolStatus::Attempted,
                arguments: Some(PrivateText::new(r#"{"query": "ast_walk"}"#)),
                result: None,
            }),
        },
        // 4. Forked tool event on sibling branch
        NormalizedEvent {
            event_id: Some(EventId::new("ev_call_fork").unwrap()),
            parent_id: None,
            turn_id: Some(TurnId::new("turn_f").unwrap()),
            agent_id: None,
            branch_id: Some(branch_fork.clone()),
            role: Role::Assistant,
            kind: EventKind::ToolInvocation,
            timestamp_unix_ms: Some(400),
            text: PrivateText::new("Forked branch rebase"),
            tool: Some(ToolEvent {
                call_id: Some(ToolCallId::new("call_fork").unwrap()),
                name: PrivateText::new("fork_tool"),
                status: ToolStatus::Succeeded,
                arguments: Some(PrivateText::new("{}")),
                result: Some(PrivateText::new("fork succeeded")),
            }),
        },
        // 5. Path-only file read success
        NormalizedEvent {
            event_id: Some(EventId::new("ev_call_path").unwrap()),
            parent_id: Some(EventId::new("ev_call_3").unwrap()),
            turn_id: Some(TurnId::new("turn_4").unwrap()),
            agent_id: None,
            branch_id: Some(branch_main.clone()),
            role: Role::Assistant,
            kind: EventKind::ToolInvocation,
            timestamp_unix_ms: Some(500),
            text: PrivateText::new("Reading skill file"),
            tool: Some(ToolEvent {
                call_id: Some(ToolCallId::new("call_path").unwrap()),
                name: PrivateText::new("read_file"),
                status: ToolStatus::Succeeded,
                arguments: Some(PrivateText::new(format!(r#"{{"path": "{}"}}"#, path_str))),
                result: Some(PrivateText::new("file contents loaded")),
            }),
        },
        // 6. Duplicate delivery of ev_call_1 (should be deduplicated)
        NormalizedEvent {
            event_id: Some(EventId::new("ev_call_1").unwrap()),
            parent_id: None,
            turn_id: Some(TurnId::new("turn_1").unwrap()),
            agent_id: None,
            branch_id: Some(branch_main.clone()),
            role: Role::Assistant,
            kind: EventKind::ToolInvocation,
            timestamp_unix_ms: Some(100),
            text: PrivateText::new("Duplicate delivery of cargo test"),
            tool: Some(ToolEvent {
                call_id: Some(ToolCallId::new("call_match").unwrap()),
                name: PrivateText::new("cargo_test"),
                status: ToolStatus::Attempted,
                arguments: Some(PrivateText::new(r#"{"command": "test"}"#)),
                result: None,
            }),
        },
    ];

    // Setup active branch for branch_main
    let active_branch = ActiveBranch {
        branch_id: Some(branch_main.clone()),
        leaf_event_id: Some(EventId::new("ev_call_path").unwrap()),
        events: events
            .iter()
            .filter(|e| e.branch_id.as_ref() == Some(&branch_main))
            .cloned()
            .collect(),
        current_epoch: ContextEpoch::new("epoch-0").unwrap(),
        compaction_count: 0,
        task_boundary_count: 0,
        ancestor_chain_truncated: false,
    };

    // Test 1: Associate tool events
    let associated = associate_tool_events(&events, 200);
    assert_eq!(associated.len(), 5); // 5 unique tool calls (duplicate ev_call_1 ignored)

    // Check matched call
    let matched_call = associated
        .iter()
        .find(|c| c.call_id.as_ref().map(|id| id.as_str()) == Some("call_match"))
        .unwrap();
    assert_eq!(matched_call.status, ToolStatus::Succeeded);
    assert_eq!(
        matched_call.invocation_event_id.as_ref().unwrap().as_str(),
        "ev_call_1"
    );
    assert_eq!(
        matched_call.result_event_id.as_ref().unwrap().as_str(),
        "ev_res_1"
    );
    assert!(
        matched_call
            .arguments_summary
            .as_ref()
            .unwrap()
            .as_str()
            .contains(r#""secret_key":"<omitted>""#)
    );

    // Check failed call and error lines
    let failed_call = associated
        .iter()
        .find(|c| c.call_id.as_ref().map(|id| id.as_str()) == Some("call_fail"))
        .unwrap();
    assert_eq!(failed_call.status, ToolStatus::Failed);
    assert!(!failed_call.error_lines.is_empty());
    assert!(
        failed_call
            .error_lines
            .iter()
            .any(|l| l.contains("fatal: connection refused"))
    );

    // Check missing-result call
    let missing_call = associated
        .iter()
        .find(|c| c.call_id.as_ref().map(|id| id.as_str()) == Some("call_missing"))
        .unwrap();
    assert_eq!(missing_call.status, ToolStatus::Attempted);
    assert!(missing_call.result_event_id.is_none());

    // Test 2: Extract load observations with active branch isolation
    let observations =
        extract_load_observations(&events, &base_session, &resolver, Some(&active_branch));

    // Must NOT contain the forked branch load
    assert!(
        !observations.iter().any(|o| o.skill_id == skill_forked),
        "Sibling fork skill must NOT appear in active branch observations"
    );

    // Check cargo test -> ObservedLoaded with known content digest
    let cargo_obs = observations
        .iter()
        .find(|o| o.skill_id == skill_cargo)
        .unwrap();
    assert_eq!(cargo_obs.state, LoadState::ObservedLoaded);
    assert_eq!(cargo_obs.source_content.as_ref(), Some(&known_digest));

    // Check deploy tool -> Attempted (due to failure)
    let deploy_obs = observations
        .iter()
        .find(|o| o.skill_id == skill_deploy)
        .unwrap();
    assert_eq!(deploy_obs.state, LoadState::Attempted);

    // Check refactor tool -> Attempted (due to missing result)
    let refactor_obs = observations
        .iter()
        .find(|o| o.skill_id == skill_refactor)
        .unwrap();
    assert_eq!(refactor_obs.state, LoadState::Attempted);

    // Check path-only read -> ObservedLoaded with None version
    let path_obs = observations
        .iter()
        .find(|o| o.skill_id == skill_path_only)
        .unwrap();
    assert_eq!(path_obs.state, LoadState::ObservedLoaded);
    assert!(
        path_obs.source_content.is_none(),
        "Path-only read must have None source_content"
    );
    assert!(
        path_obs.rendered_content.is_none(),
        "Path-only read must have None rendered_content"
    );

    // Invariant check: ensure file on disk was NOT hashed to fabricate historical version
    let disk_content = fs::read(&skill_file_path).unwrap();
    let disk_hash = ContentHash::from_bytes(&disk_content);
    assert_ne!(
        path_obs.source_content.as_ref().map(|h| h.as_str()),
        Some(disk_hash.as_str()),
        "Current file on disk must NEVER become historical version"
    );

    // Test 3: Extract loaded skill records
    let current_epoch = ContextEpoch::new("epoch-0").unwrap();
    let records =
        extract_loaded_skill_records(&events, &resolver, Some(&active_branch), &current_epoch);

    // Only successful calls without error lines qualify as LoadedSkillRecord
    assert!(records.iter().any(|r| r.skill_id == skill_cargo));
    assert!(records.iter().any(|r| r.skill_id == skill_path_only));
    assert!(
        !records.iter().any(|r| r.skill_id == skill_deploy),
        "Failed tool must NOT create LoadedSkillRecord"
    );
    assert!(
        !records.iter().any(|r| r.skill_id == skill_refactor),
        "Incomplete tool must NOT create LoadedSkillRecord"
    );
    assert!(
        !records.iter().any(|r| r.skill_id == skill_forked),
        "Sibling fork tool must NOT create LoadedSkillRecord"
    );

    // Test 4: Provider filtering with --no-tools
    let provider_events = filter_events_for_provider(&events, true, 200);
    for pe in &provider_events {
        if let Some(tool) = &pe.tool {
            assert!(
                tool.arguments.is_none(),
                "no_tools must strip tool arguments"
            );
            assert!(tool.result.is_none(), "no_tools must strip tool result");
        }
        if pe.role == Role::Tool {
            assert!(
                pe.text.as_str().is_empty(),
                "no_tools must strip Tool role text"
            );
        }
    }

    // Crucial invariant: local observations can be extracted from raw events even when no_tools is used
    assert_eq!(
        observations.len(),
        4,
        "Local observations must retain 4 skill observations"
    );

    let _ = fs::remove_dir_all(&test_dir);
}

// ==============================================================================
// Boundary p3_request_context: Render bounded request-first context and provenance
// ==============================================================================

#[test]
fn bounded_request_rendering() {
    // -------------------------------------------------------------------------
    // Sub-case 1: Request-first ordering and deduplication
    // -------------------------------------------------------------------------
    let req_event_id = EventId::new("req-event-01").unwrap();
    let old_event_id = EventId::new("old-event-01").unwrap();

    let context = NormalizedContext {
        schema_version: 1,
        harness: HarnessId::new("claude_code").unwrap(),
        producer_id: None,
        workspace_root: PrivateText::new("/workspaces/my-project"),
        session_id: Some(SessionId::new("session-01").unwrap()),
        agent_id: None,
        branch_id: None,
        context_epoch: None,
        current_request: CurrentRequest {
            event_id: Some(req_event_id.clone()),
            text: PrivateText::new("Please optimize the serialization loop."),
            attachments_omitted: false,
            essential_attachment_missing: false,
        },
        events: vec![
            NormalizedEvent {
                event_id: Some(old_event_id.clone()),
                parent_id: None,
                turn_id: None,
                agent_id: None,
                branch_id: None,
                role: Role::User,
                kind: EventKind::Message,
                timestamp_unix_ms: Some(100),
                text: PrivateText::new("Initial discussion about performance."),
                tool: None,
            },
            // Duplicate event that has the same ID as current_request
            NormalizedEvent {
                event_id: Some(req_event_id.clone()),
                parent_id: Some(old_event_id.clone()),
                turn_id: None,
                agent_id: None,
                branch_id: None,
                role: Role::User,
                kind: EventKind::Message,
                timestamp_unix_ms: Some(200),
                text: PrivateText::new("Please optimize the serialization loop."),
                tool: None,
            },
        ],
        explicit_skill_references: vec![],
        supplied_loads: vec![],
    };

    let options = RenderContextOptions::default();
    let payload = render_context(&context, &options).expect("render context should succeed");

    assert_eq!(
        payload.latest_user_request,
        "Please optimize the serialization loop."
    );
    // Crucial: The duplicate event matching current_request.event_id must NOT be in recent_messages
    assert_eq!(payload.recent_messages.len(), 1);
    assert_eq!(payload.recent_messages[0].role, "user");
    assert_eq!(
        payload.recent_messages[0].text.as_deref(),
        Some("Initial discussion about performance.")
    );
    assert_eq!(payload.context_quality, ContextQuality::Complete);

    // -------------------------------------------------------------------------
    // Sub-case 2: Unicode boundaries, emoji, and deterministic head/tail long-request truncation
    // -------------------------------------------------------------------------
    // Build a long request with complex multi-byte characters and emoji
    let repeated_phrase = "🦀 Rust is fast! こんにちは世界 🚀 ";
    let mut long_text = String::new();
    while long_text.chars().count() < 15_000 {
        long_text.push_str(repeated_phrase);
    }

    let long_context = NormalizedContext {
        schema_version: 1,
        harness: HarnessId::new("claude_code").unwrap(),
        producer_id: None,
        workspace_root: PrivateText::new("/workspaces/my-project"),
        session_id: Some(SessionId::new("session-02").unwrap()),
        agent_id: None,
        branch_id: None,
        context_epoch: None,
        current_request: CurrentRequest {
            event_id: Some(EventId::new("req-long").unwrap()),
            text: PrivateText::new(&long_text),
            attachments_omitted: false,
            essential_attachment_missing: false,
        },
        events: vec![],
        explicit_skill_references: vec![],
        supplied_loads: vec![],
    };

    let options_budget = RenderContextOptions {
        max_total_scalars: 12_000,
        ..RenderContextOptions::default()
    };
    let payload_long =
        render_context(&long_context, &options_budget).expect("render long request should succeed");

    // Assertion ID: rendered_within_budget
    assert!(
        payload_long.latest_user_request.chars().count() <= 12_000,
        "rendered_within_budget: latest request must fit 12,000 scalars"
    );
    assert_eq!(
        payload_long.context_quality,
        ContextQuality::Partial,
        "Truncated request must yield Partial context quality"
    );
    assert!(
        payload_long.latest_user_request.contains("... [")
            && payload_long
                .latest_user_request
                .contains("chars omitted] ..."),
        "Truncated request must contain head/tail omission marker"
    );
    // Verify multi-byte UTF-8 validity (no panic, valid string)
    assert!(payload_long.latest_user_request.starts_with("🦀 Rust"));

    // -------------------------------------------------------------------------
    // Sub-case 3: Reasoning / thinking block removal
    // -------------------------------------------------------------------------
    let thinking_context = NormalizedContext {
        schema_version: 1,
        harness: HarnessId::new("claude_code").unwrap(),
        producer_id: None,
        workspace_root: PrivateText::new("/workspaces/my-project"),
        session_id: Some(SessionId::new("session-03").unwrap()),
        agent_id: None,
        branch_id: None,
        context_epoch: None,
        current_request: CurrentRequest {
            event_id: Some(EventId::new("req-thinking").unwrap()),
            text: PrivateText::new("Next step?"),
            attachments_omitted: false,
            essential_attachment_missing: false,
        },
        events: vec![
            NormalizedEvent {
                event_id: Some(EventId::new("asst-with-thinking").unwrap()),
                parent_id: None,
                turn_id: None,
                agent_id: None,
                branch_id: None,
                role: Role::Assistant,
                kind: EventKind::Message,
                timestamp_unix_ms: Some(10),
                text: PrivateText::new(
                    "<thinking>\nInternal scratchpad: checking tests\n</thinking>\nI will run cargo test.",
                ),
                tool: None,
            },
            // Message that contains ONLY thinking block: must be dropped from recent_messages
            NormalizedEvent {
                event_id: Some(EventId::new("asst-only-thinking").unwrap()),
                parent_id: None,
                turn_id: None,
                agent_id: None,
                branch_id: None,
                role: Role::Assistant,
                kind: EventKind::Message,
                timestamp_unix_ms: Some(20),
                text: PrivateText::new("<thought>Silent internal deliberation only</thought>"),
                tool: None,
            },
        ],
        explicit_skill_references: vec![],
        supplied_loads: vec![],
    };

    let payload_thinking =
        render_context(&thinking_context, &options).expect("render should succeed");
    assert_eq!(payload_thinking.recent_messages.len(), 1);
    assert_eq!(
        payload_thinking.recent_messages[0].text.as_deref(),
        Some("I will run cargo test.")
    );

    // -------------------------------------------------------------------------
    // Sub-case 4: Binary and media data sanitization
    // -------------------------------------------------------------------------
    let media_raw = "Here is the error screenshot: data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg== and binary data:application/octet-stream;base64,AQIDBA==";
    let sanitized_media = sanitize_media_data(media_raw);
    assert!(sanitized_media.contains(IMAGE_OMISSION_MARKER));
    assert!(sanitized_media.contains(MEDIA_OMISSION_MARKER));
    assert!(!sanitized_media.contains("iVBORw0KGgo"));

    // -------------------------------------------------------------------------
    // Sub-case 5: Essential attachment missing semantics
    // -------------------------------------------------------------------------
    let missing_att_context = NormalizedContext {
        schema_version: 1,
        harness: HarnessId::new("claude_code").unwrap(),
        producer_id: None,
        workspace_root: PrivateText::new("/workspaces/my-project"),
        session_id: Some(SessionId::new("session-05").unwrap()),
        agent_id: None,
        branch_id: None,
        context_epoch: None,
        current_request: CurrentRequest {
            event_id: Some(EventId::new("req-att").unwrap()),
            text: PrivateText::new("Analyze the attached image [image omitted]"),
            attachments_omitted: true,
            essential_attachment_missing: true,
        },
        events: vec![],
        explicit_skill_references: vec![],
        supplied_loads: vec![],
    };

    let payload_att = render_context(&missing_att_context, &options)
        .expect("render without fail_on_unsupported_context should produce Insufficient payload");
    assert_eq!(payload_att.context_quality, ContextQuality::Insufficient);
    assert!(payload_att.is_unsupported_context());

    let strict_att_options = RenderContextOptions {
        fail_on_unsupported_context: true,
        ..RenderContextOptions::default()
    };
    let err_att = render_context(&missing_att_context, &strict_att_options)
        .expect_err("strict mode must fail on essential attachment missing");
    assert!(matches!(err_att, RenderContextError::UnsupportedContext(_)));

    // -------------------------------------------------------------------------
    // Sub-case 6: Provenance-aware prior advisory stripping vs user quote preservation
    // -------------------------------------------------------------------------
    let advisory_context = NormalizedContext {
        schema_version: 1,
        harness: HarnessId::new("claude_code").unwrap(),
        producer_id: None,
        workspace_root: PrivateText::new("/workspaces/my-project"),
        session_id: Some(SessionId::new("session-06").unwrap()),
        agent_id: None,
        branch_id: None,
        context_epoch: None,
        current_request: CurrentRequest {
            event_id: Some(EventId::new("req-adv").unwrap()),
            text: PrivateText::new("Proceed with the task."),
            attachments_omitted: false,
            essential_attachment_missing: false,
        },
        events: vec![
            // Assistant message containing prior sr advisory block + assistant response
            NormalizedEvent {
                event_id: Some(EventId::new("asst-adv").unwrap()),
                parent_id: None,
                turn_id: None,
                agent_id: None,
                branch_id: None,
                role: Role::Assistant,
                kind: EventKind::Message,
                timestamp_unix_ms: Some(10),
                text: PrivateText::new(
                    "Suggested skill for the next step: rust-cargo-basics. Use it only if it fits the user's request and current instructions.\n\nUnderstood. I will inspect Cargo.toml.",
                ),
                tool: None,
            },
            // User message quoting the advisory marker: MUST BE PRESERVED!
            NormalizedEvent {
                event_id: Some(EventId::new("user-quote").unwrap()),
                parent_id: None,
                turn_id: None,
                agent_id: None,
                branch_id: None,
                role: Role::User,
                kind: EventKind::Message,
                timestamp_unix_ms: Some(20),
                text: PrivateText::new(
                    "Why did you suggest \"Suggested skill for the next step: rust-cargo-basics\"?",
                ),
                tool: None,
            },
        ],
        explicit_skill_references: vec![],
        supplied_loads: vec![],
    };

    let payload_adv = render_context(&advisory_context, &options).expect("render should succeed");
    assert_eq!(payload_adv.recent_messages.len(), 2);
    // Assistant message must have advisory stripped
    assert_eq!(
        payload_adv.recent_messages[0].text.as_deref(),
        Some("Understood. I will inspect Cargo.toml.")
    );
    // User message MUST PRESERVE the exact quote of the advisory marker!
    assert_eq!(
        payload_adv.recent_messages[1].text.as_deref(),
        Some("Why did you suggest \"Suggested skill for the next step: rust-cargo-basics\"?")
    );

    // -------------------------------------------------------------------------
    // Sub-case 7: Whole-field redaction before truncation and payload inspection
    // -------------------------------------------------------------------------
    let secret_aws = "AKIAIOSFODNN7EXAMPLE";
    let secret_ghp = "ghp_123456789012345678901234567890123456";
    let secret_bearer = "Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.doNotLeakThisSignature123456789";

    let secret_context = NormalizedContext {
        schema_version: 1,
        harness: HarnessId::new("claude_code").unwrap(),
        producer_id: None,
        workspace_root: PrivateText::new("/workspaces/my-project"),
        session_id: Some(SessionId::new("session-07").unwrap()),
        agent_id: None,
        branch_id: None,
        context_epoch: None,
        current_request: CurrentRequest {
            event_id: Some(EventId::new("req-secret").unwrap()),
            text: PrivateText::new(format!(
                "Deploy with token {} and key {}",
                secret_ghp, secret_aws
            )),
            attachments_omitted: false,
            essential_attachment_missing: false,
        },
        events: vec![NormalizedEvent {
            event_id: Some(EventId::new("ev-secret-tool").unwrap()),
            parent_id: None,
            turn_id: None,
            agent_id: None,
            branch_id: None,
            role: Role::Tool,
            kind: EventKind::ToolInvocation,
            timestamp_unix_ms: Some(10),
            text: PrivateText::new(""),
            tool: Some(ToolEvent {
                call_id: Some(ToolCallId::new("call-sec-01").unwrap()),
                name: PrivateText::new("curl"),
                status: ToolStatus::Failed,
                arguments: Some(PrivateText::new(format!(
                    r#"{{"command":"curl -H \"Authorization: {}\" https://api.example.com"}}"#,
                    secret_bearer
                ))),
                result: Some(PrivateText::new(format!(
                    "error: authentication failed for key {}",
                    secret_aws
                ))),
            }),
        }],
        explicit_skill_references: vec![],
        supplied_loads: vec![],
    };

    let payload_secret = render_context(&secret_context, &options).expect("render should succeed");

    // Zero secret leak in latest request
    assert!(!payload_secret.latest_user_request.contains(secret_aws));
    assert!(!payload_secret.latest_user_request.contains(secret_ghp));
    assert!(payload_secret.latest_user_request.contains("[REDACTED]"));

    // Zero secret leak in tool summaries
    let tool_summary = payload_secret.recent_messages[0].summary.as_ref().unwrap();
    assert!(!tool_summary.contains(secret_aws));
    assert!(!tool_summary.contains(secret_bearer));
    assert!(tool_summary.contains("[REDACTED]"));

    // Full assembled payload inspection passes cleanly
    let json_bytes = payload_secret
        .to_json_bytes()
        .expect("inspection must pass cleanly");
    let json_str = String::from_utf8(json_bytes).unwrap();
    assert!(!json_str.contains(secret_aws));
    assert!(!json_str.contains(secret_ghp));
    assert!(!json_str.contains(secret_bearer));

    // -------------------------------------------------------------------------
    // Sub-case 8: Tool summaries, error lines, and --no-tools isolation
    // -------------------------------------------------------------------------
    let tool_context = NormalizedContext {
        schema_version: 1,
        harness: HarnessId::new("claude_code").unwrap(),
        producer_id: None,
        workspace_root: PrivateText::new("/workspaces/my-project"),
        session_id: Some(SessionId::new("session-08").unwrap()),
        agent_id: None,
        branch_id: None,
        context_epoch: None,
        current_request: CurrentRequest {
            event_id: Some(EventId::new("req-tool").unwrap()),
            text: PrivateText::new("Fix the build failure."),
            attachments_omitted: false,
            essential_attachment_missing: false,
        },
        events: vec![NormalizedEvent {
            event_id: Some(EventId::new("tool-shell-fail").unwrap()),
            parent_id: None,
            turn_id: None,
            agent_id: None,
            branch_id: None,
            role: Role::Tool,
            kind: EventKind::ToolResult,
            timestamp_unix_ms: Some(15),
            text: PrivateText::new(""),
            tool: Some(ToolEvent {
                call_id: Some(ToolCallId::new("call-shell-01").unwrap()),
                name: PrivateText::new("shell"),
                status: ToolStatus::Failed,
                arguments: Some(PrivateText::new(
                    r#"{"command":"cargo test --test universe"}"#,
                )),
                result: Some(PrivateText::new(
                    "compiling...\nerror: 3 failures in typeck::universe\nfatal: compilation failed",
                )),
            }),
        }],
        explicit_skill_references: vec![],
        supplied_loads: vec![],
    };

    let payload_with_tools =
        render_context(&tool_context, &options).expect("render should succeed");
    assert_eq!(payload_with_tools.recent_messages.len(), 1);
    let msg = &payload_with_tools.recent_messages[0];
    assert_eq!(msg.role, "tool");
    assert_eq!(msg.tool.as_deref(), Some("shell"));
    assert_eq!(msg.status.as_deref(), Some("failed"));
    let summary = msg.summary.as_ref().unwrap();
    assert!(summary.contains("error: 3 failures in typeck::universe"));

    // Now test with --no-tools
    let no_tools_options = RenderContextOptions {
        no_tools: true,
        ..RenderContextOptions::default()
    };
    let payload_no_tools =
        render_context(&tool_context, &no_tools_options).expect("render should succeed");
    // Tool message must be completely absent from recent_messages
    assert_eq!(payload_no_tools.recent_messages.len(), 0);

    // -------------------------------------------------------------------------
    // Sub-case 9: 12-message window and 12,000 Unicode scalar budget
    // -------------------------------------------------------------------------
    let mut many_events = Vec::new();
    for i in 1..=20 {
        many_events.push(NormalizedEvent {
            event_id: Some(EventId::new(format!("msg-{i}")).unwrap()),
            parent_id: None,
            turn_id: None,
            agent_id: None,
            branch_id: None,
            role: if i % 2 == 0 {
                Role::Assistant
            } else {
                Role::User
            },
            kind: EventKind::Message,
            timestamp_unix_ms: Some(i as i64 * 10),
            text: PrivateText::new(format!("Turn {i}: details about task component {i}")),
            tool: None,
        });
    }

    let many_context = NormalizedContext {
        schema_version: 1,
        harness: HarnessId::new("claude_code").unwrap(),
        producer_id: None,
        workspace_root: PrivateText::new("/workspaces/my-project"),
        session_id: Some(SessionId::new("session-09").unwrap()),
        agent_id: None,
        branch_id: None,
        context_epoch: None,
        current_request: CurrentRequest {
            event_id: Some(EventId::new("req-many").unwrap()),
            text: PrivateText::new("Final summary request"),
            attachments_omitted: false,
            essential_attachment_missing: false,
        },
        events: many_events,
        explicit_skill_references: vec![],
        supplied_loads: vec![],
    };

    let payload_many = render_context(&many_context, &options).expect("render should succeed");
    // Max messages limit is 12: only the latest 12 of 20 must be kept
    assert_eq!(payload_many.recent_messages.len(), 12);
    assert_eq!(payload_many.context_quality, ContextQuality::Partial);
    assert_eq!(
        payload_many.recent_messages[0].text.as_deref(),
        Some("Turn 9: details about task component 9")
    );
    assert_eq!(
        payload_many.recent_messages[11].text.as_deref(),
        Some("Turn 20: details about task component 20")
    );
    assert!(payload_many.total_message_scalars() <= 12_000);

    // -------------------------------------------------------------------------
    // Sub-case 10: Project signals and language detection
    // -------------------------------------------------------------------------
    let dirty = DirtyPaths {
        paths: vec![
            PrivateText::new("src/kernel/typeck.rs"),
            PrivateText::new("crates/core/lib.rs"),
        ],
        omitted_non_utf8: 0,
        omitted_unsafe: 0,
        truncated: false,
    };
    let signals = ProjectSignals {
        filenames: vec!["Cargo.toml", "lakefile.lean", "go.mod"],
        tools_on_path: vec!["cargo", "lake", "rch"],
        dirty_paths: Some(dirty),
        git_omission: None,
        inventory_partial: false,
    };

    let signals_options = RenderContextOptions {
        project_signals: Some(&signals),
        loaded_references: vec![RenderedLoadedReference {
            name: "rust-cargo-basics".to_string(),
            summary: "Cargo commands and common build errors".to_string(),
        }],
        loaded_state: "observed".to_string(),
        explicit_exclusions: vec!["skill-deprecated".to_string()],
        ..RenderContextOptions::default()
    };

    let payload_signals =
        render_context(&context, &signals_options).expect("render should succeed");
    assert_eq!(
        payload_signals.project_signals.languages,
        vec!["go", "lean", "rust"]
    );
    assert_eq!(
        payload_signals.project_signals.tools_on_path,
        vec!["cargo", "lake", "rch"]
    );
    assert_eq!(
        payload_signals.project_signals.dirty_paths,
        vec!["src/kernel/typeck.rs", "crates/core/lib.rs"]
    );
    assert!(!payload_signals.project_signals.dirty_paths_truncated);
    assert_eq!(payload_signals.session_state.loaded_references.len(), 1);
    assert_eq!(
        payload_signals.session_state.loaded_references[0].name,
        "rust-cargo-basics"
    );
    assert_eq!(payload_signals.session_state.loaded_state, "observed");
    assert_eq!(
        payload_signals.session_state.explicit_exclusions,
        vec!["skill-deprecated"]
    );

    // Verify round-trip serialization matching documented Jev payload shape
    let val = payload_signals
        .to_value()
        .expect("value conversion must succeed");
    assert_eq!(val["schema_version"], 1);
    assert_eq!(val["harness"], "claude_code");
    assert_eq!(val["context_quality"], "complete");
    assert_eq!(val["project_signals"]["languages"][0], "go");
    assert_eq!(
        val["session_state"]["loaded_references"][0]["name"],
        "rust-cargo-basics"
    );
    assert_eq!(
        val["latest_user_request"],
        "Please optimize the serialization loop."
    );
}

// ==============================================================================
// Unit Property Test: tests/context_contract.rs::task_anchors
// Boundary: p3_task_anchors (bead sr-roadmap-l1i.4.9)
// Assertion ID: anchor_intact, E2E Case: task-anchor-preserved
// ==============================================================================

#[test]
fn task_anchors() {
    let redactor = Redactor::default();

    // -------------------------------------------------------------------------
    // Sub-case 1: Terse continuation classification coverage
    // -------------------------------------------------------------------------
    let terse_examples = [
        "continue",
        "Continue",
        "CONTINUE",
        "continue.",
        "continue!",
        "go on",
        "go ahead",
        "proceed",
        "next",
        "next step",
        "more",
        "keep going",
        "carry on",
        "yes",
        "yep",
        "yeah",
        "ok",
        "okay",
        "sure",
        "do it",
        "run it",
        "apply",
        "try it",
        "resume",
        "  ok;  ",
    ];
    for terse in terse_examples {
        assert!(
            is_terse_continuation(terse),
            "expected '{}' to be recognized as terse continuation",
            terse
        );
    }

    let substantive_examples = [
        "Implement sqlite caching for skill recommendations",
        "Please fix the build error in src/context/anchor.rs",
        "continue with the migration of auth tokens",
        "proceed to deploy to staging after checking tests",
        "run tests and report failures",
    ];
    for substantive in substantive_examples {
        assert!(
            !is_terse_continuation(substantive),
            "expected '{}' to be recognized as substantive instruction",
            substantive
        );
    }

    // -------------------------------------------------------------------------
    // Sub-case 2: Continue with valid historical anchor (Antecedent recovered)
    // Assertion ID: anchor_intact
    // -------------------------------------------------------------------------
    let context_valid = NormalizedContext {
        schema_version: 1,
        harness: HarnessId::new("claude_code").unwrap(),
        producer_id: None,
        workspace_root: PrivateText::new("/data/projects/skillranker"),
        session_id: Some(SessionId::new("sess-anchor-01").unwrap()),
        agent_id: None,
        branch_id: None,
        context_epoch: Some(ContextEpoch::new("epoch-1").unwrap()),
        current_request: CurrentRequest {
            event_id: Some(EventId::new("ev-req-01").unwrap()),
            text: PrivateText::new("continue"),
            attachments_omitted: false,
            essential_attachment_missing: false,
        },
        events: vec![
            NormalizedEvent {
                event_id: Some(EventId::new("ev-hist-01").unwrap()),
                parent_id: None,
                turn_id: Some(TurnId::new("turn-1").unwrap()),
                agent_id: None,
                branch_id: None,
                role: Role::User,
                kind: EventKind::Message,
                timestamp_unix_ms: Some(1000),
                text: PrivateText::new(
                    "Implement SQLite caching for next-step skill recommendations",
                ),
                tool: None,
            },
            NormalizedEvent {
                event_id: Some(EventId::new("ev-hist-02").unwrap()),
                parent_id: Some(EventId::new("ev-hist-01").unwrap()),
                turn_id: Some(TurnId::new("turn-2").unwrap()),
                agent_id: None,
                branch_id: None,
                role: Role::Assistant,
                kind: EventKind::Message,
                timestamp_unix_ms: Some(2000),
                text: PrivateText::new("I will design the SQLite caching schema now."),
                tool: None,
            },
        ],
        explicit_skill_references: Vec::new(),
        supplied_loads: Vec::new(),
    };

    let resolution = resolve_task_anchor(&context_valid, None, &redactor);
    match resolution {
        AnchorResolution::Established(ref anchor) => {
            // Assertion ID: anchor_intact
            assert_eq!(
                anchor.text,
                "Implement SQLite caching for next-step skill recommendations"
            );
            assert!(anchor.is_terse);
            assert_eq!(
                anchor.source_event_id,
                Some(EventId::new("ev-hist-01").unwrap())
            );
            assert_eq!(
                anchor.provenance,
                AnchorProvenance::HistoricalEvent {
                    event_id: EventId::new("ev-hist-01").unwrap(),
                }
            );
            assert!(anchor.directives.is_empty());
        }
        other => panic!("expected Established anchor, got {:?}", other),
    }

    // -------------------------------------------------------------------------
    // Sub-case 3: Continue without valid antecedent (Missing context)
    // Classified as unavailable / missing-task-context
    // -------------------------------------------------------------------------
    let context_no_antecedent = NormalizedContext {
        schema_version: 1,
        harness: HarnessId::new("claude_code").unwrap(),
        producer_id: None,
        workspace_root: PrivateText::new("/data/projects/skillranker"),
        session_id: Some(SessionId::new("sess-anchor-02").unwrap()),
        agent_id: None,
        branch_id: None,
        context_epoch: Some(ContextEpoch::new("epoch-1").unwrap()),
        current_request: CurrentRequest {
            event_id: Some(EventId::new("ev-req-02").unwrap()),
            text: PrivateText::new("continue"),
            attachments_omitted: false,
            essential_attachment_missing: false,
        },
        events: vec![
            // Only assistant message, no user antecedent
            NormalizedEvent {
                event_id: Some(EventId::new("ev-hist-03").unwrap()),
                parent_id: None,
                turn_id: Some(TurnId::new("turn-1").unwrap()),
                agent_id: None,
                branch_id: None,
                role: Role::Assistant,
                kind: EventKind::Message,
                timestamp_unix_ms: Some(1000),
                text: PrivateText::new("Session initialized."),
                tool: None,
            },
        ],
        explicit_skill_references: Vec::new(),
        supplied_loads: Vec::new(),
    };

    let res_no_antecedent = resolve_task_anchor(&context_no_antecedent, None, &redactor);
    match res_no_antecedent {
        AnchorResolution::MissingTaskContext { reason } => {
            assert!(reason.contains("lacks recoverable substantive antecedent"));
        }
        other => panic!("expected MissingTaskContext, got {:?}", other),
    }

    // -------------------------------------------------------------------------
    // Sub-case 4: Task boundary stops antecedent lookback
    // -------------------------------------------------------------------------
    let context_task_boundary = NormalizedContext {
        schema_version: 1,
        harness: HarnessId::new("claude_code").unwrap(),
        producer_id: None,
        workspace_root: PrivateText::new("/data/projects/skillranker"),
        session_id: Some(SessionId::new("sess-anchor-03").unwrap()),
        agent_id: None,
        branch_id: None,
        context_epoch: Some(ContextEpoch::new("epoch-2").unwrap()),
        current_request: CurrentRequest {
            event_id: Some(EventId::new("ev-req-03").unwrap()),
            text: PrivateText::new("proceed"),
            attachments_omitted: false,
            essential_attachment_missing: false,
        },
        events: vec![
            NormalizedEvent {
                event_id: Some(EventId::new("ev-bnd-01").unwrap()),
                parent_id: None,
                turn_id: Some(TurnId::new("turn-1").unwrap()),
                agent_id: None,
                branch_id: None,
                role: Role::User,
                kind: EventKind::Message,
                timestamp_unix_ms: Some(1000),
                text: PrivateText::new("Previous task: clean up old temporary logs"),
                tool: None,
            },
            NormalizedEvent {
                event_id: Some(EventId::new("ev-bnd-02").unwrap()),
                parent_id: Some(EventId::new("ev-bnd-01").unwrap()),
                turn_id: Some(TurnId::new("turn-2").unwrap()),
                agent_id: None,
                branch_id: None,
                role: Role::System,
                kind: EventKind::TaskBoundary,
                timestamp_unix_ms: Some(2000),
                text: PrivateText::new("TaskBoundary: completed log cleanup"),
                tool: None,
            },
        ],
        explicit_skill_references: Vec::new(),
        supplied_loads: Vec::new(),
    };

    let res_boundary = resolve_task_anchor(&context_task_boundary, None, &redactor);
    assert!(
        matches!(res_boundary, AnchorResolution::MissingTaskContext { .. }),
        "task boundary must prevent antecedent lookback from preceding task"
    );

    // -------------------------------------------------------------------------
    // Sub-case 5: Explicit directives preserved beyond rendered 12-message cutoff
    // -------------------------------------------------------------------------
    let mut cutoff_events = Vec::new();

    // Turn 1 (outside 12-message cutoff): User establishes explicit directive
    cutoff_events.push(NormalizedEvent {
        event_id: Some(EventId::new("ev-long-01").unwrap()),
        parent_id: None,
        turn_id: Some(TurnId::new("turn-1").unwrap()),
        agent_id: None,
        branch_id: None,
        role: Role::User,
        kind: EventKind::Message,
        timestamp_unix_ms: Some(1000),
        text: PrivateText::new("/use-skill rust-cargo-basics"),
        tool: None,
    });
    cutoff_events.push(NormalizedEvent {
        event_id: Some(EventId::new("ev-long-02").unwrap()),
        parent_id: Some(EventId::new("ev-long-01").unwrap()),
        turn_id: Some(TurnId::new("turn-2").unwrap()),
        agent_id: None,
        branch_id: None,
        role: Role::Assistant,
        kind: EventKind::Message,
        timestamp_unix_ms: Some(2000),
        text: PrivateText::new("Acknowledged, rust-cargo-basics will be used."),
        tool: None,
    });

    // Turns 3..18: Intermediate conversation turns pushing Turn 1 past the 12-message window
    for i in 3..=18 {
        let role = if i % 2 == 1 {
            Role::User
        } else {
            Role::Assistant
        };
        cutoff_events.push(NormalizedEvent {
            event_id: Some(EventId::new(format!("ev-long-{i:02}")).unwrap()),
            parent_id: Some(EventId::new(format!("ev-long-{:02}", i - 1)).unwrap()),
            turn_id: Some(TurnId::new(format!("turn-{i}")).unwrap()),
            agent_id: None,
            branch_id: None,
            role,
            kind: EventKind::Message,
            timestamp_unix_ms: Some(1000 * i as i64),
            text: PrivateText::new(format!("Step {i} in progress")),
            tool: None,
        });
    }

    let context_cutoff = NormalizedContext {
        schema_version: 1,
        harness: HarnessId::new("claude_code").unwrap(),
        producer_id: None,
        workspace_root: PrivateText::new("/data/projects/skillranker"),
        session_id: Some(SessionId::new("sess-anchor-cutoff").unwrap()),
        agent_id: None,
        branch_id: None,
        context_epoch: Some(ContextEpoch::new("epoch-1").unwrap()),
        current_request: CurrentRequest {
            event_id: Some(EventId::new("ev-req-cutoff").unwrap()),
            text: PrivateText::new("ok, next step"),
            attachments_omitted: false,
            essential_attachment_missing: false,
        },
        events: cutoff_events,
        explicit_skill_references: Vec::new(),
        supplied_loads: Vec::new(),
    };

    // Verify that render_context would drop Turn 1 from recent_messages
    let rendered = render_context(&context_cutoff, &RenderContextOptions::default())
        .expect("render must succeed");
    assert_eq!(rendered.recent_messages.len(), 12);
    // Recent messages start at Turn 7, so Turn 1 is dropped from rendered context
    assert!(!rendered.recent_messages.iter().any(|m| {
        m.text
            .as_deref()
            .unwrap_or("")
            .contains("rust-cargo-basics")
    }));

    // But resolve_task_anchor preserves the directive extracted before windowing!
    let res_cutoff = resolve_task_anchor(&context_cutoff, None, &redactor);
    match res_cutoff {
        AnchorResolution::Established(ref anchor) => {
            assert!(anchor.is_terse);
            assert_eq!(anchor.directives.len(), 1);
            assert_eq!(anchor.directives[0].target, "rust-cargo-basics");
            assert_eq!(anchor.directives[0].kind, AnchorDirectiveKind::Require);
            // Antecedent recovered is the latest substantive user message
            assert_eq!(anchor.text, "Step 17 in progress");
        }
        other => panic!(
            "expected Established anchor with preserved directive, got {:?}",
            other
        ),
    }

    // -------------------------------------------------------------------------
    // Sub-case 6: Supplied summary with verifiable provenance accepted
    // -------------------------------------------------------------------------
    let context_supplied = NormalizedContext {
        schema_version: 1,
        harness: HarnessId::new("claude_code").unwrap(),
        producer_id: None,
        workspace_root: PrivateText::new("/data/projects/skillranker"),
        session_id: Some(SessionId::new("sess-anchor-supplied").unwrap()),
        agent_id: None,
        branch_id: None,
        context_epoch: Some(ContextEpoch::new("epoch-3").unwrap()),
        current_request: CurrentRequest {
            event_id: Some(EventId::new("ev-req-sup").unwrap()),
            text: PrivateText::new("continue"),
            attachments_omitted: false,
            essential_attachment_missing: false,
        },
        events: Vec::new(),
        explicit_skill_references: Vec::new(),
        supplied_loads: Vec::new(),
    };

    let res_supplied = resolve_task_anchor(
        &context_supplied,
        Some((
            "Refactor storage ledger to support atomic commits",
            "session-compaction-v3",
        )),
        &redactor,
    );
    match res_supplied {
        AnchorResolution::Established(ref anchor) => {
            assert_eq!(
                anchor.text,
                "Refactor storage ledger to support atomic commits"
            );
            assert!(anchor.is_terse);
            assert_eq!(
                anchor.provenance,
                AnchorProvenance::SuppliedSummary {
                    provenance: "session-compaction-v3".to_string(),
                }
            );
        }
        other => panic!(
            "expected Established from supplied summary, got {:?}",
            other
        ),
    }

    // -------------------------------------------------------------------------
    // Sub-case 7: Supplied summary with empty/whitespace provenance rejected
    // -------------------------------------------------------------------------
    let res_unverified_prov = resolve_task_anchor(
        &context_supplied,
        Some(("Refactor storage ledger to support atomic commits", "   ")),
        &redactor,
    );
    assert!(
        matches!(
            res_unverified_prov,
            AnchorResolution::MissingTaskContext { .. }
        ),
        "supplied summary with empty provenance must be rejected as unverified"
    );

    // -------------------------------------------------------------------------
    // Sub-case 8: Oversized uninspectable request rejected (>1 MiB)
    // -------------------------------------------------------------------------
    let oversized_len = HOOK_STDIN_BYTES.max() + 1;
    let oversized_text = "A".repeat(oversized_len);
    let context_oversized = NormalizedContext {
        schema_version: 1,
        harness: HarnessId::new("claude_code").unwrap(),
        producer_id: None,
        workspace_root: PrivateText::new("/data/projects/skillranker"),
        session_id: Some(SessionId::new("sess-anchor-oversized").unwrap()),
        agent_id: None,
        branch_id: None,
        context_epoch: Some(ContextEpoch::new("epoch-1").unwrap()),
        current_request: CurrentRequest {
            event_id: Some(EventId::new("ev-req-big").unwrap()),
            text: PrivateText::new(oversized_text),
            attachments_omitted: false,
            essential_attachment_missing: false,
        },
        events: Vec::new(),
        explicit_skill_references: Vec::new(),
        supplied_loads: Vec::new(),
    };

    let res_oversized = resolve_task_anchor(&context_oversized, None, &redactor);
    let err = res_oversized.clone().into_result().unwrap_err();
    match err {
        AnchorError::OversizedInput { bytes, max } => {
            assert_eq!(bytes, oversized_len);
            assert_eq!(max, HOOK_STDIN_BYTES.max());
        }
        other => panic!("expected OversizedInput error, got {:?}", other),
    }

    // -------------------------------------------------------------------------
    // Sub-case 9: Contradictory directives across turns detected
    // -------------------------------------------------------------------------
    let context_conflict = NormalizedContext {
        schema_version: 1,
        harness: HarnessId::new("claude_code").unwrap(),
        producer_id: None,
        workspace_root: PrivateText::new("/data/projects/skillranker"),
        session_id: Some(SessionId::new("sess-anchor-conflict").unwrap()),
        agent_id: None,
        branch_id: None,
        context_epoch: Some(ContextEpoch::new("epoch-1").unwrap()),
        current_request: CurrentRequest {
            event_id: Some(EventId::new("ev-req-conf").unwrap()),
            text: PrivateText::new("continue"),
            attachments_omitted: false,
            essential_attachment_missing: false,
        },
        events: vec![
            NormalizedEvent {
                event_id: Some(EventId::new("ev-c-01").unwrap()),
                parent_id: None,
                turn_id: Some(TurnId::new("turn-1").unwrap()),
                agent_id: None,
                branch_id: None,
                role: Role::User,
                kind: EventKind::Message,
                timestamp_unix_ms: Some(1000),
                text: PrivateText::new("/use-skill docker-deploy"),
                tool: None,
            },
            NormalizedEvent {
                event_id: Some(EventId::new("ev-c-02").unwrap()),
                parent_id: Some(EventId::new("ev-c-01").unwrap()),
                turn_id: Some(TurnId::new("turn-2").unwrap()),
                agent_id: None,
                branch_id: None,
                role: Role::Assistant,
                kind: EventKind::Message,
                timestamp_unix_ms: Some(2000),
                text: PrivateText::new("Understood."),
                tool: None,
            },
            NormalizedEvent {
                event_id: Some(EventId::new("ev-c-03").unwrap()),
                parent_id: Some(EventId::new("ev-c-02").unwrap()),
                turn_id: Some(TurnId::new("turn-3").unwrap()),
                agent_id: None,
                branch_id: None,
                role: Role::User,
                kind: EventKind::Message,
                timestamp_unix_ms: Some(3000),
                text: PrivateText::new("/exclude-skill docker-deploy"),
                tool: None,
            },
        ],
        explicit_skill_references: Vec::new(),
        supplied_loads: Vec::new(),
    };

    let res_conflict = resolve_task_anchor(&context_conflict, None, &redactor);
    match res_conflict {
        AnchorResolution::ConflictingDirectives { detail } => {
            assert!(detail.contains("docker-deploy"));
        }
        other => panic!(
            "expected ConflictingDirectives across turns, got {:?}",
            other
        ),
    }

    // -------------------------------------------------------------------------
    // Sub-case 10: Contradictory directives in current prompt detected
    // -------------------------------------------------------------------------
    let context_prompt_conflict = NormalizedContext {
        schema_version: 1,
        harness: HarnessId::new("claude_code").unwrap(),
        producer_id: None,
        workspace_root: PrivateText::new("/data/projects/skillranker"),
        session_id: Some(SessionId::new("sess-anchor-pconf").unwrap()),
        agent_id: None,
        branch_id: None,
        context_epoch: Some(ContextEpoch::new("epoch-1").unwrap()),
        current_request: CurrentRequest {
            event_id: Some(EventId::new("ev-req-pconf").unwrap()),
            text: PrivateText::new("/use-skill k8s-helper\n/exclude-skill k8s-helper"),
            attachments_omitted: false,
            essential_attachment_missing: false,
        },
        events: Vec::new(),
        explicit_skill_references: Vec::new(),
        supplied_loads: Vec::new(),
    };

    let res_pconflict = resolve_task_anchor(&context_prompt_conflict, None, &redactor);
    match res_pconflict {
        AnchorResolution::ConflictingDirectives { detail } => {
            assert!(detail.contains("k8s-helper"));
        }
        other => panic!(
            "expected ConflictingDirectives within prompt, got {:?}",
            other
        ),
    }

    // -------------------------------------------------------------------------
    // Sub-case 11: Substantive prompt establishes CurrentRequest provenance
    // -------------------------------------------------------------------------
    let context_substantive = NormalizedContext {
        schema_version: 1,
        harness: HarnessId::new("claude_code").unwrap(),
        producer_id: None,
        workspace_root: PrivateText::new("/data/projects/skillranker"),
        session_id: Some(SessionId::new("sess-anchor-sub").unwrap()),
        agent_id: None,
        branch_id: None,
        context_epoch: Some(ContextEpoch::new("epoch-1").unwrap()),
        current_request: CurrentRequest {
            event_id: Some(EventId::new("ev-req-sub").unwrap()),
            text: PrivateText::new("Add property tests for task anchor resolution"),
            attachments_omitted: false,
            essential_attachment_missing: false,
        },
        events: Vec::new(),
        explicit_skill_references: Vec::new(),
        supplied_loads: Vec::new(),
    };

    let res_sub = resolve_task_anchor(&context_substantive, None, &redactor);
    match res_sub {
        AnchorResolution::Established(ref anchor) => {
            assert_eq!(anchor.text, "Add property tests for task anchor resolution");
            assert!(!anchor.is_terse);
            assert_eq!(
                anchor.source_event_id,
                Some(EventId::new("ev-req-sub").unwrap())
            );
            assert_eq!(anchor.provenance, AnchorProvenance::CurrentRequest);
        }
        other => panic!(
            "expected Established CurrentRequest anchor, got {:?}",
            other
        ),
    }

    // -------------------------------------------------------------------------
    // Sub-case 12: Sensitive secret redaction on recovered anchor text
    // -------------------------------------------------------------------------
    let context_secret = NormalizedContext {
        schema_version: 1,
        harness: HarnessId::new("claude_code").unwrap(),
        producer_id: None,
        workspace_root: PrivateText::new("/data/projects/skillranker"),
        session_id: Some(SessionId::new("sess-anchor-secret").unwrap()),
        agent_id: None,
        branch_id: None,
        context_epoch: Some(ContextEpoch::new("epoch-1").unwrap()),
        current_request: CurrentRequest {
            event_id: Some(EventId::new("ev-req-sec").unwrap()),
            text: PrivateText::new("continue"),
            attachments_omitted: false,
            essential_attachment_missing: false,
        },
        events: vec![NormalizedEvent {
            event_id: Some(EventId::new("ev-sec-01").unwrap()),
            parent_id: None,
            turn_id: Some(TurnId::new("turn-1").unwrap()),
            agent_id: None,
            branch_id: None,
            role: Role::User,
            kind: EventKind::Message,
            timestamp_unix_ms: Some(1000),
            text: PrivateText::new("Deploy service with token=AKIAIOSFODNN7EXAMPLE to production"),
            tool: None,
        }],
        explicit_skill_references: Vec::new(),
        supplied_loads: Vec::new(),
    };

    let res_sec = resolve_task_anchor(&context_secret, None, &redactor);
    match res_sec {
        AnchorResolution::Established(ref anchor) => {
            assert!(!anchor.text.contains("AKIAIOSFODNN7EXAMPLE"));
            assert!(anchor.text.contains("[REDACTED]"));
            assert!(anchor.text.contains("Deploy service with token="));
            assert!(anchor.text.contains("to production"));
        }
        other => panic!(
            "expected Established anchor with redacted secret, got {:?}",
            other
        ),
    }
}

#[test]
fn native_jsonl_incremental() {
    let dir = temp_dir("native_jsonl");
    let path = dir.join("session.jsonl");

    // 1. Initial write with 2 valid records and an incomplete trailing line
    let initial_content = concat!(
        "{\"event_id\":\"e1\",\"role\":\"user\",\"kind\":\"message\",\"text\":\"first prompt\"}\n",
        "{\"event_id\":\"e2\",\"role\":\"assistant\",\"kind\":\"message\",\"text\":\"first answer\"}\n",
        "{\"event_id\":\"e3\""
    );
    fs::write(&path, initial_content).unwrap();

    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let snap1 = snapshot_jsonl(&invocation, &cx, &path, None, CursorKind::Ranking).unwrap();
    let _ = invocation.shutdown();

    assert_eq!(snap1.events.len(), 2);
    assert_eq!(snap1.events[0].event_id.as_ref().unwrap().as_str(), "e1");
    assert_eq!(snap1.events[1].event_id.as_ref().unwrap().as_str(), "e2");
    assert!(
        snap1.incomplete_tail,
        "incomplete trailing line must be deferred"
    );
    assert_eq!(snap1.cursor.generation, 1);
    assert_eq!(snap1.cursor.last_event_id.as_ref().unwrap().as_str(), "e2");

    // 2. Complete the record and append a 4th record
    let full_content = concat!(
        "{\"event_id\":\"e1\",\"role\":\"user\",\"kind\":\"message\",\"text\":\"first prompt\"}\n",
        "{\"event_id\":\"e2\",\"role\":\"assistant\",\"kind\":\"message\",\"text\":\"first answer\"}\n",
        "{\"event_id\":\"e3\",\"role\":\"user\",\"kind\":\"message\",\"text\":\"second prompt\"}\n",
        "{\"event_id\":\"e4\",\"role\":\"assistant\",\"kind\":\"message\",\"text\":\"second answer\"}\n"
    );
    fs::write(&path, full_content).unwrap();

    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let snap2 = snapshot_jsonl(
        &invocation,
        &cx,
        &path,
        Some(&snap1.cursor),
        CursorKind::Ranking,
    )
    .unwrap();
    let _ = invocation.shutdown();

    assert!(
        !snap2.rebuilt,
        "incremental read must not rebuild when cursor is valid"
    );
    assert_eq!(
        snap2.events.len(),
        2,
        "only newly added events e3 and e4 must be returned"
    );
    assert_eq!(snap2.events[0].event_id.as_ref().unwrap().as_str(), "e3");
    assert_eq!(snap2.events[1].event_id.as_ref().unwrap().as_str(), "e4");
    assert!(!snap2.incomplete_tail);
    assert_eq!(snap2.cursor.generation, 1);
    assert_eq!(snap2.cursor.last_event_id.as_ref().unwrap().as_str(), "e4");

    // 3. Truncation / compaction: replace with a smaller file
    let compacted = "{\"event_id\":\"e5\",\"role\":\"user\",\"kind\":\"message\",\"text\":\"compacted prompt\"}\n";
    fs::write(&path, compacted).unwrap();

    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let snap3 = snapshot_jsonl(
        &invocation,
        &cx,
        &path,
        Some(&snap2.cursor),
        CursorKind::Ranking,
    )
    .unwrap();
    let _ = invocation.shutdown();

    assert!(snap3.rebuilt, "truncation/compaction must trigger rebuilt");
    assert!(
        snap3.cursor.generation > snap2.cursor.generation,
        "generation must advance"
    );
    assert_eq!(snap3.events.len(), 1);
    assert_eq!(snap3.events[0].event_id.as_ref().unwrap().as_str(), "e5");
}

#[test]
fn cass_adapter() {
    use skillranker::context::cass::{ArchiveSelection, validate_capabilities};
    use skillranker::identity::{ContentHash, SourceId};

    let valid_caps = json!({
        "crate_version": "0.8.0",
        "api_version": 1,
        "contract_version": "1",
        "build_commit": "abcdef123456",
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
    let digest = ContentHash::from_bytes(b"cass-binary-content");
    let producer = validate_capabilities(&caps_bytes, digest.clone());
    assert!(producer.is_ok(), "valid cass capabilities must be accepted");
    let p = producer.unwrap();
    assert!(p.validate_support_claim().is_ok());

    let bad_crate = json!({
        "crate_version": "0.9.0",
        "api_version": 1,
        "contract_version": "1",
        "features": ["json_output"],
        "commands": []
    });
    assert!(
        validate_capabilities(&serde_json::to_vec(&bad_crate).unwrap(), digest.clone()).is_err()
    );

    let no_export = json!({
        "crate_version": "0.8.0",
        "api_version": 1,
        "contract_version": "1",
        "features": ["json_output", "export_command"],
        "commands": [
            {"name": "sessions", "arguments": []}
        ]
    });
    assert!(validate_capabilities(&serde_json::to_vec(&no_export).unwrap(), digest).is_err());

    let bad_path = PathBuf::from("relative/path.jsonl");
    assert!(ArchiveSelection::local(bad_path, SourceId::new("local").unwrap()).is_err());
}

#[test]
fn project_signals() {
    let raw = b" M src/lib.rs\0A  untracked.txt\0 M path with spaces/file.txt\0";
    let parsed = parse_dirty_paths(raw).unwrap();
    assert_eq!(parsed.paths.len(), 3);
    assert_eq!(parsed.paths[0].as_str(), "src/lib.rs");
    assert_eq!(parsed.paths[1].as_str(), "untracked.txt");
    assert_eq!(parsed.paths[2].as_str(), "path with spaces/file.txt");
    assert!(!parsed.truncated);
    assert!(parsed.paths.len() <= DIRTY_PATH_LIMIT);
    assert_eq!(parsed.omitted_non_utf8, 0);
    assert_eq!(parsed.omitted_unsafe, 0);

    let traversal = b" M ../outside.rs\0 M safe.rs\0";
    let parsed_traversal = parse_dirty_paths(traversal).unwrap();
    assert_eq!(parsed_traversal.paths.len(), 1);
    assert_eq!(parsed_traversal.paths[0].as_str(), "safe.rs");
    assert_eq!(parsed_traversal.omitted_unsafe, 1);

    let empty = parse_dirty_paths(b"").unwrap();
    assert!(empty.paths.is_empty());
    assert!(!empty.truncated);

    assert!(parse_dirty_paths(b" M incomplete_without_null").is_err());
}
