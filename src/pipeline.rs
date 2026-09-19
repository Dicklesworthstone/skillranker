//! Two-stage ranking pipeline and publication revalidation.
//!
//! Orchestrates context capture, roster discovery, explicit resolution,
//! Quill prefiltering, response cache, wide gate, detailed rerank, local
//! eligibility/scoring, and final boundary revalidation against changed
//! user controls and modified roster dependencies.

use asupersync::Cx;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::cache::{
    CacheKey, CacheNamespace, CachedResponseEntry, CandidateDigest, CoordinationKey,
    DEFAULT_CACHE_TTL_SECS, LeaderContext, LeaseAcquisition, MemoryResponseCache,
    RequestFingerprint, RequestFingerprintInput, RequestStage, compute_request_fingerprint,
};
use crate::cli::ConfigFiles;
use crate::config::{
    ConfigSources, PolicyBoundary, PolicyReceipt, PublicationKind, ResolvedConfig, Revalidation,
};
use crate::context::anchor::resolve_task_anchor;
use crate::context::branch::{LoadedSkillRecord, resolve_active_branch};
use crate::context::jsonl::{CursorKind, snapshot_jsonl};
use crate::context::render::{RenderContextOptions, render_context_and_receipt};
use crate::context::source::{SelectionOutcome, SourceOptions, SourceTarget};
use crate::context::{CurrentRequest, NormalizedContext, PrivateText, parse_normalized_context};
use crate::effects::EffectGate;
use crate::eligibility::{Eligible, Evaluation, LoadedState, Verdict, admit, after_rerank};
use crate::identity::{ContentHash, EventId, HarnessId, SessionId, SkillId, WorkspaceId};
use crate::jev::admission::{AttemptBudget, RankingStage};
use crate::jev::client::{JevClient, TransportErrorKind};
use crate::jev::codec::{Request, Response};
use crate::jev::endpoint::EndpointConfig;
use crate::jev::rerank::RerankOutcome;
use crate::jev::retry::{RetryErrorKind, RetrySession};
use crate::jev::wide::{Sizes, WideDecision, WideOutcome};
use crate::jev::{OriginScopedCredential, rerank, wide};
use crate::output::trace::{StageTrace, TraceEntry, TraceStage};
use crate::output::{ErrorKind, OutputDocument, OutputKind, SCHEMA_VERSION, TraceCursor};
use crate::privacy::redaction::Redactor;
use crate::privacy::{
    NetworkConsent, ProviderAdmissionRefusal, StoreAccess, admit_provider_attempt,
};
use crate::roster::discovery::claude_code_plan;
use crate::roster::evidence::{PolicyView, RetrievalView};
use crate::roster::explicit::{
    ExplicitResolutionRequest, ExplicitResolutionResult, ResolvedExplicitSkill,
    resolve_explicit_requirements,
};
use crate::roster::import::import_authorized;
use crate::roster::resolution::{
    AdvisorySkill, ExactResolution, ResolvedRoster, resolve_claude_plan,
};
use crate::roster::retrieval::{
    QueryInput, RetrievalBudget, RetrievalError, RetrievalMethod, retrieve,
};
use crate::roster::revalidation::{RevalidationError, capture, revalidate_claude};
use crate::roster::{InvocationKind, LoadTarget, Visibility};
use crate::runtime::{EntryClock, ProcessInvocation, admit_publication};
use crate::scoring::{Input as ScoringInput, Ranking, Weights, rank};

/// Failure tuple compatible with CLI error formatting: `(exit_code, kind, message)`.
pub type PipelineFailure = (u8, &'static str, String);

fn failure(code: u8, kind: &'static str, message: impl Into<String>) -> PipelineFailure {
    (code, kind, message.into())
}

pub use crate::jev::client::JevTransport;

/// The provisional contract rank resolves Claude skills under: the documented
/// project-over-personal precedence, not yet verified by conformance evidence.
const PROVISIONAL_CLAUDE_CONTRACT: &str = "claude-code-documented-unverified";

/// How a result's visibility is labeled: "unverified" for the provisional
/// Claude contract (and any unverified binding), "verified" only for a
/// contract backed by evidence.
fn visibility_label(visibility: &Visibility) -> &'static str {
    match visibility {
        Visibility::Verified { contract_version }
            if contract_version != PROVISIONAL_CLAUDE_CONTRACT =>
        {
            "verified"
        }
        _ => "unverified",
    }
}

/// Trusted executable roots for project-signal tool detection: fixed system
/// directories, never the process PATH or anything the workspace supplies.
const TRUSTED_TOOL_ROOTS: [&str; 3] = ["/usr/local/bin", "/usr/bin", "/bin"];

/// Parameters for running the ranking pipeline.
#[derive(Clone, Debug)]
pub struct RankArgs {
    pub workspace: PathBuf,
    pub user_config_root: Option<PathBuf>,
    /// The user's home directory: Claude's user skills live in its
    /// `.claude/skills`, not under the configuration root.
    pub home: Option<PathBuf>,
    /// Trusted host directory for the persistent response cache. `None`
    /// keeps every response in this invocation only.
    pub cache_dir: Option<PathBuf>,
    pub sources: ConfigSources,
    pub gate: EffectGate,
    pub source_options: SourceOptions,
    pub require_skills: Vec<SkillId>,
    /// Stage-2 evidence for a dry run: the shortlist whose rerank request to
    /// preview. A network-free run cannot know the model's own shortlist.
    pub shortlist_ids: Vec<SkillId>,
    pub roster_file: Option<PathBuf>,
    pub explain: bool,
    pub why_not: Option<SkillId>,
    pub output_json: bool,
    pub output_table: bool,
    pub dry_run: bool,
}

/// Pipeline execution state tracking provider attempts, requests, tokens and cache hits.
#[derive(Clone, Debug, Default)]
pub struct ExecutionMetrics {
    pub requests: u64,
    pub http_attempts: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub unknown_usage_attempts: u64,
    pub cache_hit: bool,
    pub wide_hit: bool,
    pub rerank_hit: bool,
    pub cache_age_ms: Option<u64>,
}

/// Read one explicitly selected input file: a bounded regular file under its
/// own directory, resolved against the workspace when relative. Devices, FIFOs
/// and symlinks leaving that directory are refused; errors name no path.
fn read_input_file(
    workspace: &Path,
    path: &Path,
    limit: crate::limits::ResourceLimit,
) -> Result<Vec<u8>, PipelineFailure> {
    use crate::authorized_read::{AuthorizedRoot, AuthorizedRoots, ReadError};
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    };
    let unsupported = || {
        failure(
            7,
            "unsupported-input",
            "The input is not a readable regular file",
        )
    };
    let (Some(parent), Some(name)) = (absolute.parent(), absolute.file_name()) else {
        return Err(unsupported());
    };
    let root = AuthorizedRoot::open_absolute(parent).map_err(|_| unsupported())?;
    AuthorizedRoots::single(root)
        .read_bounded(0, Path::new(name), limit)
        .map(|read| read.bytes().to_vec())
        .map_err(|error| match error {
            ReadError::TooLarge { .. } => failure(
                7,
                "oversized-input",
                "The input file exceeds its size limit",
            ),
            _ => unsupported(),
        })
}

/// What an invocation has established so far. Once input is admitted, a
/// failure is published from this record as a full unavailable decision, so
/// evaluated stages and incurred usage are never dropped.
#[derive(Default)]
struct Progress {
    admitted: Option<Admitted>,
    evaluated: Evaluated,
    metrics: ExecutionMetrics,
    /// A single-flight lease this invocation leads; completed on every path.
    lease: Option<(PathBuf, LeaderContext)>,
}

/// The admitted session and roster a full decision describes.
struct Admitted {
    event_id: String,
    harness: String,
    total: usize,
    partial: bool,
    snapshot_id: ContentHash,
    warnings: Vec<Value>,
    warnings_omitted: usize,
}

/// Assembles and executes the two-stage rank pipeline. Failures before input
/// admission are bare typed failures; later ones are full unavailable
/// decisions carrying the usage already incurred.
pub async fn execute_pipeline(
    invocation: &ProcessInvocation,
    cx: &Cx,
    mut args: RankArgs,
    transport: Option<&dyn JevTransport>,
) -> Result<OutputDocument, PipelineFailure> {
    let clock = &invocation.clock();
    // Every effect restriction comes from the gate; `args.dry_run` can only add
    // the dry-run restriction, never remove one.
    if args.dry_run && !args.gate.policy().flags().dry_run {
        let mut flags = args.gate.policy().flags();
        flags.dry_run = true;
        args.gate = EffectGate::new(flags, args.gate.scope()).map_err(|conflicts| {
            let first = conflicts
                .first()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "conflicting effect flags".to_owned());
            failure(2, "invalid-usage", first)
        })?;
    }
    let preview = args.gate.policy().flags().dry_run;
    if !preview && !args.shortlist_ids.is_empty() {
        return Err(failure(
            2,
            "invalid-usage",
            "Shortlist IDs are stage-2 evidence for --dry-run only",
        ));
    }
    let effects = args.gate.receipt();
    let mut progress = Progress::default();
    let result = rank_once(invocation, clock, cx, args, transport, &mut progress).await;
    // Release a led lease on every path. Followers then find the recorded
    // pair, or send themselves when this run recorded nothing.
    if let Some((leases, leader)) = progress.lease.take() {
        persistent::complete(invocation, cx, &leases, &leader);
    }
    // A dry run never publishes an actionable decision: a local result that
    // ends the run before any request is reported inside the preview.
    let result = match result {
        Ok(doc) if preview && matches!(doc.kind(), OutputKind::Decision(_)) => {
            preview_document(None, Some(doc.as_value().clone()), None, &effects)
        }
        result => result,
    };
    match result {
        Err(failure) => match &progress.admitted {
            Some(admitted) => {
                let evaluated = Evaluated {
                    metrics: progress.metrics.clone(),
                    ..progress.evaluated.clone()
                };
                unavailable_document(&failure, admitted, &evaluated, clock.now().as_millis())
                    .ok_or(failure)
            }
            None => Err(failure),
        },
        result => result,
    }
}

/// A full unavailable decision for a typed failure after admission, or `None`
/// when the failure has no matching public error kind and exit code.
fn unavailable_document(
    failure: &PipelineFailure,
    admitted: &Admitted,
    evaluated: &Evaluated,
    elapsed_ms: u64,
) -> Option<OutputDocument> {
    let (warnings, warnings_omitted) = (&admitted.warnings, admitted.warnings_omitted);
    let (code, kind, message) = failure;
    let kind = ErrorKind::ALL
        .iter()
        .copied()
        .find(|k| k.as_str() == *kind)?;
    if kind.exit_code() as u8 != *code {
        return None;
    }
    let error = OutputDocument::failure_with_details(
        kind,
        message,
        "Inspect local readiness and the structured error kind.",
        false,
    )
    .as_value()["error"]
        .clone();
    let val = json!({
        "schema_version": SCHEMA_VERSION,
        "event_id": admitted.event_id,
        "decision": "unavailable",
        "reason": kind.as_str(),
        "harness": admitted.harness,
        "context_quality": evaluated.quality.summary(),
        "quality": evaluated.quality.flags(),
        "roster": {
            "total": admitted.total,
            "eligible": evaluated.eligible,
            "wide_candidates": evaluated.wide,
            "shortlist": evaluated.shortlist,
            "partial": admitted.partial,
            "retrieval": evaluated.retrieval(),
            "provenance": {
                "snapshot_id": admitted.snapshot_id.as_str(),
                "policy_version": "ranking-v1",
                "wide_set_id": evaluated.wide_set_id.as_ref().map(ContentHash::as_str),
                "rerank_set_id": evaluated.rerank_set_id.as_ref().map(ContentHash::as_str),
            }
        },
        "needs_skill": evaluated.needs_skill,
        "choice_confidence": evaluated.choice_confidence,
        "none_probability": evaluated.none_probability,
        "phase": evaluated.phase,
        "skills": [],
        "omitted_rank_mass": null,
        "cache": evaluated.cache(),
        "model": evaluated.model(),
        "usage": evaluated.usage(),
        "persistence": evaluated.persistence(),
        "warnings": warnings,
        "warnings_omitted": warnings_omitted,
        "elapsed_ms": elapsed_ms,
        "error": error,
    });
    OutputDocument::from_value(val).ok()
}

