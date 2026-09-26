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
use std::time::{SystemTime, UNIX_EPOCH};

use crate::blocking::{BlockingLeafKind, run_blocking_leaf};
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
use crate::context::branch::{SkillUsageKind, resolve_active_branch};
use crate::context::jsonl::{CursorKind, snapshot_jsonl};
use crate::context::render::{RenderContextOptions, render_context_and_receipt};
use crate::context::source::{
    SelectionOutcome, SelectionReason, SourceError, SourceOptions, SourceTarget,
};
use crate::context::tool::{SimpleSkillResolver, SkillMatch, extract_loaded_skill_records};
use crate::context::{
    CurrentRequest, NormalizedContext, NormalizedEvent, PrivateText, parse_normalized_context,
};
use crate::effects::EffectGate;
use crate::eligibility::{Eligible, Evaluation, LoadedState, Verdict, admit, after_rerank};
use crate::identity::{ContentHash, HarnessId, SkillId, WorkspaceId};
use crate::jev::admission::{AttemptBudget, RankingStage};
use crate::jev::client::{JevClient, TransportErrorKind};
use crate::jev::codec::{Request, Response};
use crate::jev::endpoint::EndpointConfig;
use crate::jev::rerank::RerankOutcome;
use crate::jev::retry::{RetryErrorKind, RetrySession};
use crate::jev::wide::{Sizes, WideDecision, WideOutcome};
use crate::jev::{OriginScopedCredential, rerank, wide};
use crate::output::{
    ContractError, ErrorKind, OutputDocument, OutputKind, SCHEMA_VERSION, StageTrace, TraceCursor,
    TraceEntry, TraceQueryScope, TraceStage,
};
use crate::privacy::redaction::Redactor;
use crate::privacy::{
    NetworkConsent, ProviderAdmissionRefusal, StoreAccess, admit_provider_attempt,
};
use crate::replay::{
    CandidateFitItem, CapturedCandidate, CapturedLoadedReference, CapturedLocalEvidence,
    CapturedRequest, CapturedScoringProfile, ChoiceDistributionItem, RecordedRerankChoice,
    RecordedResponses, RecordedWideChoice, ReplayCase, ReplayManifest,
};
use crate::roster::evidence::{PolicyView, RetrievalView};
use crate::roster::explicit::{
    ExplicitResolutionRequest, ExplicitResolutionResult, ResolvedExplicitSkill,
    resolve_explicit_requirements,
};
use crate::roster::resolution::{AdvisorySkill, ExactResolution, ResolvedOption, ResolvedRoster};
use crate::roster::retrieval::{
    QueryInput, RetrievalBudget, RetrievalError, RetrievalMethod, retrieve,
};
use crate::roster::{InvocationKind, LoadTarget, Visibility};
use crate::runtime::{EntryClock, ProcessInvocation, admit_publication};
use crate::scoring::{Input as ScoringInput, Ranking, Weights, rank};
use std::sync::{Arc, Mutex};

mod cass_source;
mod roster;

/// Failure tuple compatible with CLI error formatting: `(exit_code, kind, message)`.
pub type PipelineFailure = (u8, &'static str, String);

/// The exit code is derived from the kind, so a pair that disagrees with the
/// documented table cannot be written (sr-jeua).
fn failure(kind: ErrorKind, message: impl Into<String>) -> PipelineFailure {
    (kind.exit_code() as u8, kind.as_str(), message.into())
}

pub use crate::jev::client::JevTransport;

/// The provisional contract rank resolves Claude skills under: the documented
/// project-over-personal precedence, not yet verified by conformance evidence.
pub(crate) const PROVISIONAL_CLAUDE_CONTRACT: &str = "claude-code-documented-unverified";

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
    /// Explicit directory for the SQLite ledger. When `None`, platform default is used.
    pub ledger_dir: Option<PathBuf>,
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
    pub cursor: Option<TraceCursor>,
    pub output_json: bool,
    pub output_table: bool,
    pub dry_run: bool,
    pub save_case: Option<PathBuf>,
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
            ErrorKind::UnsupportedInput,
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
                ErrorKind::OversizedInput,
                "The input file exceeds its size limit",
            ),
            _ => unsupported(),
        })
}

#[derive(Default)]
struct CaseCapture {
    manifest: Option<ReplayManifest>,
    captured_request: Option<CapturedRequest>,
    recorded_responses: RecordedResponses,
    local_evidence: Option<CapturedLocalEvidence>,
}

/// What an invocation has established so far. Once input is admitted, a
/// failure is published from this record as a full unavailable decision, so
/// evaluated stages and incurred usage are never dropped.
#[derive(Default)]
struct Progress {
    admitted: Option<Admitted>,
    evaluated: Evaluated,
    metrics: ExecutionMetrics,
    cache_recording_failures: u64,
    /// A single-flight lease this invocation leads; completed on every path.
    lease: Option<(PathBuf, LeaderContext)>,
    capture: Option<CaseCapture>,
    /// What a run that then fails needs in order to still record the attempts it
    /// already paid for. A failing run emits no decision to record, so without
    /// this its provider cost exists nowhere durable — and a failed attempt is
    /// exactly the cost that no other record accounts for.
    failed_recording: Option<FailureRecording>,
    /// Normalized context bytes supplied in memory instead of the named file.
    supplied_context: Option<Vec<u8>>,
    /// Per-stage answers captured for an evaluation run; `None` otherwise.
    stage_evidence: Option<StageEvidence>,
}

/// Identity and cost carried out of a failing run, so an unavailable event can
/// own the attempts that were already made.
///
/// Everything here is captured before the first send and refreshed after each
/// stage settles, because the error itself unwinds past the point where the
/// session, roster and context are still in scope.
/// Writes each attempt's admission and reached-wire moment into the ledger as it happens.
///
/// The invocation's own [`crate::jev::AttemptProvenance`] already describes every attempt,
/// and for every ending this process survives that is the better record: it is complete,
/// and it is written once. This exists for the ending the process does not survive. If it
/// is killed while a request is on the wire, the provider may already have done the work
/// and charged for it, and no record written afterwards can establish that. Only a write
/// that happened before the wait survives it.
///
/// Both transitions are therefore written on the sending path, ahead of anything being
/// awaited, and both are best effort. A journal that cannot write loses evidence; one that
/// propagated its errors would cost the user the recommendation they asked for, which is
/// the worse failure. Every outcome here is discarded deliberately.
struct LedgerAttemptJournal<'a> {
    invocation: &'a ProcessInvocation,
    cx: Cx,
    location: crate::storage::LedgerLocation,
    owner_event_id: String,
    invocation_token: String,
    /// One reading of the wall clock at process entry, so that the monotonic instants the
    /// journal is handed become wall-clock times consistent with each other.
    entry_wall_clock_unix_ms: u64,
    wide_fingerprint: String,
    /// Not known when the session is built: the rerank request is derived from the wide
    /// answer. The pipeline publishes it here once it exists, through a lock rather than a
    /// second borrow, because by then the session holds this journal.
    rerank_fingerprint: Arc<Mutex<Option<String>>>,
}

impl LedgerAttemptJournal<'_> {
    /// The request fingerprint for a stage, or `None` when this stage has no durable home
    /// in the ranking-attempt table or its request is not yet known.
    fn stage_row(&self, stage: RankingStage) -> Option<(crate::storage::CandidateStage, String)> {
        match stage {
            RankingStage::Wide => Some((
                crate::storage::CandidateStage::Wide,
                self.wide_fingerprint.clone(),
            )),
            RankingStage::Rerank => self
                .rerank_fingerprint
                .lock()
                .ok()
                .and_then(|held| held.clone())
                .map(|fingerprint| (crate::storage::CandidateStage::Rerank, fingerprint)),
            // A breaker probe and an evaluation batch attempt are not ranking stages, and
            // filing either as one would attribute its cost to a ranking nobody asked for.
            RankingStage::Probe | RankingStage::Evaluation => None,
        }
    }
}

impl crate::jev::admission::AttemptJournal for LedgerAttemptJournal<'_> {
    fn admitted(
        &mut self,
        attempt_id: &str,
        stage: RankingStage,
        admitted_at: crate::limits::MonotonicMillis,
    ) {
        let Some((stage, request_fingerprint)) = self.stage_row(stage) else {
            return;
        };
        let attempt = crate::storage::NewProviderAttempt {
            attempt_id: crate::storage::ledger::attempt_row_key(
                &self.owner_event_id,
                &self.invocation_token,
                attempt_id,
            ),
            owner_event_id: self.owner_event_id.clone(),
            stage,
            request_fingerprint,
            admitted_at_unix_ms: self
                .entry_wall_clock_unix_ms
                .saturating_add(admitted_at.as_millis()),
            // Nothing has been sent and nothing has settled, so both stay absent rather
            // than being dated now. An admitted attempt cannot have been charged for.
            sent_at_unix_ms: None,
            completed_at_unix_ms: None,
            status: crate::storage::ledger::AttemptStatus::Admitted,
            input_tokens: None,
            output_tokens: None,
            http_status: None,
            error_kind: None,
        };
        let _ = crate::storage::ledger::record_attempt_admission(
            self.invocation,
            &self.cx,
            crate::storage::LedgerAccess::ExistingOnly,
            self.location.clone(),
            &attempt,
        );
    }

    fn reached_wire(&mut self, attempt_id: &str, sent_at: crate::limits::MonotonicMillis) {
        let key = crate::storage::ledger::attempt_row_key(
            &self.owner_event_id,
            &self.invocation_token,
            attempt_id,
        );
        let _ = crate::storage::ledger::record_attempt_reached_wire(
            self.invocation,
            &self.cx,
            crate::storage::LedgerAccess::ExistingOnly,
            self.location.clone(),
            &key,
            self.entry_wall_clock_unix_ms
                .saturating_add(sent_at.as_millis()),
        );
    }
}

struct FailureRecording {
    ledger_dir: Option<PathBuf>,
    workspace_root: String,
    session_id: String,
    agent_branch: String,
    mode_channel: String,
    policy_version: &'static str,
    event_id: String,
    attempts: Vec<crate::storage::NewProviderAttempt>,
}

impl Progress {
    /// Rebuild the stashed attempt rows from the session as it stands now.
    ///
    /// Called after each provider stage *including when that stage failed*: the
    /// attempt is settled by then, and the failing path is the one that needs it.
    fn stash_failed_attempts(
        &mut self,
        session: Option<&RetrySession<'_>>,
        wide_fingerprint: &RequestFingerprint,
        rerank_fingerprint: Option<&str>,
        clock: &EntryClock,
    ) {
        let Some(recording) = self.failed_recording.as_mut() else {
            return;
        };
        if let Some(evidence) =
            attempt_evidence(session, wide_fingerprint, rerank_fingerprint, clock)
        {
            recording.attempts = evidence.rows(&recording.event_id);
        }
    }
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
    args: RankArgs,
    transport: Option<&dyn JevTransport>,
) -> Result<OutputDocument, PipelineFailure> {
    execute_pipeline_supplied(invocation, cx, args, transport, None, None).await
}

/// Rank one normalized context already held in memory, such as a case of an
/// evaluation batch. `args.source_options.context` must name the case (it
/// selects normalized input); these bytes replace reading that path and meet
/// the same size bound and parser.
pub async fn execute_pipeline_with_context(
    invocation: &ProcessInvocation,
    cx: &Cx,
    args: RankArgs,
    transport: Option<&dyn JevTransport>,
    context: Vec<u8>,
    evidence: &mut StageEvidence,
) -> Result<OutputDocument, PipelineFailure> {
    execute_pipeline_supplied(
        invocation,
        cx,
        args,
        transport,
        Some(context),
        Some(evidence),
    )
    .await
}

/// How many lexical hits an evaluation keeps for the Quill-only baseline.
const LEXICAL_EVIDENCE_LIMIT: usize = 10;

/// Quill's ranking of exactly the admitted skills, for evaluation only. A
/// lexical miss is `Some(empty)`; an engine, budget or deadline failure is
/// `None`, never an invented miss.
async fn lexical_baseline(
    roster: &ResolvedRoster,
    admitted: &[crate::roster::resolution::AdvisorySkill<'_>],
    input: QueryInput<'_>,
    cx: &Cx,
    clock: &EntryClock,
) -> Option<Vec<String>> {
    let admitted_ids: BTreeSet<&SkillId> = admitted.iter().map(|s| &s.binding.id).collect();
    let not_admitted: BTreeSet<SkillId> = roster
        .skills()
        .iter()
        .filter(|skill| {
            !skill
                .bindings()
                .iter()
                .any(|b| admitted_ids.contains(&b.id))
        })
        .filter_map(|skill| skill.bindings().first().map(|b| b.id.clone()))
        .collect();
    match crate::roster::retrieval::retrieve_ranked(
        roster,
        &not_admitted,
        input,
        RetrievalBudget::default(),
        cx,
        clock,
    )
    .await
    {
        Ok(selection) => Some(
            selection
                .candidates
                .iter()
                .take(LEXICAL_EVIDENCE_LIMIT)
                .map(|s| s.binding.id.as_str().to_owned())
                .collect(),
        ),
        Err(failure) if failure.kind == RetrievalError::RetrievalEmpty => Some(Vec::new()),
        Err(_) => None,
    }
}

/// What each ranking stage produced for one evaluated case: the input to
/// baseline comparisons, which reuse these answers and make no extra request.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StageEvidence {
    /// Advisory candidates admitted to the wide stage, in admission order.
    pub admitted: Vec<String>,
    /// Whether Quill ranked the admitted set (roster overflow) rather than
    /// the whole advisory roster being admitted.
    pub quill_ranked: bool,
    pub wide: Option<WideEvidence>,
    pub rerank: Option<RerankEvidence>,
    /// Quill's lexical ranking of the admitted skills (at most ten), run for
    /// evaluation even when production admits the whole roster. `None` when
    /// it could not run; an empty list is a lexical miss.
    pub lexical: Option<Vec<String>>,
    /// Time the evaluation-only lexical pass took inside this ranking; zero
    /// when production ran Quill itself or no pass ran.
    #[serde(default)]
    pub lexical_elapsed_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WideEvidence {
    pub needs_skill: f64,
    pub none_probability: f64,
    /// True when the gate stopped the run before a rerank.
    pub low_need: bool,
    /// Shortlisted skills with their raw wide probability, highest first.
    pub shortlist: Vec<(String, f64)>,
    /// The same top candidates whatever the gate decided.
    pub intrinsic_shortlist: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RerankEvidence {
    pub none_probability: f64,
    /// Each shortlisted skill's raw rerank probability and fit.
    pub candidates: Vec<(String, f64, f64)>,
}

async fn execute_pipeline_supplied(
    invocation: &ProcessInvocation,
    cx: &Cx,
    mut args: RankArgs,
    transport: Option<&dyn JevTransport>,
    supplied_context: Option<Vec<u8>>,
    evidence: Option<&mut StageEvidence>,
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
            failure(ErrorKind::InvalidUsage, first)
        })?;
    }
    let preview = args.gate.policy().flags().dry_run;
    if !preview && !args.shortlist_ids.is_empty() {
        return Err(failure(
            ErrorKind::InvalidUsage,
            "Shortlist IDs are stage-2 evidence for --dry-run only",
        ));
    }
    if args.source_options.claude_hook && args.save_case.is_some() {
        return Err(failure(
            ErrorKind::InvalidUsage,
            "--save-case is not permitted in hook mode",
        ));
    }
    let save_case = args.save_case.clone();
    let effects = args.gate.receipt();
    let mut progress = Progress {
        supplied_context,
        stage_evidence: evidence.is_some().then(StageEvidence::default),
        ..Progress::default()
    };
    let result = rank_once(invocation, clock, cx, args, transport, &mut progress).await;
    if let (Some(sink), Some(captured)) = (evidence, progress.stage_evidence.take()) {
        *sink = captured;
    }
    // Finalize every armed failure, including refusals before attempt admission:
    // those runs have finished and must not retain their generated/in-flight row.
    // Recording is optional and best effort; failure to write does not change
    // the error the caller sees. Disabled ledger policy never arms this state.
    if let Err(error) = result.as_ref()
        && let Some(recording) = progress.failed_recording.take()
    {
        record_failed_attempts(
            invocation,
            &recording,
            error.1,
            clock.now().as_millis(),
            &progress.metrics,
        );
    }
    // Release a led lease on every path. Followers then find the recorded
    // pair, or send themselves when this run recorded nothing.
    let mut completion_superseded = false;
    let mut completion_unconfirmed = false;
    if let Some((leases, leader)) = progress.lease.take() {
        match persistent::complete(invocation, cx, &leases, &leader) {
            Some(crate::cache::PublishOutcome::Superseded { .. }) => completion_superseded = true,
            Some(crate::cache::PublishOutcome::Published) => {}
            None => completion_unconfirmed = true,
        }
    }
    // An optional coordination failure does not prove a successor exists.
    // Cache writes were already fenced; do not discard a valid answer merely
    // because completing that lease is busy or unavailable.
    let result = if completion_superseded && result.is_ok() {
        Err(failure(
            ErrorKind::Timeout,
            "Leader was superseded by successor before lease completion",
        ))
    } else {
        result
    };
    // Include final validation and lease release in reported latency. A lease
    // completion is a bounded storage effect, so check publication again after
    // it rather than trusting the earlier check inside rank_once.
    let result = result.and_then(|doc| {
        with_storage_warnings(
            doc,
            progress.cache_recording_failures,
            completion_unconfirmed,
            progress.evaluated.store_refused,
        )
    });
    let result = result.and_then(|mut doc| {
        if matches!(
            doc.kind(),
            OutputKind::Decision(
                crate::output::Decision::Ranked
                    | crate::output::Decision::Abstain
                    | crate::output::Decision::Explicit
            )
        ) {
            admit_publication(clock.deadline(), clock.now(), clock.now()).map_err(|error| {
                failure(
                    ErrorKind::Timeout,
                    format!("Runtime suppressed late result: {error}"),
                )
            })?;
        }
        doc.record_elapsed(clock.now().as_millis());
        if let Some(save_path) = &save_case
            && let Some(capture) = progress.capture.take()
        {
            let manifest = capture.manifest.unwrap_or_else(|| ReplayManifest {
                evidence_origin: "recorded".to_string(),
                adapter: progress
                    .admitted
                    .as_ref()
                    .map(|a| a.harness.clone())
                    .unwrap_or_else(|| "unknown".to_string()),
                model: None,
                stages_recorded: Vec::new(),
                prompt_summary: None,
            });
            let captured_request = capture.captured_request.unwrap_or_else(|| CapturedRequest {
                context_text: None,
                current_constraints: Vec::new(),
                candidate_options: Vec::new(),
            });
            let local_evidence = capture
                .local_evidence
                .unwrap_or_else(|| CapturedLocalEvidence {
                    as_of_unix_ms: clock.now().as_millis(),
                    active_snoozes: Vec::new(),
                    loaded_references: Vec::new(),
                    scoring_profile: CapturedScoringProfile {
                        gate_threshold: 0.30,
                        fit_threshold: 0.30,
                        w_fit: 1.0,
                        w_prior: 0.0,
                        w_phase: 0.0,
                        top_k: 5,
                    },
                });
            let case_id = progress
                .admitted
                .as_ref()
                .map(|a| format!("case-{}", a.event_id))
                .unwrap_or_else(|| format!("case-{}", clock.now().as_millis()));
            let case = ReplayCase {
                schema_version: SCHEMA_VERSION,
                case_id,
                created_at_unix_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
                manifest,
                captured_request,
                recorded_responses: capture.recorded_responses,
                local_evidence,
                historical_decision: doc.as_value().clone(),
            };
            case.validate().map_err(|err| {
                failure(err.kind(), format!("Replay case validation failed: {err}"))
            })?;
            case.save_to_file(save_path)
                .map_err(|err| failure(err.kind(), err.to_string()))?;
        }
        Ok(doc)
    });
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
                    .and_then(|doc| {
                        with_storage_warnings(
                            doc,
                            progress.cache_recording_failures,
                            completion_unconfirmed,
                            progress.evaluated.store_refused,
                        )
                    })
            }
            None => Err(failure),
        },
        result => result,
    }
}

