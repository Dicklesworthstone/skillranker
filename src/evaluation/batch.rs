//! Bounded offline replay batches. Recorded decisions are observations, not
//! independent usefulness labels: replay success cannot pass a quality gate.
//! A labeled case frame is scored against independent judgments by
//! `execute_labeled_frame_evaluation`; `execute_live_frame_evaluation` ranks
//! labeled requests fresh under batch-wide request and runtime caps.

use crate::evaluation::design_weighted::{
    DesignWeightedLossReport, SampledCaseLoss, compute_design_weighted_loss_from_manifest,
};
use crate::evaluation::stratified::{
    AllocationMethod, DesignStatus, FamilyRepresentativeRule, FrozenSampleManifest,
    RandomizationProvenance, draw_os_seed, draw_stratified_sample, select_family_representatives,
    verify_manifest_against_frame,
};
use crate::evaluation::{
    CaseKey, EvaluationCaseRecord, EvaluationError, EvaluationMetrics, EvaluationSplit,
    LabelStatus, ReconciliationManifest, RelevanceClass, compute_metrics, join_evaluation_frame,
    parse_bounded_json, parse_case_records_streaming, parse_labels_streaming, read_bounded_line,
    resolve_label_revisions, verify_split_isolation,
};
use crate::limits::{
    DEFAULT_EVAL_BATCH_RUNTIME_MS, EVALUATION_CASE_RECORDS, EVALUATION_DATASET_BYTES,
    EVALUATION_DATASET_DEPTH,
};
use crate::output::{GateStatus, OutputDocument, RunStatus, SCHEMA_VERSION};
use crate::replay::{ReplayCase, ReplayPolicy, execute_replay_comparison};
use crate::runtime::EntryClock;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io::BufRead;

/// Origin class of evidence evaluated in the batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceOrigin {
    Synthetic,
    Recorded,
    Live,
}

impl EvidenceOrigin {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Synthetic => "synthetic",
            Self::Recorded => "recorded",
            Self::Live => "live",
        }
    }
}

/// Configuration options for an evaluation batch run.
#[derive(Clone, Debug, PartialEq)]
pub struct BatchConfig {
    /// Maximum HTTP attempts across the entire live batch. Required when `online == true`.
    pub max_requests: Option<u32>,
    /// Maximum wall/monotonic runtime for the entire batch in milliseconds.
    pub max_runtime_ms: u64,
    /// Optional per-case deadline in milliseconds.
    pub per_case_timeout_ms: Option<u64>,
    /// Whether to run live against the provider (requires network consent and explicit max_requests).
    pub online: bool,
    /// Explicit network authorization consent flag.
    pub allow_network: bool,
    /// Evidence origin classification (synthetic, recorded, or live).
    pub evidence_origin: EvidenceOrigin,
    /// Optional baseline or evaluation policy override.
    pub policy: Option<ReplayPolicy>,
    /// Optional comparison policy override.
    pub compare_policy: Option<ReplayPolicy>,
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            max_requests: None,
            max_runtime_ms: DEFAULT_EVAL_BATCH_RUNTIME_MS,
            per_case_timeout_ms: None,
            online: false,
            allow_network: false,
            evidence_origin: EvidenceOrigin::Recorded,
            policy: None,
            compare_policy: None,
        }
    }
}

/// Execution status and outcome for an individual case in the batch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum CaseExecutionStatus {
    /// Case successfully evaluated to completion.
    Completed {
        decision: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        recomputed: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        loss: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        normalized_loss: Option<f64>,
    },
    /// A required recorded stage was missing, making the case not estimable.
    NotEstimable { reason: String },
    /// Case suffered an operational failure (timeout, network drop, etc.).
    OperationalFailure {
        error_kind: String,
        loss: u32,
        normalized_loss: f64,
    },
    /// Case was not started because the batch runtime deadline or request cap was exhausted.
    Unfinished { reason: String },
}

/// Detailed execution report for a single case in the batch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BatchCaseReport {
    pub case_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub family_id: Option<String>,
    pub status: CaseExecutionStatus,
    pub elapsed_ms: u64,
}

/// Verification completeness counts matching `src/output/mod.rs` requirements.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CompletenessReport {
    pub cases_requested: usize,
    pub cases_completed: usize,
    pub stages_required: usize,
    pub stages_completed: usize,
    pub evidence_compatible: bool,
}