async fn rank_once(
    invocation: &ProcessInvocation,
    clock: &EntryClock,
    cx: &Cx,
    args: RankArgs,
    transport: Option<&dyn JevTransport>,
    progress: &mut Progress,
) -> Result<OutputDocument, PipelineFailure> {
    clock.admit_new_work().map_err(|_| {
        failure(
            6,
            "timeout",
            "Invocation deadline exceeded before ranking start",
        )
    })?;

    // The entry point already folded `--dry-run` into the gate.
    let gate = args.gate;
    let dry_run = gate.policy().flags().dry_run;
    progress.evaluated.ledger_disabled = matches!(gate.ledger(), StoreAccess::Disabled(_));

    // 1. Initial configuration loading and policy receipt capture
    let config_files = ConfigFiles::new(args.workspace.clone(), args.user_config_root.clone());
    let resolved_config = config_files.load(clock, args.sources.clone())?;
    let mut current_receipt = resolved_config.receipt(gate.policy());

    let effective = resolved_config.effective();
    let top = effective.top() as usize;
    let shortlist = effective.shortlist() as usize;
    let gate_threshold = effective.gate();
    let fits_threshold = effective.fits();

    // Validate size invariants: 1 <= top <= shortlist <= 32
    let sizes = Sizes::new(top, shortlist).map_err(|e| {
        failure(
            2,
            "invalid-configuration",
            format!("Invalid ranking sizes: {e:?}"),
        )
    })?;

    // 2. Select and ingest context source.
    let source_policy = gate.source_policy();
    let workspace_id = WorkspaceId::new(args.workspace.to_string_lossy().as_ref())
        .map_err(|_| failure(2, "invalid-configuration", "Invalid workspace root path"))?;

    let selection_outcome = args
        .source_options
        .clone()
        .resolve(
            workspace_id.clone(),
            source_policy,
            false, // non-interactive by default
            |_, _| {
                // Inventory discovery fallback if needed
                Ok(crate::context::source::SessionInventory::default())
            },
        )
        .map_err(|err| {
            failure(
                err.kind().exit_code() as u8,
                err.kind().as_str(),
                err.to_string(),
            )
        })?;

    let source_selection = match selection_outcome {
        SelectionOutcome::Selected(sel) => sel,
        SelectionOutcome::NeedsChoice(_) => {
            return Err(failure(
                2,
                "ambiguous-session",
                "Multiple sessions available; selection required",
            ));
        }
    };

    // Ingest normalized context or native transcript
    let normalized_context = match source_selection.target() {
        SourceTarget::NormalizedFile(path) => {
            let bytes = read_input_file(
                &args.workspace,
                path.as_path(),
                crate::limits::NORMALIZED_CONTEXT_JSON_BYTES,
            )?;
            parse_normalized_context(&bytes).map_err(|e| {
                failure(
                    7,
                    "malformed-input",
                    format!("Invalid normalized context: {e}"),
                )
            })?
        }
        SourceTarget::NormalizedStdin => {
            let limit = crate::limits::NORMALIZED_CONTEXT_JSON_BYTES.max();
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(
                &mut std::io::Read::take(std::io::stdin().lock(), limit as u64 + 1),
                &mut bytes,
            )
            .map_err(|_| failure(7, "malformed-input", "Failed to read context from stdin"))?;
            if bytes.len() > limit {
                return Err(failure(
                    7,
                    "oversized-input",
                    "Normalized context on stdin exceeds 1 MiB",
                ));
            }
            parse_normalized_context(&bytes).map_err(|e| {
                failure(
                    7,
                    "malformed-input",
                    format!("Invalid normalized context: {e}"),
                )
            })?
        }
        SourceTarget::ClaudeTranscript(path) => {
            // Snapshot JSONL transcript
            let invocation = ProcessInvocation::from_clock(*clock)
                .map_err(|_| failure(6, "timeout", "Deadline exceeded creating runtime"))?;
            let req_cx = invocation
                .request_cx()
                .map_err(|_| failure(6, "timeout", "Deadline exceeded creating context"))?;
            let snapshot = snapshot_jsonl(
                &invocation,
                &req_cx,
                path.as_path(),
                None,
                CursorKind::Ranking,
            )
            .map_err(|e| {
                failure(
                    7,
                    "malformed-input",
                    format!("Transcript snapshot failed: {e}"),
                )
            })?;

            let events = snapshot.events;
            let current_req_text = events
                .iter()
                .rev()
                .find(|e| e.role == crate::context::Role::User)
                .map(|e| e.text.clone())
                .unwrap_or_else(|| PrivateText::new(""));

            NormalizedContext {
                schema_version: 1,
                harness: HarnessId::new("claude_code").unwrap(),
                producer_id: None,
                workspace_root: PrivateText::new(args.workspace.to_string_lossy()),
                session_id: Some(SessionId::new("session-0").unwrap()),
                agent_id: None,
                branch_id: None,
                context_epoch: None,
                current_request: CurrentRequest {
                    event_id: Some(EventId::new("req-0").unwrap()),
                    text: current_req_text,
                    attachments_omitted: false,
                    essential_attachment_missing: false,
                },
                events,
                explicit_skill_references: Vec::new(),
                supplied_loads: Vec::new(),
            }
        }
        SourceTarget::ClaudeHookStdin => {
            return Err(failure(
                2,
                "unsupported-source-mode",
                "Use sr hook claude for hook protocol stdin mode",
            ));
        }
        SourceTarget::CassSession(_) => {
            return Err(failure(
                7,
                "unsupported-source-mode",
                "Cass session mode unavailable in this execution lane",
            ));
        }
    };

    // 3. Resolve active branch & task anchor
    let branch_target = crate::context::branch::BranchResolutionTarget {
        target_event_id: normalized_context.current_request.event_id.clone(),
        target_branch_id: normalized_context.branch_id.clone(),
        target_agent_id: normalized_context.agent_id.clone(),
    };
    let resolved_branch = resolve_active_branch(&normalized_context.events, &branch_target);
    let active_branch = resolved_branch.active_branch();

    let redactor = Redactor::default();
    let anchor_res = resolve_task_anchor(&normalized_context, None, &redactor);
    // An evaluation needs a task anchor. A terse continuation without an
    // antecedent, an uninspectable request or contradictory directives never
    // reaches ranking.
    let task_anchor_text = match &anchor_res {
        crate::context::anchor::AnchorResolution::Established(a) => a.text.as_str(),
        crate::context::anchor::AnchorResolution::MissingTaskContext { .. } => {
            return Err(input_failure(
                ErrorKind::InsufficientContext,
                "The request continues a task whose instruction is not in the context",
            ));
        }
        crate::context::anchor::AnchorResolution::OversizedInput { .. } => {
            return Err(input_failure(
                ErrorKind::OversizedInput,
                "The request is too large to inspect",
            ));
        }
        crate::context::anchor::AnchorResolution::ConflictingDirectives { .. } => {
            return Err(input_failure(
                ErrorKind::UnresolvedExplicit,
                "The context both requires and excludes the same skill",
            ));
        }
    };
    let has_history = normalized_context
        .events
        .iter()
        .any(|e| e.event_id.is_none() || e.event_id != normalized_context.current_request.event_id);
    progress.evaluated.quality = Quality {
        summary: if has_history {
            crate::output::ContextQuality::Complete
        } else {
            crate::output::ContextQuality::PromptOnly
        },
        prompt_complete: !normalized_context
            .current_request
            .essential_attachment_missing,
        attachments_omitted: normalized_context.current_request.attachments_omitted,
        ..Quality::default()
    };

    // Combine explicit skill directives from CLI flags, context, and anchor
    let mut explicit_directives = args.require_skills.clone();
    for ref_id in &normalized_context.explicit_skill_references {
        if !explicit_directives.contains(ref_id) {
            explicit_directives.push(ref_id.clone());
        }
    }
    if let crate::context::anchor::AnchorResolution::Established(ref a) = anchor_res {
        for directive in &a.directives {
            if directive.kind == crate::context::anchor::AnchorDirectiveKind::Require
                && let Ok(id) = SkillId::new(&directive.target)
                && !explicit_directives.contains(&id)
            {
                explicit_directives.push(id);
            }
        }
    }

    // 4. Discover Roster. Claude's documented precedence (project skills over
    // personal ones) resolves collisions, but no conformance evidence verifies
    // it yet. Rank uses it under an explicit provisional label and says so in
    // every result; withheld, ambiguous and shadowed names stay excluded.
    let visibility = Visibility::Verified {
        contract_version: PROVISIONAL_CLAUDE_CONTRACT.into(),
    };
    let overrides = BTreeMap::new();
    let roster = if let Some(roster_path) = &args.roster_file {
        let roster_path = if roster_path.is_absolute() {
            roster_path.clone()
        } else {
            args.workspace.join(roster_path)
        };
        let bytes = crate::roster::import::read_roster_file(&roster_path).map_err(|e| {
            failure(
                5,
                "unusable-roster",
                format!("Failed to read roster file: {e}"),
            )
        })?;
        let plan = claude_code_plan(&args.workspace, args.home.as_deref(), visibility.clone())
            .map_err(|e| {
                failure(
                    5,
                    "unusable-roster",
                    format!("Failed to create discovery plan: {e}"),
                )
            })?;
        import_authorized(&bytes, &plan, &overrides, cx, clock).map_err(|e| {
            failure(
                5,
                "unusable-roster",
                format!("Failed to import roster: {e:?}"),
            )
        })?
    } else {
        let plan = claude_code_plan(&args.workspace, args.home.as_deref(), visibility.clone())
            .map_err(|e| {
                failure(
                    5,
                    "unusable-roster",
                    format!("Failed to create discovery plan: {e}"),
                )
            })?;
        resolve_claude_plan(&plan, &overrides, cx, clock).map_err(|e| {
            failure(
                5,
                "unusable-roster",
                format!("Failed to resolve discovery plan: {e}"),
            )
        })?
    };

    let (warnings, warnings_omitted) = roster_warnings(&roster);
    progress.admitted = Some(Admitted {
        event_id: normalized_context
            .current_request
            .event_id
            .as_ref()
            .map_or_else(|| "event-0".to_owned(), |e| e.as_str().to_owned()),
        harness: normalized_context.harness.as_str().to_owned(),
        total: roster.skills().len(),
        partial: roster.is_partial(),
        snapshot_id: crate::roster::evidence::snapshot_id(&roster),
        warnings,
        warnings_omitted,
    });

    // 5. Explicit Directives Check (bypasses Jev and Quill)
    if !explicit_directives.is_empty() {
        let explicit_req = ExplicitResolutionRequest {
            cli_required_skills: explicit_directives
                .iter()
                .map(|id| id.as_str().to_string())
                .collect(),
            cli_excluded_skills: Vec::new(),
            context_skill_references: normalized_context.explicit_skill_references.clone(),
            context_excluded_skills: Vec::new(),
            user_prompt: Some(normalized_context.current_request.text.as_str().to_string()),
        };
        let explicit_result =
            resolve_explicit_requirements(&explicit_req, &roster).map_err(|e| {
                failure(
                    2,
                    "invalid-usage",
                    format!("Explicit resolution error: {e}"),
                )
            })?;

        match explicit_result {
            ExplicitResolutionResult::Resolved { skills, .. } => {
                // Revalidate policy receipt before publication of explicit result
                let (_refreshed, reval) = config_files.refresh(
                    clock,
                    &resolved_config,
                    &current_receipt,
                    PolicyBoundary::CliPublication(PublicationKind::Explicit),
                )?;
                if let Revalidation::Superseded(fields) = reval {
                    return Err(failure(
                        3,
                        "superseded",
                        format!("Policy changed during evaluation: {fields:?}"),
                    ));
                }
                if let Revalidation::InvalidConfiguration = reval {
                    return Err(failure(
                        2,
                        "invalid-configuration",
                        "Configuration became invalid before publication",
                    ));
                }
                let mut doc = build_explicit_document(
                    &skills,
                    &normalized_context,
                    &roster,
                    clock.now().as_millis(),
                    &progress.evaluated,
                );
                if let Some(trace_val) = generate_trace(
                    &args,
                    &roster,
                    &normalized_context,
                    None,
                    &RetrievalView::NotEvaluated,
                    None,
                    gate_threshold,
                    None,
                    fits_threshold,
                    None,
                    None,
                    true,
                ) {
                    doc = doc.with_trace(trace_val).map_err(|e| {
                        failure(
                            5,
                            "contract-violation",
                            format!("Trace contract error: {e:?}"),
                        )
                    })?;
                }
                return Ok(doc);
            }
            ExplicitResolutionResult::Unavailable { unresolved } => {
                let msg = if let Some(first) = unresolved.first() {
                    format!("Unresolved explicit skill: {}", first.target)
                } else {
                    "Unresolved explicit skill".to_string()
                };
                let unresolved_refs: Vec<crate::output::UnresolvedReference> = unresolved
                    .iter()
                    .map(|u| {
                        let reason = match u.reason {
                            crate::roster::explicit::UnresolvedReason::Missing => {
                                crate::output::UnresolvedReason::Missing
                            }
                            crate::roster::explicit::UnresolvedReason::Ambiguous => {
                                crate::output::UnresolvedReason::Ambiguous
                            }
                            crate::roster::explicit::UnresolvedReason::Forbidden
                            | crate::roster::explicit::UnresolvedReason::Shadowed
                            | crate::roster::explicit::UnresolvedReason::ConflictingDirective
                            | crate::roster::explicit::UnresolvedReason::Unverified
                            | crate::roster::explicit::UnresolvedReason::InvalidName => {
                                crate::output::UnresolvedReason::Restricted
                            }
                        };
                        crate::output::UnresolvedReference {
                            reference: u.target.clone(),
                            reason,
                        }
                    })
                    .collect();
                let doc = OutputDocument::failure_with_details(
                    ErrorKind::UnresolvedExplicit,
                    &msg,
                    "Inspect available skills in the roster or configure skill roots.",
                    false,
                )
                .with_unresolved(unresolved_refs)
                .map_err(|e| failure(5, "unresolved-explicit", format!("Contract error: {e:?}")))?;
                return Ok(doc);
            }
            ExplicitResolutionResult::NoneSpecified { .. } => {}
        }
    }

    // 6. Advisory candidate admission & local policy filtering
    let loaded_records: Vec<LoadedSkillRecord> = Vec::new();
    let loaded_state = LoadedState {
        branch: active_branch,
        records: &loaded_records,
    };

    let mut excluded_skills = BTreeSet::new();
    for ex in effective.exclude_skills() {
        if let Ok(id) = SkillId::new(ex.as_str()) {
            excluded_skills.insert(id);
        }
        for skill in roster.skills() {
            if skill.record().display_name.as_str() == ex.as_str()
                || skill
                    .bindings()
                    .iter()
                    .any(|b| b.invocation.as_str() == ex.as_str())
            {
                excluded_skills.insert(skill.record().id.clone());
                for b in skill.bindings() {
                    excluded_skills.insert(b.id.clone());
                }
            }
        }
    }
    let excluded_refs: BTreeSet<&SkillId> = excluded_skills.iter().collect();
    let loaded_refs: BTreeSet<&SkillId> = BTreeSet::new();
    let policy_view = PolicyView {
        excluded: &excluded_refs,
        already_loaded: &loaded_refs,
    };

    // Roster skills as AdvisorySkills
    let mut initial_advisory: Vec<AdvisorySkill> = Vec::new();
    for skill in roster.skills() {
        for binding in skill.bindings() {
            if matches!(binding.visibility, Visibility::Verified { .. }) {
                initial_advisory.push(AdvisorySkill {
                    record: skill.record(),
                    binding,
                });
            }
        }
    }

    if initial_advisory.is_empty() {
        let doc = OutputDocument::failure_with_details(
            ErrorKind::EmptyRoster,
            "No eligible skills found in roster",
            "Check skill directory paths or frontmatter syntax.",
            false,
        );
        return Ok(doc);
    }

    // Initial admission before Wide
    let admission = admit(&initial_advisory, &excluded_refs, loaded_state);
    if let Some(verdict) = admission.verdict {
        match verdict {
            Verdict::Abstain(reason) => {
                let mut doc = build_abstain_document(
                    reason.as_str(),
                    &normalized_context,
                    &roster,
                    clock.now().as_millis(),
                    None,
                    &Evaluated {
                        eligible: admission.admitted.len(),
                        ..progress.evaluated.clone()
                    },
                );
                if let Some(trace_val) = generate_trace(
                    &args,
                    &roster,
                    &normalized_context,
                    Some(&policy_view),
                    &RetrievalView::NotEvaluated,
                    None,
                    gate_threshold,
                    None,
                    fits_threshold,
                    None,
                    None,
                    false,
                ) {
                    doc = doc.with_trace(trace_val).map_err(|e| {
                        failure(
                            5,
                            "contract-violation",
                            format!("Trace contract error: {e:?}"),
                        )
                    })?;
                }
                return Ok(doc);
            }
            Verdict::Unavailable(reason) => {
                let doc = OutputDocument::failure_with_details(
                    reason.kind(),
                    &format!("Candidate unavailable: {}", reason.as_str()),
                    "Check candidate eligibility and loaded state.",
                    false,
                );
                return Ok(doc);
            }
        }
    }

    let eligible_count = admission.admitted.len();
    progress.evaluated.eligible = eligible_count;

    // 7. Bounded Quill retrieval if > 254 candidates
    let (candidate_skills, ran_quill, quill_method) = if admission.admitted.len()
        > wide::MAX_REAL_OPTIONS
    {
        let query_input = QueryInput {
            latest_request: normalized_context.current_request.text.as_str(),
            active_task: task_anchor_text,
            recent_errors: "",
        };
        let budget = RetrievalBudget::default();
        let selection = retrieve(&roster, &excluded_skills, query_input, budget, cx, clock)
            .await
            .map_err(|err| match err.kind {
                RetrievalError::RetrievalEmpty => {
                    failure(5, "retrieval-empty", "Quill retrieval yielded 0 matches")
                }
                RetrievalError::Deadline => failure(6, "timeout", "Retrieval exceeded deadline"),
                _ => failure(5, "retrieval-failure", format!("Retrieval error: {err}")),
            })?;
        (selection.candidates, true, selection.diagnostics.method)
    } else {
        (admission.admitted, false, None)
    };

    let admitted_ids: BTreeSet<&SkillId> = candidate_skills.iter().map(|s| &s.binding.id).collect();
    let retrieval_view = if ran_quill {
        RetrievalView::Admitted {
            method: quill_method.unwrap_or(RetrievalMethod::FullRoster),
            ids: admitted_ids,
        }
    } else {
        RetrievalView::NotEvaluated
    };

    if candidate_skills.is_empty() {
        let mut doc = build_abstain_document(
            "no-shortlist-match",
            &normalized_context,
            &roster,
            clock.now().as_millis(),
            None,
            &Evaluated {
                eligible: eligible_count,
                quill: ran_quill,
                ..progress.evaluated.clone()
            },
        );
        if let Some(trace_val) = generate_trace(
            &args,
            &roster,
            &normalized_context,
            Some(&policy_view),
            &retrieval_view,
            None,
            gate_threshold,
            None,
            fits_threshold,
            None,
            None,
            false,
        ) {
            doc = doc.with_trace(trace_val).map_err(|e| {
                failure(
                    5,
                    "contract-violation",
                    format!("Trace contract error: {e:?}"),
                )
            })?;
        }
        return Ok(doc);
    }

    // 8. Capture roster dependencies for final revalidation
    let mut content_scope = BTreeSet::new();
    for candidate in &candidate_skills {
        content_scope.insert(candidate.binding.id.clone());
    }
    let dependencies = capture(&roster, &content_scope, clock).map_err(|e| {
        failure(
            5,
            "unusable-roster",
            format!("Failed to capture dependencies: {e:?}"),
        )
    })?;

    // 9. Render context payload for Jev
    // Optional local project signals: filename markers, allowlisted tools in
    // fixed root-owned system directories (never PATH or repository-supplied
    // roots) and bounded repository-relative dirty paths from the safe Git
    // helper. Rendering redacts them and the profile may omit them.
    let tool_roots: Vec<PathBuf> = TRUSTED_TOOL_ROOTS.iter().map(PathBuf::from).collect();
    let signals = crate::context::signals::collect(cx, clock, &args.workspace, &tool_roots).await;
    let render_opts = RenderContextOptions {
        no_tools: effective.no_tools(),
        context_profile: effective.context_profile(),
        max_messages: effective.messages() as usize,
        max_total_scalars: effective.budget_chars() as usize,
        redactor,
        project_signals: Some(&signals),
        ..Default::default()
    };
    let (rendered_context, disclosure_receipt) =
        render_context_and_receipt(&normalized_context, &render_opts).map_err(|e| match e {
            crate::context::render::RenderContextError::UnsupportedContext(_) => input_failure(
                ErrorKind::InsufficientContext,
                "The request depends on content that is not in the context",
            ),
            crate::context::render::RenderContextError::Redaction(_)
            | crate::context::render::RenderContextError::SecretsDetected(_) => input_failure(
                ErrorKind::UnsupportedInput,
                "The context could not be made safe to send",
            ),
            crate::context::render::RenderContextError::Serialization(_) => input_failure(
                ErrorKind::OversizedInput,
                "The context could not be rendered within its bounds",
            ),
        })?;
    // Report the rendered input truthfully. Essential content that is missing,
    // or a latest request that had to be truncated, cannot support a ranked
    // result, so nothing is sent.
    let category = |c: crate::privacy::receipt::SourceCategory| {
        disclosure_receipt
            .categories
            .iter()
            .find(|r| r.category == c)
            .map_or((0, 0), |r| (r.omitted_count, r.truncated_count))
    };
    let (_, request_truncated) = category(crate::privacy::receipt::SourceCategory::UserRequest);
    let (history_omitted, history_truncated) =
        category(crate::privacy::receipt::SourceCategory::MessageHistory);
    progress.evaluated.quality.summary = rendered_context.context_quality;
    progress.evaluated.quality.prompt_complete &= request_truncated == 0;
    progress.evaluated.quality.history_windowed = history_omitted + history_truncated > 0;
    if rendered_context.is_unsupported_context() || request_truncated > 0 {
        return Err(input_failure(
            ErrorKind::InsufficientContext,
            "The request or its essential context does not fit the admitted input",
        ));
    }

    // 10. The exact response cache. Persistent entries need a trusted cache
    // directory, an enabled response-cache effect and a session identity to
    // scope them. Otherwise fingerprints are keyed with fresh randomness and
    // nothing outlives this invocation. An unusable store degrades to that.
    let mut store = match (&args.cache_dir, &normalized_context.session_id) {
        (Some(dir), Some(_)) => persistent::Store::open(invocation, cx, &gate, dir),
        _ => None,
    };
    let cache_key = match &store {
        Some(store) => store.key(),
        None => CacheKey::generate().map_err(|_| {
            failure(
                9,
                "storage-failure",
                "Fingerprint key could not be generated",
            )
        })?,
    };
    let mut cache_ns = CacheNamespace::new(
        normalized_context.harness.clone(),
        store.as_ref().map_or(0, persistent::Store::generation),
    )
    .with_workspace(workspace_id.clone());
    if let Some(id) = &normalized_context.session_id {
        cache_ns = cache_ns.with_session(id.clone());
    }
    if let Some(id) = &normalized_context.branch_id {
        cache_ns = cache_ns.with_branch(id.clone());
    }
    if let Some(epoch) = &normalized_context.context_epoch {
        cache_ns = cache_ns.with_context_epoch(epoch.clone());
    }
    let namespace = MemoryResponseCache::namespace_hash(&cache_key, &cache_ns);

    let candidate_ids: Vec<SkillId> = candidate_skills
        .iter()
        .map(|s| s.binding.id.clone())
        .collect();
    let candidate_digests: Vec<CandidateDigest> = candidate_skills
        .iter()
        .map(|s| CandidateDigest {
            skill_id: s.binding.id.clone(),
            content_hash: s.record.source_content.clone(),
            excerpt_hash: None,
        })
        .collect();

    // One transport, credential binding and attempt allowance per invocation:
    // at most two logical requests and four HTTP attempts, with classified
    // retries inside the entry deadline. Opened only when a send is due, so
    // refused runs never construct a client.
    let mut owned_client: Option<JevClient> = None;
    let mut bound_credential: Option<OriginScopedCredential> = None;
    let mut session: Option<RetrySession<'_>> = None;

    // 11. Stage 1 (Wide) Call or Cache Hit
    let wide_builder = wide::build(
        &roster,
        &candidate_ids,
        &rendered_context,
        effective.model().as_str(),
        false, // include_stuck
    )
    .map_err(|e| {
        failure(
            e.kind().exit_code() as u8,
            e.kind().as_str(),
            format!("Wide build failed: {e:?}"),
        )
    })?;

    // A stateless preview: the exact redacted bytes a matching `--no-persist`
    // run would send. Stage 2 is previewed only for supplied shortlist IDs.
    if dry_run {
        let mut stages = vec![preview_stage(
            "wide",
            wide_builder.bytes(),
            candidate_ids.len(),
        )?];
        if !args.shortlist_ids.is_empty() {
            let (_, shortlist) = sizes.effective(candidate_ids.len());
            let mut seen = BTreeSet::new();
            if args.shortlist_ids.len() > shortlist
                || args
                    .shortlist_ids
                    .iter()
                    .any(|id| !candidate_ids.contains(id) || !seen.insert(id))
            {
                return Err(failure(
                    2,
                    "invalid-usage",
                    format!("Shortlist IDs must be distinct wide candidates, at most {shortlist}"),
                ));
            }
            let rerank_builder = rerank::build(
                &roster,
                &args.shortlist_ids,
                &rendered_context,
                effective.model().as_str(),
            )
            .map_err(|e| {
                failure(
                    e.kind().exit_code() as u8,
                    e.kind().as_str(),
                    format!("Rerank build failed: {e:?}"),
                )
            })?;
            stages.push(preview_stage(
                "rerank",
                rerank_builder.bytes(),
                args.shortlist_ids.len(),
            )?);
        }
        let disclosure = serde_json::to_value(&disclosure_receipt).map_err(|_| {
            let kind = ErrorKind::OutputLimit;
            failure(
                kind.exit_code() as u8,
                kind.as_str(),
                "Disclosure receipt could not be encoded",
            )
        })?;
        return preview_document(
            Some(json!({
                "model": effective.model().as_str(),
                "stages": stages,
                "trimming": {
                    "dropped_messages": wide_builder.trimming().dropped_messages,
                    "description_cap": wide_builder.trimming().description_cap,
                    "omitted_description_scalars":
                        wide_builder.trimming().omitted_description_scalars,
                    "redactions": wide_builder.trimming().redactions,
                },
            })),
            None,
            Some(disclosure),
            &gate.receipt(),
        );
    }

    // From here on the wide candidate set is committed to a request.
    progress.evaluated = Evaluated {
        eligible: eligible_count,
        wide: candidate_skills.len(),
        quill: ran_quill,
        wide_set_id: Some(candidate_set_id("wide", candidate_skills.iter())),
        requested_model: Some(effective.model().as_str().to_owned()),
        ..progress.evaluated.clone()
    };

    // The request identity binds the exact serialized request (context,
    // questions and options) and the endpoint it would be sent to.
    let endpoint = match effective.endpoint() {
        Some(ep) => EndpointConfig::from_override(ep).map_err(|_| {
            failure(
                2,
                "invalid-configuration",
                "The configured endpoint is invalid",
            )
        })?,
        None => EndpointConfig::production(),
    };
    let canonical_state = rendered_context.to_json_bytes().unwrap_or_default();
    let active_model = effective.model().as_str();
    let fingerprint = |stage: RequestStage,
                       candidates: &[CandidateDigest],
                       request: &[u8],
                       version: &str|
     -> RequestFingerprint {
        compute_request_fingerprint(
            &cache_key,
            &cache_ns,
            &RequestFingerprintInput {
                stage,
                canonical_redacted_state: &canonical_state,
                candidates,
                questions_digest: *blake3::hash(request).as_bytes(),
                endpoint_url: endpoint.target_url().as_str(),
                model: active_model,
                prompt_version: version,
                adapter_version: "v1",
                privacy_policy_version: "v1",
                excerpt_strategy: "default",
            },
        )
    };
    let wide_req_fp = fingerprint(
        RequestStage::Wide,
        &candidate_digests,
        wide_builder.bytes(),
        wide::WIDE_POLICY_VERSION,
    );

    // An exact cached pair is served without a send. A cached wide answer is
    // used only when it needs no rerank, or when the rerank answer for its
    // shortlist is cached too: under an unpinned model alias a cached wide
    // answer is never paired with a fresh rerank.
    let lookup_pair =
        |store: &mut Option<persistent::Store>| -> Option<(Response, u64, Option<Response>)> {
            let now_unix_ms = wall_clock_ms();
            let (bytes, age_ms) = persistent::lookup(
                store,
                invocation,
                cx,
                namespace,
                RequestStage::Wide,
                wide_req_fp,
                active_model,
                now_unix_ms,
            )?;
            let response = wide_builder.request().decode_response(&bytes).ok()?;
            let outcome = wide::evaluate(&wide_builder, &response, gate_threshold, sizes).ok()?;
            let mut rerank = None;
            if let WideDecision::Shortlist(list) = &outcome.decision {
                let ids: Vec<SkillId> = list.iter().map(|s| s.skill.binding.id.clone()).collect();
                let builder = rerank::build(&roster, &ids, &rendered_context, active_model).ok()?;
                let rerank_fp = fingerprint(
                    RequestStage::Rerank,
                    &shortlist_digests(list),
                    builder.bytes(),
                    rerank::RERANK_POLICY_VERSION,
                );
                let (bytes, _) = persistent::lookup(
                    store,
                    invocation,
                    cx,
                    namespace,
                    RequestStage::Rerank,
                    rerank_fp,
                    active_model,
                    now_unix_ms,
                )?;
                rerank = Some(builder.request().decode_response(&bytes).ok()?);
            }
            Some((response, age_ms, rerank))
        };
    let mut cached = lookup_pair(&mut store);
    // Single flight: on a miss, one process per exact request sends. Another
    // process with the same request waits for that lease and is then served
    // the pair it recorded; if the leader fails, the follower sends itself.
    // Leases need the persistent cache and persistent runtime state.
    if cached.is_none()
        && store.is_some()
        && matches!(gate.runtime_state(), StoreAccess::Enabled)
        && let Some(dir) = &args.cache_dir
    {
        let leases = dir.join(persistent::LEASES_FILE);
        let key = CoordinationKey::compute(&cache_key, &cache_ns, &wide_req_fp);
        match persistent::acquire(invocation, cx, &leases, key) {
            Some(LeaseAcquisition::Leading(leader)) => progress.lease = Some((leases, leader)),
            Some(LeaseAcquisition::Following(follower)) => {
                persistent::wait_for_leader(
                    invocation,
                    cx,
                    clock,
                    &leases,
                    key,
                    follower.lease_expires_at_unix_ms,
                )
                .await;
                cached = lookup_pair(&mut store);
            }
            Some(LeaseAcquisition::AlreadyCompleted) => cached = lookup_pair(&mut store),
            None => {}
        }
    }
    let mut cached_rerank: Option<Response> = None;
    let wide_fresh = cached.is_none();
    let cached = cached.map(|(response, age_ms, rerank)| {
        progress.metrics.cache_hit = true;
        progress.metrics.wide_hit = true;
        progress.metrics.cache_age_ms = Some(age_ms);
        cached_rerank = rerank;
        response
    });
    let wide_response = match cached {
        Some(response) => response,
        None => {
            // Refuse before building a client when policy forbids any send.
            authorize_send(
                clock,
                &config_files,
                &resolved_config,
                &mut current_receipt,
                &gate,
                RankingStage::Wide,
            )?;
            let client: &dyn JevTransport = match transport {
                Some(transport) => transport,
                None => &*owned_client.insert(JevClient::new(endpoint.clone()).map_err(|e| {
                    failure(
                        2,
                        "invalid-configuration",
                        format!("Client init failed: {e}"),
                    )
                })?),
            };
            let credential =
                match (resolved_config.credential(), client.origin()) {
                    (Some(credential), Some(origin)) => Some(&*bound_credential.insert(
                        OriginScopedCredential::bind(credential.clone(), origin).map_err(|_| {
                            failure(4, "authentication", "Credential origin mismatch")
                        })?,
                    )),
                    _ => None,
                };
            let active = session.insert(
                RetrySession::new(
                    client,
                    credential,
                    *clock,
                    AttemptBudget::default_invocation(),
                    "rank",
                )
                .map_err(|_| {
                    let kind = ErrorKind::BudgetState;
                    failure(
                        kind.exit_code() as u8,
                        kind.as_str(),
                        "The attempt allowance could not be opened",
                    )
                })?,
            );
            provider_stage(
                active,
                RankingStage::Wide,
                wide_builder.request(),
                &config_files,
                &resolved_config,
                &mut current_receipt,
                &gate,
                &mut progress.metrics,
                cx,
                clock,
            )
            .await?
        }
    };

    // Evaluate Wide Response
    let wide_outcome = wide::evaluate(&wide_builder, &wide_response, gate_threshold, sizes)
        .map_err(|e| {
            failure(
                10,
                "invalid-provider-response",
                format!("Wide evaluation failed: {e:?}"),
            )
        })?;
    if wide_fresh {
        persistent::record(
            &mut store,
            invocation,
            cx,
            namespace,
            cache_entry(
                RequestStage::Wide,
                wide_req_fp,
                &wide_response,
                active_model,
            ),
        );
    }
    progress.evaluated.wide_returned = Some(wide_response.returned_model.clone());
    progress.evaluated.needs_skill = Some(wide_outcome.needs_skill);
    progress.evaluated.phase = dominant_phase(&wide_outcome.phase).map(str::to_owned);

    let shortlisted = match &wide_outcome.decision {
        WideDecision::LowNeed => {
            // Revalidate policy before emitting LowNeed abstention
            let (refreshed, reval) = config_files.refresh(
                clock,
                &resolved_config,
                &current_receipt,
                PolicyBoundary::CliPublication(PublicationKind::Advisory),
            )?;
            if let Revalidation::Superseded(fields) = reval {
                return Err(failure(
                    3,
                    "superseded",
                    format!("Policy changed during evaluation: {fields:?}"),
                ));
            }
            if let Revalidation::InvalidConfiguration = reval {
                return Err(failure(
                    2,
                    "invalid-configuration",
                    "Configuration became invalid before publication",
                ));
            }
            let _ = refreshed;

            let mut doc = build_abstain_document(
                "low-need",
                &normalized_context,
                &roster,
                clock.now().as_millis(),
                None,
                &Evaluated {
                    metrics: progress.metrics.clone(),
                    ..progress.evaluated.clone()
                },
            );
            if let Some(trace_val) = generate_trace(
                &args,
                &roster,
                &normalized_context,
                Some(&policy_view),
                &retrieval_view,
                Some(&wide_outcome),
                gate_threshold,
                None,
                fits_threshold,
                None,
                None,
                false,
            ) {
                doc = doc.with_trace(trace_val).map_err(|e| {
                    failure(
                        5,
                        "contract-violation",
                        format!("Trace contract error: {e:?}"),
                    )
                })?;
            }
            return Ok(doc);
        }
        WideDecision::Shortlist(list) => list.clone(),
    };

    let shortlist_ids: Vec<SkillId> = shortlisted
        .iter()
        .map(|s| s.skill.binding.id.clone())
        .collect();

    // 12. Stage 2 (Rerank). The shortlist is committed to a request.
    progress.evaluated.shortlist = shortlisted.len();
    progress.evaluated.rerank_set_id = Some(candidate_set_id(
        "rerank",
        shortlisted.iter().map(|s| &s.skill),
    ));
    let rerank_builder = rerank::build(
        &roster,
        &shortlist_ids,
        &rendered_context,
        effective.model().as_str(),
    )
    .map_err(|e| {
        failure(
            e.kind().exit_code() as u8,
            e.kind().as_str(),
            format!("Rerank build failed: {e:?}"),
        )
    })?;

    // A cached wide answer arrives with its cached rerank answer. Otherwise
    // rerank pairs with the wide answer this session produced; a wide answer
    // from elsewhere is never paired with a fresh rerank.
    let rerank_fresh = cached_rerank.is_none();
    let rerank_response = match cached_rerank.take() {
        Some(response) => {
            progress.metrics.rerank_hit = true;
            response
        }
        None => {
            let Some(active) = session.as_mut() else {
                return Err(failure(
                    11,
                    "cache-miss",
                    "A cached wide answer cannot be paired with a fresh rerank",
                ));
            };
            provider_stage(
                active,
                RankingStage::Rerank,
                rerank_builder.request(),
                &config_files,
                &resolved_config,
                &mut current_receipt,
                &gate,
                &mut progress.metrics,
                cx,
                clock,
            )
            .await?
        }
    };

    let rerank_outcome = rerank::evaluate(&rerank_builder, &rerank_response).map_err(|e| {
        failure(
            10,
            "invalid-provider-response",
            format!("Rerank evaluation failed: {e:?}"),
        )
    })?;

    if rerank_fresh {
        let rerank_fp = fingerprint(
            RequestStage::Rerank,
            &shortlist_digests(&shortlisted),
            rerank_builder.bytes(),
            rerank::RERANK_POLICY_VERSION,
        );
        persistent::record(
            &mut store,
            invocation,
            cx,
            namespace,
            cache_entry(
                RequestStage::Rerank,
                rerank_fp,
                &rerank_response,
                active_model,
            ),
        );
    }
    progress.evaluated.rerank_returned = Some(rerank_response.returned_model.clone());
    progress.evaluated.choice_confidence = Some(rerank_outcome.choice_confidence);
    progress.evaluated.none_probability = Some(rerank_outcome.none_probability);

    // 13. Local Eligibility & Fit Filtering after Rerank
    let shortlisted_advisory: Vec<AdvisorySkill> = shortlisted.iter().map(|s| s.skill).collect();
    let raw_estimates = rerank_outcome.estimates();

    let evaluation = after_rerank(
        &shortlisted_advisory,
        &raw_estimates,
        rerank_outcome.none_probability,
        fits_threshold,
        &excluded_refs,
        loaded_state,
    );

    if let Some(verdict) = evaluation.verdict {
        match verdict {
            Verdict::Abstain(reason) => {
                let mut doc = build_abstain_document(
                    reason.as_str(),
                    &normalized_context,
                    &roster,
                    clock.now().as_millis(),
                    None,
                    &Evaluated {
                        metrics: progress.metrics.clone(),
                        ..progress.evaluated.clone()
                    },
                );
                if let Some(trace_val) = generate_trace(
                    &args,
                    &roster,
                    &normalized_context,
                    Some(&policy_view),
                    &retrieval_view,
                    Some(&wide_outcome),
                    gate_threshold,
                    Some(&rerank_outcome),
                    fits_threshold,
                    Some(&evaluation),
                    None,
                    false,
                ) {
                    doc = doc.with_trace(trace_val).map_err(|e| {
                        failure(
                            5,
                            "contract-violation",
                            format!("Trace contract error: {e:?}"),
                        )
                    })?;
                }
                return Ok(doc);
            }
            Verdict::Unavailable(reason) => {
                let doc = OutputDocument::failure_with_details(
                    reason.kind(),
                    &format!("Candidate unavailable after rerank: {}", reason.as_str()),
                    "Check candidate eligibility after rerank.",
                    false,
                );
                return Ok(doc);
            }
        }
    }

    // 14. Scoring of surviving eligible candidates
    let scoring_inputs: Vec<ScoringInput> = evaluation
        .eligible
        .iter()
        .map(ScoringInput::from_eligible)
        .collect();

    let weights = Weights::new(effective.w_fit(), effective.w_prior(), effective.w_phase())
        .map_err(|_| {
            failure(
                2,
                "invalid-configuration",
                "Ranking weights are out of bounds",
            )
        })?;
    let scored_ranking = rank(&scoring_inputs, weights, top).map_err(|e| {
        failure(
            10,
            "invalid-provider-response",
            format!("Scoring failed: {e:?}"),
        )
    })?;

    // 15. Final Revalidation Before Publication
    // a. Roster dependencies
    let reval_outcome = revalidate_claude(
        &dependencies,
        &args.workspace,
        args.home.as_deref(),
        visibility,
        &overrides,
        cx,
        clock,
    );
    match reval_outcome {
        Ok(_) => {}
        Err(RevalidationError::Changed) => {
            return Err(failure(
                5,
                "roster-changed",
                "Roster changed during ranking",
            ));
        }
        Err(RevalidationError::Incomplete) => {
            return Err(failure(
                5,
                "incomplete-roster",
                "Roster scope incomplete during revalidation",
            ));
        }
        Err(RevalidationError::Deadline) => {
            return Err(failure(
                6,
                "timeout",
                "Deadline exceeded during revalidation",
            ));
        }
        Err(e) => {
            return Err(failure(
                5,
                "unusable-roster",
                format!("Revalidation failed: {e:?}"),
            ));
        }
    }

    // b. Policy receipt revalidation
    let (refreshed, reval) = config_files.refresh(
        clock,
        &resolved_config,
        &current_receipt,
        PolicyBoundary::CliPublication(PublicationKind::Advisory),
    )?;
    if let Revalidation::Superseded(fields) = reval {
        return Err(failure(
            3,
            "superseded",
            format!("Policy superseded before publication: {fields:?}"),
        ));
    }
    if let Revalidation::InvalidConfiguration = reval {
        return Err(failure(
            2,
            "invalid-configuration",
            "Configuration became invalid before publication",
        ));
    }
    let _ = refreshed;

    // Check runtime publication admission (deadline cleanup reserve check)
    admit_publication(clock.deadline(), clock.now(), clock.now())
        .map_err(|e| failure(6, "timeout", format!("Runtime suppressed late result: {e}")))?;

    // 16. Build Ranked OutputDocument
    let mut doc = build_ranked_document(
        &evaluation.eligible,
        &scored_ranking,
        &shortlisted,
        &wide_outcome,
        &rerank_outcome,
        &normalized_context,
        &roster,
        &Evaluated {
            metrics: progress.metrics.clone(),
            ..progress.evaluated.clone()
        },
        clock.now().as_millis(),
    )?;

    if let Some(trace_val) = generate_trace(
        &args,
        &roster,
        &normalized_context,
        Some(&policy_view),
        &retrieval_view,
        Some(&wide_outcome),
        gate_threshold,
        Some(&rerank_outcome),
        fits_threshold,
        Some(&evaluation),
        Some(&scored_ranking),
        true,
    ) {
        doc = doc.with_trace(trace_val).map_err(|e| {
            failure(
                5,
                "contract-violation",
                format!("Trace contract error: {e:?}"),
            )
        })?;
    }

    Ok(doc)
}