fn with_storage_warnings(
    doc: OutputDocument,
    cache_recording_failures: u64,
    completion_unconfirmed: bool,
    store_refused: bool,
) -> Result<OutputDocument, PipelineFailure> {
    if cache_recording_failures == 0 && !completion_unconfirmed && !store_refused {
        return Ok(doc);
    }
    let mut value = doc.as_value().clone();
    let Some(warnings) = value.get_mut("warnings").and_then(Value::as_array_mut) else {
        return Ok(doc);
    };
    let mut omitted = 0;
    for (kind, count, message) in [
        (
            "cache-recording-unavailable",
            cache_recording_failures,
            "Optional response cache recording was skipped",
        ),
        (
            "coordination-completion-unconfirmed",
            u64::from(completion_unconfirmed),
            "Optional lease completion could not be confirmed",
        ),
        (
            "cache-unavailable",
            u64::from(store_refused),
            "Response cache unavailable; this run could not reuse or record responses",
        ),
    ] {
        if count == 0 {
            continue;
        }
        if warnings.len() < crate::output::MAX_WARNING_DETAILS {
            warnings.push(json!({"kind":kind,"count":count,"message":message}));
        } else {
            omitted += 1;
        }
    }
    value["warnings_omitted"] = json!(
        value["warnings_omitted"]
            .as_u64()
            .unwrap_or(0)
            .saturating_add(omitted)
    );
    let doc = OutputDocument::from_value(value).map_err(|_| {
        failure(
            ErrorKind::OutputLimit,
            "Optional storage warning is invalid",
        )
    })?;
    Ok(doc)
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

fn compute_context_digest(ctx: &NormalizedContext) -> ContentHash {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"sr.norm-context.v1\0");
    bytes.extend_from_slice(ctx.harness.as_str().as_bytes());
    bytes.push(0);
    if let Some(s) = &ctx.session_id {
        bytes.extend_from_slice(s.as_str().as_bytes());
    }
    bytes.push(0);
    if let Some(b) = &ctx.branch_id {
        bytes.extend_from_slice(b.as_str().as_bytes());
    }
    bytes.push(0);
    if let Some(e) = &ctx.context_epoch {
        bytes.extend_from_slice(e.as_str().as_bytes());
    }
    bytes.push(0);
    bytes.extend_from_slice(ctx.current_request.text.as_str().as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&(ctx.events.len() as u64).to_le_bytes());
    if let Ok(evt_json) = serde_json::to_vec(&ctx.events) {
        bytes.extend_from_slice(&evt_json);
    }
    ContentHash::from_bytes(&bytes)
}

