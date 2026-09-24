//! Bounded offline replay batches. Recorded decisions are observations, not
//! independent usefulness labels: replay success cannot pass a quality gate.
//! Live execution remains unsupported until provider admission and accounting
//! are connected to the batch runner. A labeled case frame is scored against
//! independent judgments by `execute_labeled_frame_evaluation`.

use crate::evaluation::design_weighted::{
    DesignWeightedLossReport, SampledCaseLoss, compute_design_weighted_loss_from_manifest,
};
use crate::evaluation::stratified::{
    AllocationMethod, DesignStatus, FamilyRepresentativeRule, FrozenSampleManifest,
    RandomizationProvenance, draw_os_seed, draw_stratified_sample, select_family_representatives,
    verify_manifest_against_frame,
};
use crate::evaluation::{
    CaseKey, EvaluationError, EvaluationMetrics, LabelStatus, ReconciliationManifest,
    RelevanceClass, compute_metrics, join_evaluation_frame, parse_case_records_streaming,
    parse_labels_streaming, read_bounded_line, verify_split_isolation,
};
use crate::limits::{
    DEFAULT_EVAL_BATCH_RUNTIME_MS, EVALUATION_CASE_RECORDS, EVALUATION_DATASET_BYTES,
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
        return Err(EvaluationError::InvalidField(
            "live batch execution is not implemented; supply recorded replay cases in offline mode"
                .into(),
        ));
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
    let (evaluated, manifest) = match sampling {
        None => (records, None),
        Some(request) => {
            let (manifest, representatives) =
                freeze_frame_sample(&records, request, created_at_unix_ms)?;
            let selected: BTreeSet<&CaseKey> = manifest
                .selected_cases
                .iter()
                .map(|entry| &entry.case_key)
                .collect();
            let sampled = representatives
                .into_iter()
                .filter(|case| selected.contains(&case.key))
                .collect();
            (sampled, Some(manifest))
        }
    };
    let (resolved, reconciliation) = join_evaluation_frame(&evaluated, &labels)?;
    let metrics = compute_metrics(&resolved);

    let mut loss_summary = BatchLossSummary::default();
    let mut losses_by_case = BTreeMap::new();
    let mut reports = Vec::with_capacity(resolved.len());
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
    // inclusion probabilities to weight by. Explicit and unjudged cases stay
    // Missing, so the reported bounds widen rather than hide them.
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

    let requested = resolved.len();
    let report = EvaluationBatchReport {
        schema_version: SCHEMA_VERSION,
        kind: "report".into(),
        actionable: false,
        run_status: if completed == requested {
            RunStatus::Complete
        } else {
            RunStatus::Partial
        },
        // Recorded decisions scored here are evidence for a promotion review,
        // not a passed gate; the frozen promotion thresholds are applied there.
        gate_status: GateStatus::NotEstablished,
        evidence_origin: "recorded".into(),
        completeness: CompletenessReport {
            cases_requested: requested,
            cases_completed: completed,
            stages_required: 0,
            stages_completed: 0,
            evidence_compatible: reconciliation.reconciled,
        },
        loss_summary,
        accounting: BatchAccounting::default(),
        metrics: Some(metrics),
        reconciliation: Some(reconciliation),
        sample_manifest: manifest,
        design_weighted_loss,
        explanation: None,
        cases: reports,
        error: None,
    };
    report.to_document().map_err(|e| {
        EvaluationError::InvalidField(format!(
            "evaluation report exceeds or violates output contract: {e}"
        ))
    })?;
    Ok(report)
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