/// Roster coverage gaps as bounded warnings: records excluded while resolving
/// discovered files, and sources that could not be fully observed, by stable
/// code and count. A normally absent optional root is not a gap. Paths and
/// skill text never appear.
fn roster_warnings(roster: &ResolvedRoster) -> (Vec<Value>, usize) {
    let mut records: BTreeMap<&'static str, usize> = BTreeMap::new();
    for (_, error) in roster.diagnostics() {
        *records
            .entry(crate::roster::evidence::record_code(*error))
            .or_insert(0) += 1;
    }
    let mut sources: BTreeMap<&'static str, usize> = BTreeMap::new();
    for diagnostic in roster.source_diagnostics() {
        let code = crate::roster::evidence::source_code(diagnostic);
        if code != "root-missing" {
            *sources.entry(code).or_insert(0) += 1;
        }
    }
    // The provisional precedence caveat comes first so it is never truncated.
    let provisional = roster
        .skills()
        .iter()
        .flat_map(|skill| skill.bindings())
        .filter(|binding| visibility_label(&binding.visibility) == "unverified")
        .count();
    let caveat = (provisional > 0).then(|| {
        json!({
            "kind": "unverified-visibility",
            "count": provisional,
            "message": "Claude's skill precedence is not yet conformance-verified; confirm a suggested skill loads before relying on it",
        })
    });
    let mut warnings: Vec<Value> = caveat
        .into_iter()
        .chain(records.into_iter().map(|(code, count)| {
            json!({
                "kind": code,
                "count": count,
                "message": "Skill records were excluded while resolving the roster",
            })
        }))
        .chain(sources.into_iter().map(|(code, count)| {
            json!({
                "kind": code,
                "count": count,
                "message": "A skill source was not fully observed",
            })
        }))
        .collect();
    let omitted = warnings
        .len()
        .saturating_sub(crate::output::MAX_WARNING_DETAILS);
    warnings.truncate(crate::output::MAX_WARNING_DETAILS);
    (warnings, omitted)
}