pub(crate) fn skill_evidence_resolver_from_roster(roster: &ResolvedRoster) -> SimpleSkillResolver {
    let mut resolver = SimpleSkillResolver::new();
    for skill in roster.skills() {
        let record = skill.record();
        let usage_kind = match record.usage_kind {
            crate::roster::UsageKind::Workflow => SkillUsageKind::Workflow,
            crate::roster::UsageKind::Reference => SkillUsageKind::Reference,
            crate::roster::UsageKind::Unknown => SkillUsageKind::Unknown,
        };
        let match_info = SkillMatch {
            skill_id: record.id.clone(),
            usage_kind,
            source_content: Some(record.source_content.clone()),
            rendered_content: record.rendered_content.clone(),
            has_dynamic_arguments: false,
            turn_scoped: false,
        };
        resolver.register_tool(record.invocation_name.as_str(), match_info.clone());
        resolver.register_tool(record.display_name.as_str(), match_info.clone());

        for binding in skill.bindings() {
            resolver.register_tool(
                binding.invocation.as_str(),
                SkillMatch {
                    skill_id: binding.id.clone(),
                    ..match_info.clone()
                },
            );
        }

        for alias in &record.aliases {
            resolver.register_tool(alias.invocation.as_str(), match_info.clone());
        }

        let path_str = match &record.target {
            crate::roster::LoadTarget::File(p) => Some(p.as_path().to_string_lossy().to_string()),
            _ => None,
        };
        if let Some(p) = path_str {
            let path_match = SkillMatch {
                source_content: None,
                rendered_content: None,
                ..match_info
            };
            resolver.register_path(p, path_match);
        }
    }
    resolver
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
            ErrorKind::Timeout,
            "Invocation deadline exceeded before ranking start",
        )
    })?;

    // The entry point already folded `--dry-run` into the gate.
    let gate = args.gate;
    let dry_run = gate.policy().flags().dry_run;
    progress.evaluated.ledger_disabled = matches!(gate.ledger(), StoreAccess::Disabled(_));
    if args.save_case.is_some() {
        progress.capture = Some(CaseCapture::default());
    }

    // 1. Initial configuration loading and policy receipt capture
    let config_files = ConfigFiles::new(args.workspace.clone(), args.user_config_root.clone());
    let resolved_config = config_files.load(clock, args.sources.clone())?;
    let mut current_receipt = resolved_config.receipt(gate.policy());

    let effective = resolved_config.effective();
    let mode_channel: &'static str = if dry_run {
        "shadow"
    } else if args.source_options.claude_hook {
        match effective.hook_mode() {
            crate::config::HookMode::Shadow => "shadow",
            crate::config::HookMode::Advisory => "advisory-hook",
        }
    } else {
        "cli"
    };
    let top = effective.top() as usize;
    let shortlist = effective.shortlist() as usize;
    let gate_threshold = effective.gate();
    let fits_threshold = effective.fits();

    // Validate size invariants: 1 <= top <= shortlist <= 32
    let sizes = Sizes::new(top, shortlist).map_err(|e| {
        failure(
            ErrorKind::InvalidConfiguration,
            format!("Invalid ranking sizes: {e:?}"),
        )
    })?;

    // 2. Select and ingest context source.
    let source_policy = gate.source_policy();
    let workspace_id =
        WorkspaceId::new(args.workspace.to_string_lossy().as_ref()).map_err(|_| {
            failure(
                ErrorKind::InvalidConfiguration,
                "Invalid workspace root path",
            )
        })?;

    // Without an explicit source, rank discovers this workspace's Claude
    // sessions under trusted transcript roots: Claude's projects directory
    // and any trusted-user `context.transcript_roots`.
    let transcript_roots: Vec<PathBuf> = args
        .home
        .iter()
        .map(|home| home.join(".claude").join("projects"))
        .chain(
            effective
                .transcript_roots()
                .iter()
                .map(|root| root.as_path().to_path_buf()),
        )
        .collect();
    let selection_outcome = args
        .source_options
        .clone()
        .resolve(
            workspace_id.clone(),
            source_policy,
            false, // non-interactive by default
            |workspace, _| {
                let (roots, path, id) = (
                    transcript_roots.clone(),
                    args.workspace.clone(),
                    workspace.clone(),
                );
                run_blocking_leaf(
                    invocation,
                    cx,
                    BlockingLeafKind::Filesystem,
                    false,
                    move || crate::context::discovery::discover_claude_sessions(&roots, &path, &id),
                )
                .map(|outcome| outcome.value)
                .map_err(|_| SourceError::IncompleteInventory)
            },
        )
        .map_err(|err| failure(err.kind(), err.to_string()))?;

    let source_selection = match selection_outcome {
        SelectionOutcome::Selected(sel) => sel,
        SelectionOutcome::NeedsChoice(_) => {
            // Ambiguity is a session condition, so it exits 3, the same code the typed site a few
            // lines above derives for `SourceError::AmbiguousSession`. Exit 2 reported it as a
            // usage error while the published envelope's `error.code` still said 3, because that
            // field comes from `kind.exit_code()`.
            return Err(failure(
                ErrorKind::AmbiguousSession,
                "Multiple sessions available; selection required",
            ));
        }
    };
    // A discovered session is disclosed with how it was chosen and how many
    // sessions were eligible. Recency never proves a session is the live one.
    progress.evaluated.source_warning = match source_selection.reason() {
        SelectionReason::UniqueInWorkspace => Some(json!({
            "kind": "discovered-session",
            "count": source_selection.candidate_count(),
            "message": "Used the only Claude session recorded for this workspace",
        })),
        SelectionReason::LatestRequested => Some(json!({
            "kind": "latest-session",
            "count": source_selection.candidate_count(),
            "message": "--latest chose the most recently active of this workspace's Claude sessions by recorded time; recency does not prove it is the live session",
        })),
        SelectionReason::Explicit | SelectionReason::InteractiveChoice => None,
    };

    // Ingest normalized context or native transcript. A transcript read only
    // from its tail has windowed history; one with an unfinished last record,
    // an unread backlog, oversized or duplicate-key records, or tool results
    // whose invocations the read window does not explain has source gaps.
    let mut transcript_windowed = false;
    let mut transcript_gaps = false;
    let mut normalized_context = match source_selection.target() {
        SourceTarget::NormalizedFile(path) => {
            let bytes = match progress.supplied_context.take() {
                Some(bytes) if bytes.len() > crate::limits::NORMALIZED_CONTEXT_JSON_BYTES.max() => {
                    return Err(failure(
                        ErrorKind::OversizedInput,
                        "Normalized context exceeds 1 MiB",
                    ));
                }
                Some(bytes) => bytes,
                None => read_input_file(
                    &args.workspace,
                    path.as_path(),
                    crate::limits::NORMALIZED_CONTEXT_JSON_BYTES,
                )?,
            };
            parse_normalized_context(&bytes).map_err(|e| {
                failure(
                    ErrorKind::MalformedInput,
                    format!("Invalid normalized context: {e}"),
                )
            })?
        }
        SourceTarget::NormalizedStdin => {
            let limit = crate::limits::NORMALIZED_CONTEXT_JSON_BYTES.max();
            let mut bytes = Vec::new();
            crate::runtime::read_stdin_platform_before_cleanup(clock, limit, &mut bytes).map_err(
                |error| match error {
                    crate::runtime::RuntimeError::StdinTimeout => failure(
                        ErrorKind::Timeout,
                        "Normalized stdin reached the ranking deadline",
                    ),
                    crate::runtime::RuntimeError::Deadline(
                        crate::limits::LimitError::AboveLimit { .. },
                    ) => failure(
                        ErrorKind::OversizedInput,
                        "Normalized context on stdin exceeds 1 MiB",
                    ),
                    crate::runtime::RuntimeError::UnboundedLeaf => failure(
                        ErrorKind::UnsupportedInput,
                        "Bounded stdin is unavailable on this platform",
                    ),
                    _ => failure(
                        ErrorKind::MalformedInput,
                        "Failed to read context from stdin",
                    ),
                },
            )?;
            parse_normalized_context(&bytes).map_err(|e| {
                failure(
                    ErrorKind::MalformedInput,
                    format!("Invalid normalized context: {e}"),
                )
            })?
        }
        SourceTarget::ClaudeTranscript(path) => {
            // The transcript's own records attribute it to a session of this
            // workspace. Without that it has no durable identity, and its cache
            // and lease namespace stays private to this run. A discovered
            // session must still be the one discovery chose.
            // A relative path is resolved against this invocation's workspace,
            // once, so reading and attribution agree. Reading the raw relative
            // path while attributing the resolved one reported a transcript as
            // "not a regular file" when it was simply named relatively.
            let transcript_path = if path.as_path().is_absolute() {
                path.as_path().to_path_buf()
            } else {
                args.workspace.join(path.as_path())
            };
            let session = {
                let absolute = transcript_path.clone();
                let workspace = args.workspace.clone();
                run_blocking_leaf(
                    invocation,
                    cx,
                    BlockingLeafKind::Filesystem,
                    false,
                    move || crate::context::discovery::transcript_session(&absolute, &workspace),
                )
                .ok()
                .and_then(|outcome| outcome.value)
            };
            if let Some(expected) = source_selection.expected_identity()
                && expected.session != session
            {
                return Err(failure(
                    ErrorKind::MissingSession,
                    "The discovered session changed before it was read",
                ));
            }
            // Snapshot JSONL transcript
            let snapshot =
                snapshot_jsonl(invocation, cx, &transcript_path, None, CursorKind::Ranking)
                    .map_err(|e| {
                        failure(
                            ErrorKind::MalformedInput,
                            format!("Transcript snapshot failed: {e}"),
                        )
                    })?;

            transcript_windowed = snapshot.truncated_history;
            transcript_gaps = snapshot.incomplete_tail
                || snapshot.unread_backlog
                || snapshot
                    .skipped
                    .iter()
                    .any(|record| record.kind != crate::context::jsonl::SkipKind::Unmodeled)
                || (snapshot.missing_tool_counterpart && !snapshot.truncated_history);
            let events = snapshot.events;
            // The current request is the latest user message. Keeping its
            // native event identity lets rendering send it once, not again as
            // history, and lets branch resolution find the active leaf.
            let current = events.iter().rev().find(|e| {
                e.role == crate::context::Role::User && e.kind == crate::context::EventKind::Message
            });
            let current_req_text = current.map_or_else(|| PrivateText::new(""), |e| e.text.clone());
            let current_event_id = current.and_then(|e| e.event_id.clone());

            NormalizedContext {
                schema_version: 1,
                harness: HarnessId::new("claude_code").unwrap(),
                producer_id: None,
                workspace_root: PrivateText::new(args.workspace.to_string_lossy()),
                session_id: session,
                agent_id: None,
                branch_id: None,
                context_epoch: None,
                current_request: CurrentRequest {
                    event_id: current_event_id,
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
            let limit = crate::limits::HOOK_STDIN_BYTES.max();
            let mut bytes = Vec::new();
            crate::runtime::read_stdin_platform_before_cleanup(clock, limit, &mut bytes).map_err(
                |error| match error {
                    crate::runtime::RuntimeError::StdinTimeout => failure(
                        ErrorKind::Timeout,
                        "Hook stdin reached the ranking deadline",
                    ),
                    crate::runtime::RuntimeError::Deadline(
                        crate::limits::LimitError::AboveLimit { .. },
                    ) => failure(
                        ErrorKind::OversizedInput,
                        "Hook payload on stdin exceeds 1 MiB",
                    ),
                    crate::runtime::RuntimeError::UnboundedLeaf => failure(
                        ErrorKind::UnsupportedInput,
                        "Bounded stdin is unavailable on this platform",
                    ),
                    _ => failure(
                        ErrorKind::MalformedInput,
                        "Failed to read hook payload from stdin",
                    ),
                },
            )?;
            let hook_input = crate::adapter::ClaudeUserPromptSubmit::from_json(
                &bytes,
                crate::adapter::UnknownFieldPolicy::RetainAdditive,
            )
            .map_err(|e| {
                failure(
                    ErrorKind::MalformedInput,
                    format!("Invalid hook input: {e}"),
                )
            })?;
            // A turn a background task's notification started, while the agent
            // was idle, is a harness event rather than a user request. Under the
            // default policy it is counted apart as a non-turn and never sent,
            // like a notification inside a running turn (sr-sif6). Only Claude's
            // exact envelope matches, so a prompt that merely quotes it still ranks.
            if effective.notification_turns() == crate::config::NotificationTurns::Skip
                && is_task_notification_envelope(hook_input.prompt.as_str())
            {
                if matches!(gate.ledger(), StoreAccess::Enabled) {
                    crate::storage::hook_entries::record_hook_invocation(
                        args.ledger_dir.as_deref(),
                        crate::storage::hook_entries::HookCounter::NonTurns,
                    );
                }
                return Err(failure(
                    ErrorKind::UnsupportedInput,
                    "A task notification is not a user turn (hook.notification_turns = skip)",
                ));
            }
            // From here on a failure is a hook turn the availability cohort must
            // count. Arm the failure row now rather than after context capture, or
            // an overlay, roster or retrieval failure leaves no trace at all. The
            // row is replaced by the full one once the context names the turn.
            // Its id has its own namespace: the branch is still unresolved, so
            // sharing the full row's id would make a later successful delivery of
            // this prompt collide with it. Without the harness's prompt id the turn
            // has no stable identity, so the row is keyed to this invocation and a
            // redelivery counts again.
            if matches!(gate.ledger(), StoreAccess::Enabled) {
                progress.failed_recording = Some(FailureRecording {
                    ledger_dir: args.ledger_dir.clone(),
                    workspace_root: args.workspace.to_string_lossy().into_owned(),
                    session_id: hook_input
                        .session_id
                        .as_ref()
                        .map_or_else(|| "session-0".to_string(), |s| s.as_str().to_string()),
                    agent_branch: "unresolved".to_string(),
                    mode_channel: mode_channel.to_string(),
                    policy_version: "ranking-v1",
                    event_id: hook_input.prompt_id.as_ref().map_or_else(
                        || format!("pre-context-invocation-{}", invocation_row_mark()),
                        |id| format!("pre-context-{}", id.as_str()),
                    ),
                    attempts: Vec::new(),
                });
            }
            let overlay_request = crate::context::overlay::ClaudeOverlayRequest {
                hook_input,
                transcript_path: None,
                authorized_root: None,
            };
            let overlay = match crate::context::overlay::apply_claude_prompt_overlay_before(
                &overlay_request,
                clock,
            ) {
                // Not a turn, so neither ranked nor recorded as one. It is
                // counted apart so `hook_entries` can subtract it (sr-jdji).
                Err(crate::context::overlay::OverlayError::NotificationInTurn) => {
                    progress.failed_recording = None;
                    if matches!(gate.ledger(), StoreAccess::Enabled) {
                        crate::storage::hook_entries::record_hook_invocation(
                            args.ledger_dir.as_deref(),
                            crate::storage::hook_entries::HookCounter::NonTurns,
                        );
                    }
                    return Err(failure(
                        ErrorKind::UnsupportedInput,
                        "A task notification inside a running turn is not a user turn",
                    ));
                }
                overlay => overlay,
            }
            .map_err(|e| match e {
                crate::context::overlay::OverlayError::Deadline => {
                    failure(ErrorKind::Timeout, "Transcript overlay deadline exhausted")
                }
                crate::context::overlay::OverlayError::MissingPrompt => failure(
                    ErrorKind::MalformedInput,
                    "Hook stdin payload is missing prompt text",
                ),
                crate::context::overlay::OverlayError::SessionMismatch { .. }
                | crate::context::overlay::OverlayError::CrossSessionReadForbidden => failure(
                    ErrorKind::MissingSession,
                    "Transcript session does not match hook session",
                ),
                crate::context::overlay::OverlayError::AmbiguousBranch => failure(
                    ErrorKind::AmbiguousBranch,
                    "Transcript branch cannot be resolved",
                ),
                _ => failure(
                    ErrorKind::MalformedInput,
                    format!("Claude prompt overlay failed: {e}"),
                ),
            })?;
            transcript_gaps = overlay.context_quality == crate::output::ContextQuality::Partial;
            transcript_windowed = overlay
                .active_branch
                .as_ref()
                .is_some_and(|b| b.ancestor_chain_truncated);

            NormalizedContext {
                schema_version: 1,
                harness: HarnessId::new("claude_code").unwrap(),
                producer_id: None,
                workspace_root: PrivateText::new(args.workspace.to_string_lossy()),
                session_id: overlay.session_id,
                agent_id: None,
                branch_id: overlay.branch_id,
                context_epoch: None,
                current_request: overlay.current_request,
                events: overlay.events,
                explicit_skill_references: Vec::new(),
                supplied_loads: Vec::new(),
            }
        }
        SourceTarget::CassSession(path) => {
            cass_source::read(
                &args.workspace,
                args.home.as_deref(),
                path.as_path(),
                !effective.no_tools(),
                source_policy,
                cx,
                clock,
            )
            .await?
        }
    };

    if normalized_context.current_request.event_id.is_none() {
        let derived = derive_request_event_id(&normalized_context);
        if let Ok(id) = crate::identity::EventId::new(&derived) {
            if let Some(last_ev) = normalized_context.events.last_mut()
                && last_ev.event_id.is_none()
                && last_ev.text == normalized_context.current_request.text
            {
                last_ev.event_id = Some(id.clone());
            }
            normalized_context.current_request.event_id = Some(id);
        }
    }

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
            return Err(failure(
                ErrorKind::InsufficientContext,
                "The request continues a task whose instruction is not in the context",
            ));
        }
        crate::context::anchor::AnchorResolution::OversizedInput { .. } => {
            return Err(failure(
                ErrorKind::OversizedInput,
                "The request is too large to inspect",
            ));
        }
        crate::context::anchor::AnchorResolution::ConflictingDirectives { .. } => {
            return Err(failure(
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
        summary: if transcript_gaps {
            crate::output::ContextQuality::Partial
        } else if has_history {
            crate::output::ContextQuality::Complete
        } else {
            crate::output::ContextQuality::PromptOnly
        },
        prompt_complete: !normalized_context
            .current_request
            .essential_attachment_missing,
        history_windowed: transcript_windowed,
        attachments_omitted: normalized_context.current_request.attachments_omitted,
        source_gaps: transcript_gaps,
        ..Quality::default()
    };

    // Combine explicit skill directives from CLI flags, context, and anchor.
    // Exclusions come from trusted configuration and from any turn's
    // directives: they defeat advisory suggestions and conflict with a request.
    let mut explicit_directives = args.require_skills.clone();
    for ref_id in &normalized_context.explicit_skill_references {
        if !explicit_directives.contains(ref_id) {
            explicit_directives.push(ref_id.clone());
        }
    }
    let mut explicit_exclusions: Vec<String> = effective
        .exclude_skills()
        .iter()
        .map(|ex| ex.as_str().to_owned())
        .collect();
    if let crate::context::anchor::AnchorResolution::Established(ref a) = anchor_res {
        for directive in &a.directives {
            match directive.kind {
                crate::context::anchor::AnchorDirectiveKind::Require => {
                    if let Ok(id) = SkillId::new(&directive.target)
                        && !explicit_directives.contains(&id)
                    {
                        explicit_directives.push(id);
                    }
                }
                crate::context::anchor::AnchorDirectiveKind::Exclude => {
                    if !explicit_exclusions.contains(&directive.target) {
                        explicit_exclusions.push(directive.target.clone());
                    }
                }
            }
        }
    }

    let mut trace_exclude_skills: Vec<SkillId> = Vec::new();
    for ex in &explicit_exclusions {
        if let Ok(id) = SkillId::new(ex)
            && !trace_exclude_skills.contains(&id)
        {
            trace_exclude_skills.push(id);
        }
    }

    let context_hash = compute_context_digest(&normalized_context);
    let trace_query_scope = TraceQueryScope {
        request_text: normalized_context.current_request.text.as_str(),
        why_not: args.why_not.as_ref(),
        gate_threshold,
        fits_threshold,
        top,
        shortlist,
        require_skills: &args.require_skills,
        exclude_skills: &trace_exclude_skills,
        context_hash: Some(context_hash),
        model: Some(effective.model().as_str()),
        endpoint: effective.endpoint().map(|e| e.as_str()),
        evaluation_hash: None,
    };

    // 4. Discover Roster. Claude's documented precedence (project skills over
    // personal ones) resolves collisions, but no conformance evidence verifies
    // it yet. Rank uses it under an explicit provisional label and says so in
    // every result; withheld, ambiguous and shadowed names stay excluded.
    let roster_source = roster::Source {
        workspace: &args.workspace,
        home: args.home.as_deref(),
        manifest: args.roster_file.as_deref(),
        configured: effective.roster_roots(),
        harness: &normalized_context.harness,
    };
    let roster = roster_source.load(cx, clock)?;

    let (warnings, warnings_omitted) =
        roster_warnings(&roster, progress.evaluated.source_warning.as_ref());
    progress.admitted = Some(Admitted {
        event_id: normalized_context
            .current_request
            .event_id
            .as_ref()
            .map(|e| e.as_str().to_owned())
            .unwrap_or_else(|| derive_request_event_id(&normalized_context)),
        harness: normalized_context.harness.as_str().to_owned(),
        total: roster.skills().len(),
        partial: roster.is_partial(),
        snapshot_id: crate::roster::evidence::snapshot_id(&roster),
        warnings,
        warnings_omitted,
    });

    let current_snapshot_id = crate::roster::evidence::snapshot_id(&roster);
    if let Some(cursor) = &args.cursor
        && cursor.snapshot_id != current_snapshot_id
    {
        return Err(failure(
            ErrorKind::RosterChanged,
            "The roster or query changed between trace pages.",
        ));
    }

    // 5. Explicit directives resolve locally, bypassing Jev and Quill. The
    // resolver runs even without a positive request: exclusions from the prompt,
    // earlier turns and configuration restrict the advisory candidates.
    let explicit_req = ExplicitResolutionRequest {
        cli_required_skills: explicit_directives
            .iter()
            .map(|id| id.as_str().to_string())
            .collect(),
        cli_excluded_skills: explicit_exclusions,
        context_skill_references: normalized_context.explicit_skill_references.clone(),
        context_excluded_skills: Vec::new(),
        user_prompt: Some(normalized_context.current_request.text.as_str().to_string()),
    };
    let explicit_result = resolve_explicit_requirements(&explicit_req, &roster).map_err(|e| {
        failure(
            ErrorKind::InvalidUsage,
            format!("Explicit resolution error: {e}"),
        )
    })?;

    let mut excluded_skills = BTreeSet::new();
    match explicit_result {
        ExplicitResolutionResult::Resolved { skills, .. } => {
            let dependencies =
                roster::capture_dependencies(&roster, skills.iter().map(|skill| &skill.id), clock)?;
            // Revalidate policy receipt before publication of explicit result
            let (_refreshed, reval) = config_files.refresh(
                clock,
                &resolved_config,
                &current_receipt,
                PolicyBoundary::CliPublication(PublicationKind::Explicit),
            )?;
            if let Revalidation::Superseded(fields) = reval {
                return Err(failure(
                    ErrorKind::Superseded,
                    format!("Policy changed during evaluation: {fields:?}"),
                ));
            }
            if let Revalidation::InvalidConfiguration = reval {
                return Err(failure(
                    ErrorKind::InvalidConfiguration,
                    "Configuration became invalid before publication",
                ));
            }
            roster_source.validate(&dependencies, cx, clock)?;
            let event_id_str = normalized_context
                .current_request
                .event_id
                .as_ref()
                .map(|e| e.as_str().to_string())
                .unwrap_or_else(|| derive_request_event_id(&normalized_context));
            let explicit_candidates: Vec<crate::storage::NewRankingCandidate> = skills
                .iter()
                .enumerate()
                .map(|(i, s)| crate::storage::NewRankingCandidate {
                    event_id: event_id_str.clone(),
                    stage: crate::storage::CandidateStage::Wide,
                    skill_id: s.id.as_str().to_string(),
                    // Explicit resolution yields a binding id, which differs
                    // from the record id when the skill is reached by an alias.
                    skill_version: roster
                        .skills()
                        .iter()
                        .find(|sk| {
                            sk.record().id == s.id || sk.bindings().iter().any(|b| b.id == s.id)
                        })
                        .map(|sk| sk.record().source_content.as_str().to_string())
                        .unwrap_or_default(),
                    raw_probability: None,
                    normalized_probability: None,
                    fit_score: None,
                    rank_score: None,
                    rank_position: Some((i + 1) as u32),
                    excluded: false,
                    exclusion_reason: None,
                })
                .collect();
            let recorded = try_record_ledger(
                invocation,
                cx,
                &gate,
                args.ledger_dir.as_deref(),
                &roster,
                &normalized_context,
                crate::storage::DecisionKind::Explicit,
                "explicit-match",
                mode_channel,
                clock.now().as_millis(),
                &progress.metrics,
                &explicit_candidates,
                None,
            );
            progress.evaluated.ledger_recorded = recorded;
            let mut doc = build_explicit_document(
                &skills,
                &normalized_context,
                &roster,
                clock.now().as_millis(),
                &progress.evaluated,
            );
            if let Some(trace_val) =
                generate_explicit_trace(&args, &roster, &trace_query_scope, &skills)?
            {
                doc = doc.with_trace(trace_val).map_err(|e| {
                    failure(
                        ErrorKind::OutputLimit,
                        format!("Trace contract error: {e:?}"),
                    )
                })?;
            }
            if let Some(capture) = &mut progress.capture {
                let mut candidate_options = Vec::with_capacity(skills.len());
                for s in &skills {
                    let sk = roster.skills().iter().find(|sk| sk.record().id == s.id);
                    let (content_hash, source, usage_kind, desc, visibility) = if let Some(sk) = sk
                    {
                        let rec = sk.record();
                        (
                            rec.source_content.as_str().to_string(),
                            rec.source.as_str().to_string(),
                            rec.usage_kind.as_str().to_string(),
                            Some(rec.description_full.as_str().to_string()),
                            Some(visibility_label(&rec.visibility).to_owned()),
                        )
                    } else {
                        (
                            "0".repeat(64),
                            "workspace".to_string(),
                            "workflow".to_string(),
                            None,
                            None,
                        )
                    };
                    candidate_options.push(CapturedCandidate {
                        skill_id: s.id.as_str().to_string(),
                        invocation_name: s.invocation.as_str().to_string(),
                        visibility,
                        content_hash,
                        source,
                        usage_kind,
                        description: desc,
                        excerpt: None,
                    });
                }
                capture.manifest = Some(ReplayManifest {
                    evidence_origin: "recorded".to_string(),
                    adapter: normalized_context.harness.as_str().to_string(),
                    model: Some(effective.model().as_str().to_string()),
                    stages_recorded: Vec::new(),
                    prompt_summary: Some(
                        normalized_context
                            .current_request
                            .text
                            .as_str()
                            .lines()
                            .next()
                            .unwrap_or("")
                            .chars()
                            .take(120)
                            .collect(),
                    ),
                });
                capture.captured_request = Some(CapturedRequest {
                    context_text: Some(
                        normalized_context.current_request.text.as_str().to_string(),
                    ),
                    current_constraints: Vec::new(),
                    candidate_options,
                });
                capture.local_evidence = Some(CapturedLocalEvidence {
                    as_of_unix_ms: clock.now().as_millis(),
                    active_snoozes: Vec::new(),
                    loaded_references: Vec::new(),
                    scoring_profile: CapturedScoringProfile {
                        gate_threshold: effective.gate(),
                        fit_threshold: effective.fits(),
                        w_fit: effective.w_fit(),
                        w_prior: effective.w_prior(),
                        w_phase: effective.w_phase(),
                        top_k: effective.top() as usize,
                    },
                });
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
            .map_err(|e| {
                failure(
                    ErrorKind::UnresolvedExplicit,
                    format!("Contract error: {e:?}"),
                )
            })?;
            return Ok(doc);
        }
        ExplicitResolutionResult::NoneSpecified {
            excluded_skills: resolved,
        } => excluded_skills.extend(resolved),
    }

    // 6. Advisory candidate admission & local policy filtering
    let evidence_resolver = skill_evidence_resolver_from_roster(&roster);
    let default_epoch = crate::identity::ContextEpoch::new("epoch-0").expect("valid default epoch");
    let current_epoch = active_branch
        .map(|b| &b.current_epoch)
        .unwrap_or(&default_epoch);
    let loaded_records = extract_loaded_skill_records(
        &normalized_context.events,
        &evidence_resolver,
        active_branch,
        current_epoch,
    );
    let loaded_state = LoadedState {
        branch: active_branch,
        records: &loaded_records,
    };

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
    let loaded_refs: BTreeSet<&SkillId> = loaded_records.iter().map(|r| &r.skill_id).collect();
    let policy_view = PolicyView {
        excluded: &excluded_refs,
        already_loaded: &loaded_refs,
    };

    // Roster skills as AdvisorySkills: one per physical skill. Bindings of one
    // skill are aliases of the same file (reached through two roots, or a
    // symlink), and the provider's options are skills. Offering two would fail
    // the whole ranking as a duplicate option and inflate the overflow count.
    // Prefer a binding the agent may invoke, so admission still reports a
    // restriction only when every alias carries it.
    let mut initial_advisory: Vec<AdvisorySkill> = Vec::new();
    for skill in roster.skills() {
        let mut verified = skill
            .bindings()
            .iter()
            .filter(|binding| matches!(binding.visibility, Visibility::Verified { .. }));
        let chosen = verified
            .clone()
            .find(|binding| binding.restrictions.agent_invocable)
            .or_else(|| verified.next());
        if let Some(binding) = chosen {
            initial_advisory.push(AdvisorySkill {
                record: skill.record(),
                binding,
            });
        }
    }

    if initial_advisory.is_empty() {
        // Reuse bounded, stable diagnostic codes; source paths and skill text
        // must not become part of this error, including on malformed input.
        let (warnings, _) = roster_warnings(&roster, None);
        let causes = warnings
            .iter()
            .filter_map(|warning| {
                Some(format!(
                    "{} ({})",
                    warning.get("kind")?.as_str()?,
                    warning.get("count")?.as_u64()?
                ))
            })
            .collect::<Vec<_>>()
            .join(", ");
        let mut hint = "Run sr roster --json to inspect excluded records, visibility, and source causes. Check the selected harness and skill roots.".to_owned();
        if !causes.is_empty() {
            hint.push_str(" Observed causes: ");
            hint.push_str(&causes);
            hint.push('.');
        }
        let doc = OutputDocument::failure_with_details(
            ErrorKind::EmptyRoster,
            "No skills in the observed roster could be admitted",
            &hint,
            false,
        );
        return Ok(doc);
    }

    // Initial admission before Wide
    let admission = admit(&initial_advisory, &excluded_refs, loaded_state);
    if let Some(verdict) = admission.verdict {
        match verdict {
            Verdict::Abstain(reason) => {
                let recorded = try_record_ledger(
                    invocation,
                    cx,
                    &gate,
                    args.ledger_dir.as_deref(),
                    &roster,
                    &normalized_context,
                    crate::storage::DecisionKind::Abstain,
                    reason.as_str(),
                    mode_channel,
                    clock.now().as_millis(),
                    &progress.metrics,
                    &[],
                    None,
                );
                progress.evaluated.ledger_recorded = recorded;
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
                    &trace_query_scope,
                    Some(&policy_view),
                    &RetrievalView::NotEvaluated,
                    None,
                    gate_threshold,
                    None,
                    fits_threshold,
                    None,
                    None,
                    false,
                )? {
                    doc = doc.with_trace(trace_val).map_err(|e| {
                        failure(
                            ErrorKind::OutputLimit,
                            format!("Trace contract error: {e:?}"),
                        )
                    })?;
                }
                let dependencies =
                    roster::capture_dependencies(&roster, std::iter::empty(), clock)?;
                validate_advisory_publication(
                    &roster_source,
                    &dependencies,
                    &config_files,
                    &resolved_config,
                    &current_receipt,
                    cx,
                    clock,
                )?;
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
    let (candidate_skills, ran_quill, quill_method) =
        if admission.admitted.len() > wide::MAX_REAL_OPTIONS {
            let query_input = QueryInput {
                latest_request: normalized_context.current_request.text.as_str(),
                active_task: task_anchor_text,
                recent_errors: "",
            };
            let budget = RetrievalBudget::default();
            // Quill must choose among what admission admitted, but it selects
            // from the whole roster. So every skill admission removed (excluded,
            // restricted or a loaded reference) is excluded here. One binding
            // per skill suffices: any excluded binding drops its skill.
            let admitted: BTreeSet<&SkillId> =
                admission.admitted.iter().map(|s| &s.binding.id).collect();
            let not_admitted: BTreeSet<SkillId> = roster
                .skills()
                .iter()
                .filter(|skill| !skill.bindings().iter().any(|b| admitted.contains(&b.id)))
                .filter_map(|skill| skill.bindings().first().map(|b| b.id.clone()))
                .collect();
            let selection = retrieve(&roster, &not_admitted, query_input, budget, cx, clock)
                .await
                .map_err(|err| match err.kind {
                    RetrievalError::RetrievalEmpty => failure(
                        ErrorKind::RetrievalEmpty,
                        "Quill retrieval yielded 0 matches",
                    ),
                    RetrievalError::Deadline => {
                        failure(ErrorKind::Timeout, "Retrieval exceeded deadline")
                    }
                    _ => failure(
                        ErrorKind::RetrievalFailure,
                        format!("Retrieval error: {err}"),
                    ),
                })?;
            if let Some(evidence) = progress.stage_evidence.as_mut() {
                evidence.lexical = Some(
                    selection
                        .candidates
                        .iter()
                        .take(LEXICAL_EVIDENCE_LIMIT)
                        .map(|s| s.binding.id.as_str().to_owned())
                        .collect(),
                );
            }
            (selection.candidates, true, selection.diagnostics.method)
        } else {
            if progress.stage_evidence.is_some() {
                let started = std::time::Instant::now();
                let lexical = lexical_baseline(
                    &roster,
                    &admission.admitted,
                    QueryInput {
                        latest_request: normalized_context.current_request.text.as_str(),
                        active_task: task_anchor_text,
                        recent_errors: "",
                    },
                    cx,
                    clock,
                )
                .await;
                if let Some(evidence) = progress.stage_evidence.as_mut() {
                    evidence.lexical = lexical;
                    evidence.lexical_elapsed_ms =
                        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                }
            }
            (admission.admitted, false, None)
        };

    if let Some(evidence) = progress.stage_evidence.as_mut() {
        evidence.admitted = candidate_skills
            .iter()
            .map(|s| s.binding.id.as_str().to_owned())
            .collect();
        evidence.quill_ranked = ran_quill;
    }
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
        let recorded = try_record_ledger(
            invocation,
            cx,
            &gate,
            args.ledger_dir.as_deref(),
            &roster,
            &normalized_context,
            crate::storage::DecisionKind::Abstain,
            "no-shortlist-match",
            mode_channel,
            clock.now().as_millis(),
            &progress.metrics,
            &[],
            None,
        );
        progress.evaluated.ledger_recorded = recorded;
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
            &trace_query_scope,
            Some(&policy_view),
            &retrieval_view,
            None,
            gate_threshold,
            None,
            fits_threshold,
            None,
            None,
            false,
        )? {
            doc = doc.with_trace(trace_val).map_err(|e| {
                failure(
                    ErrorKind::OutputLimit,
                    format!("Trace contract error: {e:?}"),
                )
            })?;
        }
        let dependencies = roster::capture_dependencies(
            &roster,
            initial_advisory.iter().map(|skill| &skill.binding.id),
            clock,
        )?;
        validate_advisory_publication(
            &roster_source,
            &dependencies,
            &config_files,
            &resolved_config,
            &current_receipt,
            cx,
            clock,
        )?;
        return Ok(doc);
    }

    // 8. Capture roster dependencies for final revalidation
    let mut content_scope = BTreeSet::new();
    // Quill's cutoff depends on the indexed inventory, not just its winners.
    // Conservatively retain every visible binding when retrieval ran: a skill
    // omitted from the wide set may become relevant while Jev is evaluating it.
    let content_candidates = if ran_quill {
        &initial_advisory
    } else {
        &candidate_skills
    };
    for candidate in content_candidates {
        content_scope.insert(candidate.binding.id.clone());
    }
    let dependencies = roster::capture_dependencies(&roster, &content_scope, clock)?;

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
            crate::context::render::RenderContextError::UnsupportedContext(_) => failure(
                ErrorKind::InsufficientContext,
                "The request depends on content that is not in the context",
            ),
            crate::context::render::RenderContextError::Redaction(_)
            | crate::context::render::RenderContextError::SecretsDetected(_) => failure(
                ErrorKind::UnsupportedInput,
                "The context could not be made safe to send",
            ),
            crate::context::render::RenderContextError::Serialization(_) => failure(
                ErrorKind::OversizedInput,
                "The context could not be rendered within its bounds",
            ),
        })?;
    // The receipt is the disclosure account shown to the user; a receipt that
    // disagrees with the bytes about to be sent fails closed before any request.
    disclosure_receipt
        .verify_against_payload(&rendered_context)
        .map_err(|_| {
            failure(
                ErrorKind::UnsupportedInput,
                "The disclosure receipt does not match the rendered context",
            )
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
    let quality = &mut progress.evaluated.quality;
    quality.summary = match rendered_context.context_quality {
        crate::output::ContextQuality::Complete | crate::output::ContextQuality::PromptOnly
            if quality.source_gaps =>
        {
            crate::output::ContextQuality::Partial
        }
        rendered => rendered,
    };
    quality.prompt_complete &= request_truncated == 0;
    quality.history_windowed |= history_omitted + history_truncated > 0;
    if rendered_context.is_unsupported_context() || request_truncated > 0 {
        return Err(failure(
            ErrorKind::InsufficientContext,
            "The request or its essential context does not fit the admitted input",
        ));
    }

    // 10. The exact response cache. Persistent entries need a trusted cache
    // directory, an enabled response-cache effect and a session identity to
    // scope them. Otherwise fingerprints are keyed with fresh randomness and
    // nothing outlives this invocation. An unusable store degrades to that.
    let store = match (&args.cache_dir, &normalized_context.session_id) {
        (Some(dir), Some(_)) => {
            match persistent::Store::open(invocation, cx, clock, &gate, dir).await {
                Ok(opened) => {
                    progress.evaluated.store_backed = opened.is_some();
                    progress.evaluated.store_disabled = opened.is_none();
                    opened
                }
                // A refused store is a real condition the run must disclose:
                // an ancestor a group can write to, a replaced directory, an
                // unqualified engine. Ranking continues uncached.
                Err(_) => {
                    progress.evaluated.store_refused = true;
                    None
                }
            }
        }
        _ => None,
    };
    let cache_key = match &store {
        Some(store) => store.key(),
        None => CacheKey::generate().map_err(|_| {
            failure(
                ErrorKind::StorageFailure,
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
    // Where the context came from and whose it is. A normalized import never
    // joins a native namespace, and producers, agents and native forks with
    // identical redacted requests never share a response or a lease.
    cache_ns = cache_ns.with_source_kind(match source_selection.target() {
        SourceTarget::NormalizedFile(_) | SourceTarget::NormalizedStdin => {
            crate::cache::SourceKind::Normalized
        }
        SourceTarget::CassSession(_) => crate::cache::SourceKind::Cass,
        SourceTarget::ClaudeTranscript(_) | SourceTarget::ClaudeHookStdin => {
            crate::cache::SourceKind::Native
        }
    });
    if matches!(
        source_selection.target(),
        SourceTarget::ClaudeTranscript(_) | SourceTarget::ClaudeHookStdin
    ) && let (Ok(adapter), Ok(version)) = (
        crate::identity::AdapterId::new(crate::adapter::CLAUDE_CODE_ID),
        crate::identity::AdapterVersion::new(crate::adapter::CONTRACT_VERSION.to_string()),
    ) {
        cache_ns = cache_ns.with_adapter(adapter, version);
    }
    if let Some(id) = &normalized_context.producer_id {
        cache_ns = cache_ns.with_producer(id.clone());
    }
    if let Some(id) = &normalized_context.agent_id {
        cache_ns = cache_ns.with_agent(id.clone());
    }
    if let Some(leaf) = active_branch.and_then(|branch| branch.leaf_event_id.clone()) {
        cache_ns = cache_ns.with_leaf_event(leaf);
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

    if let Some(capture) = &mut progress.capture {
        let mut candidate_options = Vec::with_capacity(candidate_skills.len());
        for s in &candidate_skills {
            candidate_options.push(CapturedCandidate {
                skill_id: s.binding.id.as_str().to_string(),
                invocation_name: s.binding.invocation.as_str().to_string(),
                visibility: Some(visibility_label(&s.record.visibility).to_owned()),
                content_hash: s.record.source_content.as_str().to_string(),
                source: s.record.source.as_str().to_string(),
                usage_kind: s.record.usage_kind.as_str().to_string(),
                description: Some(s.record.description_full.as_str().to_string()),
                excerpt: None,
            });
        }
        capture.manifest = Some(ReplayManifest {
            evidence_origin: "recorded".to_string(),
            adapter: normalized_context.harness.as_str().to_string(),
            model: Some(effective.model().as_str().to_string()),
            stages_recorded: Vec::new(),
            prompt_summary: Some(
                rendered_context
                    .latest_user_request
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(120)
                    .collect(),
            ),
        });
        capture.captured_request = Some(CapturedRequest {
            context_text: Some(rendered_context.latest_user_request.clone()),
            current_constraints: Vec::new(),
            candidate_options,
        });
        capture.local_evidence = Some(CapturedLocalEvidence {
            as_of_unix_ms: clock.now().as_millis(),
            active_snoozes: Vec::new(),
            loaded_references: loaded_records
                .iter()
                .map(|r| CapturedLoadedReference {
                    skill_id: r.skill_id.as_str().to_string(),
                    content_hash: r
                        .source_content
                        .as_ref()
                        .map(|h| h.as_str().to_string())
                        .unwrap_or_else(|| "0".repeat(64)),
                    availability: "available".to_string(),
                })
                .collect(),
            scoring_profile: CapturedScoringProfile {
                gate_threshold: effective.gate(),
                fit_threshold: effective.fits(),
                w_fit: effective.w_fit(),
                w_prior: effective.w_prior(),
                w_phase: effective.w_phase(),
                top_k: effective.top() as usize,
            },
        });
    }

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
    .map_err(|e| failure(e.kind(), format!("Wide build failed: {e:?}")))?;

    // A stateless preview: the exact redacted bytes a matching `--no-persist`
    // run would send. Stage 2 is previewed only for supplied shortlist IDs.
    if dry_run {
        let mut stages = vec![preview_stage(
            "wide",
            wide_builder.bytes(),
            candidate_ids.len(),
        )?];
        if !args.shortlist_ids.is_empty() {
            // `effective` is `(M, K)`: a stage-2 shortlist holds up to M.
            let (shortlist, _) = sizes.effective(candidate_ids.len());
            let mut seen = BTreeSet::new();
            if args.shortlist_ids.len() > shortlist
                || args
                    .shortlist_ids
                    .iter()
                    .any(|id| !candidate_ids.contains(id) || !seen.insert(id))
            {
                return Err(failure(
                    ErrorKind::InvalidUsage,
                    format!("Shortlist IDs must be distinct wide candidates, at most {shortlist}"),
                ));
            }
            let rerank_builder = rerank::build(
                &roster,
                &args.shortlist_ids,
                &rendered_context,
                effective.model().as_str(),
            )
            .map_err(|e| failure(e.kind(), format!("Rerank build failed: {e:?}")))?;
            stages.push(preview_stage(
                "rerank",
                rerank_builder.bytes(),
                args.shortlist_ids.len(),
            )?);
        }
        let disclosure = serde_json::to_value(&disclosure_receipt).map_err(|_| {
            let kind = ErrorKind::OutputLimit;
            failure(kind, "Disclosure receipt could not be encoded")
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
                ErrorKind::InvalidConfiguration,
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
                       version: &str,
                       paired_wide: Option<&RequestFingerprint>|
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
                paired_wide,
            },
        )
    };
    let wide_req_fp = fingerprint(
        RequestStage::Wide,
        &candidate_digests,
        wide_builder.bytes(),
        wide::WIDE_POLICY_VERSION,
        None,
    );

    // An exact cached pair is served without a send. A cached wide answer is
    // used only when it needs no rerank, or when the rerank answer for its
    // shortlist is cached too: under an unpinned model alias a cached wide
    // answer is never paired with a fresh rerank.
    let mut store = CacheSlot { store, lost: false };
    let lookup_pair = |store: &mut CacheSlot| -> Option<(Response, u64, Option<Response>)> {
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
            // Keyed to this wide answer, so only the rerank recorded with it
            // can complete the pair (sr-z1u0).
            let rerank_fp = fingerprint(
                RequestStage::Rerank,
                &shortlist_digests(list),
                builder.bytes(),
                rerank::RERANK_POLICY_VERSION,
                Some(&wide_req_fp),
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
    // Set only when another process leads this exact request and this run is
    // not taking over: then sending would duplicate its evaluation. A lease that
    // could not be read or acquired is no evidence of a leader, and the run
    // sends uncoordinated.
    let mut deferred_to_leader = false;
    // Single flight: on a miss, one process per exact request sends. Another
    // process with the same request waits for that lease and is then served
    // the pair it recorded; if the leader fails, the follower sends itself.
    // Leases need the persistent cache and persistent runtime state.
    if cached.is_none()
        && store.store.is_some()
        && matches!(gate.runtime_state(), StoreAccess::Enabled)
        && let Some(dir) = &args.cache_dir
    {
        let leases = dir.join(persistent::LEASES_FILE);
        let key = CoordinationKey::compute(&cache_key, &cache_ns, &wide_req_fp);
        match persistent::acquire(invocation, cx, clock, &leases, key).await {
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
                // Take over only with a second left for our own evaluation.
                // A shorter remainder means the leader is still the owner:
                // do not acquire and send.
                if cached.is_none()
                    && clock.remaining_before_cleanup().as_millis() >= 1_000
                    && let Some(LeaseAcquisition::Leading(leader)) =
                        persistent::acquire(invocation, cx, clock, &leases, key).await
                {
                    progress.lease = Some((leases, leader));
                }
                deferred_to_leader = cached.is_none() && progress.lease.is_none();
            }
            Some(LeaseAcquisition::AlreadyCompleted) => {
                cached = lookup_pair(&mut store);
                if cached.is_none()
                    && let Some(LeaseAcquisition::Leading(leader)) =
                        persistent::force_reacquire(invocation, cx, &leases, key)
                {
                    progress.lease = Some((leases, leader));
                }
            }
            None => {}
        }
    }
    // Armed before any send. A run that fails after paying for an attempt unwinds
    // past every recording site, so what an unavailable event needs is captured
    // here, while the context that names it is still in scope.
    // The in-flight and failure paths share this optional recording state. Do
    // not even open an existing ledger when the user's effect policy disables it.
    progress.failed_recording =
        matches!(gate.ledger(), StoreAccess::Enabled).then(|| FailureRecording {
            ledger_dir: args.ledger_dir.clone(),
            workspace_root: normalized_context.workspace_root.as_str().to_string(),
            session_id: normalized_context
                .session_id
                .as_ref()
                .map_or_else(|| "session-0".to_string(), |s| s.as_str().to_string()),
            agent_branch: normalized_context
                .branch_id
                .as_ref()
                .map_or_else(|| "main".to_string(), |b| b.as_str().to_string()),
            mode_channel: mode_channel.to_string(),
            policy_version: "ranking-v1",
            event_id: normalized_context
                .current_request
                .event_id
                .as_ref()
                .map_or_else(
                    || derive_request_event_id(&normalized_context),
                    |e| e.as_str().to_string(),
                ),
            attempts: Vec::new(),
        });
    // Written before the first send, so an invocation killed while waiting on the
    // provider is not simply absent from the ledger. `unavailable` and `generated`
    // are the truth at this moment: work has started, nothing has been decided, and
    // nothing has been delivered. A completing invocation updates both fields; one
    // that dies leaves this row behind as the only evidence that it ran and may have
    // incurred cost. A reader treats a `generated` row older than the invocation
    // deadline as died in flight, with unknown cost rather than none.
    // A journal is possible only once that row is durably in place, because every attempt
    // row references it. When the row did not land there is nothing to attach attempts to,
    // and the invocation proceeds with no durable per-attempt record rather than failing.
    let rerank_fingerprint_for_journal: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let attempt_journal: Option<Arc<Mutex<dyn crate::jev::AttemptJournal + Send + '_>>> = progress
        .failed_recording
        .as_ref()
        .filter(|recording| record_inflight_ranking(invocation, cx, recording))
        .map(
            |recording| -> Arc<Mutex<dyn crate::jev::AttemptJournal + Send + '_>> {
                Arc::new(Mutex::new(LedgerAttemptJournal {
                    invocation,
                    cx: cx.clone(),
                    location: match recording.ledger_dir.as_deref() {
                        Some(dir) => crate::storage::LedgerLocation::Directory(dir.to_path_buf()),
                        None => crate::storage::LedgerLocation::Platform,
                    },
                    owner_event_id: recording.event_id.clone(),
                    invocation_token: invocation_row_mark().to_string(),
                    entry_wall_clock_unix_ms: entry_wall_clock_unix_ms(clock),
                    wide_fingerprint: wide_req_fp.to_hex(),
                    rerank_fingerprint: Arc::clone(&rerank_fingerprint_for_journal),
                }))
            },
        );

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
            if args.cursor.is_some() {
                return Err(failure(
                    ErrorKind::CacheMiss,
                    "Trace continuation requires exact cached evaluation evidence",
                ));
            }
            if deferred_to_leader {
                return Err(failure(
                    ErrorKind::Timeout,
                    "Follower deadline reached while request was owned by active leader",
                ));
            }
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
                        ErrorKind::InvalidConfiguration,
                        format!("Client init failed: {e}"),
                    )
                })?),
            };
            let credential = match (resolved_config.credential(), client.origin()) {
                (Some(credential), Some(origin)) => Some(&*bound_credential.insert(
                    OriginScopedCredential::bind(credential.clone(), origin).map_err(|_| {
                        failure(ErrorKind::Authentication, "Credential origin mismatch")
                    })?,
                )),
                _ => None,
            };
            let active = session.insert({
                let opened = RetrySession::new(
                    client,
                    credential,
                    *clock,
                    AttemptBudget::default_invocation(),
                    "rank",
                )
                .map_err(|_| {
                    let kind = ErrorKind::BudgetState;
                    failure(kind, "The attempt allowance could not be opened")
                })?;
                // Attached here rather than passed to `new`, because a session is usable
                // without one: only a caller that wants crash evidence pays for it.
                match attempt_journal.as_ref() {
                    Some(journal) => opened.with_journal(Arc::clone(journal)),
                    None => opened,
                }
            });
            let stage = provider_stage(
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
            .await;
            // Before propagating: this attempt has settled either way, and a send
            // that failed is the one whose cost nothing else records.
            progress.stash_failed_attempts(session.as_ref(), &wide_req_fp, None, clock);
            stage?
        }
    };

    // Evaluate Wide Response
    let wide_outcome = wide::evaluate(&wide_builder, &wide_response, gate_threshold, sizes)
        .map_err(|e| {
            failure(
                ErrorKind::InvalidProviderResponse,
                format!("Wide evaluation failed: {e:?}"),
            )
        })?;
    if let Some(evidence) = progress.stage_evidence.as_mut() {
        evidence.wide = Some(WideEvidence {
            needs_skill: wide_outcome.needs_skill,
            none_probability: wide_outcome.none_probability,
            low_need: matches!(wide_outcome.decision, WideDecision::LowNeed),
            shortlist: match &wide_outcome.decision {
                WideDecision::Shortlist(list) => list
                    .iter()
                    .map(|s| (s.skill.binding.id.as_str().to_owned(), s.wide_probability))
                    .collect(),
                WideDecision::LowNeed => Vec::new(),
            },
            intrinsic_shortlist: wide_outcome
                .intrinsic_shortlist
                .iter()
                .map(|s| s.skill.binding.id.as_str().to_owned())
                .collect(),
        });
    }
    // A fresh wide answer is held and published together with the rerank it
    // leads to, or alone when it needs none (sr-ron8). A rerank failure then
    // caches no wide answer, which is the price of never mixing evaluations.
    let mut pending_wide = wide_fresh.then(|| {
        cache_entry(
            RequestStage::Wide,
            wide_req_fp,
            &wide_response,
            active_model,
        )
    });
    progress.evaluated.wide_returned = Some(wide_response.returned_model.clone());
    progress.evaluated.needs_skill = Some(wide_outcome.needs_skill);
    progress.evaluated.phase = dominant_phase(&wide_outcome.phase).map(str::to_owned);

    if let Some(capture) = &mut progress.capture {
        if let Some(manifest) = &mut capture.manifest {
            manifest.stages_recorded.push("wide".to_string());
        }
        if let Some(crate::jev::codec::Answer::Choice(which)) =
            wide_response.answers.get(wide::WHICH)
        {
            let choice_str = if which.choice() == wide::NONE_OPTION {
                "__none__".to_string()
            } else if let Ok(ResolvedOption::Skill(skill)) =
                wide_builder.options().resolve(which.choice())
            {
                skill.binding.id.as_str().to_string()
            } else {
                which.choice().to_string()
            };
            let mut distribution = Vec::new();
            for (opt_id, prob) in which.normalized_probabilities() {
                let mapped_id = if opt_id == wide::NONE_OPTION {
                    "__none__".to_string()
                } else if let Ok(ResolvedOption::Skill(skill)) =
                    wide_builder.options().resolve(&opt_id)
                {
                    skill.binding.id.as_str().to_string()
                } else {
                    continue;
                };
                distribution.push(ChoiceDistributionItem {
                    option_id: mapped_id,
                    probability: prob,
                });
            }
            distribution.sort_by(|a, b| a.option_id.cmp(&b.option_id));
            capture.recorded_responses.wide = Some(RecordedWideChoice {
                choice: choice_str,
                choices_probability: which.normalized_probability(which.choice()).unwrap_or(0.0),
                gate_score: Some(wide_outcome.needs_skill),
                distribution,
            });
        }
    }

    let shortlisted = match &wide_outcome.decision {
        WideDecision::LowNeed => {
            if let Some(wide) = pending_wide.take() {
                publish_evaluation(&mut store, invocation, cx, namespace, wide, None, progress)?;
            }
            let recorded = try_record_ledger(
                invocation,
                cx,
                &gate,
                args.ledger_dir.as_deref(),
                &roster,
                &normalized_context,
                crate::storage::DecisionKind::Abstain,
                "low-need",
                mode_channel,
                clock.now().as_millis(),
                &progress.metrics,
                &[],
                attempt_evidence(session.as_ref(), &wide_req_fp, None, clock).as_ref(),
            );
            progress.evaluated.ledger_recorded = recorded;
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
                &trace_query_scope,
                Some(&policy_view),
                &retrieval_view,
                Some(&wide_outcome),
                gate_threshold,
                None,
                fits_threshold,
                None,
                None,
                false,
            )? {
                doc = doc.with_trace(trace_val).map_err(|e| {
                    failure(
                        ErrorKind::OutputLimit,
                        format!("Trace contract error: {e:?}"),
                    )
                })?;
            }
            validate_advisory_publication(
                &roster_source,
                &dependencies,
                &config_files,
                &resolved_config,
                &current_receipt,
                cx,
                clock,
            )?;
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
    .map_err(|e| failure(e.kind(), format!("Rerank build failed: {e:?}")))?;

    // A cached wide answer arrives with its cached rerank answer. Otherwise
    // rerank pairs with the wide answer this session produced; a wide answer
    // from elsewhere is never paired with a fresh rerank.
    let rerank_fresh = cached_rerank.is_none();
    // Computed before the send rather than after it: an attempt row names the
    // request it served, and a rerank send that fails still made an attempt.
    let rerank_fp = rerank_fresh.then(|| {
        fingerprint(
            RequestStage::Rerank,
            &shortlist_digests(&shortlisted),
            rerank_builder.bytes(),
            rerank::RERANK_POLICY_VERSION,
            Some(&wide_req_fp),
        )
    });
    let rerank_req_fp: Option<String> = rerank_fp.map(|fp| fp.to_hex());
    // Published before any rerank attempt can be admitted, so a rerank attempt's durable
    // row names the request it was made for rather than going unrecorded.
    if let Some(fingerprint) = rerank_req_fp.as_ref()
        && let Ok(mut held) = rerank_fingerprint_for_journal.lock()
    {
        *held = Some(fingerprint.clone());
    }
    let rerank_response = match cached_rerank.take() {
        Some(response) => {
            progress.metrics.rerank_hit = true;
            response
        }
        None => {
            if args.cursor.is_some() {
                return Err(failure(
                    ErrorKind::CacheMiss,
                    "Trace continuation requires exact cached evaluation evidence",
                ));
            }
            // The wide answer is held for one atomic publication (sr-ron8), so
            // no fenced write has proven this leader current yet. A superseded
            // leader must not pay for a rerank it can no longer publish.
            if let Some((leases, leader)) = progress.lease.as_ref()
                && !persistent::still_leads(invocation, cx, leases, leader)
            {
                return Err(failure(
                    ErrorKind::Timeout,
                    "Leader was superseded before its rerank; no result can be published",
                ));
            }
            let Some(active) = session.as_mut() else {
                return Err(failure(
                    ErrorKind::CacheMiss,
                    "A cached wide answer cannot be paired with a fresh rerank",
                ));
            };
            let stage = provider_stage(
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
            .await;
            progress.stash_failed_attempts(
                session.as_ref(),
                &wide_req_fp,
                rerank_req_fp.as_deref(),
                clock,
            );
            stage?
        }
    };

    let rerank_outcome = rerank::evaluate(&rerank_builder, &rerank_response).map_err(|e| {
        failure(
            ErrorKind::InvalidProviderResponse,
            format!("Rerank evaluation failed: {e:?}"),
        )
    })?;
    if let Some(evidence) = progress.stage_evidence.as_mut() {
        evidence.rerank = Some(RerankEvidence {
            none_probability: rerank_outcome.none_probability,
            candidates: rerank_outcome
                .candidates
                .iter()
                .map(|(skill, estimate)| {
                    (
                        skill.binding.id.as_str().to_owned(),
                        estimate.rerank,
                        estimate.fit,
                    )
                })
                .collect(),
        });
    }

    if let Some(rerank_fp) = rerank_fp {
        let rerank_entry = cache_entry(
            RequestStage::Rerank,
            rerank_fp,
            &rerank_response,
            active_model,
        );
        match pending_wide.take() {
            Some(wide) => publish_evaluation(
                &mut store,
                invocation,
                cx,
                namespace,
                wide,
                Some(rerank_entry),
                progress,
            )?,
            // Not reached while the cache serves only complete pairs; kept so
            // a fresh rerank is never silently dropped.
            None => {
                if !persistent::record(
                    &mut store,
                    invocation,
                    cx,
                    namespace,
                    rerank_entry,
                    progress.lease.as_ref(),
                )? {
                    progress.cache_recording_failures += 1;
                }
            }
        }
    }
    progress.evaluated.rerank_returned = Some(rerank_response.returned_model.clone());
    progress.evaluated.choice_confidence = Some(rerank_outcome.choice_confidence);
    progress.evaluated.none_probability = Some(rerank_outcome.none_probability);

    if let Some(capture) = &mut progress.capture {
        if let Some(manifest) = &mut capture.manifest {
            manifest.stages_recorded.push("rerank".to_string());
        }
        if let Some(crate::jev::codec::Answer::Choice(choice)) =
            rerank_response.answers.get(rerank::RERANK)
        {
            let choice_str = if choice.choice() == wide::NONE_OPTION {
                "__none__".to_string()
            } else if let Ok(ResolvedOption::Skill(skill)) =
                rerank_builder.options().resolve(choice.choice())
            {
                skill.binding.id.as_str().to_string()
            } else {
                choice.choice().to_string()
            };
            let mut distribution = Vec::new();
            for (opt_id, prob) in choice.normalized_probabilities() {
                let mapped_id = if opt_id == wide::NONE_OPTION {
                    "__none__".to_string()
                } else if let Ok(ResolvedOption::Skill(skill)) =
                    rerank_builder.options().resolve(&opt_id)
                {
                    skill.binding.id.as_str().to_string()
                } else {
                    continue;
                };
                distribution.push(ChoiceDistributionItem {
                    option_id: mapped_id,
                    probability: prob,
                });
            }
            distribution.sort_by(|a, b| a.option_id.cmp(&b.option_id));
            let fits = rerank_outcome
                .candidates
                .iter()
                .map(|(skill, estimate)| CandidateFitItem {
                    skill_id: skill.binding.id.as_str().to_string(),
                    fit: estimate.fit,
                })
                .collect();
            capture.recorded_responses.rerank = Some(RecordedRerankChoice {
                choice: choice_str,
                choices_probability: choice
                    .normalized_probability(choice.choice())
                    .unwrap_or(0.0),
                stated_confidence: Some(choice.confidence()),
                fits,
                distribution,
            });
        }
    }

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
                let recorded = try_record_ledger(
                    invocation,
                    cx,
                    &gate,
                    args.ledger_dir.as_deref(),
                    &roster,
                    &normalized_context,
                    crate::storage::DecisionKind::Abstain,
                    reason.as_str(),
                    mode_channel,
                    clock.now().as_millis(),
                    &progress.metrics,
                    &[],
                    attempt_evidence(
                        session.as_ref(),
                        &wide_req_fp,
                        rerank_req_fp.as_deref(),
                        clock,
                    )
                    .as_ref(),
                );
                progress.evaluated.ledger_recorded = recorded;
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
                    &trace_query_scope,
                    Some(&policy_view),
                    &retrieval_view,
                    Some(&wide_outcome),
                    gate_threshold,
                    Some(&rerank_outcome),
                    fits_threshold,
                    Some(&evaluation),
                    None,
                    false,
                )? {
                    doc = doc.with_trace(trace_val).map_err(|e| {
                        failure(
                            ErrorKind::OutputLimit,
                            format!("Trace contract error: {e:?}"),
                        )
                    })?;
                }
                validate_advisory_publication(
                    &roster_source,
                    &dependencies,
                    &config_files,
                    &resolved_config,
                    &current_receipt,
                    cx,
                    clock,
                )?;
                return Ok(doc);
            }
            Verdict::Unavailable(reason) => {
                let recorded = try_record_ledger(
                    invocation,
                    cx,
                    &gate,
                    args.ledger_dir.as_deref(),
                    &roster,
                    &normalized_context,
                    crate::storage::DecisionKind::Unavailable,
                    reason.as_str(),
                    mode_channel,
                    clock.now().as_millis(),
                    &progress.metrics,
                    &[],
                    attempt_evidence(
                        session.as_ref(),
                        &wide_req_fp,
                        rerank_req_fp.as_deref(),
                        clock,
                    )
                    .as_ref(),
                );
                progress.evaluated.ledger_recorded = recorded;
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
                ErrorKind::InvalidConfiguration,
                "Ranking weights are out of bounds",
            )
        })?;
    let scored_ranking = rank(&scoring_inputs, weights, top).map_err(|e| {
        failure(
            ErrorKind::InvalidProviderResponse,
            format!("Scoring failed: {e:?}"),
        )
    })?;

    // 16. Build Ranked OutputDocument
    let event_id_str = normalized_context
        .current_request
        .event_id
        .as_ref()
        .map(|e| e.as_str().to_string())
        .unwrap_or_else(|| derive_request_event_id(&normalized_context));

    // Scoring uses renormalized probabilities; the ledger keeps the values the
    // provider actually returned beside them.
    let wide_raw = raw_skill_probabilities(
        wide_response.answers.get(wide::WHICH),
        wide_builder.options(),
    );
    let rerank_raw = raw_skill_probabilities(
        rerank_response.answers.get(rerank::RERANK),
        rerank_builder.options(),
    );
    let mut ranking_candidates = Vec::new();
    for s in &shortlisted {
        ranking_candidates.push(crate::storage::NewRankingCandidate {
            event_id: event_id_str.clone(),
            stage: crate::storage::CandidateStage::Wide,
            skill_id: s.skill.binding.id.as_str().to_string(),
            skill_version: s.skill.record.source_content.as_str().to_string(),
            raw_probability: wide_raw.get(&s.skill.binding.id).copied(),
            normalized_probability: Some(s.wide_probability),
            fit_score: None,
            rank_score: None,
            rank_position: None,
            excluded: false,
            exclusion_reason: None,
        });
    }
    for (i, scored) in scored_ranking.returned.iter().enumerate() {
        let el = &evaluation.eligible[scored.index];
        ranking_candidates.push(crate::storage::NewRankingCandidate {
            event_id: event_id_str.clone(),
            stage: crate::storage::CandidateStage::Rerank,
            skill_id: el.skill.binding.id.as_str().to_string(),
            skill_version: el.skill.record.source_content.as_str().to_string(),
            raw_probability: rerank_raw.get(&el.skill.binding.id).copied(),
            normalized_probability: Some(el.rerank),
            fit_score: Some(el.fit),
            rank_score: Some(scored.rank_score),
            rank_position: Some((i + 1) as u32),
            excluded: false,
            exclusion_reason: None,
        });
    }
    for (id, reason) in &evaluation.removed {
        let version = shortlisted
            .iter()
            .find(|s| s.skill.binding.id == *id)
            .map(|s| s.skill.record.source_content.as_str().to_string())
            .unwrap_or_default();
        ranking_candidates.push(crate::storage::NewRankingCandidate {
            event_id: event_id_str.clone(),
            stage: crate::storage::CandidateStage::Rerank,
            skill_id: id.as_str().to_string(),
            skill_version: version,
            raw_probability: None,
            normalized_probability: None,
            fit_score: None,
            rank_score: None,
            rank_position: None,
            excluded: true,
            exclusion_reason: Some(reason.as_str().to_string()),
        });
    }

    let recorded = try_record_ledger(
        invocation,
        cx,
        &gate,
        args.ledger_dir.as_deref(),
        &roster,
        &normalized_context,
        crate::storage::DecisionKind::Ranked,
        "eligible-candidates",
        mode_channel,
        clock.now().as_millis(),
        &progress.metrics,
        &ranking_candidates,
        attempt_evidence(
            session.as_ref(),
            &wide_req_fp,
            rerank_req_fp.as_deref(),
            clock,
        )
        .as_ref(),
    );
    progress.evaluated.ledger_recorded = recorded;

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
        &trace_query_scope,
        Some(&policy_view),
        &retrieval_view,
        Some(&wide_outcome),
        gate_threshold,
        Some(&rerank_outcome),
        fits_threshold,
        Some(&evaluation),
        Some(&scored_ranking),
        true,
    )? {
        doc = doc.with_trace(trace_val).map_err(|e| {
            failure(
                ErrorKind::OutputLimit,
                format!("Trace contract error: {e:?}"),
            )
        })?;
    }

    validate_advisory_publication(
        &roster_source,
        &dependencies,
        &config_files,
        &resolved_config,
        &current_receipt,
        cx,
        clock,
    )?;
    Ok(doc)
}