/// Accounting totals for HTTP attempts and token usage across the batch.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct BatchAccounting {
    pub requests: usize,
    pub http_attempts: usize,
    pub unknown_usage_attempts: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Common 0/1/2 evaluation loss metrics matching `evaluation_policy.v1.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BatchLossSummary {
    pub attempted_cases: usize,
    pub total_loss: u32,
    pub mean_loss: Option<f64>,
    pub mean_normalized_loss: Option<f64>,
    pub not_estimable_cases: usize,
    pub unfinished_cases: usize,
    pub operational_failures: usize,
}

/// Structured error envelope for partial or aborted batches.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReportError {
    pub code: u8,
    pub kind: String,
    pub message: String,
    pub hint: String,
    pub retryable: bool,
}

/// Complete evaluation batch report satisfying the `ArtifactKind::Report` contract.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvaluationBatchReport {
    pub schema_version: u64,
    pub kind: String,
    pub actionable: bool,
    pub run_status: RunStatus,
    pub gate_status: GateStatus,
    pub evidence_origin: String,
    pub completeness: CompletenessReport,
    pub loss_summary: BatchLossSummary,
    pub accounting: BatchAccounting,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<EvaluationMetrics>,
    /// Pre/post join counts of a labeled frame; absent for recorded replay.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reconciliation: Option<ReconciliationManifest>,
    /// The sample frozen before its selected cases were joined and scored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_manifest: Option<FrozenSampleManifest>,
    /// Horvitz-Thompson loss over a probability sample or census; never for a
    /// diagnostic fixed-seed selection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub design_weighted_loss: Option<DesignWeightedLossReport>,
    /// A live batch's disclosure, previewed for every case before any send.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disclosure_preflight: Option<DisclosurePreflight>,
    /// Intrinsic stage coverage and baseline policies over the same live answers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baselines: Option<BaselineComparison>,
    /// Requested by `--explain`; derived only from this report's own values.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explanation: Option<ReportExplanation>,
    pub cases: Vec<BatchCaseReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ReportError>,
}

impl EvaluationBatchReport {
    /// Validate and serialize this report into an official [`OutputDocument`].
    pub fn to_document(&self) -> Result<OutputDocument, crate::output::ContractError> {
        let val =
            serde_json::to_value(self).map_err(|_| crate::output::ContractError::InvalidJson)?;
        OutputDocument::from_value(val)
    }
}

/// Parse only executable replay evidence, never an oracle or an unjudged row.
fn read_cases_streaming<R: BufRead>(mut reader: R) -> Result<Vec<ReplayCase>, EvaluationError> {
    let mut items = Vec::new();
    let mut ids = BTreeSet::new();
    let mut total_bytes = 0;
    let mut line = String::new();
    while read_bounded_line(
        &mut reader,
        &mut line,
        &mut total_bytes,
        EVALUATION_DATASET_BYTES.max(),
    )? > 0
    {
        let trimmed = line.trim_matches([' ', '\t', '\r', '\n']);
        if trimmed.is_empty() {
            continue;
        }
        if items.len() >= EVALUATION_CASE_RECORDS.max() {
            return Err(EvaluationError::RecordLimitReached {
                count: items.len() + 1,
                max: EVALUATION_CASE_RECORDS.max(),
            });
        }
        let case = ReplayCase::from_json_bytes(trimmed.as_bytes())
            .map_err(|e| EvaluationError::InvalidField(format!("malformed replay case: {e}")))?;
        if case.case_id.trim().is_empty() || !ids.insert(case.case_id.clone()) {
            return Err(EvaluationError::CardinalityViolation(
                "empty or duplicate replay case ID".into(),
            ));
        }
        items.push(case);
    }
    Ok(items)
}

// A local explicit resolution consumes no model stages. Otherwise a missing
// wide answer cannot establish that rerank was unnecessary. Use the stricter
// policy when comparing two policies over the same recorded evidence.
fn required_stages(case: &ReplayCase, config: &BatchConfig) -> usize {
    if case
        .historical_decision
        .get("decision")
        .and_then(Value::as_str)
        == Some("explicit")
    {
        return 0;
    }
    let required_for = |policy: Option<&ReplayPolicy>| {
        let threshold = policy
            .and_then(|p| p.gate_threshold)
            .unwrap_or(case.local_evidence.scoring_profile.gate_threshold);
        let gate = case.recorded_responses.wide.as_ref().map(|wide| {
            wide.gate_score.unwrap_or_else(|| {
                1.0 - wide
                    .distribution
                    .iter()
                    .find(|d| d.option_id == "__none__")
                    .map_or(0.0, |d| d.probability)
            })
        });
        if gate.is_some_and(|score| score < threshold) {
            1
        } else {
            2
        }
    };
    let base = required_for(config.policy.as_ref());
    config
        .compare_policy
        .as_ref()
        .map_or(base, |p| base.max(required_for(Some(p))))
}

/// Validate mode and limits before callers open any evaluation inputs.
pub(crate) fn validate_batch_config(config: &BatchConfig) -> Result<(), EvaluationError> {
    if config.online {
        if !config.allow_network {
            return Err(EvaluationError::InvalidField(
                "online live evaluation requires --allow-network or trusted network consent".into(),
            ));
        }
        if config.max_requests.is_none_or(|n| n == 0) {
            return Err(EvaluationError::InvalidField(
                "online live evaluation requires an explicit --max-requests cap".into(),
            ));
        }
    }
    if config.evidence_origin == EvidenceOrigin::Live {
        return Err(EvaluationError::InvalidField(
            "offline replay cannot claim fresh live evidence".into(),
        ));
    }
    if config.max_runtime_ms == 0 || config.max_runtime_ms > 86_400_000 {
        return Err(EvaluationError::InvalidField(
            "batch_runtime must be positive and at most 86400000 ms".into(),
        ));
    }
    if config.per_case_timeout_ms == Some(0) {
        return Err(EvaluationError::InvalidField(
            "per-case timeout must be positive".into(),
        ));
    }
    Ok(())
}

/// Recompute validated recorded cases without network or persistence effects.
/// Independent judgments are not part of ReplayCase; quality loss stays unknown.
pub fn execute_evaluation_batch<R: BufRead>(
    reader: R,
    config: &BatchConfig,
    clock: &EntryClock,
) -> Result<EvaluationBatchReport, EvaluationError> {
    validate_batch_config(config)?;
    if config.online {
        return Err(EvaluationError::InvalidField(
            "a live batch ranks labeled cases fresh; replay datasets run offline".into(),
        ));
    }
    let expires = clock
        .now()
        .as_millis()
        .saturating_add(config.max_runtime_ms);
    let cases = read_cases_streaming(reader)?;
    let synthetic = config.evidence_origin == EvidenceOrigin::Synthetic
        || cases
            .iter()
            .any(|c| c.manifest.evidence_origin == "synthetic");
    let mut completeness = CompletenessReport {
        cases_requested: cases.len(),
        cases_completed: 0,
        stages_required: cases.iter().map(|c| required_stages(c, config)).sum(),
        stages_completed: 0,
        evidence_compatible: true,
    };
    let mut loss_summary = BatchLossSummary::default();
    let mut reports = Vec::with_capacity(cases.len());
    let mut error = None;
    for case in cases {
        let started = clock.now().as_millis();
        let required = required_stages(&case, config);
        let status = if started >= expires {
            loss_summary.unfinished_cases += 1;
            error = Some(ReportError {
                code: 6,
                kind: "deadline-expired".into(),
                message: "Evaluation batch exceeded max-runtime-ms deadline".into(),
                hint: "Increase --max-runtime-ms or reduce dataset size".into(),
                retryable: false,
            });
            CaseExecutionStatus::Unfinished {
                reason: "batch runtime deadline expired".into(),
            }
        } else if required > 0 && case.recorded_responses.wide.is_none() {
            loss_summary.not_estimable_cases += 1;
            completeness.evidence_compatible = false;
            CaseExecutionStatus::NotEstimable {
                reason: "missing wide recorded response".into(),
            }
        } else {
            let outcome = execute_replay_comparison(
                &case,
                config.policy.as_ref(),
                config.compare_policy.as_ref(),
            );
            let case_expires = config
                .per_case_timeout_ms
                .map_or(expires, |ms| started.saturating_add(ms).min(expires));
            if clock.now().as_millis() >= case_expires {
                // Actual replay work started but missed its deadline. It cannot
                // publish a late success, and remains in the failure denominator.
                loss_summary.operational_failures += 1;
                loss_summary.attempted_cases += 1;
                loss_summary.total_loss += 2;
                CaseExecutionStatus::OperationalFailure {
                    error_kind: "deadline-expired".into(),
                    loss: 2,
                    normalized_loss: 1.0,
                }
            } else {
                completeness.stages_completed +=
                    usize::from(required >= 1 && case.recorded_responses.wide.is_some())
                        + usize::from(required >= 2 && case.recorded_responses.rerank.is_some());
                match outcome {
                    Ok(outcome)
                        if outcome.run_status == RunStatus::Complete
                            && outcome.recomputed_decision.is_some() =>
                    {
                        completeness.cases_completed += 1;
                        CaseExecutionStatus::Completed {
                            decision: outcome.recomputed_decision.unwrap(),
                            recomputed: outcome.explanation,
                            loss: None,
                            normalized_loss: None,
                        }
                    }
                    other => {
                        loss_summary.not_estimable_cases += 1;
                        completeness.evidence_compatible = false;
                        let reason = match other {
                            Ok(outcome) => outcome
                                .explanation
                                .unwrap_or_else(|| "recorded evidence is incomplete".into()),
                            Err(err) => err.to_string(),
                        };
                        CaseExecutionStatus::NotEstimable { reason }
                    }
                }
            }
        };
        reports.push(BatchCaseReport {
            case_id: case.case_id,
            family_id: None,
            status,
            elapsed_ms: clock.now().as_millis().saturating_sub(started),
        });
    }
    // No mean over the failures alone: successfully replayed cases remain
    // unjudged, so such a denominator would not represent the requested cohort.
    let complete = completeness.cases_completed == completeness.cases_requested
        && completeness.stages_completed == completeness.stages_required;
    let report = EvaluationBatchReport {
        schema_version: SCHEMA_VERSION,
        kind: "report".into(),
        actionable: false,
        run_status: if complete {
            RunStatus::Complete
        } else {
            RunStatus::Partial
        },
        gate_status: if synthetic && complete {
            GateStatus::NotApplicable
        } else {
            GateStatus::NotEstablished
        },
        evidence_origin: if synthetic { "synthetic" } else { "recorded" }.into(),
        completeness,
        loss_summary,
        accounting: BatchAccounting::default(),
        metrics: None,
        reconciliation: None,
        sample_manifest: None,
        design_weighted_loss: None,
        disclosure_preflight: None,
        baselines: None,
        explanation: None,
        cases: reports,
        error,
    };
    report.to_document().map_err(|e| {
        EvaluationError::InvalidField(format!(
            "batch report exceeds or violates output contract: {e}"
        ))
    })?;
    Ok(report)
}

/// A requested stratified sample of a labeled frame's task families.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameSampling {
    /// Families to select, capped by the frame's family count.
    pub sample_size: usize,
    /// `None` draws a fresh recorded seed from OS randomness. A supplied seed
    /// reproduces a selection for diagnosis; it is not probability-sampling evidence.
    pub seed: Option<u64>,
}

/// Error budget for the design-weighted conservative upper bound.
pub const DESIGN_WEIGHTED_ALPHA: f64 = 0.05;

/// Score recorded decisions of a labeled frame against independent judgments.
///
/// With `sampling`, the frame must hold one split and one policy. One
/// representative per task family is selected and the manifest is frozen and
/// verified before any label is joined, so outcomes cannot steer selection.
/// Loss follows `evaluation_policy.v1`: explicit requests and unjudged cases
/// carry none, and unjudged cases leave the run partial.
pub fn execute_labeled_frame_evaluation<C: BufRead, L: BufRead>(
    cases: C,
    labels: L,
    sampling: Option<FrameSampling>,
    created_at_unix_ms: u64,
) -> Result<EvaluationBatchReport, EvaluationError> {
    let records = parse_case_records_streaming(cases)?;
    let labels = parse_labels_streaming(labels)?;
    verify_split_isolation(&records)?;
    let (evaluated, manifest) = select_frame(records, sampling, created_at_unix_ms)?;
    score_frame(evaluated, &labels, manifest, FrameRun::recorded())
}

/// The frame's cases to evaluate: all of them, or the representatives a
/// sample froze (and verified) before any label or outcome is consulted.
fn select_frame(
    records: Vec<crate::evaluation::EvaluationCaseRecord>,
    sampling: Option<FrameSampling>,
    created_at_unix_ms: u64,
) -> Result<
    (
        Vec<crate::evaluation::EvaluationCaseRecord>,
        Option<FrozenSampleManifest>,
    ),
    EvaluationError,
> {
    let Some(request) = sampling else {
        return Ok((records, None));
    };
    let (manifest, representatives) = freeze_frame_sample(&records, request, created_at_unix_ms)?;
    let selected: BTreeSet<&CaseKey> = manifest
        .selected_cases
        .iter()
        .map(|entry| &entry.case_key)
        .collect();
    let sampled = representatives
        .into_iter()
        .filter(|case| selected.contains(&case.key))
        .collect();
    Ok((sampled, Some(manifest)))
}

/// How a scored frame's decisions were produced, and the selected cases that
/// never produced one.
struct FrameRun {
    evidence_origin: &'static str,
    accounting: BatchAccounting,
    /// Selected cases never ranked, in selection order, with why.
    skipped: Vec<(CaseKey, CaseExecutionStatus)>,
    /// Why scheduling stopped early; reported only for a partial run.
    error: Option<ReportError>,
    elapsed_ms: BTreeMap<CaseKey, u64>,
    preflight: Option<DisclosurePreflight>,
    baselines: Option<BaselineComparison>,
}

impl FrameRun {
    fn recorded() -> Self {
        Self {
            evidence_origin: "recorded",
            accounting: BatchAccounting::default(),
            skipped: Vec::new(),
            error: None,
            elapsed_ms: BTreeMap::new(),
            preflight: None,
            baselines: None,
        }
    }
}

fn score_frame(
    evaluated: Vec<crate::evaluation::EvaluationCaseRecord>,
    labels: &[crate::evaluation::JudgedLabel],
    manifest: Option<FrozenSampleManifest>,
    run: FrameRun,
) -> Result<EvaluationBatchReport, EvaluationError> {
    let (resolved, reconciliation) = join_evaluation_frame(&evaluated, labels)?;
    let metrics = compute_metrics(&resolved);

    let mut loss_summary = BatchLossSummary::default();
    let mut losses_by_case = BTreeMap::new();
    let mut reports = Vec::with_capacity(resolved.len() + run.skipped.len());
    let mut completed = 0;
    for case in &resolved {
        let judged = matches!(case.label_status, LabelStatus::Resolved { .. });
        let loss = case.relevance_class.policy_loss();
        let status = match (case.relevance_class, loss) {
            (RelevanceClass::OperationalFailure, Some(loss)) => {
                loss_summary.operational_failures += 1;
                CaseExecutionStatus::OperationalFailure {
                    error_kind: "operational-failure".into(),
                    loss,
                    normalized_loss: f64::from(loss) / 2.0,
                }
            }
            _ if !judged => {
                loss_summary.not_estimable_cases += 1;
                CaseExecutionStatus::NotEstimable {
                    reason: match case.label_status {
                        LabelStatus::NullKey => "case key is incomplete",
                        _ => "no independent judgment for this case",
                    }
                    .into(),
                }
            }
            (_, loss) => CaseExecutionStatus::Completed {
                decision: case.record.decision.clone(),
                recomputed: None,
                loss,
                normalized_loss: loss.map(|loss| f64::from(loss) / 2.0),
            },
        };
        if judged || loss.is_some() {
            completed += 1;
        }
        if let Some(loss) = loss {
            loss_summary.attempted_cases += 1;
            loss_summary.total_loss += loss;
            losses_by_case.insert(
                case.record.key.clone(),
                SampledCaseLoss::Observed(f64::from(loss) / 2.0),
            );
        }
        reports.push(BatchCaseReport {
            case_id: case.record.key.case_id.clone(),
            family_id: Some(case.record.key.family_id.clone()),
            status,
            elapsed_ms: run.elapsed_ms.get(&case.record.key).copied().unwrap_or(0),
        });
    }
    // Never-ranked cases stay in the denominator of what was requested; they
    // carry no loss and are never counted as successes.
    for (key, status) in run.skipped {
        match &status {
            CaseExecutionStatus::Unfinished { .. } => loss_summary.unfinished_cases += 1,
            _ => loss_summary.not_estimable_cases += 1,
        }
        reports.push(BatchCaseReport {
            case_id: key.case_id,
            family_id: Some(key.family_id),
            status,
            elapsed_ms: 0,
        });
    }
    if loss_summary.attempted_cases > 0 {
        let attempted = loss_summary.attempted_cases as f64;
        let mean = f64::from(loss_summary.total_loss) / attempted;
        loss_summary.mean_loss = Some(mean);
        loss_summary.mean_normalized_loss = Some(mean / 2.0);
    }
    // A fixed seed supports reproduction only; its selection has no design
    // inclusion probabilities to weight by. Explicit, unjudged and never-ranked
    // cases stay Missing, so the reported bounds widen rather than hide them.
    let design_weighted_loss = match &manifest {
        Some(manifest) if manifest.design_status != DesignStatus::DiagnosticFixed => Some(
            compute_design_weighted_loss_from_manifest(
                manifest,
                &losses_by_case,
                DESIGN_WEIGHTED_ALPHA,
            )
            .map_err(|err| EvaluationError::SamplingFailure(err.to_string()))?,
        ),
        _ => None,
    };

    let requested = reports.len();
    let complete = completed == requested;
    let report = EvaluationBatchReport {
        schema_version: SCHEMA_VERSION,
        kind: "report".into(),
        actionable: false,
        run_status: if complete {
            RunStatus::Complete
        } else {
            RunStatus::Partial
        },
        // Decisions scored here are evidence for a promotion review, not a
        // passed gate; the frozen promotion thresholds are applied there.
        gate_status: GateStatus::NotEstablished,
        evidence_origin: run.evidence_origin.into(),
        completeness: CompletenessReport {
            cases_requested: requested,
            cases_completed: completed,
            stages_required: 0,
            stages_completed: 0,
            evidence_compatible: reconciliation.reconciled,
        },
        loss_summary,
        accounting: run.accounting,
        metrics: Some(metrics),
        reconciliation: Some(reconciliation),
        sample_manifest: manifest,
        design_weighted_loss,
        disclosure_preflight: run.preflight,
        baselines: run.baselines,
        explanation: None,
        cases: reports,
        error: if complete { None } else { run.error },
    };
    report.to_document().map_err(|e| {
        EvaluationError::InvalidField(format!(
            "evaluation report exceeds or violates output contract: {e}"
        ))
    })?;
    Ok(report)
}

/// One case of a live batch: its frame identity and the normalized request
/// context ranked fresh against the current roster. A fresh evaluation, not a
/// replay: no historical candidate evidence or embedded path is consulted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiveEvaluationCase {
    pub schema_version: u64,
    pub key: CaseKey,
    pub split: EvaluationSplit,
    pub context: Value,
}

/// Stream live cases with the dataset's size, record and depth bounds.
pub fn parse_live_cases_streaming<R: BufRead>(
    mut reader: R,
) -> Result<Vec<LiveEvaluationCase>, EvaluationError> {
    let mut cases = Vec::new();
    let mut keys = BTreeSet::new();
    let mut total_bytes = 0usize;
    let mut line = String::new();
    while read_bounded_line(
        &mut reader,
        &mut line,
        &mut total_bytes,
        EVALUATION_DATASET_BYTES.max(),
    )? > 0
    {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            line.clear();
            continue;
        }
        if cases.len() >= EVALUATION_CASE_RECORDS.max() {
            return Err(EvaluationError::RecordLimitReached {
                count: cases.len() + 1,
                max: EVALUATION_CASE_RECORDS.max(),
            });
        }
        let value = parse_bounded_json(trimmed.as_bytes(), EVALUATION_DATASET_DEPTH.max())?;
        let case: LiveEvaluationCase = serde_json::from_value(value)
            .map_err(|e| EvaluationError::InvalidField(format!("malformed live case: {e}")))?;
        if case.schema_version != SCHEMA_VERSION {
            return Err(EvaluationError::UnsupportedVersion(case.schema_version));
        }
        if case.key.is_null() || !keys.insert(case.key.clone()) {
            return Err(EvaluationError::CardinalityViolation(
                "empty or duplicate live case key".into(),
            ));
        }
        cases.push(case);
        line.clear();
    }
    Ok(cases)
}

/// What one live ranking produced, read from its decision document.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LiveRankOutcome {
    pub decision: String,
    pub suggested_skills: Vec<String>,
    pub requests: usize,
    pub http_attempts: usize,
    pub unknown_usage_attempts: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// The error kind of an unavailable decision.
    pub error_kind: Option<String>,
    pub elapsed_ms: u64,
    /// What each stage answered, for baseline comparisons.
    pub evidence: Option<crate::pipeline::StageEvidence>,
}

/// Batch-wide caps of a live run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LiveBatchLimits {
    /// HTTP attempts across the whole batch, retries included.
    pub max_requests: usize,
    pub max_runtime_ms: u64,
    /// The most attempts one ranking can make; a case is admitted only while
    /// its worst case still fits under `max_requests`.
    pub attempts_per_case: usize,
    /// The minimum fit the fit-only baseline requires, as production uses.
    pub fit_threshold: f64,
}

/// What a live batch will disclose, frozen before its first request: every
/// case to be sent was previewed locally with no network, and a case whose
/// preview was refused is never sent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisclosurePreflight {
    pub cases_checked: usize,
    pub cases_refused: usize,
    /// Cases whose preview ends locally, so a live run sends nothing for them.
    pub cases_without_request: usize,
    pub disclosed_bytes: u64,
    pub total_redactions: u64,
    pub total_truncated: u64,
    pub total_omitted: u64,
    /// BLAKE3 over each checked case's ID and receipt, in selection order.
    pub receipts_digest: String,
}

/// Errors after which no further case can succeed: scheduling stops.
const FATAL_LIVE_KINDS: [(&str, u8); 3] = [
    ("authentication", 4),
    ("credential-absent", 4),
    ("network-denied", 8),
];

/// Rank selected labeled cases live, then score them like a recorded frame.
///
/// Selection (and any sample manifest) is frozen before the first ranking.
/// A case without an independent judgment, or whose judgment names a skill
/// absent from `current_roster`, is not estimable and is never sent. Every
/// remaining case is then previewed by `preflight` with no network (`Ok(None)`
/// when its preview sends nothing), and the batch's disclosure is frozen into
/// the report; a refused preview is never sent. A case
/// is admitted only while the batch deadline and its worst-case attempts fit;
/// every later case is reported unfinished, as is every case after a fatal
/// provider error. Provider usage is summed exactly as each ranking reported it.
#[allow(clippy::too_many_arguments)]
pub fn execute_live_frame_evaluation<L: BufRead>(
    cases: Vec<LiveEvaluationCase>,
    labels: L,
    sampling: Option<FrameSampling>,
    current_roster: &BTreeSet<String>,
    limits: LiveBatchLimits,
    clock: &EntryClock,
    created_at_unix_ms: u64,
    mut preflight: impl FnMut(&LiveEvaluationCase) -> Result<Option<Value>, String>,
    mut rank: impl FnMut(&LiveEvaluationCase) -> LiveRankOutcome,
) -> Result<EvaluationBatchReport, EvaluationError> {
    if limits.max_requests == 0 || limits.attempts_per_case == 0 || limits.max_runtime_ms == 0 {
        return Err(EvaluationError::InvalidField(
            "live batches need positive request, attempt and runtime caps".into(),
        ));
    }
    let labels = parse_labels_streaming(labels)?;
    let judgments = resolve_label_revisions(&labels)?;
    let stub = |case: &LiveEvaluationCase| EvaluationCaseRecord {
        schema_version: SCHEMA_VERSION,
        key: case.key.clone(),
        split: case.split,
        prompt_summary: Some("live request".into()),
        roster_skills: Vec::new(),
        decision: "pending".into(),
        suggested_skills: Vec::new(),
        fits: BTreeMap::new(),
        gate_score: None,
        relevance_abstention: false,
        operational_failure: false,
    };
    let stubs: Vec<EvaluationCaseRecord> = cases.iter().map(stub).collect();
    verify_split_isolation(&stubs)?;
    let (selected, manifest) = select_frame(stubs, sampling, created_at_unix_ms)?;
    let by_key: BTreeMap<&CaseKey, &LiveEvaluationCase> =
        cases.iter().map(|case| (&case.key, case)).collect();

    let expires = clock
        .now()
        .as_millis()
        .saturating_add(limits.max_runtime_ms);
    let mut run = FrameRun {
        evidence_origin: "live",
        ..FrameRun::recorded()
    };
    let mut pending = Vec::with_capacity(selected.len());
    for record in selected {
        let judgment = judgments.get(&record.key.case_id);
        let off_roster = judgment.is_some_and(|label| {
            label
                .acceptable_skills
                .iter()
                .chain(label.explicit_directive.as_ref())
                .any(|skill| !current_roster.contains(skill))
        });
        let skip = match (judgment, off_roster) {
            (None, _) => Some("no independent judgment for this case; not sent"),
            (Some(_), true) => Some("a judged skill is absent from the current roster; not sent"),
            _ => None,
        };
        if let Some(reason) = skip {
            run.skipped.push((
                record.key,
                CaseExecutionStatus::NotEstimable {
                    reason: reason.into(),
                },
            ));
            continue;
        }
        pending.push(record);
    }

    // Freeze the whole batch's disclosure before its first request.
    let mut frozen = DisclosurePreflight::default();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"skillranker:live_disclosure_preflight:v1\n");
    let mut admitted = Vec::with_capacity(pending.len());
    for record in pending {
        frozen.cases_checked += 1;
        let case_id = record.key.case_id.as_bytes();
        hasher.update(&(case_id.len() as u64).to_le_bytes());
        hasher.update(case_id);
        match preflight(by_key[&record.key]) {
            Ok(receipt) => {
                let bytes = serde_json::to_vec(&receipt).unwrap_or_default();
                hasher.update(&(bytes.len() as u64).to_le_bytes());
                hasher.update(&bytes);
                match &receipt {
                    Some(receipt) => {
                        let count = |name: &str| receipt[name].as_u64().unwrap_or(0);
                        frozen.disclosed_bytes += count("disclosed_bytes");
                        frozen.total_redactions += count("total_redactions");
                        frozen.total_truncated += count("total_truncated");
                        frozen.total_omitted += count("total_omitted");
                    }
                    None => frozen.cases_without_request += 1,
                }
                admitted.push(record);
            }
            Err(kind) => {
                hasher.update(b"refused\0");
                hasher.update(kind.as_bytes());
                frozen.cases_refused += 1;
                run.skipped.push((
                    record.key,
                    CaseExecutionStatus::NotEstimable {
                        reason: format!("disclosure preflight refused ({kind}); not sent"),
                    },
                ));
            }
        }
    }
    frozen.receipts_digest = hasher.finalize().to_hex().to_string();
    run.preflight = Some(frozen);

    let mut executed = Vec::with_capacity(admitted.len());
    let mut evidence_by_case = BTreeMap::new();
    let mut stopped: Option<(String, ReportError)> = None;
    for mut record in admitted {
        let case = by_key[&record.key];
        if stopped.is_none() {
            if clock.now().as_millis() >= expires {
                stopped = Some((
                    "batch runtime deadline expired".into(),
                    ReportError {
                        code: 6,
                        kind: "timeout".into(),
                        message: "The live batch reached --max-runtime-ms".into(),
                        hint: "Raise --max-runtime-ms or evaluate a smaller sample".into(),
                        retryable: false,
                    },
                ));
            } else if run.accounting.http_attempts + limits.attempts_per_case > limits.max_requests
            {
                stopped = Some((
                    "request cap reached before this case".into(),
                    ReportError {
                        code: 4,
                        kind: "request-budget".into(),
                        message: "The live batch reached --max-requests".into(),
                        hint: "Raise --max-requests or evaluate a smaller sample".into(),
                        retryable: false,
                    },
                ));
            }
        }
        if let Some((reason, _)) = &stopped {
            run.skipped.push((
                record.key,
                CaseExecutionStatus::Unfinished {
                    reason: reason.clone(),
                },
            ));
            continue;
        }
        let outcome = rank(case);
        let accounting = &mut run.accounting;
        accounting.requests += outcome.requests;
        accounting.http_attempts += outcome.http_attempts;
        accounting.unknown_usage_attempts += outcome.unknown_usage_attempts;
        accounting.input_tokens += outcome.input_tokens;
        accounting.output_tokens += outcome.output_tokens;
        run.elapsed_ms
            .insert(record.key.clone(), outcome.elapsed_ms);
        if let Some((kind, code)) = FATAL_LIVE_KINDS
            .iter()
            .find(|(kind, _)| outcome.error_kind.as_deref() == Some(*kind))
        {
            stopped = Some((
                format!("stopped after a fatal {kind} error"),
                ReportError {
                    code: *code,
                    kind: (*kind).into(),
                    message: "A fatal provider error stopped the live batch".into(),
                    hint: "Fix the credential or network authorization, then rerun".into(),
                    retryable: false,
                },
            ));
        }
        if let Some(evidence) = outcome.evidence.clone() {
            evidence_by_case.insert(record.key.clone(), evidence);
        }
        record.decision = outcome.decision.clone();
        record.relevance_abstention = outcome.decision == "abstain";
        record.operational_failure = outcome.decision == "unavailable";
        record.suggested_skills = outcome.suggested_skills;
        executed.push(record);
    }
    run.error = stopped.map(|(_, error)| error);
    let mut comparison = compare_baselines(
        &executed,
        &judgments,
        &evidence_by_case,
        limits.fit_threshold,
    );
    comparison.latency_ms = latency_summary(run.elapsed_ms.values().copied().collect());
    run.baselines = Some(comparison);
    score_frame(executed, &labels, manifest, run)
}

/// A rate with its exact denominator and a 95% Wilson interval.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Ratio {
    pub successes: usize,
    pub denominator: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wilson_95: Option<(f64, f64)>,
}

impl Ratio {
    fn of(successes: usize, denominator: usize) -> Self {
        Self {
            successes,
            denominator,
            rate: (denominator > 0).then(|| successes as f64 / denominator as f64),
            wilson_95: crate::evaluation::numerics::wilson_ci(successes, denominator, 0.95).ok(),
        }
    }
}

/// Whether the stages kept an acceptable skill in play, before any decision.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StageCoverage {
    /// Judged positive cases whose admitted wide candidates include an
    /// acceptable skill.
    pub admitted: Ratio,
    /// Judged positive cases that passed the gate and whose shortlist includes
    /// an acceptable skill.
    pub shortlist: Ratio,
    /// Judged positive cases the gate stopped before a shortlist.
    pub shortlist_gated_out: usize,
    /// Whether Quill ranked any case's admitted set (roster overflow).
    pub quill_ranked_cases: usize,
}

/// One selection policy scored on the judged advisory cases.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PolicyScore {
    pub policy: String,
    pub definition: String,
    pub evaluated_cases: usize,
    /// Judged cases this policy's stage evidence could not score.
    pub not_evaluated_cases: usize,
    /// Precision of emitted top-one suggestions.
    pub top1_precision: Ratio,
    /// Positive cases whose emitted top-one suggestion is acceptable.
    pub positive_suggestion_rate: Ratio,
    /// No-match cases that received any suggestion.
    pub needless_suggestion_rate: Ratio,
    /// Positive cases the policy abstained on.
    pub false_abstention_rate: Ratio,
    /// Positive cases where any published suggestion is acceptable; only the
    /// production blend publishes more than one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k_coverage: Option<Ratio>,
    /// Mean `evaluation_policy.v1` loss over the evaluated cases.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mean_loss: Option<f64>,
}

/// Baselines computed from one run's stage answers: no extra request, and the
/// same judged cohort for every policy.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BaselineComparison {
    pub coverage: StageCoverage,
    pub policies: Vec<PolicyScore>,
    /// Judged advisory cases without stage evidence (for example an
    /// unavailable ranking); excluded from every policy alike.
    pub cases_without_evidence: usize,
    /// Judged explicit requests, checked separately from advisory quality.
    pub explicit_cases_excluded: usize,
    /// Baselines this run's answers cannot support, and why.
    pub not_computed: Vec<String>,
    /// Fit calibration on judged reranked pairs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fit_calibration: Option<FitCalibration>,
    /// Wall time of each live ranking, including local work and retries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<LatencySummary>,
}

/// Nearest-rank percentiles of per-case ranking time; every executed case
/// counts, failures and timeouts included.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencySummary {
    pub cases: usize,
    pub p50: u64,
    pub p95: u64,
    pub p99: u64,
    pub max: u64,
}

fn latency_summary(mut samples: Vec<u64>) -> Option<LatencySummary> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_unstable();
    let rank = |q: f64| {
        let index = ((q * samples.len() as f64).ceil() as usize).clamp(1, samples.len()) - 1;
        samples[index]
    };
    Some(LatencySummary {
        cases: samples.len(),
        p50: rank(0.50),
        p95: rank(0.95),
        p99: rank(0.99),
        max: samples[samples.len() - 1],
    })
}

/// Fit estimates scored against judgments. The sampling frame is every
/// reranked (case, skill) pair of the judged advisory cohort; it says nothing
/// about calibration over the whole roster.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FitCalibration {
    pub frame: String,
    pub pairs: usize,
    /// Mean squared error of fit against acceptability (1 if the skill is in
    /// the case's acceptable set, else 0).
    pub brier: f64,
    pub bins: Vec<CalibrationBin>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CalibrationBin {
    pub lower: f64,
    pub upper: f64,
    pub pairs: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mean_fit: Option<f64>,
    pub acceptable: Ratio,
}

fn fit_calibration(pairs: &[(f64, bool)]) -> Option<FitCalibration> {
    if pairs.is_empty() {
        return None;
    }
    let brier = pairs
        .iter()
        .map(|(fit, ok)| (fit - f64::from(u8::from(*ok))).powi(2))
        .sum::<f64>()
        / pairs.len() as f64;
    let bins = (0..5)
        .map(|bin| {
            let lower = f64::from(bin) / 5.0;
            let upper = f64::from(bin + 1) / 5.0;
            let members: Vec<&(f64, bool)> = pairs
                .iter()
                .filter(|(fit, _)| *fit >= lower && (*fit < upper || (bin == 4 && *fit <= upper)))
                .collect();
            CalibrationBin {
                lower,
                upper,
                pairs: members.len(),
                mean_fit: (!members.is_empty()).then(|| {
                    members.iter().map(|(fit, _)| fit).sum::<f64>() / members.len() as f64
                }),
                acceptable: Ratio::of(members.iter().filter(|(_, ok)| *ok).count(), members.len()),
            }
        })
        .collect();
    Some(FitCalibration {
        frame: "every reranked (case, skill) pair of the judged advisory cohort".into(),
        pairs: pairs.len(),
        brier,
        bins,
    })
}

/// A policy's top-one pick (`None` abstains), or `Err` when this case's stage
/// evidence cannot score it.
type Policy =
    fn(&crate::pipeline::StageEvidence, &EvaluationCaseRecord, f64) -> Result<Option<String>, ()>;

const BASELINE_POLICIES: [(&str, &str, Policy); 4] = [
    (
        "quill-only",
        "Quill's top lexical match among the admitted skills; no gate and no Jev answer.",
        |evidence, _, _| {
            evidence
                .lexical
                .as_ref()
                .map(|hits| hits.first().cloned())
                .ok_or(())
        },
    ),
    (
        "choice-only",
        "The wide gate, then the shortlist's highest raw wide probability; no rerank.",
        |evidence, _, _| {
            let wide = evidence.wide.as_ref().ok_or(())?;
            if wide.low_need {
                return Ok(None);
            }
            Ok(wide.shortlist.first().map(|(id, _)| id.clone()))
        },
    ),
    (
        "fit-only",
        "The wide gate, then the reranked skill with the highest fit at or above the \
         fit threshold; the rerank choice distribution is ignored.",
        |evidence, _, threshold| {
            let wide = evidence.wide.as_ref().ok_or(())?;
            let Some(rerank) = evidence.rerank.as_ref() else {
                // The gate stopped the run: this policy abstains with it.
                return if wide.low_need { Ok(None) } else { Err(()) };
            };
            Ok(rerank
                .candidates
                .iter()
                .filter(|(_, _, fit)| *fit >= threshold)
                .max_by(|a, b| a.2.total_cmp(&b.2).then_with(|| b.0.cmp(&a.0)))
                .map(|(id, _, _)| id.clone()))
        },
    ),
    (
        "blend",
        "The production decision: gate, rerank choice, per-candidate none check, fit \
         eligibility and blended score.",
        |_, record, _| Ok(record.suggested_skills.first().cloned()),
    ),
];

fn compare_baselines(
    executed: &[EvaluationCaseRecord],
    judgments: &BTreeMap<String, crate::evaluation::JudgedLabel>,
    evidence: &BTreeMap<CaseKey, crate::pipeline::StageEvidence>,
    fit_threshold: f64,
) -> BaselineComparison {
    let mut comparison = BaselineComparison {
        not_computed: vec![
            "context ablation: needs a second, latest-request-only run of every case".into(),
        ],
        ..BaselineComparison::default()
    };
    let mut cohort = Vec::new();
    for record in executed {
        let Some(label) = judgments.get(&record.key.case_id) else {
            continue;
        };
        if label.explicit_directive.is_some() {
            comparison.explicit_cases_excluded += 1;
            continue;
        }
        match evidence.get(&record.key) {
            Some(stages) if !record.operational_failure => cohort.push((record, label, stages)),
            _ => comparison.cases_without_evidence += 1,
        }
    }
    let acceptable = |label: &crate::evaluation::JudgedLabel, id: &str| {
        !label.no_skill_needed && label.acceptable_skills.contains(id)
    };
    let positive = |label: &crate::evaluation::JudgedLabel| {
        !label.no_skill_needed && !label.acceptable_skills.is_empty()
    };

    let (mut admitted_hits, mut shortlist_hits, mut shortlist_cases, mut positives) = (0, 0, 0, 0);
    for (_, label, stages) in &cohort {
        comparison.coverage.quill_ranked_cases += usize::from(stages.quill_ranked);
        if !positive(label) {
            continue;
        }
        positives += 1;
        admitted_hits += usize::from(stages.admitted.iter().any(|id| acceptable(label, id)));
        match &stages.wide {
            Some(wide) if !wide.low_need => {
                shortlist_cases += 1;
                shortlist_hits +=
                    usize::from(wide.shortlist.iter().any(|(id, _)| acceptable(label, id)));
            }
            _ => comparison.coverage.shortlist_gated_out += 1,
        }
    }
    let calibration_pairs: Vec<(f64, bool)> = cohort
        .iter()
        .flat_map(|(_, label, stages)| {
            stages.rerank.iter().flat_map(move |rerank| {
                rerank
                    .candidates
                    .iter()
                    .map(move |(id, _, fit)| (*fit, acceptable(label, id)))
            })
        })
        .collect();
    comparison.fit_calibration = fit_calibration(&calibration_pairs);
    comparison.coverage.admitted = Ratio::of(admitted_hits, positives);
    comparison.coverage.shortlist = Ratio::of(shortlist_hits, shortlist_cases);

    for (name, definition, policy) in BASELINE_POLICIES {
        let (mut emitted, mut precise, mut positive_hits, mut positive_cases) = (0, 0, 0, 0);
        let (mut needless, mut no_match_cases, mut abstentions, mut total_loss) = (0, 0, 0, 0u32);
        let mut evaluated = 0;
        let mut not_evaluated = 0;
        for (record, label, stages) in &cohort {
            let Ok(top) = policy(stages, record, fit_threshold) else {
                not_evaluated += 1;
                continue;
            };
            evaluated += 1;
            let hit = top.as_deref().is_some_and(|id| acceptable(label, id));
            emitted += usize::from(top.is_some());
            precise += usize::from(hit);
            if positive(label) {
                positive_cases += 1;
                positive_hits += usize::from(hit);
                abstentions += usize::from(top.is_none());
                total_loss += match (&top, hit) {
                    (_, true) => 0,
                    (None, _) => 1,
                    _ => 2,
                };
            } else {
                no_match_cases += 1;
                needless += usize::from(top.is_some());
                total_loss += if top.is_some() { 2 } else { 0 };
            }
        }
        comparison.policies.push(PolicyScore {
            policy: name.into(),
            definition: definition.into(),
            evaluated_cases: evaluated,
            not_evaluated_cases: not_evaluated,
            top1_precision: Ratio::of(precise, emitted),
            positive_suggestion_rate: Ratio::of(positive_hits, positive_cases),
            needless_suggestion_rate: Ratio::of(needless, no_match_cases),
            false_abstention_rate: Ratio::of(abstentions, positive_cases),
            top_k_coverage: (name == "blend").then(|| {
                let covered = cohort
                    .iter()
                    .filter(|(record, label, _)| {
                        positive(label)
                            && record
                                .suggested_skills
                                .iter()
                                .any(|id| acceptable(label, id))
                    })
                    .count();
                Ratio::of(covered, positive_cases)
            }),
            mean_loss: (evaluated > 0).then(|| f64::from(total_loss) / evaluated as f64),
        });
    }
    comparison
}

/// Draw and verify the manifest before any label is read into the result.
fn freeze_frame_sample(
    records: &[crate::evaluation::EvaluationCaseRecord],
    request: FrameSampling,
    created_at_unix_ms: u64,
) -> Result<
    (
        FrozenSampleManifest,
        Vec<crate::evaluation::EvaluationCaseRecord>,
    ),
    EvaluationError,
> {
    if request.sample_size == 0 {
        return Err(EvaluationError::SamplingFailure(
            "sample size must be positive".into(),
        ));
    }
    let splits: BTreeSet<_> = records.iter().map(|case| case.split).collect();
    let policies: BTreeSet<&str> = records
        .iter()
        .map(|case| case.key.policy_id.as_str())
        .collect();
    let (Some(&split), 1, Some(&policy_id), 1) = (
        splits.first(),
        splits.len(),
        policies.first(),
        policies.len(),
    ) else {
        return Err(EvaluationError::SamplingFailure(
            "sampling needs a non-empty frame with exactly one split and one policy".into(),
        ));
    };
    let provenance = match request.seed {
        Some(seed) => RandomizationProvenance::SuppliedManual { seed },
        None => RandomizationProvenance::OsRandom {
            entropy_source: "/dev/urandom".into(),
            seed: draw_os_seed()?,
        },
    };
    let rule = FamilyRepresentativeRule::default();
    let representatives = select_family_representatives(records, split, rule)?;
    let manifest = draw_stratified_sample(
        &representatives,
        split,
        request.sample_size,
        &AllocationMethod::Proportional { min_floor: 1 },
        provenance,
        policy_id,
        created_at_unix_ms,
    )?;
    verify_manifest_against_frame(&manifest, &representatives)?;
    Ok((manifest, representatives))
}

/// One reported quantity with its defining equation and substituted operands.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExplainedQuantity {
    pub name: String,
    pub equation: String,
    pub substituted: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
}

/// Equations, assumptions, and interpretation behind an evaluation report.
/// Computed from the report's values alone: it adds no model-generated reason.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReportExplanation {
    pub quantities: Vec<ExplainedQuantity>,
    pub assumptions: Vec<String>,
    pub interpretation: Vec<String>,
}

impl EvaluationBatchReport {
    /// Attach the explanation of this report's own quantities.
    pub fn explain(&mut self) {
        self.explanation = Some(explain_report(self));
    }
}

fn explain_report(report: &EvaluationBatchReport) -> ReportExplanation {
    let loss = &report.loss_summary;
    let mut quantities = Vec::new();
    let mut assumptions = Vec::new();
    let mut interpretation = Vec::new();
    let labeled = report.reconciliation.is_some();
    if labeled {
        quantities.push(ExplainedQuantity {
            name: "mean_loss".into(),
            equation: "mean_loss = total_loss / attempted_cases".into(),
            substituted: format!("{} / {}", loss.total_loss, loss.attempted_cases),
            value: loss.mean_loss,
        });
        quantities.push(ExplainedQuantity {
            name: "mean_normalized_loss".into(),
            equation: "y_i = loss_i / 2; mean_normalized_loss = mean_loss / 2".into(),
            substituted: loss
                .mean_loss
                .map_or_else(|| "no attempted case".into(), |mean| format!("{mean} / 2")),
            value: loss.mean_normalized_loss,
        });
        assumptions.push(
            "Labels are independent judgments joined by case_id at their highest revision.".into(),
        );
        assumptions.push(
            "Loss follows evaluation_policy.v1: a correct suggestion or abstention 0, a false \
             abstention 1, a wrong or needless suggestion or an operational failure 2."
                .into(),
        );
        assumptions.push(
            "Explicit requests are checked separately and unjudged cases carry no loss; \
             neither enters the loss denominator."
                .into(),
        );
    } else {
        assumptions.push(
            "Recorded decisions are observations, not usefulness labels; replay supplies no loss."
                .into(),
        );
    }
    match (&report.sample_manifest, &report.design_weighted_loss) {
        (Some(manifest), design) => {
            assumptions.push(format!(
                "One representative per task family was selected ({:?} rule) and the \
                 manifest was frozen before labels were joined.",
                manifest.representative_rule
            ));
            match manifest.design_status {
                DesignStatus::DiagnosticFixed => interpretation.push(
                    "A supplied seed reproduces a selection; it supports no inclusion \
                     probabilities or design-based uncertainty."
                        .into(),
                ),
                DesignStatus::StratifiedProbabilitySample => assumptions.push(
                    "Selection is uniform without replacement within each stratum from a \
                     recorded OS-random seed."
                        .into(),
                ),
                DesignStatus::FullCensus => assumptions.push(
                    "Every family in the frame was evaluated; stratum means are exact.".into(),
                ),
            }
            if let Some(design) = design {
                push_design_quantities(&mut quantities, design);
                if !design.point_estimate_guaranteed {
                    interpretation.push(format!(
                        "{} sampled cases lack a loss; read the design-weighted mean as the \
                         interval [{}, {}], not the observed point estimate.",
                        design.total_missing_labels, design.r_hat_lower, design.r_hat_upper
                    ));
                }
            }
        }
        (None, _) if labeled => assumptions.push(
            "The full supplied frame was evaluated; no sampling randomness is involved.".into(),
        ),
        (None, _) => {}
    }
    let unfinished = report
        .completeness
        .cases_requested
        .saturating_sub(report.completeness.cases_completed);
    if unfinished > 0 {
        interpretation.push(format!(
            "{unfinished} of {} cases are not estimable; the run is partial and they are \
             excluded from loss rather than counted as successes.",
            report.completeness.cases_requested
        ));
    }
    interpretation.push(match report.gate_status {
        GateStatus::Passed => "The report's quality gate passed.".into(),
        GateStatus::Failed => "The report's quality gate failed.".into(),
        GateStatus::NotApplicable => {
            "Synthetic evidence is not eligible for a quality gate.".into()
        }
        GateStatus::NotEstablished => {
            "This report is evidence for a promotion review, not a passed quality gate.".into()
        }
    });
    ReportExplanation {
        quantities,
        assumptions,
        interpretation,
    }
}

fn push_design_quantities(
    quantities: &mut Vec<ExplainedQuantity>,
    design: &DesignWeightedLossReport,
) {
    let terms = |value: fn(&crate::evaluation::design_weighted::StratumLossReport) -> f64| {
        design
            .strata
            .values()
            .map(|stratum| format!("{} * {}", stratum.weight, value(stratum)))
            .collect::<Vec<_>>()
            .join(" + ")
    };
    quantities.push(ExplainedQuantity {
        name: "design_weighted_mean_loss".into(),
        equation: "R_hat = sum_h W_h * ybar_h, W_h = N_h / N, over observed normalized loss".into(),
        substituted: design
            .strata
            .values()
            .map(|stratum| {
                format!(
                    "{} * {}",
                    stratum.weight,
                    stratum
                        .mean_loss_observed
                        .map_or_else(|| "unobserved".into(), |mean| mean.to_string())
                )
            })
            .collect::<Vec<_>>()
            .join(" + "),
        value: design.r_hat_observed,
    });
    quantities.push(ExplainedQuantity {
        name: "design_weighted_upper_bound".into(),
        equation: "U = sum_h W_h * U_h, U_h = min(1, ybar_h_upper + sqrt(ln(H / alpha) / \
                   (2 * n_h))), exact for census strata"
            .into(),
        substituted: format!(
            "H = {}, alpha = {}; {}",
            design.num_strata,
            design.alpha,
            terms(|stratum| stratum.upper_bound_uh)
        ),
        value: Some(design.conservative_upper_bound),
    });
}