/// A typed input failure whose exit comes from its public error kind.
fn input_failure(kind: ErrorKind, message: &str) -> PipelineFailure {
    failure(kind.exit_code() as u8, kind.as_str(), message)
}

/// One previewed provider request: the exact serialized bytes as text.
fn preview_stage(stage: &str, request: &[u8], candidates: usize) -> Result<Value, PipelineFailure> {
    let text = std::str::from_utf8(request).map_err(|_| {
        let kind = ErrorKind::OutputLimit;
        failure(
            kind.exit_code() as u8,
            kind.as_str(),
            "The request is not valid UTF-8",
        )
    })?;
    Ok(json!({
        "stage": stage,
        "request_bytes": request.len(),
        "request": text,
        "candidates": candidates,
    }))
}

/// The non-actionable dry-run artifact. Exactly one of `provider_request` and
/// `local_decision` is present; nothing was sent, published or stored.
fn preview_document(
    provider_request: Option<Value>,
    local_decision: Option<Value>,
    disclosure: Option<Value>,
    effects: &crate::effects::EffectReceipt,
) -> Result<OutputDocument, PipelineFailure> {
    OutputDocument::from_value(json!({
        "schema_version": SCHEMA_VERSION,
        "kind": "preview",
        "actionable": false,
        "stateless": true,
        "effects": effects,
        "provider_request": provider_request,
        "local_decision": local_decision,
        "disclosure": disclosure,
    }))
    .map_err(|_| {
        let kind = ErrorKind::OutputLimit;
        failure(
            kind.exit_code() as u8,
            kind.as_str(),
            "The preview exceeds the output contract",
        )
    })
}