/// Revalidate every advisory outcome, including a negative recommendation.
/// Cache reuse never bypasses this boundary: its responses still feed the same
/// evaluation paths. Run after rendering so trace work shares the deadline too.
fn validate_advisory_publication(
    source: &roster::Source<'_>,
    dependencies: &crate::roster::revalidation::Dependencies,
    config_files: &ConfigFiles,
    config: &ResolvedConfig,
    receipt: &PolicyReceipt,
    cx: &Cx,
    clock: &EntryClock,
) -> Result<(), PipelineFailure> {
    source.validate(dependencies, cx, clock)?;
    let (_, revalidation) = config_files.refresh(
        clock,
        config,
        receipt,
        PolicyBoundary::CliPublication(PublicationKind::Advisory),
    )?;
    match revalidation {
        Revalidation::Superseded(fields) => {
            return Err(failure(
                ErrorKind::Superseded,
                format!("Policy superseded before publication: {fields:?}"),
            ));
        }
        Revalidation::InvalidConfiguration => {
            return Err(failure(
                ErrorKind::InvalidConfiguration,
                "Configuration became invalid before publication",
            ));
        }
        _ => {}
    }
    admit_publication(clock.deadline(), clock.now(), clock.now()).map_err(|error| {
        failure(
            ErrorKind::Timeout,
            format!("Runtime suppressed late result: {error}"),
        )
    })?;
    Ok(())
}

