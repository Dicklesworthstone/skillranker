//! Bounded offline replay batches. Recorded decisions are observations, not
//! independent usefulness labels: replay success cannot pass a quality gate.
//! Live execution remains unsupported until provider admission and accounting
//! are connected to the batch runner.

use crate::evaluation::{EvaluationError, EvaluationMetrics, read_bounded_line};
use crate::limits::{
    DEFAULT_EVAL_BATCH_RUNTIME_MS, EVALUATION_CASE_RECORDS, EVALUATION_DATASET_BYTES,
    EVALUATION_DATASET_DEPTH,
};
use crate::output::{GateStatus, OutputDocument, RunStatus, SCHEMA_VERSION};
use crate::replay::{ReplayCase, ReplayPolicy, execute_replay_comparison};
use crate::runtime::EntryClock;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
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

/// Recompute validated recorded cases without network or persistence effects.
/// Independent judgments are not part of ReplayCase; quality loss stays unknown.
pub fn execute_evaluation_batch<R: BufRead>(
    reader: R,
    config: &BatchConfig,
    clock: &EntryClock,
) -> Result<EvaluationBatchReport, EvaluationError> {
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