/// Wall-clock milliseconds for cache receipt and freshness. A clock before the
/// epoch reads as zero, which no stored entry can be fresh against.
fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn shortlist_digests(shortlisted: &[wide::Shortlisted<'_>]) -> Vec<CandidateDigest> {
    shortlisted
        .iter()
        .map(|s| CandidateDigest {
            skill_id: s.skill.binding.id.clone(),
            content_hash: s.skill.record.source_content.clone(),
            excerpt_hash: None,
        })
        .collect()
}

/// A validated response as it is cached: received now, for at most the
/// ten-minute TTL, under the requested model alias it answered.
fn cache_entry(
    stage: RequestStage,
    fingerprint: RequestFingerprint,
    response: &Response,
    model: &str,
) -> CachedResponseEntry {
    CachedResponseEntry {
        stage,
        request_fingerprint: fingerprint,
        response_bytes: response.to_wire_bytes(),
        received_at_unix_ms: wall_clock_ms(),
        ttl_seconds: DEFAULT_CACHE_TTL_SECS,
        model: model.to_owned(),
        model_revision: None,
        original_usage: response.usage,
        attempt_id: None,
    }
}

/// The persistent exact response cache. Only Linux has the qualified store;
/// elsewhere every run keys fingerprints with fresh randomness and caches
/// nothing. Store failures never fail ranking: the store is dropped and the
/// run continues uncached.
#[cfg(target_os = "linux")]
mod persistent {
    use super::{EffectGate, ProcessInvocation, wall_clock_ms};
    use crate::blocking::{BlockingLeafKind, run_blocking_leaf};
    use crate::cache::{
        CacheKey, CachedResponseEntry, CoordinationKey, CoordinationPolicy, FreshnessStatus,
        LeaderContext, LeaseAcquisition, LeaseCoordinator, RequestFingerprint, RequestStage,
        SqliteLeaseCoordinator,
    };
    use crate::runtime::EntryClock;
    use crate::storage::{CacheAccess, CacheLocation, CacheOpen, CacheStore};
    use asupersync::Cx;
    use std::path::Path;
    use std::time::Duration;

    /// Single-flight leases live beside the cache store in its private
    /// directory. They never hold response bodies.
    pub(super) const LEASES_FILE: &str = "leases.sqlite3";

    /// Open the lease coordinator, creating its file owner-only first so
    /// SQLite and its sidecars inherit private permissions.
    fn coordinator(path: &Path) -> Option<SqliteLeaseCoordinator> {
        use std::os::unix::fs::OpenOptionsExt;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return None,
        }
        SqliteLeaseCoordinator::open(path).ok()
    }

    pub(super) fn acquire(
        invocation: &ProcessInvocation,
        cx: &Cx,
        path: &Path,
        key: CoordinationKey,
    ) -> Option<LeaseAcquisition> {
        let path = path.to_path_buf();
        run_blocking_leaf(
            invocation,
            cx,
            BlockingLeafKind::Database,
            false,
            move || {
                coordinator(&path)?
                    .acquire(key, wall_clock_ms(), &CoordinationPolicy::default())
                    .ok()
            },
        )
        .ok()?
        .value
    }

    /// True once the lease is gone, completed or expired.
    fn settled(invocation: &ProcessInvocation, cx: &Cx, path: &Path, key: CoordinationKey) -> bool {
        let path = path.to_path_buf();
        run_blocking_leaf(
            invocation,
            cx,
            BlockingLeafKind::Database,
            false,
            move || match coordinator(&path).map(|c| c.check_lease(key)) {
                Some(Ok(Some(lease))) => {
                    lease.is_completed || wall_clock_ms() >= lease.expires_at_unix_ms
                }
                _ => true,
            },
        )
        .map_or(true, |outcome| outcome.value)
    }

    /// Wait for a leader's lease to settle, within the lease and while at
    /// least a second of this invocation's budget remains for its own work.
    pub(super) async fn wait_for_leader(
        invocation: &ProcessInvocation,
        cx: &Cx,
        clock: &EntryClock,
        path: &Path,
        key: CoordinationKey,
        expires_at_unix_ms: u64,
    ) {
        let path = path.to_path_buf();
        loop {
            if cx.is_cancel_requested()
                || clock.remaining_before_cleanup().as_millis() < 1_000
                || wall_clock_ms() >= expires_at_unix_ms
                || settled(invocation, cx, &path, key)
            {
                return;
            }
            asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(25)).await;
        }
    }

    pub(super) fn complete(
        invocation: &ProcessInvocation,
        cx: &Cx,
        path: &Path,
        leader: &LeaderContext,
    ) {
        let path = path.to_path_buf();
        let leader = leader.clone();
        let _ = run_blocking_leaf(
            invocation,
            cx,
            BlockingLeafKind::Database,
            false,
            move || {
                if let Some(coordinator) = coordinator(&path) {
                    let _ = coordinator.complete(
                        leader.key,
                        leader.owner_token,
                        leader.fencing_generation,
                        wall_clock_ms(),
                    );
                }
            },
        );
    }

    pub(super) struct Store(CacheStore);

    impl Store {
        pub(super) fn open(
            invocation: &ProcessInvocation,
            cx: &Cx,
            gate: &EffectGate,
            dir: &Path,
        ) -> Option<Self> {
            match gate.open_cache(
                invocation,
                cx,
                CacheAccess::Initialize,
                CacheLocation::Directory(dir.to_path_buf()),
            ) {
                Ok(CacheOpen::Ready(store)) => Some(Self(*store)),
                _ => None,
            }
        }
        pub(super) const fn key(&self) -> CacheKey {
            self.0.fingerprint_key()
        }
        pub(super) const fn generation(&self) -> u64 {
            self.0.stamp().generation()
        }
    }

    /// A fresh stored answer's bytes and age, or nothing.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn lookup(
        slot: &mut Option<Store>,
        invocation: &ProcessInvocation,
        cx: &Cx,
        namespace: [u8; 32],
        stage: RequestStage,
        fingerprint: RequestFingerprint,
        model: &str,
        now_unix_ms: u64,
    ) -> Option<(Vec<u8>, u64)> {
        let Store(store) = slot.take()?;
        let (store, entry) = store
            .response(invocation, cx, namespace, stage, fingerprint)
            .ok()?;
        *slot = Some(Store(store));
        let entry = entry?;
        match entry.evaluate_freshness(now_unix_ms, model, None) {
            FreshnessStatus::Fresh { age_ms, .. } => Some((entry.response_bytes, age_ms)),
            _ => None,
        }
    }

    pub(super) fn record(
        slot: &mut Option<Store>,
        invocation: &ProcessInvocation,
        cx: &Cx,
        namespace: [u8; 32],
        entry: CachedResponseEntry,
    ) {
        let Some(Store(store)) = slot.take() else {
            return;
        };
        let now = entry.received_at_unix_ms;
        if let Ok(store) = store.record_response(invocation, cx, namespace, entry, now) {
            *slot = Some(Store(store));
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod persistent {
    use super::{EffectGate, ProcessInvocation};
    use crate::cache::{
        CacheKey, CachedResponseEntry, CoordinationKey, LeaderContext, LeaseAcquisition,
        RequestFingerprint, RequestStage,
    };
    use crate::runtime::EntryClock;
    use asupersync::Cx;
    use std::path::Path;

    pub(super) const LEASES_FILE: &str = "leases.sqlite3";

    pub(super) fn acquire(
        _: &ProcessInvocation,
        _: &Cx,
        _: &Path,
        _: CoordinationKey,
    ) -> Option<LeaseAcquisition> {
        None
    }

    pub(super) async fn wait_for_leader(
        _: &ProcessInvocation,
        _: &Cx,
        _: &EntryClock,
        _: &Path,
        _: CoordinationKey,
        _: u64,
    ) {
    }

    pub(super) fn complete(_: &ProcessInvocation, _: &Cx, _: &Path, _: &LeaderContext) {}

    pub(super) enum Store {}

    impl Store {
        pub(super) fn open(
            _: &ProcessInvocation,
            _: &Cx,
            _: &EffectGate,
            _: &Path,
        ) -> Option<Self> {
            None
        }
        pub(super) const fn key(&self) -> CacheKey {
            match *self {}
        }
        pub(super) const fn generation(&self) -> u64 {
            match *self {}
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn lookup(
        _: &mut Option<Store>,
        _: &ProcessInvocation,
        _: &Cx,
        _: [u8; 32],
        _: RequestStage,
        _: RequestFingerprint,
        _: &str,
        _: u64,
    ) -> Option<(Vec<u8>, u64)> {
        None
    }

    pub(super) fn record(
        _: &mut Option<Store>,
        _: &ProcessInvocation,
        _: &Cx,
        _: [u8; 32],
        _: CachedResponseEntry,
    ) {
    }
}

/// Refresh trusted policy immediately before a provider attempt. Invalid
/// configuration fails; offline, withdrawn consent or a missing credential
/// refuse with their typed error; any other relevant change supersedes.
fn authorize_send(
    clock: &EntryClock,
    config_files: &ConfigFiles,
    resolved_config: &ResolvedConfig,
    receipt: &mut PolicyReceipt,
    gate: &EffectGate,
    stage: RankingStage,
) -> Result<NetworkConsent, PipelineFailure> {
    let (refreshed, reval) = config_files.refresh(
        clock,
        resolved_config,
        receipt,
        PolicyBoundary::ProviderAdmission,
    )?;
    if let Revalidation::InvalidConfiguration = reval {
        return Err(failure(
            2,
            "invalid-configuration",
            format!("Configuration invalid before {} send", stage.as_str()),
        ));
    }
    let current = refreshed.receipt(gate.policy());
    let consent = current.network_consent();
    admit_provider_attempt(consent, current.credential()).map_err(admission_refusal)?;
    if let Revalidation::Superseded(fields) = reval {
        return Err(failure(
            3,
            "superseded",
            format!("Policy changed before {} send: {fields:?}", stage.as_str()),
        ));
    }
    *receipt = current;
    Ok(consent)
}

/// Run one logical stage through the invocation's retry session. Policy is
/// re-authorized before every attempt, retries included. Usage comes from the
/// allowance's receipt, so unknown usage from attempts that returned nothing
/// is kept, never counted as zero.
#[allow(clippy::too_many_arguments)]
async fn provider_stage(
    session: &mut RetrySession<'_>,
    stage: RankingStage,
    request: &Request,
    config_files: &ConfigFiles,
    resolved_config: &ResolvedConfig,
    receipt: &mut PolicyReceipt,
    gate: &EffectGate,
    metrics: &mut ExecutionMetrics,
    cx: &Cx,
    clock: &EntryClock,
) -> Result<Response, PipelineFailure> {
    let sent_before = session.receipt().sent_attempts;
    let mut refusal = None;
    let result = session
        .send_stage(stage, request, cx, || {
            authorize_send(clock, config_files, resolved_config, receipt, gate, stage)
                .map_err(|failure| refusal = Some(failure))
        })
        .await;
    let cost = session.receipt();
    if cost.sent_attempts > sent_before {
        metrics.requests += 1;
    }
    metrics.http_attempts = u64::from(cost.sent_attempts);
    metrics.input_tokens = cost.known_usage.input_tokens;
    metrics.output_tokens = cost.known_usage.output_tokens;
    metrics.unknown_usage_attempts = u64::from(cost.unknown_usage_attempts);
    match result {
        Ok(answer) => Ok(answer.response),
        Err(error) => Err(match (error.kind, refusal) {
            (RetryErrorKind::PolicyChanged, Some(refusal)) => refusal,
            (kind, _) => retry_failure(kind, error.last_transport.map(|t| t.kind)),
        }),
    }
}

/// The terminal reason for a stage that exhausted or stopped its retries.
fn retry_failure(kind: RetryErrorKind, last: Option<TransportErrorKind>) -> PipelineFailure {
    match kind {
        RetryErrorKind::Transport(kind) => map_transport_error(kind),
        RetryErrorKind::Admission(refusal) => {
            let kind = refusal.error_kind();
            failure(kind.exit_code() as u8, kind.as_str(), refusal.to_string())
        }
        RetryErrorKind::Accounting => {
            let kind = ErrorKind::BudgetState;
            failure(
                kind.exit_code() as u8,
                kind.as_str(),
                "Attempt accounting failed",
            )
        }
        RetryErrorKind::PolicyChanged => failure(3, "superseded", "Policy changed before send"),
        // A transient provider failure with no retry left: report that failure.
        RetryErrorKind::RetryStopped(_) => last.map_or_else(
            || failure(4, "provider-failure", "The provider request failed"),
            map_transport_error,
        ),
        RetryErrorKind::ModelPairMismatch => failure(
            10,
            "invalid-provider-response",
            "Wide and rerank answers came from different models",
        ),
    }
}

/// Offline is a cache miss (exit 11), not an authorization failure; missing
/// consent is a privacy denial (8) and a missing credential is authentication (4).
fn admission_refusal(refusal: ProviderAdmissionRefusal) -> PipelineFailure {
    let kind = refusal.kind();
    failure(kind.exit_code() as u8, kind.as_str(), refusal.to_string())
}

fn map_transport_error(kind: TransportErrorKind) -> PipelineFailure {
    match kind {
        TransportErrorKind::Deadline | TransportErrorKind::Cancelled => {
            (6, "timeout", "Jev request exceeded deadline".into())
        }
        TransportErrorKind::Admission(refusal) => admission_refusal(refusal),
        TransportErrorKind::Response(_) => (
            10,
            "invalid-provider-response",
            "The provider response failed validation".into(),
        ),
        TransportErrorKind::CredentialOriginMismatch => {
            (4, "authentication", "Credential origin mismatch".into())
        }
        TransportErrorKind::HttpStatus(401 | 403) => {
            (4, "authentication", "Invalid credentials".into())
        }
        TransportErrorKind::HttpStatus(429) => {
            (4, "provider-cooldown", "Rate limited by provider".into())
        }
        TransportErrorKind::HttpStatus(code) => {
            (4, "provider-failure", format!("HTTP error {code}"))
        }
        _ => (4, "network-failure", format!("Transport error: {kind:?}")),
    }
}

fn build_explicit_document(
    skills: &[ResolvedExplicitSkill],
    context: &NormalizedContext,
    roster: &ResolvedRoster,
    elapsed_ms: u64,
    evaluated: &Evaluated,
) -> OutputDocument {
    let quality = &evaluated.quality;
    let (warnings, warnings_omitted) = roster_warnings(roster);
    let skill_values: Vec<Value> = skills
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let sk = roster.skills().iter().find(|sk| sk.record().id == s.id);
            let (name, path_str, content_hash, visibility) = if let Some(sk) = sk {
                let rec = sk.record();
                let p = match &rec.target {
                    LoadTarget::File(p) => p.as_path().display().to_string(),
                    LoadTarget::Harness(h) => h.as_str().to_string(),
                };
                (
                    rec.display_name.as_str(),
                    p,
                    rec.source_content.as_str().to_string(),
                    visibility_label(&rec.visibility),
                )
            } else {
                (
                    s.invocation.as_str(),
                    String::new(),
                    String::new(),
                    "unverified",
                )
            };
            json!({
                "rank": i + 1,
                "skill_id": s.id.as_str(),
                "name": name,
                "invocation_name": s.invocation.as_str(),
                "rank_score": null,
                "rerank_probability": null,
                "wide_probability": null,
                "fits": null,
                "path": path_str,
                "content_hash": content_hash,
                "visibility": visibility,
            })
        })
        .collect();

    let event_id = context
        .current_request
        .event_id
        .as_ref()
        .map(|e| e.as_str())
        .unwrap_or("event-0");

    let val = json!({
        "schema_version": SCHEMA_VERSION,
        "event_id": event_id,
        "decision": "explicit",
        "reason": "user-required",
        "harness": context.harness.as_str(),
        "context_quality": quality.summary(),
        "quality": quality.flags(),
        "roster": {
            "total": roster.skills().len(),
            "eligible": roster.skills().len(),
            "wide_candidates": 0,
            "shortlist": 0,
            "partial": roster.is_partial(),
            "retrieval": "not-evaluated",
            "provenance": {
                "snapshot_id": crate::roster::evidence::snapshot_id(roster).as_str(),
                "policy_version": "ranking-v1",
                "wide_set_id": null,
                "rerank_set_id": null,
            }
        },
        "needs_skill": null,
        "choice_confidence": null,
        "none_probability": null,
        "phase": null,
        "skills": skill_values,
        "omitted_rank_mass": null,
        "cache": {
            "hit": false,
            "wide_hit": false,
            "rerank_hit": false,
            "age_ms": null,
            "stale": false,
        },
        "model": {
            "requested": null,
            "wide_returned": null,
            "rerank_returned": null,
            "immutable_revision": null,
        },
        "usage": {
            "requests": 0,
            "http_attempts": 0,
            "input_tokens": 0,
            "output_tokens": 0,
            "unknown_usage_attempts": 0,
        },
        "persistence": evaluated.persistence(),
        "warnings": warnings,
        "warnings_omitted": warnings_omitted,
        "elapsed_ms": elapsed_ms,
    });

    OutputDocument::from_value(val).expect("valid explicit document")
}

/// The admitted input's quality. Before rendering it describes the full
/// bounded input that explicit resolution and local policy read; afterwards,
/// the rendered context and its disclosure receipt.
#[derive(Clone)]
struct Quality {
    summary: crate::output::ContextQuality,
    prompt_complete: bool,
    task_anchor_known: bool,
    history_windowed: bool,
    attachments_omitted: bool,
    source_gaps: bool,
}

impl Default for Quality {
    fn default() -> Self {
        Self {
            summary: crate::output::ContextQuality::Complete,
            prompt_complete: true,
            task_anchor_known: true,
            history_windowed: false,
            attachments_omitted: false,
            source_gaps: false,
        }
    }
}

impl Quality {
    fn summary(&self) -> Value {
        serde_json::to_value(self.summary).unwrap_or(Value::Null)
    }
    fn flags(&self) -> Value {
        json!({
            "prompt_complete": self.prompt_complete,
            "task_anchor_known": self.task_anchor_known,
            "history_windowed": self.history_windowed,
            "attachments_omitted": self.attachments_omitted,
            "source_gaps": self.source_gaps,
        })
    }
}

/// What an invocation actually executed before its decision: real counts and
/// usage, and only the stage estimates and model identities that exist.
#[derive(Clone, Default)]
struct Evaluated {
    quality: Quality,
    /// The ledger effect is disabled for this run (`--no-ledger`,
    /// `--no-persist` or `--dry-run`). Otherwise recording is unavailable:
    /// this build has no observation ledger yet.
    ledger_disabled: bool,
    metrics: ExecutionMetrics,
    eligible: usize,
    wide: usize,
    shortlist: usize,
    quill: bool,
    wide_set_id: Option<ContentHash>,
    rerank_set_id: Option<ContentHash>,
    requested_model: Option<String>,
    wide_returned: Option<String>,
    rerank_returned: Option<String>,
    needs_skill: Option<f64>,
    phase: Option<String>,
    choice_confidence: Option<f64>,
    none_probability: Option<f64>,
}

impl Evaluated {
    fn persistence(&self) -> &'static str {
        if self.ledger_disabled {
            "disabled"
        } else {
            "unavailable"
        }
    }
    fn retrieval(&self) -> &'static str {
        if self.quill {
            "quill-bm25"
        } else if self.wide == 0 {
            "not-evaluated"
        } else {
            "full"
        }
    }
    fn usage(&self) -> Value {
        json!({
            "requests": self.metrics.requests,
            "http_attempts": self.metrics.http_attempts,
            "input_tokens": self.metrics.input_tokens,
            "output_tokens": self.metrics.output_tokens,
            "unknown_usage_attempts": self.metrics.unknown_usage_attempts,
        })
    }
    fn cache(&self) -> Value {
        json!({
            "hit": self.metrics.cache_hit,
            "wide_hit": self.metrics.wide_hit,
            "rerank_hit": self.metrics.rerank_hit,
            "age_ms": self.metrics.cache_age_ms,
            "stale": false,
        })
    }
    fn model(&self) -> Value {
        json!({
            "requested": self.requested_model,
            "wide_returned": self.wide_returned,
            "rerank_returned": self.rerank_returned,
            "immutable_revision": null,
        })
    }
}