/// Roster coverage gaps as bounded warnings: records excluded while resolving
/// discovered files, and sources that could not be fully observed, by stable
/// code and count. A normally absent optional root is not a gap. Paths and
/// skill text never appear.
fn roster_warnings(roster: &ResolvedRoster, source: Option<&Value>) -> (Vec<Value>, usize) {
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
    // The session-selection disclosure and the provisional precedence caveat
    // come first so neither is truncated.
    let mut warnings: Vec<Value> = source
        .cloned()
        .into_iter()
        .chain(caveat)
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
/// One previewed provider request: the exact serialized bytes as text.
fn preview_stage(stage: &str, request: &[u8], candidates: usize) -> Result<Value, PipelineFailure> {
    let text = std::str::from_utf8(request).map_err(|_| {
        let kind = ErrorKind::OutputLimit;
        failure(kind, "The request is not valid UTF-8")
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
        failure(kind, "The preview exceeds the output contract")
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

/// Each skill's probability as the provider returned it in one Choice answer,
/// before renormalization. Option IDs resolve only through the request's own
/// option map.
fn raw_skill_probabilities(
    answer: Option<&crate::jev::codec::Answer>,
    options: &crate::roster::resolution::OptionMap<'_>,
) -> BTreeMap<SkillId, f64> {
    let Some(crate::jev::codec::Answer::Choice(choice)) = answer else {
        return BTreeMap::new();
    };
    choice
        .raw_probabilities()
        .iter()
        .filter_map(|(option, &probability)| match options.resolve(option) {
            Ok(ResolvedOption::Skill(skill)) => Some((skill.binding.id.clone(), probability)),
            _ => None,
        })
        .collect()
}

/// Claude's exact wrapper for a background task's completion message, as
/// every live delivery measured it: `<task-notification>` then a `<task-id>`
/// element, closing with `</task-notification>`.
fn is_task_notification_envelope(prompt: &str) -> bool {
    let prompt = prompt.trim();
    prompt
        .strip_prefix("<task-notification>")
        .is_some_and(|rest| rest.trim_start().starts_with("<task-id>"))
        && prompt.ends_with("</task-notification>")
}

/// The run's response-cache store. A store operation consumes the store, and a
/// failed one does not return it, so the slot remembers the loss. Later
/// recordings are then reported as skipped rather than as written.
/// Publish a complete evaluation through the run's cache slot. A skipped
/// publication is counted, and a lease completed inside the publishing
/// transaction is dropped from `progress`, so the final release does not
/// complete it again.
fn publish_evaluation(
    store: &mut CacheSlot,
    invocation: &ProcessInvocation,
    cx: &Cx,
    namespace: [u8; 32],
    wide: crate::cache::CachedResponseEntry,
    rerank: Option<crate::cache::CachedResponseEntry>,
    progress: &mut Progress,
) -> Result<(), PipelineFailure> {
    let (recorded, lease_completed) = persistent::publish(
        store,
        invocation,
        cx,
        namespace,
        wide,
        rerank,
        progress.lease.as_ref(),
    )?;
    if !recorded {
        progress.cache_recording_failures += 1;
    }
    if lease_completed {
        progress.lease = None;
    }
    Ok(())
}

struct CacheSlot {
    store: Option<persistent::Store>,
    lost: bool,
}

/// The persistent exact response cache. Linux and macOS use the qualified store;
/// elsewhere every run keys fingerprints with fresh randomness and caches
/// nothing. Store failures never fail ranking: the store is dropped and the
/// run continues uncached.
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod persistent {
    use super::{EffectGate, ProcessInvocation, wall_clock_ms};
    use crate::cache::{
        CacheKey, CachedResponseEntry, CoordinationKey, FreshnessStatus, LeaderContext,
        LeaseAcquisition, PublishOutcome, RequestFingerprint, RequestStage,
    };
    use crate::runtime::EntryClock;
    use crate::storage::{CacheAccess, CacheLocation, CacheOpen, CacheStore, StoreError};
    use asupersync::Cx;
    use std::path::Path;
    use std::time::Duration;

    /// Leases and validated response rows share the qualified cache database.
    pub(super) const LEASES_FILE: &str = crate::storage::CACHE_FILE;

    fn coordinator(invocation: &ProcessInvocation, cx: &Cx, path: &Path) -> Option<CacheStore> {
        if path.file_name()? != crate::storage::CACHE_FILE {
            return None;
        }
        match crate::storage::open_cache(
            invocation,
            cx,
            CacheAccess::ExistingOnly,
            CacheLocation::Directory(path.parent()?.to_owned()),
        )
        .ok()?
        {
            CacheOpen::Ready(store) => Some(*store),
            CacheOpen::Disabled | CacheOpen::Missing => None,
        }
    }

    /// Acquire the lease, retrying briefly: two processes opening the lease
    /// store at once can meet SQLite busy beyond its 25 ms wait. Without a
    /// lease the run sends uncoordinated, so give up only after a few tries,
    /// on cancellation, or while a second of budget remains for its own work.
    pub(super) async fn acquire(
        invocation: &ProcessInvocation,
        cx: &Cx,
        clock: &EntryClock,
        path: &Path,
        key: CoordinationKey,
    ) -> Option<LeaseAcquisition> {
        for _ in 0..5 {
            if let Some(acquisition) = try_acquire(invocation, cx, path, key) {
                return Some(acquisition);
            }
            if cx.is_cancel_requested() || clock.remaining_before_cleanup().as_millis() < 1_000 {
                break;
            }
            asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(10)).await;
        }
        None
    }

    fn try_acquire(
        invocation: &ProcessInvocation,
        cx: &Cx,
        path: &Path,
        key: CoordinationKey,
    ) -> Option<LeaseAcquisition> {
        coordinator(invocation, cx, path)?
            .acquire_lease(invocation, cx, key, false)
            .ok()
            .map(|(_, result)| result)
    }

    /// True once the lease is gone, completed or expired. A failed read (for
    /// example SQLite busy) is not settlement: the caller keeps waiting within
    /// the lease and its budget rather than sending a duplicate evaluation.
    fn settled(invocation: &ProcessInvocation, cx: &Cx, path: &Path, key: CoordinationKey) -> bool {
        match coordinator(invocation, cx, path)
            .and_then(|store| store.lease(invocation, cx, key).ok())
        {
            Some((_, Some(lease))) => {
                lease.is_completed || wall_clock_ms() >= lease.expires_at_unix_ms
            }
            Some((_, None)) => true,
            None => false,
        }
    }

    /// Wait while the leader's lease is active.
    ///
    /// The one-second remainder is a takeover reserve, not a reason to stop
    /// waiting. Returning as soon as less than a second remained made a short
    /// follower skip the wait and left a contended start no time to arrive.
    /// While the lease is still held, wait until it settles or this
    /// invocation's cleanup reserve. Once it has settled or expired, return
    /// so a follower that still has a second can take over.
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
            if cx.is_cancel_requested() || wall_clock_ms() >= expires_at_unix_ms {
                return;
            }
            let remaining = clock.remaining_before_cleanup().as_millis();
            if remaining < 25 || settled(invocation, cx, &path, key) {
                return;
            }
            let slice = remaining.min(25);
            asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(slice))
                .await;
        }
    }

    pub(super) fn force_reacquire(
        invocation: &ProcessInvocation,
        cx: &Cx,
        path: &Path,
        key: CoordinationKey,
    ) -> Option<LeaseAcquisition> {
        coordinator(invocation, cx, path)?
            .acquire_lease(invocation, cx, key, true)
            .ok()
            .map(|(_, result)| result)
    }

    pub(super) fn complete(
        invocation: &ProcessInvocation,
        cx: &Cx,
        path: &Path,
        leader: &LeaderContext,
    ) -> Option<PublishOutcome> {
        coordinator(invocation, cx, path)?
            .complete_lease(invocation, cx, leader.clone())
            .ok()
            .map(|(_, result)| result)
    }

    /// Whether this run still holds its lease: same owner and fencing
    /// generation, not completed, not expired. A read without a writer lock.
    /// When the store cannot answer, `true`: the fenced publication still
    /// refuses a superseded owner, so this check only saves spend.
    pub(super) fn still_leads(
        invocation: &ProcessInvocation,
        cx: &Cx,
        path: &Path,
        leader: &LeaderContext,
    ) -> bool {
        let Some(store) = coordinator(invocation, cx, path) else {
            return true;
        };
        match store.lease(invocation, cx, leader.key) {
            Ok((_, Some(record))) => {
                record.owner_token == leader.owner_token
                    && record.fencing_generation == leader.fencing_generation
                    && !record.is_completed
                    && wall_clock_ms() < record.expires_at_unix_ms
            }
            Ok((_, None)) => false,
            Err(_) => true,
        }
    }

    pub(super) struct Store(CacheStore);

    impl Store {
        /// Open the store, retrying briefly while another process holds its
        /// lock (two processes initializing it at once): an unopened store
        /// means no cache and no single flight for this run.
        /// `Ok(Some)` opened the store, `Ok(None)` means an effect flag turned
        /// it off, and `Err` keeps the reason it could not be used. Callers
        /// disclose that reason: a silently missing cache is indistinguishable
        /// from a correct refusal, and costs every later run a fresh pair.
        pub(super) async fn open(
            invocation: &ProcessInvocation,
            cx: &Cx,
            clock: &EntryClock,
            gate: &EffectGate,
            dir: &Path,
        ) -> Result<Option<Self>, StoreError> {
            let mut last = StoreError::Busy;
            for _ in 0..5 {
                match gate.open_cache(
                    invocation,
                    cx,
                    CacheAccess::Initialize,
                    CacheLocation::Directory(dir.to_path_buf()),
                ) {
                    Ok(CacheOpen::Ready(store)) => return Ok(Some(Self(*store))),
                    Ok(_) => return Ok(None),
                    Err(StoreError::Busy)
                        if !cx.is_cancel_requested()
                            && clock.remaining_before_cleanup().as_millis() >= 1_000 =>
                    {
                        asupersync::time::sleep(
                            asupersync::time::wall_now(),
                            Duration::from_millis(10),
                        )
                        .await;
                    }
                    Err(error) => {
                        last = error;
                        break;
                    }
                }
            }
            Err(last)
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
        slot: &mut super::CacheSlot,
        invocation: &ProcessInvocation,
        cx: &Cx,
        namespace: [u8; 32],
        stage: RequestStage,
        fingerprint: RequestFingerprint,
        model: &str,
        now_unix_ms: u64,
    ) -> Option<(Vec<u8>, u64)> {
        let Store(store) = slot.store.take()?;
        let Ok((store, entry)) = store.response(invocation, cx, namespace, stage, fingerprint)
        else {
            slot.lost = true;
            return None;
        };
        slot.store = Some(Store(store));
        let entry = entry?;
        match entry.evaluate_freshness(now_unix_ms, model, None) {
            FreshnessStatus::Fresh { age_ms, .. } => Some((entry.response_bytes, age_ms)),
            _ => None,
        }
    }

    /// Whether the entry was written, or nothing was required: `false` when a
    /// store this run opened failed now or earlier.
    pub(super) fn record(
        slot: &mut super::CacheSlot,
        invocation: &ProcessInvocation,
        cx: &Cx,
        namespace: [u8; 32],
        entry: CachedResponseEntry,
        fence: Option<&(std::path::PathBuf, LeaderContext)>,
    ) -> Result<bool, super::PipelineFailure> {
        let Some(Store(store)) = slot.store.take() else {
            return Ok(!slot.lost);
        };
        let result = match fence {
            Some(fence) => {
                store.record_response_fenced(invocation, cx, namespace, entry, fence.clone())
            }
            None => {
                let now = entry.received_at_unix_ms;
                store.record_response(invocation, cx, namespace, entry, now)
            }
        };
        match result {
            Ok(store) => slot.store = Some(Store(store)),
            Err(StoreError::LeaseSuperseded) => {
                slot.lost = true;
                return Err(super::failure(
                    super::ErrorKind::Timeout,
                    "Leader cache publication could not verify and retain lease ownership",
                ));
            }
            Err(_) => {
                slot.lost = true;
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Publish one complete evaluation (the wide answer and, when the wide
    /// decision needed one, its rerank) in a single transaction that also
    /// completes a led lease (sr-ron8). The rerank key names only the wide
    /// request, so publishing the stages separately let a crash or an
    /// interleaved writer pair a newer wide answer with an older rerank.
    /// Returns (written or nothing required, lease completed here); `false`
    /// in the first place when a store this run opened failed now or earlier.
    pub(super) fn publish(
        slot: &mut super::CacheSlot,
        invocation: &ProcessInvocation,
        cx: &Cx,
        namespace: [u8; 32],
        wide: CachedResponseEntry,
        rerank: Option<CachedResponseEntry>,
        fence: Option<&(std::path::PathBuf, LeaderContext)>,
    ) -> Result<(bool, bool), super::PipelineFailure> {
        let Some(Store(store)) = slot.store.take() else {
            return Ok((!slot.lost, false));
        };
        match store.publish_evaluation(invocation, cx, namespace, wide, rerank, fence.cloned()) {
            Ok(store) => {
                slot.store = Some(Store(store));
                Ok((true, fence.is_some()))
            }
            Err(StoreError::LeaseSuperseded) => {
                slot.lost = true;
                Err(super::failure(
                    super::ErrorKind::Timeout,
                    "Leader cache publication could not verify and retain lease ownership",
                ))
            }
            Err(_) => {
                slot.lost = true;
                Ok((false, false))
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod persistent {
    use super::{EffectGate, ProcessInvocation};
    use crate::cache::{
        CacheKey, CachedResponseEntry, CoordinationKey, LeaderContext, LeaseAcquisition,
        PublishOutcome, RequestFingerprint, RequestStage,
    };
    use crate::runtime::EntryClock;
    use asupersync::Cx;
    use std::path::Path;

    pub(super) const LEASES_FILE: &str = "leases.sqlite3";

    pub(super) async fn acquire(
        _: &ProcessInvocation,
        _: &Cx,
        _: &EntryClock,
        _: &Path,
        _: CoordinationKey,
    ) -> Option<LeaseAcquisition> {
        None
    }

    pub(super) fn force_reacquire(
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

    pub(super) fn complete(
        _: &ProcessInvocation,
        _: &Cx,
        _: &Path,
        _: &LeaderContext,
    ) -> Option<PublishOutcome> {
        None
    }

    pub(super) fn still_leads(_: &ProcessInvocation, _: &Cx, _: &Path, _: &LeaderContext) -> bool {
        true
    }

    pub(super) enum Store {}

    impl Store {
        /// No qualified store exists off Linux, so persistence is genuinely
        /// unavailable rather than disabled by choice.
        pub(super) async fn open(
            _: &ProcessInvocation,
            _: &Cx,
            _: &EntryClock,
            _: &EffectGate,
            _: &Path,
        ) -> Result<Option<Self>, StoreError> {
            Err(StoreError::UnqualifiedEngine)
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
        _: &mut super::CacheSlot,
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
        _: &mut super::CacheSlot,
        _: &ProcessInvocation,
        _: &Cx,
        _: [u8; 32],
        _: CachedResponseEntry,
        _: Option<&(std::path::PathBuf, LeaderContext)>,
    ) -> Result<bool, super::PipelineFailure> {
        Ok(true)
    }

    pub(super) fn publish(
        _: &mut super::CacheSlot,
        _: &ProcessInvocation,
        _: &Cx,
        _: [u8; 32],
        _: CachedResponseEntry,
        _: Option<CachedResponseEntry>,
        _: Option<&(std::path::PathBuf, LeaderContext)>,
    ) -> Result<(bool, bool), super::PipelineFailure> {
        Ok((true, false))
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
            ErrorKind::InvalidConfiguration,
            format!("Configuration invalid before {} send", stage.as_str()),
        ));
    }
    let current = refreshed.receipt(gate.policy());
    let consent = current.network_consent();
    admit_provider_attempt(consent, current.credential()).map_err(admission_refusal)?;
    if let Revalidation::Superseded(fields) = reval {
        return Err(failure(
            ErrorKind::Superseded,
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
            failure(kind, refusal.to_string())
        }
        RetryErrorKind::Accounting => {
            let kind = ErrorKind::BudgetState;
            failure(kind, "Attempt accounting failed")
        }
        RetryErrorKind::PolicyChanged => {
            failure(ErrorKind::Superseded, "Policy changed before send")
        }
        // A transient provider failure with no retry left: report that failure.
        RetryErrorKind::RetryStopped(_) => last.map_or_else(
            || failure(ErrorKind::ProviderFailure, "The provider request failed"),
            map_transport_error,
        ),
        RetryErrorKind::ModelPairMismatch => failure(
            ErrorKind::InvalidProviderResponse,
            "Wide and rerank answers came from different models",
        ),
    }
}

/// Offline is a cache miss (exit 11), not an authorization failure; missing
/// consent is a privacy denial (8). A missing credential is `credential-absent`
/// (4): an environment gap, kept apart from a key the provider rejected.
fn admission_refusal(refusal: ProviderAdmissionRefusal) -> PipelineFailure {
    let kind = refusal.kind();
    failure(kind, refusal.to_string())
}

fn map_transport_error(kind: TransportErrorKind) -> PipelineFailure {
    match kind {
        TransportErrorKind::Deadline | TransportErrorKind::Cancelled => {
            failure(ErrorKind::Timeout, "Jev request exceeded deadline")
        }
        TransportErrorKind::Admission(refusal) => admission_refusal(refusal),
        TransportErrorKind::Response(_) => failure(
            ErrorKind::InvalidProviderResponse,
            "The provider response failed validation",
        ),
        TransportErrorKind::CredentialOriginMismatch => {
            failure(ErrorKind::Authentication, "Credential origin mismatch")
        }
        TransportErrorKind::HttpStatus(401 | 403) => {
            failure(ErrorKind::Authentication, "Invalid credentials")
        }
        TransportErrorKind::HttpStatus(429) => {
            failure(ErrorKind::ProviderCooldown, "Rate limited by provider")
        }
        TransportErrorKind::HttpStatus(code) => {
            failure(ErrorKind::ProviderFailure, format!("HTTP error {code}"))
        }
        _ => failure(
            ErrorKind::NetworkFailure,
            format!("Transport error: {kind:?}"),
        ),
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
    let (warnings, warnings_omitted) = roster_warnings(roster, evaluated.source_warning.as_ref());
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

    let derived_event_id;
    let event_id = match context.current_request.event_id.as_ref() {
        Some(e) => e.as_str(),
        None => {
            derived_event_id = derive_request_event_id(context);
            derived_event_id.as_str()
        }
    };

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
    /// A qualified persistent store backed this run's cache lookups and
    /// records. Reported as `recorded`.
    store_backed: bool,
    /// Persistence was permitted, but no store could be used: a refused
    /// directory, a replaced store, or an unqualified engine. Disclosed as a
    /// warning so an uncached run is never silent.
    store_refused: bool,
    /// An effect flag turned the response cache off, so nothing persisted by
    /// choice rather than by failure.
    store_disabled: bool,
    /// A qualified persistent ledger recorded this run's event and snapshot.
    ledger_recorded: bool,
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
    /// How a discovered session was chosen, disclosed first among warnings.
    source_warning: Option<Value>,
}

impl Evaluated {
    /// `disabled` when an effect flag turned persistence off, `recorded` when a
    /// qualified store actually backed this run, `unavailable` otherwise. It
    /// reported `unavailable` unconditionally before, which said nothing true
    /// about a run whose cache was serving hits.
    fn persistence(&self) -> &'static str {
        if self.ledger_disabled || self.store_disabled {
            "disabled"
        } else if self.store_backed || self.ledger_recorded {
            "recorded"
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

pub(crate) fn derive_request_event_id(context: &NormalizedContext) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"skillranker.ranking-event-id.v1\n");
    if let Some(session) = &context.session_id {
        hasher.update(session.as_str().as_bytes());
    }
    hasher.update(b"\n");

    let prior_events: &[NormalizedEvent] = match context.events.last() {
        Some(last) if context.current_request.event_id.is_none() && last.event_id.is_none() => {
            &context.events[..context.events.len().saturating_sub(1)]
        }
        Some(last)
            if context.current_request.event_id.is_some()
                && last.event_id == context.current_request.event_id =>
        {
            &context.events[..context.events.len().saturating_sub(1)]
        }
        _ => &context.events[..],
    };

    let last_event_id = prior_events
        .iter()
        .rev()
        .find_map(|e| e.event_id.as_ref().map(|id| id.as_str()));
    if let Some(last_id) = last_event_id {
        hasher.update(last_id.as_bytes());
    }
    hasher.update(b"\n");

    let prior_count = prior_events.iter().filter(|e| e.event_id.is_some()).count();
    hasher.update(&prior_count.to_le_bytes());
    hasher.update(b"\n");
    hasher.update(context.current_request.text.as_str().as_bytes());

    format!("ev-{}", &hasher.finalize().to_hex()[..16])
}

/// Write this invocation down before it sends anything.
///
/// Best effort, like every other optional recording: a store that cannot take the
/// row does not change the ranking. The row carries no snapshot and no candidates,
/// because neither exists yet, and no attempts, because none have been admitted —
/// what it establishes is that an invocation with this identity was in flight.
/// Returns whether the row was durably written, which decides whether per-attempt rows
/// can follow it: `provider_attempts.owner_event_id` references this event, so without
/// it there is nothing for an attempt to belong to.
fn record_inflight_ranking(
    invocation: &ProcessInvocation,
    cx: &Cx,
    recording: &FailureRecording,
) -> bool {
    let event = crate::storage::NewRankingEvent {
        event_id: recording.event_id.clone(),
        verified_delivery_key: None,
        workspace_root: recording.workspace_root.clone(),
        session_id: recording.session_id.clone(),
        agent_branch: recording.agent_branch.clone(),
        mode_channel: recording.mode_channel.clone(),
        policy_version: recording.policy_version.to_string(),
        schema_version: SCHEMA_VERSION as u32,
        decision: crate::storage::DecisionKind::Unavailable,
        reason: crate::storage::ledger::IN_FLIGHT_REASON.to_string(),
        exposure_state: crate::storage::ExposureState::Generated,
        elapsed_ms: 0,
        created_at_unix_ms: wall_clock_ms(),
        input_tokens: None,
        output_tokens: None,
        snapshot_id: None,
    };
    let location = match recording.ledger_dir.as_deref() {
        Some(dir) => crate::storage::LedgerLocation::Directory(dir.to_path_buf()),
        None => crate::storage::LedgerLocation::Platform,
    };
    // `Ok(false)` means a store was not available to write to, which is not a recorded
    // row: only `Ok(true)` establishes that an attempt may now reference this event.
    matches!(
        crate::storage::record_ranking_with_attempts(
            invocation,
            invocation.clock(),
            cx,
            crate::storage::LedgerAccess::ExistingOnly,
            location,
            &event,
            &[],
            None,
            &[],
        ),
        Ok(true)
    )
}

/// Record an unavailable event owning the attempts a failed run already made.
///
/// The event carries no roster snapshot, because a run that failed mid-flight has
/// no decision whose membership it could describe; `snapshot_id` is nullable for
/// exactly this case. `reason` is the failure's own typed kind, never its message,
/// so nothing free-form reaches the ledger.
fn record_failed_attempts(
    invocation: &ProcessInvocation,
    recording: &FailureRecording,
    reason: &str,
    elapsed_ms: u64,
    metrics: &ExecutionMetrics,
) {
    let event = crate::storage::NewRankingEvent {
        event_id: recording.event_id.clone(),
        verified_delivery_key: None,
        workspace_root: recording.workspace_root.clone(),
        session_id: recording.session_id.clone(),
        agent_branch: recording.agent_branch.clone(),
        mode_channel: recording.mode_channel.clone(),
        policy_version: recording.policy_version.to_string(),
        schema_version: SCHEMA_VERSION as u32,
        decision: crate::storage::DecisionKind::Unavailable,
        reason: reason.to_string(),
        exposure_state: crate::storage::ExposureState::Prepared,
        elapsed_ms,
        created_at_unix_ms: wall_clock_ms(),
        input_tokens: (metrics.input_tokens > 0).then_some(metrics.input_tokens),
        output_tokens: (metrics.output_tokens > 0).then_some(metrics.output_tokens),
        snapshot_id: None,
    };
    let location = match recording.ledger_dir.as_deref() {
        Some(dir) => crate::storage::LedgerLocation::Directory(dir.to_path_buf()),
        None => crate::storage::LedgerLocation::Platform,
    };
    // The run may have failed because its work deadline passed, which would refuse this
    // write too and drop the failure from the availability denominator. Record it inside
    // the cleanup reserve on a cleanup context instead (sr-73b6).
    let _ = crate::storage::record_ranking_with_attempts(
        invocation,
        invocation.clock().for_failure_finalization(),
        &invocation.request_cleanup_cx(),
        crate::storage::LedgerAccess::ExistingOnly,
        location,
        &event,
        &[],
        None,
        &recording.attempts,
    );
}

/// What a recorded event needs in order to attribute the provider cost it caused:
/// the attempts this invocation admitted, the request identity each stage actually
/// sent, and the single wall-clock reading their monotonic times convert against.
///
/// A cache-served invocation admits no attempt and produces no evidence, so a
/// follower reusing an owner's response cannot appear to have paid for it.
struct AttemptEvidence<'a> {
    attempts: Vec<&'a crate::jev::AttemptProvenance>,
    wide_fingerprint: String,
    rerank_fingerprint: Option<String>,
    entry_wall_clock_unix_ms: u64,
    /// Distinguishes this invocation's attempts from another invocation's under the
    /// same event. A duplicate delivery is deliberately the same event, so without
    /// this a repeat that paid again would collide with the first delivery's rows.
    invocation_token: String,
}

impl AttemptEvidence<'_> {
    fn rows(&self, owner_event_id: &str) -> Vec<crate::storage::NewProviderAttempt> {
        self.attempts
            .iter()
            .filter_map(|attempt| {
                let fingerprint = match attempt.stage {
                    RankingStage::Wide => Some(self.wide_fingerprint.as_str()),
                    RankingStage::Rerank => self.rerank_fingerprint.as_deref(),
                    // A breaker probe or an evaluation batch is not one of this
                    // event's ranking requests, so it has no fingerprint here to
                    // claim and no row to occupy.
                    RankingStage::Probe | RankingStage::Evaluation => None,
                }?;
                crate::storage::NewProviderAttempt::from_provenance(
                    owner_event_id,
                    &self.invocation_token,
                    fingerprint,
                    self.entry_wall_clock_unix_ms,
                    attempt,
                )
            })
            .collect()
    }
}

/// Collect the attempts a session admitted, if it admitted any.
///
/// The wall-clock base is derived from one reading minus the elapsed monotonic
/// time, so every attempt time in this event shares a single conversion.
/// Distinguishes this invocation's attempt rows from another invocation's under the same
/// event, and does so identically every time it is asked.
///
/// A duplicate delivery is deliberately the *same* event, so the owner alone cannot key an
/// attempt row; the invocation has to contribute something too. Entry time and process id
/// together are enough — two deliveries are separate processes, and a shared start
/// millisecond is still separated by the pid.
///
/// It is computed once per process on purpose. The rows for one attempt are written more
/// than once, first on the sending path and again at settlement, and the entry time these
/// writers would each derive from the monotonic clock can differ by a millisecond. A token
/// recomputed per writer would therefore key the same attempt differently and leave two
/// rows where there was one attempt, which is exactly the double-counting the key exists
/// to prevent. Nothing in it is private or externally meaningful.
/// Not a credential, despite the word: it is a row-key discriminator, is derived from the
/// clock and the process id rather than from any source of randomness, and is neither secret
/// nor externally meaningful. Named `MARK` here because a static called `TOKEN` built this
/// way reads — to a human and to `ubs`'s `rust.security.non-crypto-random` rule — like a
/// session token generated without a CSPRNG, which is a mistake worth not appearing to make.
fn invocation_row_mark() -> &'static str {
    static MARK: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    MARK.get_or_init(|| format!("{:x}-{}", wall_clock_ms(), std::process::id()))
}

/// The wall-clock time this process entered, from one reading, shared by everything that
/// turns a monotonic instant into a stored timestamp.
///
/// An attempt's three timestamps are written by two different callers — the sending path
/// records when it was admitted and when its request reached the wire, settlement records
/// when it finished. Each deriving its own base from `wall_clock_ms() - clock.now()` gives
/// answers that can differ by a millisecond, which is enough to make one attempt's stored
/// times inconsistent with each other and, in a fast failure, to date its completion before
/// its send. Taking the base once removes the class of problem rather than the symptom.
///
/// The first caller's reading wins, which is also the closest one to actual entry.
fn entry_wall_clock_unix_ms(clock: &EntryClock) -> u64 {
    static ENTRY: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *ENTRY.get_or_init(|| wall_clock_ms().saturating_sub(clock.now().as_millis()))
}

fn attempt_evidence<'a>(
    session: Option<&'a RetrySession<'_>>,
    wide_fingerprint: &RequestFingerprint,
    rerank_fingerprint: Option<&str>,
    clock: &EntryClock,
) -> Option<AttemptEvidence<'a>> {
    let session = session?;
    let attempts: Vec<_> = session.attempts().collect();
    if attempts.is_empty() {
        return None;
    }
    let entry_wall_clock_unix_ms = entry_wall_clock_unix_ms(clock);
    Some(AttemptEvidence {
        attempts,
        wide_fingerprint: wide_fingerprint.to_hex(),
        rerank_fingerprint: rerank_fingerprint.map(str::to_owned),
        entry_wall_clock_unix_ms,
        invocation_token: invocation_row_mark().to_string(),
    })
}

#[allow(clippy::too_many_arguments)]
fn try_record_ledger(
    invocation: &ProcessInvocation,
    cx: &Cx,
    gate: &EffectGate,
    ledger_dir: Option<&Path>,
    roster: &ResolvedRoster,
    context: &NormalizedContext,
    decision: crate::storage::DecisionKind,
    reason: &str,
    mode_channel: &str,
    elapsed_ms: u64,
    metrics: &ExecutionMetrics,
    candidates: &[crate::storage::NewRankingCandidate],
    attempts: Option<&AttemptEvidence<'_>>,
) -> bool {
    if matches!(gate.ledger(), crate::privacy::StoreAccess::Disabled(_)) {
        return false;
    }
    let snapshot_members: Vec<crate::storage::SnapshotMember> = roster
        .skills()
        .iter()
        .map(|skill| {
            let rec = skill.record();
            let eligible_binding = skill.bindings().iter().find(|b| {
                matches!(b.visibility, crate::roster::Visibility::Verified { .. })
                    && b.restrictions.agent_invocable
            });
            let (eligible, invocation_name, exclusion_reason) = match eligible_binding {
                Some(b) => (true, Some(b.invocation.as_str().to_string()), None),
                None => {
                    let first_b = skill.bindings().first();
                    let reason = first_b.map(|b| {
                        if !b.restrictions.agent_invocable {
                            "not-agent-invocable".to_string()
                        } else {
                            match &b.visibility {
                                crate::roster::Visibility::Shadowed { .. } => {
                                    "shadowed".to_string()
                                }
                                crate::roster::Visibility::Ambiguous => "ambiguous".to_string(),
                                crate::roster::Visibility::Unverified => "unverified".to_string(),
                                _ => "ineligible".to_string(),
                            }
                        }
                    });
                    let inv_name = first_b.map(|b| b.invocation.as_str().to_string());
                    (false, inv_name, reason)
                }
            };
            crate::storage::SnapshotMember {
                skill_id: rec.id.as_str().to_string(),
                invocation_name: if eligible {
                    invocation_name
                } else {
                    invocation_name.or_else(|| Some(rec.display_name.as_str().to_string()))
                },
                content_hash: Some(rec.source_content.as_str().to_string()),
                source: rec.source.as_str().to_string(),
                eligible,
                exclusion_reason,
            }
        })
        .collect();

    let members_json = match serde_json::to_string(&snapshot_members) {
        Ok(j) => j,
        Err(_) => return false,
    };

    let now_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let snapshot_id = crate::roster::evidence::snapshot_id(roster)
        .as_str()
        .to_string();
    let total_candidates = snapshot_members.len() as u64;
    let eligible_candidates = snapshot_members.iter().filter(|m| m.eligible).count() as u64;

    let snapshot = crate::storage::NewRosterSnapshot {
        snapshot_id: snapshot_id.clone(),
        workspace_root: context.workspace_root.as_str().to_string(),
        adapter: context.harness.as_str().to_string(),
        total_candidates,
        eligible_candidates,
        membership_coverage: if roster.is_partial() {
            crate::storage::MembershipCoverage::Partial
        } else {
            crate::storage::MembershipCoverage::Complete
        },
        members_json,
        created_at_unix_ms: now_unix_ms,
    };

    let event_id = context
        .current_request
        .event_id
        .as_ref()
        .map(|e| e.as_str().to_string())
        .unwrap_or_else(|| derive_request_event_id(context));

    // Built against the id this event is about to be written under, so cost cannot
    // be attributed to an event that does not exist.
    let attempt_rows = attempts
        .map(|evidence| evidence.rows(&event_id))
        .unwrap_or_default();

    let event = crate::storage::NewRankingEvent {
        event_id,
        verified_delivery_key: None,
        workspace_root: context.workspace_root.as_str().to_string(),
        session_id: context
            .session_id
            .as_ref()
            .map(|s| s.as_str().to_string())
            .unwrap_or_else(|| "session-0".to_string()),
        agent_branch: context
            .branch_id
            .as_ref()
            .map(|b| b.as_str().to_string())
            .unwrap_or_else(|| "main".to_string()),
        mode_channel: mode_channel.to_string(),
        policy_version: "ranking-v1".to_string(),
        schema_version: SCHEMA_VERSION as u32,
        decision,
        reason: reason.to_string(),
        exposure_state: crate::storage::ExposureState::Prepared,
        elapsed_ms,
        created_at_unix_ms: now_unix_ms,
        input_tokens: if metrics.input_tokens > 0 {
            Some(metrics.input_tokens)
        } else {
            None
        },
        output_tokens: if metrics.output_tokens > 0 {
            Some(metrics.output_tokens)
        } else {
            None
        },
        snapshot_id: Some(snapshot_id),
    };

    let location = match ledger_dir {
        Some(dir) => crate::storage::LedgerLocation::Directory(dir.to_path_buf()),
        None => crate::storage::LedgerLocation::Platform,
    };

    crate::storage::record_ranking_with_attempts(
        invocation,
        invocation.clock(),
        cx,
        crate::storage::LedgerAccess::ExistingOnly,
        location,
        &event,
        candidates,
        Some(&snapshot),
        &attempt_rows,
    )
    .unwrap_or(false)
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
    let (warnings, warnings_omitted) = roster_warnings(roster, evaluated.source_warning.as_ref());
    let derived_event_id;
    let event_id = match context.current_request.event_id.as_ref() {
        Some(e) => e.as_str(),
        None => {
            derived_event_id = derive_request_event_id(context);
            derived_event_id.as_str()
        }
    };

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
    let (warnings, warnings_omitted) = roster_warnings(roster, evaluated.source_warning.as_ref());
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

    let derived_event_id;
    let event_id = match context.current_request.event_id.as_ref() {
        Some(e) => e.as_str(),
        None => {
            derived_event_id = derive_request_event_id(context);
            derived_event_id.as_str()
        }
    };

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
            ErrorKind::InvalidProviderResponse,
            format!("Output contract validation failed: {e:?}"),
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn generate_trace(
    args: &RankArgs,
    roster: &ResolvedRoster,
    query_scope: &TraceQueryScope<'_>,
    policy: Option<&PolicyView<'_>>,
    retrieval: &RetrievalView<'_>,
    wide_outcome: Option<&WideOutcome<'_>>,
    gate_threshold: f64,
    rerank_outcome: Option<&RerankOutcome<'_>>,
    fits_threshold: f64,
    evaluation: Option<&Evaluation<'_>>,
    ranking: Option<&Ranking>,
    publication_passed: bool,
) -> Result<Option<Value>, PipelineFailure> {
    if !args.explain && args.why_not.is_none() {
        return Ok(None);
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

    let eval_hash = crate::output::compute_entries_hash(&entries);
    let mut scope = query_scope.clone();
    scope.evaluation_hash = Some(eval_hash);
    let query_id = scope.compute_id();

    let snapshot_id = crate::roster::evidence::snapshot_id(roster);
    trace_page(args.cursor.as_ref(), snapshot_id, query_id, entries)
}

/// Explicit resolution bypasses advisory admission and every inference stage.
/// Its trace must come from the resolved targets, not a hypothetical advisory
/// run (which would reject manual-only skills the user is allowed to request).
fn generate_explicit_trace(
    args: &RankArgs,
    roster: &ResolvedRoster,
    query_scope: &TraceQueryScope<'_>,
    resolved: &[ResolvedExplicitSkill],
) -> Result<Option<Value>, PipelineFailure> {
    if !args.explain && args.why_not.is_none() {
        return Ok(None);
    }
    let targets: Vec<&SkillId> = match &args.why_not {
        Some(target) => vec![target],
        None => resolved.iter().map(|skill| &skill.id).collect(),
    };
    let mut entries = Vec::new();
    for target in targets {
        let resolution = match roster.exact_id(target) {
            ExactResolution::Missing => roster.exact_name(target.as_str()),
            other => other,
        };
        let selected = match resolution {
            ExactResolution::Resolved { id, .. } => resolved.iter().any(|skill| &skill.id == id),
            _ => false,
        };
        let present = selected
            || roster.skills().iter().any(|skill| {
                skill.record().id == *target
                    || skill.bindings().iter().any(|binding| {
                        binding.id == *target || binding.invocation.as_str() == target.as_str()
                    })
            });
        entries.push(if present {
            TraceEntry::passed(
                target.clone(),
                TraceStage::Discovery,
                "discovered",
                None,
                None,
            )
        } else {
            TraceEntry::not_in_snapshot(target.clone())
        });
        for stage in &TraceStage::ALL[1..] {
            let reason = if selected {
                match stage {
                    TraceStage::Visibility => Some("explicit-invocation-permitted"),
                    TraceStage::LocalPolicy => Some("explicit-requirement-resolved"),
                    // This is the prepared decision, not proof of stdout delivery.
                    TraceStage::Publication => Some("explicit-ready-for-publication"),
                    _ => None,
                }
            } else {
                None
            };
            entries.push(match reason {
                Some(reason) => TraceEntry::passed(target.clone(), *stage, reason, None, None),
                None => TraceEntry::not_evaluated(target.clone(), *stage),
            });
        }
    }
    let eval_hash = crate::output::compute_entries_hash(&entries);
    let mut scope = query_scope.clone();
    scope.evaluation_hash = Some(eval_hash);
    let query_id = scope.compute_id();

    let snapshot_id = crate::roster::evidence::snapshot_id(roster);
    trace_page(args.cursor.as_ref(), snapshot_id, query_id, entries)
}

fn trace_page(
    cursor: Option<&TraceCursor>,
    snapshot_id: ContentHash,
    query_id: ContentHash,
    entries: Vec<TraceEntry>,
) -> Result<Option<Value>, PipelineFailure> {
    let total = entries.len() as u64;

    let offset = match cursor {
        Some(c) => match c.resume(&snapshot_id, &query_id, total) {
            Ok(off) => off as usize,
            Err(ContractError::SnapshotChanged) => {
                return Err(failure(
                    ErrorKind::RosterChanged,
                    "The roster or query changed between trace pages.",
                ));
            }
            Err(ContractError::UnsupportedVersion) => {
                return Err(failure(
                    ErrorKind::InvalidUsage,
                    "Unsupported trace cursor schema version.",
                ));
            }
            Err(ContractError::InvalidField) | Err(_) => {
                return Err(failure(
                    ErrorKind::InvalidUsage,
                    "Cursor offset exceeds total trace entries.",
                ));
            }
        },
        None => 0,
    };

    let start = offset;
    let end = (start + crate::output::MAX_TRACE_PAGE_ITEMS).min(entries.len());
    let page_entries = if start <= entries.len() {
        entries[start..end].to_vec()
    } else {
        Vec::new()
    };
    let next_offset = if end < entries.len() {
        Some(end as u64)
    } else {
        None
    };

    let next_cursor = next_offset.map(|off| {
        TraceCursor {
            schema_version: SCHEMA_VERSION,
            snapshot_id: snapshot_id.clone(),
            query_id: query_id.clone(),
            offset: off,
        }
        .to_token()
    });

    let trace_cursor = TraceCursor {
        schema_version: SCHEMA_VERSION,
        snapshot_id,
        query_id,
        offset: start as u64,
    };

    let stage_trace = StageTrace {
        cursor: trace_cursor,
        total,
        next_offset,
        next_cursor,
        entries: page_entries,
    };

    serde_json::to_value(stage_trace).map(Some).map_err(|e| {
        failure(
            ErrorKind::OutputLimit,
            format!("Trace serialization error: {e:?}"),
        )
    })
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