/// Digest of one evaluated candidate set: its stage and every member's stable
/// ID and content hash, in stable-ID order.
fn candidate_set_id<'a>(
    stage: &str,
    members: impl IntoIterator<Item = &'a AdvisorySkill<'a>>,
) -> ContentHash {
    let mut members: Vec<(&str, &str)> = members
        .into_iter()
        .map(|m| (m.binding.id.as_str(), m.record.source_content.as_str()))
        .collect();
    members.sort_unstable();
    let mut bytes = Vec::new();
    for part in std::iter::once(stage)
        .chain(std::iter::once("skillranker.candidate-set.v1"))
        .chain(members.iter().flat_map(|(id, hash)| [*id, *hash]))
    {
        bytes.extend_from_slice(&(part.len() as u64).to_le_bytes());
        bytes.extend_from_slice(part.as_bytes());
    }
    ContentHash::from_bytes(&bytes)
}

/// The dominant declared phase, when the wide stage evaluated one.
fn dominant_phase(phase: &BTreeMap<String, f64>) -> Option<&str> {
    phase
        .iter()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map(|(p, _)| p.as_str())
}

fn build_abstain_document(
    reason: &str,
    context: &NormalizedContext,
    roster: &ResolvedRoster,
    elapsed_ms: u64,
    dry_run: Option<Value>,
    evaluated: &Evaluated,
) -> OutputDocument {
    let quality = &evaluated.quality;
    let (warnings, warnings_omitted) = roster_warnings(roster);
    let event_id = context
        .current_request
        .event_id
        .as_ref()
        .map(|e| e.as_str())
        .unwrap_or("event-0");

    let mut val = json!({
        "schema_version": SCHEMA_VERSION,
        "event_id": event_id,
        "decision": "abstain",
        "reason": reason,
        "harness": context.harness.as_str(),
        "context_quality": quality.summary(),
        "quality": quality.flags(),
        "roster": {
            "total": roster.skills().len(),
            "eligible": evaluated.eligible,
            "wide_candidates": evaluated.wide,
            "shortlist": evaluated.shortlist,
            "partial": roster.is_partial(),
            "retrieval": evaluated.retrieval(),
            "provenance": {
                "snapshot_id": crate::roster::evidence::snapshot_id(roster).as_str(),
                "policy_version": "ranking-v1",
                "wide_set_id": evaluated.wide_set_id.as_ref().map(ContentHash::as_str),
                "rerank_set_id": evaluated.rerank_set_id.as_ref().map(ContentHash::as_str),
            }
        },
        "needs_skill": evaluated.needs_skill,
        "choice_confidence": evaluated.choice_confidence,
        "none_probability": evaluated.none_probability,
        "phase": evaluated.phase,
        "skills": [],
        "omitted_rank_mass": null,
        "cache": evaluated.cache(),
        "model": evaluated.model(),
        "usage": evaluated.usage(),
        "persistence": evaluated.persistence(),
        "warnings": warnings,
        "warnings_omitted": warnings_omitted,
        "elapsed_ms": elapsed_ms,
    });

    if let Some(dr) = dry_run {
        val["dry_run"] = dr;
    }

    OutputDocument::from_value(val).expect("valid abstain document")
}

#[allow(clippy::too_many_arguments)]
fn build_ranked_document(
    eligible: &[Eligible<'_>],
    ranking: &Ranking,
    shortlisted: &[wide::Shortlisted<'_>],
    wide_outcome: &WideOutcome<'_>,
    rerank_outcome: &RerankOutcome<'_>,
    context: &NormalizedContext,
    roster: &ResolvedRoster,
    evaluated: &Evaluated,
    elapsed_ms: u64,
) -> Result<OutputDocument, PipelineFailure> {
    let quality = &evaluated.quality;
    let (warnings, warnings_omitted) = roster_warnings(roster);
    let mut skill_values = Vec::new();
    for (i, scored) in ranking.returned.iter().enumerate() {
        let el = &eligible[scored.index];
        let wide_prob = shortlisted
            .iter()
            .find(|s| s.skill.binding.id == el.skill.binding.id)
            .map(|s| s.wide_probability)
            .unwrap_or(0.0);

        let rec = el.skill.record;
        let path_str = match &rec.target {
            LoadTarget::File(p) => p.as_path().display().to_string(),
            LoadTarget::Harness(h) => h.as_str().to_string(),
        };

        skill_values.push(json!({
            "rank": i + 1,
            "skill_id": el.skill.binding.id.as_str(),
            "name": rec.display_name.as_str(),
            "invocation_name": el.skill.binding.invocation.as_str(),
            "rank_score": scored.rank_score,
            "rerank_probability": el.rerank,
            "wide_probability": wide_prob,
            "fits": el.fit,
            "path": path_str,
            "content_hash": rec.source_content.as_str(),
            "visibility": visibility_label(&el.skill.binding.visibility),
        }));
    }

    let event_id = context
        .current_request
        .event_id
        .as_ref()
        .map(|e| e.as_str())
        .unwrap_or("event-0");

    let val = json!({
        "schema_version": SCHEMA_VERSION,
        "event_id": event_id,
        "decision": "ranked",
        "reason": "eligible-candidates",
        "harness": context.harness.as_str(),
        "context_quality": quality.summary(),
        "quality": quality.flags(),
        "roster": {
            "total": roster.skills().len(),
            "eligible": evaluated.eligible,
            "wide_candidates": evaluated.wide,
            "shortlist": evaluated.shortlist,
            "partial": roster.is_partial(),
            "retrieval": evaluated.retrieval(),
            "provenance": {
                "snapshot_id": crate::roster::evidence::snapshot_id(roster).as_str(),
                "policy_version": "ranking-v1",
                "wide_set_id": evaluated.wide_set_id.as_ref().map(ContentHash::as_str),
                "rerank_set_id": evaluated.rerank_set_id.as_ref().map(ContentHash::as_str),
            }
        },
        "needs_skill": wide_outcome.needs_skill,
        "choice_confidence": rerank_outcome.choice_confidence,
        "none_probability": rerank_outcome.none_probability,
        "phase": dominant_phase(&wide_outcome.phase).unwrap_or("other"),
        "skills": skill_values,
        "omitted_rank_mass": ranking.omitted_mass,
        "cache": evaluated.cache(),
        "model": evaluated.model(),
        "usage": evaluated.usage(),
        "persistence": evaluated.persistence(),
        "warnings": warnings,
        "warnings_omitted": warnings_omitted,
        "elapsed_ms": elapsed_ms,
    });

    OutputDocument::from_value(val).map_err(|e| {
        failure(
            10,
            "invalid-provider-response",
            format!("Output contract validation failed: {e:?}"),
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn generate_trace(
    args: &RankArgs,
    roster: &ResolvedRoster,
    context: &NormalizedContext,
    policy: Option<&PolicyView<'_>>,
    retrieval: &RetrievalView<'_>,
    wide_outcome: Option<&WideOutcome<'_>>,
    gate_threshold: f64,
    rerank_outcome: Option<&RerankOutcome<'_>>,
    fits_threshold: f64,
    evaluation: Option<&Evaluation<'_>>,
    ranking: Option<&Ranking>,
    publication_passed: bool,
) -> Option<Value> {
    if !args.explain && args.why_not.is_none() {
        return None;
    }

    let mut targets: Vec<SkillId> = Vec::new();
    if let Some(target) = &args.why_not {
        targets.push(target.clone());
    } else {
        if let Some(r) = ranking
            && let Some(eval) = evaluation
        {
            for scored in &r.returned {
                let id = eval.eligible[scored.index].skill.binding.id.clone();
                if !targets.contains(&id) {
                    targets.push(id);
                }
            }
        }
        if let Some(wo) = wide_outcome
            && let WideDecision::Shortlist(ref list) = wo.decision
        {
            for s in list {
                let id = s.skill.binding.id.clone();
                if !targets.contains(&id) && targets.len() < 16 {
                    targets.push(id);
                }
            }
        }
        if targets.is_empty() {
            for s in roster.skills() {
                for b in s.bindings() {
                    if !targets.contains(&b.id) && targets.len() < 16 {
                        targets.push(b.id.clone());
                    }
                }
            }
        }
    }

    let mut entries = Vec::new();
    for target in &targets {
        let skill_entries = compute_stage_trace(
            target,
            roster,
            policy,
            retrieval,
            wide_outcome,
            gate_threshold,
            rerank_outcome,
            fits_threshold,
            evaluation,
            ranking,
            publication_passed,
        );
        entries.extend(skill_entries);
    }

    let snapshot_id = crate::roster::evidence::snapshot_id(roster);
    let query_bytes = context.current_request.text.as_str().as_bytes();
    let query_id = ContentHash::from_bytes(query_bytes);
    let total = entries.len() as u64;
    // Explanation pages cannot invalidate an otherwise valid ranking. Keep the
    // complete count so consumers can distinguish a first page from a full trace.
    entries.truncate(crate::output::MAX_TRACE_PAGE_ITEMS);
    let next_offset = ((entries.len() as u64) < total).then_some(entries.len() as u64);

    let trace_cursor = TraceCursor {
        schema_version: 1,
        snapshot_id,
        query_id,
        offset: 0,
    };

    let stage_trace = StageTrace {
        cursor: trace_cursor,
        total,
        next_offset,
        entries,
    };

    serde_json::to_value(stage_trace).ok()
}

#[allow(clippy::too_many_arguments)]
fn compute_stage_trace(
    target: &SkillId,
    roster: &ResolvedRoster,
    policy: Option<&PolicyView<'_>>,
    retrieval: &RetrievalView<'_>,
    wide_outcome: Option<&WideOutcome<'_>>,
    gate_threshold: f64,
    rerank_outcome: Option<&RerankOutcome<'_>>,
    fits_threshold: f64,
    evaluation: Option<&Evaluation<'_>>,
    ranking: Option<&Ranking>,
    publication_passed: bool,
) -> Vec<TraceEntry> {
    let mut entries = Vec::with_capacity(8);

    // 1. discovery
    let matching_skill = roster.skills().iter().find(|s| {
        s.bindings().iter().any(|b| {
            &b.id == target || b.invocation.as_str() == target.as_str() || s.record().id == *target
        })
    });

    if matching_skill.is_none() {
        entries.push(TraceEntry::not_in_snapshot(target.clone()));
        for stage in &TraceStage::ALL[1..] {
            entries.push(TraceEntry::not_evaluated(target.clone(), *stage));
        }
        return entries;
    }

    entries.push(TraceEntry::passed(
        target.clone(),
        TraceStage::Discovery,
        "discovered",
        None,
        None,
    ));

    // 2. visibility
    let exact_res = match roster.exact_id(target) {
        ExactResolution::Missing => roster.exact_name(target.as_str()),
        other => other,
    };
    match exact_res {
        ExactResolution::Missing => {
            entries.push(TraceEntry::excluded(
                target.clone(),
                TraceStage::Visibility,
                "missing",
                None,
                None,
                Some("inspect-roster".into()),
            ));
            for stage in &TraceStage::ALL[2..] {
                entries.push(TraceEntry::not_evaluated(target.clone(), *stage));
            }
            return entries;
        }
        ExactResolution::Shadowed => {
            entries.push(TraceEntry::excluded(
                target.clone(),
                TraceStage::Visibility,
                "shadowed",
                None,
                None,
                Some("inspect-precedence".into()),
            ));
            for stage in &TraceStage::ALL[2..] {
                entries.push(TraceEntry::not_evaluated(target.clone(), *stage));
            }
            return entries;
        }
        ExactResolution::Ambiguous => {
            entries.push(TraceEntry::excluded(
                target.clone(),
                TraceStage::Visibility,
                "ambiguous",
                None,
                None,
                Some("inspect-precedence".into()),
            ));
            for stage in &TraceStage::ALL[2..] {
                entries.push(TraceEntry::not_evaluated(target.clone(), *stage));
            }
            return entries;
        }
        ExactResolution::Unverified => {
            entries.push(TraceEntry::excluded(
                target.clone(),
                TraceStage::Visibility,
                "unverified",
                None,
                None,
                Some("verify-adapter-visibility".into()),
            ));
            for stage in &TraceStage::ALL[2..] {
                entries.push(TraceEntry::not_evaluated(target.clone(), *stage));
            }
            return entries;
        }
        ExactResolution::Forbidden => {
            entries.push(TraceEntry::excluded(
                target.clone(),
                TraceStage::Visibility,
                "forbidden",
                None,
                None,
                Some("check-invocation-restrictions".into()),
            ));
            for stage in &TraceStage::ALL[2..] {
                entries.push(TraceEntry::not_evaluated(target.clone(), *stage));
            }
            return entries;
        }
        ExactResolution::Resolved { kind, .. } => match kind {
            InvocationKind::Forbidden => {
                entries.push(TraceEntry::excluded(
                    target.clone(),
                    TraceStage::Visibility,
                    "forbidden",
                    None,
                    None,
                    Some("check-invocation-restrictions".into()),
                ));
                for stage in &TraceStage::ALL[2..] {
                    entries.push(TraceEntry::not_evaluated(target.clone(), *stage));
                }
                return entries;
            }
            InvocationKind::ManualOnly => {
                entries.push(TraceEntry::excluded(
                    target.clone(),
                    TraceStage::Visibility,
                    "manual-only",
                    None,
                    None,
                    Some("request-explicitly".into()),
                ));
                for stage in &TraceStage::ALL[2..] {
                    entries.push(TraceEntry::not_evaluated(target.clone(), *stage));
                }
                return entries;
            }
            InvocationKind::Agent => {
                entries.push(TraceEntry::passed(
                    target.clone(),
                    TraceStage::Visibility,
                    "verified",
                    None,
                    None,
                ));
            }
        },
    }

    // 3. local-policy
    let siblings: Vec<SkillId> = if let Some(sk) = matching_skill {
        sk.bindings().iter().map(|b| b.id.clone()).collect()
    } else {
        vec![target.clone()]
    };

    if let Some(pv) = policy {
        let is_excluded =
            siblings.iter().any(|id| pv.excluded.contains(id)) || pv.excluded.contains(target);
        let is_loaded = siblings.iter().any(|id| pv.already_loaded.contains(id))
            || pv.already_loaded.contains(target);
        if is_excluded {
            entries.push(TraceEntry::excluded(
                target.clone(),
                TraceStage::LocalPolicy,
                "excluded",
                None,
                None,
                Some("review-exclusions".into()),
            ));
            for stage in &TraceStage::ALL[3..] {
                entries.push(TraceEntry::not_evaluated(target.clone(), *stage));
            }
            return entries;
        } else if is_loaded {
            entries.push(TraceEntry::excluded(
                target.clone(),
                TraceStage::LocalPolicy,
                "already-loaded",
                None,
                None,
                None,
            ));
            for stage in &TraceStage::ALL[3..] {
                entries.push(TraceEntry::not_evaluated(target.clone(), *stage));
            }
            return entries;
        } else {
            entries.push(TraceEntry::passed(
                target.clone(),
                TraceStage::LocalPolicy,
                "policy-admitted",
                None,
                None,
            ));
        }
    } else {
        entries.push(TraceEntry::not_evaluated(
            target.clone(),
            TraceStage::LocalPolicy,
        ));
    }

    // 4. quill-admission
    match retrieval {
        RetrievalView::Admitted { ids, .. } => {
            let admitted = siblings.iter().any(|id| ids.contains(id)) || ids.contains(target);
            if admitted {
                entries.push(TraceEntry::passed(
                    target.clone(),
                    TraceStage::QuillAdmission,
                    "quill-admitted",
                    None,
                    None,
                ));
            } else {
                entries.push(TraceEntry::excluded(
                    target.clone(),
                    TraceStage::QuillAdmission,
                    "not-retrieved",
                    None,
                    None,
                    Some("refine-request".into()),
                ));
                for stage in &TraceStage::ALL[4..] {
                    entries.push(TraceEntry::not_evaluated(target.clone(), *stage));
                }
                return entries;
            }
        }
        RetrievalView::Empty => {
            entries.push(TraceEntry::excluded(
                target.clone(),
                TraceStage::QuillAdmission,
                "retrieval-empty",
                None,
                None,
                Some("refine-request".into()),
            ));
            for stage in &TraceStage::ALL[4..] {
                entries.push(TraceEntry::not_evaluated(target.clone(), *stage));
            }
            return entries;
        }
        RetrievalView::NotEvaluated => {
            entries.push(TraceEntry::passed(
                target.clone(),
                TraceStage::QuillAdmission,
                "full-roster",
                None,
                None,
            ));
        }
        RetrievalView::Failed => {
            entries.push(TraceEntry::not_evaluated(
                target.clone(),
                TraceStage::QuillAdmission,
            ));
            for stage in &TraceStage::ALL[4..] {
                entries.push(TraceEntry::not_evaluated(target.clone(), *stage));
            }
            return entries;
        }
    }

    // 5. wide-shortlist
    let Some(wo) = wide_outcome else {
        for stage in &TraceStage::ALL[4..] {
            entries.push(TraceEntry::not_evaluated(target.clone(), *stage));
        }
        return entries;
    };

    match &wo.decision {
        WideDecision::LowNeed => {
            entries.push(TraceEntry::excluded(
                target.clone(),
                TraceStage::WideShortlist,
                "gate-not-met",
                Some(wo.needs_skill),
                Some(gate_threshold),
                Some("refine-request".into()),
            ));
            for stage in &TraceStage::ALL[5..] {
                entries.push(TraceEntry::not_evaluated(target.clone(), *stage));
            }
            return entries;
        }
        WideDecision::Shortlist(list) => {
            if let Some(item) = list
                .iter()
                .find(|s| siblings.contains(&s.skill.binding.id) || &s.skill.binding.id == target)
            {
                entries.push(TraceEntry::passed(
                    target.clone(),
                    TraceStage::WideShortlist,
                    "shortlisted",
                    Some(item.wide_probability),
                    Some(gate_threshold),
                ));
            } else {
                entries.push(TraceEntry::excluded(
                    target.clone(),
                    TraceStage::WideShortlist,
                    "shortlist-overflow",
                    None,
                    Some(gate_threshold),
                    Some("refine-request".into()),
                ));
                for stage in &TraceStage::ALL[5..] {
                    entries.push(TraceEntry::not_evaluated(target.clone(), *stage));
                }
                return entries;
            }
        }
    }

    // 6. fit-none
    let Some(ro) = rerank_outcome else {
        for stage in &TraceStage::ALL[5..] {
            entries.push(TraceEntry::not_evaluated(target.clone(), *stage));
        }
        return entries;
    };

    let estimates = ro.estimates();
    let est_entry = siblings
        .iter()
        .find_map(|id| estimates.get(id))
        .or_else(|| estimates.get(target));

    if let Some(est) = est_entry {
        if est.fit < fits_threshold {
            entries.push(TraceEntry::excluded(
                target.clone(),
                TraceStage::FitNone,
                "fit-below-threshold",
                Some(est.fit),
                Some(fits_threshold),
                Some("refine-request".into()),
            ));
            for stage in &TraceStage::ALL[6..] {
                entries.push(TraceEntry::not_evaluated(target.clone(), *stage));
            }
            return entries;
        } else if est.rerank <= ro.none_probability {
            entries.push(TraceEntry::excluded(
                target.clone(),
                TraceStage::FitNone,
                "none-won",
                Some(est.rerank),
                Some(ro.none_probability),
                Some("refine-request".into()),
            ));
            for stage in &TraceStage::ALL[6..] {
                entries.push(TraceEntry::not_evaluated(target.clone(), *stage));
            }
            return entries;
        } else {
            entries.push(TraceEntry::passed(
                target.clone(),
                TraceStage::FitNone,
                "fit-eligible",
                Some(est.fit),
                Some(fits_threshold),
            ));
        }
    } else {
        entries.push(TraceEntry::not_evaluated(
            target.clone(),
            TraceStage::FitNone,
        ));
        for stage in &TraceStage::ALL[6..] {
            entries.push(TraceEntry::not_evaluated(target.clone(), *stage));
        }
        return entries;
    }

    // 7. ordering
    let Some(r) = ranking else {
        for stage in &TraceStage::ALL[6..] {
            entries.push(TraceEntry::not_evaluated(target.clone(), *stage));
        }
        return entries;
    };

    if let Some(eval) = evaluation {
        if let Some(scored) = r.returned.iter().find(|s| {
            let id = &eval.eligible[s.index].skill.binding.id;
            siblings.contains(id) || id == target
        }) {
            entries.push(TraceEntry::passed(
                target.clone(),
                TraceStage::Ordering,
                "top-k-selected",
                Some(scored.rank_score),
                None,
            ));
        } else {
            entries.push(TraceEntry::excluded(
                target.clone(),
                TraceStage::Ordering,
                "below-top-k",
                None,
                None,
                Some("refine-request".into()),
            ));
            entries.push(TraceEntry::not_evaluated(
                target.clone(),
                TraceStage::Publication,
            ));
            return entries;
        }
    } else {
        entries.push(TraceEntry::not_evaluated(
            target.clone(),
            TraceStage::Ordering,
        ));
        entries.push(TraceEntry::not_evaluated(
            target.clone(),
            TraceStage::Publication,
        ));
        return entries;
    }

    // 8. publication
    if publication_passed {
        entries.push(TraceEntry::passed(
            target.clone(),
            TraceStage::Publication,
            "published",
            None,
            None,
        ));
    } else {
        entries.push(TraceEntry::excluded(
            target.clone(),
            TraceStage::Publication,
            "revalidation-failed",
            None,
            None,
            Some("inspect-roster".into()),
        ));
    }

    entries
}
