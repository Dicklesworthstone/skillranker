//! Design-weighted Horvitz-Thompson loss estimation and conservative stratum bounds (P5, sr-roadmap-l1i.6.22).
//!
//! Provides:
//! 1. Horvitz-Thompson design-weighted bounded-loss mean:
//!    $$\hat{R} = \sum_h W_h \bar{y}_h = \frac{1}{N} \sum_{i \in \text{sample}} \frac{y_i}{\pi_i}$$
//!    for bounded loss $y_i \in [0, 1]$, where $W_h = N_h / N$ and $\pi_i = n_h / N_h$.
//! 2. Conservative per-stratum and frame upper risk bounds via Hoeffding's inequality
//!    without replacement (Bardenet & Maillard 2015, Prop 1.2) plus a union bound:
//!    $$U_h = \min\left(1, \bar{y}_h + \sqrt{\frac{\ln(H / \alpha)}{2 \cdot n_h}}\right), \quad U = \sum_h W_h U_h$$
//!    with exact means for fully enumerated census strata ($n_h = N_h$).
//! 3. Missing label handling: explicit bounds $[0, 1]$ bounding loss from below and above,
//!    using the upper assignment for conservative upper risk bounds, and invalidating
//!    the unbiased point estimate guarantee when nonresponse is present.
//! 4. Non-clipping guarantee: small inclusion probabilities and extreme stratum weights
//!    are preserved and reported; weights are never silently clipped.
//! 5. Separation of non-linear weighted ratio estimators from unbiased Horvitz-Thompson means.
//! 6. Multi-endpoint alpha allocation preserving family-wise error budgets.

use crate::evaluation::CaseKey;
use crate::evaluation::stratified::{
    DesignStatus, FrozenSampleManifest, RandomizationProvenance, StratumAllocation,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// Mathematical or domain errors in design-weighted estimation.
#[derive(Clone, Debug, PartialEq)]
pub enum DesignWeightedError {
    InvalidEndpoints,
    UnsupportedDesign,
    InvalidInclusionProbability {
        stratum: String,
        probability: Option<f64>,
    },
    InvalidAlpha(f64),
    AlphaBudgetExceeded {
        allocated: f64,
        budget: f64,
    },
    InvalidLoss {
        value: f64,
    },
    EmptyFrame,
    EmptyStratum {
        stratum: String,
    },
    ZeroSampleSize {
        stratum: String,
    },
    SampleExceedsPopulation {
        stratum: String,
        sample: usize,
        population: usize,
    },
    NonFiniteWeight {
        stratum: String,
        weight: f64,
    },
    InconsistentWeights {
        sum: f64,
    },
    UnknownStratum {
        stratum: String,
    },
    InconsistentSampleCount {
        stratum: String,
        expected: usize,
        actual: usize,
    },
    MissingAllocation {
        stratum: String,
    },
}

impl fmt::Display for DesignWeightedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEndpoints => f.write_str(
                "endpoint names must be nonempty and unique, with at least one endpoint",
            ),
            Self::UnsupportedDesign => {
                f.write_str("design-based inference requires a probability sample or census")
            }
            Self::InvalidInclusionProbability {
                stratum,
                probability,
            } => write!(
                f,
                "stratum '{stratum}' requires a finite positive inclusion probability matching its sampling design, got {probability:?}"
            ),
            Self::InvalidAlpha(a) => {
                write!(f, "error budget alpha must be in (0, 1), got {a}")
            }
            Self::AlphaBudgetExceeded { allocated, budget } => {
                write!(
                    f,
                    "allocated alpha sum {allocated} exceeds total error budget {budget}"
                )
            }
            Self::InvalidLoss { value } => {
                write!(f, "loss must be a finite number in [0.0, 1.0], got {value}")
            }
            Self::EmptyFrame => f.write_str("evaluation frame cannot be empty (N = 0)"),
            Self::EmptyStratum { stratum } => {
                write!(f, "stratum '{stratum}' has zero population (N_h = 0)")
            }
            Self::ZeroSampleSize { stratum } => {
                write!(f, "stratum '{stratum}' has zero sample size (n_h = 0)")
            }
            Self::SampleExceedsPopulation {
                stratum,
                sample,
                population,
            } => {
                write!(
                    f,
                    "stratum '{stratum}' sample size n_h ({sample}) exceeds population N_h ({population})"
                )
            }
            Self::NonFiniteWeight { stratum, weight } => {
                write!(f, "stratum '{stratum}' weight must be finite, got {weight}")
            }
            Self::InconsistentWeights { sum } => {
                write!(
                    f,
                    "stratum weights sum {sum} must equal 1.0 within numerical tolerance"
                )
            }
            Self::UnknownStratum { stratum } => {
                write!(f, "case references unknown stratum '{stratum}'")
            }
            Self::InconsistentSampleCount {
                stratum,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "stratum '{stratum}' expected {expected} samples from allocation, got {actual}"
                )
            }
            Self::MissingAllocation { stratum } => {
                write!(f, "stratum '{stratum}' has cases but no allocation entry")
            }
        }
    }
}

impl std::error::Error for DesignWeightedError {}

/// Loss status of a sampled case.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", content = "loss", rename_all = "snake_case")]
pub enum SampledCaseLoss {
    /// Fully observed bounded loss $y_i \in [0, 1]$.
    Observed(f64),
    /// Unobserved or missing label. Bounded by $[0, 1]$ rather than silently removed.
    Missing,
}

impl SampledCaseLoss {
    /// Validates that an observed loss is finite and in $[0.0, 1.0]$.
    pub fn validate(&self) -> Result<(), DesignWeightedError> {
        if let Self::Observed(v) = *self
            && (!v.is_finite() || !(0.0..=1.0).contains(&v))
        {
            return Err(DesignWeightedError::InvalidLoss { value: v });
        }
        Ok(())
    }

    /// Returns the lower bound assignment (0.0 for missing).
    #[must_use]
    pub fn lower_assignment(&self) -> f64 {
        match *self {
            Self::Observed(v) => v,
            Self::Missing => 0.0,
        }
    }

    /// Returns the upper bound assignment (1.0 for missing).
    #[must_use]
    pub fn upper_assignment(&self) -> f64 {
        match *self {
            Self::Observed(v) => v,
            Self::Missing => 1.0,
        }
    }

    #[must_use]
    pub const fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }
}

/// Evaluated loss record for one sampled case.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StratumCaseLoss {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub case_key: Option<CaseKey>,
    pub stratum_key: String,
    pub loss: SampledCaseLoss,
}

impl StratumCaseLoss {
    #[must_use]
    pub fn new(stratum_key: impl Into<String>, loss: SampledCaseLoss) -> Self {
        Self {
            case_key: None,
            stratum_key: stratum_key.into(),
            loss,
        }
    }

    #[must_use]
    pub fn with_key(
        case_key: CaseKey,
        stratum_key: impl Into<String>,
        loss: SampledCaseLoss,
    ) -> Self {
        Self {
            case_key: Some(case_key),
            stratum_key: stratum_key.into(),
            loss,
        }
    }
}

/// Loss summary and conservative bound for a single observable stratum $h$.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StratumLossReport {
    pub stratum_key: String,
    /// Frame population size $N_h$.
    pub population_size: usize,
    /// Sample size $n_h$.
    pub sample_size: usize,
    /// Frame weight $W_h = N_h / N$.
    pub weight: f64,
    /// Design inclusion probability $\pi_h = n_h / N_h$.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inclusion_probability: Option<f64>,
    /// Number of sampled cases with fully observed labels.
    pub observed_count: usize,
    /// Number of sampled cases with missing labels.
    pub missing_count: usize,
    /// True if stratum is fully enumerated ($n_h = N_h$).
    pub is_census: bool,
    /// Unweighted mean of observed cases in this stratum, if any were observed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mean_loss_observed: Option<f64>,
    /// Stratum mean under lower assignment (missing $= 0.0$).
    pub mean_loss_lower: f64,
    /// Stratum mean under upper assignment (missing $= 1.0$).
    pub mean_loss_upper: f64,
    /// Conservative upper confidence bound $U_h$.
    /// For census strata, this is exact ($\bar{y}_h^{\text{upper}}$).
    /// For sampled strata, $U_h = \min(1.0, \bar{y}_h^{\text{upper}} + \sqrt{\ln(H/\alpha) / (2 n_h)})$.
    pub upper_bound_uh: f64,
}

/// Comprehensive design-weighted Horvitz-Thompson loss report across all strata.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DesignWeightedLossReport {
    /// Total frame population $N = \sum N_h$.
    pub total_frame_cases: usize,
    /// Total sampled units $n = \sum n_h$.
    pub total_sampled_cases: usize,
    /// Total number of observable strata $H$.
    pub num_strata: usize,
    /// Total number of sampled cases with missing labels across all strata.
    pub total_missing_labels: usize,
    /// Error budget $\alpha \in (0, 1)$ used for the conservative bounds.
    pub alpha: f64,
    /// Smallest inclusion probability among sampled strata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_inclusion_probability: Option<f64>,
    /// Largest inclusion probability among sampled strata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_inclusion_probability: Option<f64>,
    /// Invariant: weights are never silently clipped while claiming unbiasedness.
    pub weights_clipped: bool,
    /// Point estimate guarantee holds if and only if there are zero missing labels
    /// and all inclusion probabilities are known and valid.
    pub point_estimate_guaranteed: bool,
    /// Design-weighted mean $\hat{R}_{\text{observed}} = \sum W_h \bar{y}_h$ over observed cases.
    /// None if any stratum had zero observed cases.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r_hat_observed: Option<f64>,
    /// Lower bound on design-weighted mean $\sum W_h \bar{y}_h^{\text{lower}}$ (missing $= 0.0$).
    pub r_hat_lower: f64,
    /// Upper bound on design-weighted mean $\sum W_h \bar{y}_h^{\text{upper}}$ (missing $= 1.0$).
    pub r_hat_upper: f64,
    /// Conservative frame-level upper bound $U = \sum W_h U_h$.
    /// Satisfies $P(\mu \le U) \ge 1 - \alpha$ under the uniform-within-stratum design.
    pub conservative_upper_bound: f64,
    /// Detailed per-stratum loss reports.
    pub strata: BTreeMap<String, StratumLossReport>,
}

/// Non-linear design-weighted ratio estimator $\hat{R} = \hat{T}_Y / \hat{T}_X$.
///
/// Invariant: Ratios such as correct emissions divided by total emissions are ratio
/// estimators, not unbiased means. Ordinary Wilson score intervals do not apply to
/// weighted pseudo-counts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DesignWeightedRatio {
    /// Weighted estimated numerator total $\hat{T}_Y = \sum W_h \bar{y}_h$.
    pub numerator_hat: f64,
    /// Weighted estimated denominator total $\hat{T}_X = \sum W_h \bar{x}_h$.
    pub denominator_hat: f64,
    /// Point ratio estimate $\hat{R} = \hat{T}_Y / \hat{T}_X$.
    pub ratio: f64,
    pub missing_numerator_count: usize,
    pub missing_denominator_count: usize,
}

/// Pair of (numerator_loss, denominator_indicator) for ratio estimation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StratumCaseRatioPair {
    pub stratum_key: String,
    pub numerator: SampledCaseLoss,
    pub denominator: SampledCaseLoss,
}

/// Multi-endpoint error budget allocator.
///
/// Ensures the sum of allocated error budgets across multiple claimed endpoints
/// does not exceed the total report error budget $\alpha$.
#[derive(Clone, Debug, PartialEq)]
pub struct MultiEndpointAlphaAllocation {
    total_alpha: f64,
    allocations: BTreeMap<String, f64>,
}

impl MultiEndpointAlphaAllocation {
    /// Creates an allocator with an equal split of `total_alpha` across the given endpoint names.
    pub fn equal_split(
        total_alpha: f64,
        endpoint_names: &[impl AsRef<str>],
    ) -> Result<Self, DesignWeightedError> {
        validate_alpha(total_alpha)?;
        if endpoint_names.is_empty() {
            return Err(DesignWeightedError::InvalidEndpoints);
        }
        let per_endpoint = total_alpha / (endpoint_names.len() as f64);
        validate_alpha(per_endpoint)?;
        let mut allocations = BTreeMap::new();
        for name in endpoint_names {
            if name.as_ref().trim().is_empty()
                || allocations
                    .insert(name.as_ref().to_string(), per_endpoint)
                    .is_some()
            {
                return Err(DesignWeightedError::InvalidEndpoints);
            }
        }
        match Self::explicit(total_alpha, allocations.clone()) {
            Err(DesignWeightedError::AlphaBudgetExceeded { .. }) => {
                // Division can round a share upward. Reduce each share by one
                // representable step, then revalidate rather than grant slack.
                for value in allocations.values_mut() {
                    *value = value.next_down();
                }
                Self::explicit(total_alpha, allocations)
            }
            result => result,
        }
    }

    /// Creates an allocator with explicit per-endpoint alpha budgets, verifying $\sum \alpha_m \le \alpha$.
    pub fn explicit(
        total_alpha: f64,
        allocations: BTreeMap<String, f64>,
    ) -> Result<Self, DesignWeightedError> {
        validate_alpha(total_alpha)?;
        if allocations.is_empty() || allocations.keys().any(|name| name.trim().is_empty()) {
            return Err(DesignWeightedError::InvalidEndpoints);
        }
        let mut sum = 0.0;
        let mut correction = 0.0;
        for &alpha_m in allocations.values() {
            validate_alpha(alpha_m)?;
            // Compensated summation avoids rejecting ordinary equal splits
            // solely because repeated additions accumulate rounding error.
            let next = sum + alpha_m;
            correction += if sum >= alpha_m {
                (sum - next) + alpha_m
            } else {
                (alpha_m - next) + sum
            };
            sum = next;
        }
        sum += correction;
        if !sum.is_finite() || sum > total_alpha {
            return Err(DesignWeightedError::AlphaBudgetExceeded {
                allocated: sum,
                budget: total_alpha,
            });
        }
        Ok(Self {
            total_alpha,
            allocations,
        })
    }

    /// The immutable total budget validated at construction.
    #[must_use]
    pub const fn total_alpha(&self) -> f64 {
        self.total_alpha
    }

    /// Returns the allocated alpha for a given endpoint name.
    #[must_use]
    pub fn get(&self, endpoint_name: &str) -> Option<f64> {
        self.allocations.get(endpoint_name).copied()
    }

    #[must_use]
    pub fn endpoints(&self) -> Vec<String> {
        self.allocations.keys().cloned().collect()
    }
}

fn validate_alpha(alpha: f64) -> Result<(), DesignWeightedError> {
    if !alpha.is_finite() || alpha <= 0.0 || alpha >= 1.0 {
        return Err(DesignWeightedError::InvalidAlpha(alpha));
    }
    Ok(())
}

/// Compute design-weighted Horvitz-Thompson bounded loss and conservative stratum bounds.
///
/// # Arguments
/// * `strata_allocations` - Map of stratum key to its allocation and weighting parameters.
/// * `case_losses` - Slice of evaluated case losses.
/// * `alpha` - Prespecified report error budget $\alpha \in (0, 1)$.
///
/// # Mathematical guarantees
/// 1. Unbiased point estimate: $E[\hat{R}] = \mu$ for fully observed samples under the uniform design.
/// 2. Bounded coverage: $P(\mu \le U) \ge 1 - \alpha$ under the frozen uniform-within-stratum design.
/// 3. Fully enumerated census strata contribute zero sampling variance ($U_h = \bar{y}_h$).
///
/// The caller must establish the sampling design before using this mathematical
/// primitive. Matching numeric probabilities alone do not prove random selection.
/// Missing non-census probabilities are refused, not inferred from sample sizes.
pub fn compute_design_weighted_loss(
    strata_allocations: &BTreeMap<String, StratumAllocation>,
    case_losses: &[StratumCaseLoss],
    alpha: f64,
) -> Result<DesignWeightedLossReport, DesignWeightedError> {
    validate_alpha(alpha)?;

    if strata_allocations.is_empty() {
        return Err(DesignWeightedError::EmptyFrame);
    }

    let mut total_frame_cases = 0usize;
    let mut total_sampled_cases = 0usize;
    let mut weight_sum = 0.0f64;

    for (key, alloc) in strata_allocations {
        if alloc.population_size == 0 {
            return Err(DesignWeightedError::EmptyStratum {
                stratum: key.clone(),
            });
        }
        if alloc.sample_size == 0 {
            return Err(DesignWeightedError::ZeroSampleSize {
                stratum: key.clone(),
            });
        }
        if alloc.sample_size > alloc.population_size {
            return Err(DesignWeightedError::SampleExceedsPopulation {
                stratum: key.clone(),
                sample: alloc.sample_size,
                population: alloc.population_size,
            });
        }
        let expected_pi = alloc.sample_size as f64 / alloc.population_size as f64;
        let valid_pi = match alloc.inclusion_probability {
            Some(pi) => {
                pi.is_finite()
                    && pi > 0.0
                    && pi <= 1.0
                    && (pi - expected_pi).abs() <= expected_pi * 1e-12
            }
            None => alloc.sample_size == alloc.population_size,
        };
        if !valid_pi {
            return Err(DesignWeightedError::InvalidInclusionProbability {
                stratum: key.clone(),
                probability: alloc.inclusion_probability,
            });
        }
        if !alloc.weight.is_finite() || alloc.weight < 0.0 {
            return Err(DesignWeightedError::NonFiniteWeight {
                stratum: key.clone(),
                weight: alloc.weight,
            });
        }
        total_frame_cases = total_frame_cases.saturating_add(alloc.population_size);
        total_sampled_cases = total_sampled_cases.saturating_add(alloc.sample_size);
        weight_sum += alloc.weight;
    }

    if (weight_sum - 1.0).abs() > 1e-4 {
        return Err(DesignWeightedError::InconsistentWeights { sum: weight_sum });
    }

    // Group case losses by stratum
    let mut losses_by_stratum: BTreeMap<&str, Vec<&SampledCaseLoss>> = BTreeMap::new();
    for alloc_key in strata_allocations.keys() {
        losses_by_stratum.insert(alloc_key.as_str(), Vec::new());
    }

    for case in case_losses {
        case.loss.validate()?;
        let list = losses_by_stratum
            .get_mut(case.stratum_key.as_str())
            .ok_or_else(|| DesignWeightedError::UnknownStratum {
                stratum: case.stratum_key.clone(),
            })?;
        list.push(&case.loss);
    }

    let num_strata = strata_allocations.len();
    let num_strata_f = num_strata as f64;
    let h_over_alpha = num_strata_f / alpha;
    let log_h_over_alpha = h_over_alpha.ln();

    let mut stratum_reports = BTreeMap::new();
    let mut total_missing_labels = 0usize;
    let mut all_strata_have_observed = true;
    let mut all_probabilities_known = true;
    let mut min_pi: Option<f64> = None;
    let mut max_pi: Option<f64> = None;

    let mut r_hat_observed_acc = 0.0f64;
    let mut r_hat_lower = 0.0f64;
    let mut r_hat_upper = 0.0f64;
    let mut conservative_upper_bound = 0.0f64;

    for (stratum_key, alloc) in strata_allocations {
        let cases = &losses_by_stratum[stratum_key.as_str()];
        if cases.len() != alloc.sample_size {
            return Err(DesignWeightedError::InconsistentSampleCount {
                stratum: stratum_key.clone(),
                expected: alloc.sample_size,
                actual: cases.len(),
            });
        }

        let n_h = alloc.sample_size;
        let n_h_f = n_h as f64;
        let is_census = n_h == alloc.population_size;

        // Track inclusion probabilities
        // Only complete enumeration justifies filling an absent probability.
        // Non-census absence was rejected before computing any inference.
        let pi_h = alloc
            .inclusion_probability
            .or(if is_census { Some(1.0) } else { None });

        if let Some(pi) = pi_h {
            min_pi = Some(min_pi.map_or(pi, |m| m.min(pi)));
            max_pi = Some(max_pi.map_or(pi, |m| m.max(pi)));
        } else {
            all_probabilities_known = false;
        }

        let mut observed_sum = 0.0f64;
        let mut observed_count = 0usize;
        let mut missing_count = 0usize;
        let mut lower_sum = 0.0f64;
        let mut upper_sum = 0.0f64;

        for loss in cases {
            lower_sum += loss.lower_assignment();
            upper_sum += loss.upper_assignment();
            match **loss {
                SampledCaseLoss::Observed(v) => {
                    observed_sum += v;
                    observed_count += 1;
                }
                SampledCaseLoss::Missing => {
                    missing_count += 1;
                }
            }
        }

        total_missing_labels = total_missing_labels.saturating_add(missing_count);

        let mean_observed = if observed_count > 0 {
            Some(observed_sum / observed_count as f64)
        } else {
            all_strata_have_observed = false;
            None
        };

        let mean_lower = lower_sum / n_h_f;
        let mean_upper = upper_sum / n_h_f;

        // Hoeffding without replacement bound:
        // U_h = min(1.0, mean_upper + sqrt(ln(H/alpha) / (2 * n_h)))
        // For census strata, variance is zero, so U_h is exact mean_upper.
        let upper_bound_uh = if is_census {
            mean_upper
        } else {
            let margin = (log_h_over_alpha / (2.0 * n_h_f)).sqrt();
            (mean_upper + margin).min(1.0)
        };

        let w_h = alloc.weight;
        if let Some(mo) = mean_observed {
            r_hat_observed_acc += w_h * mo;
        }
        r_hat_lower += w_h * mean_lower;
        r_hat_upper += w_h * mean_upper;
        conservative_upper_bound += w_h * upper_bound_uh;

        stratum_reports.insert(
            stratum_key.clone(),
            StratumLossReport {
                stratum_key: stratum_key.clone(),
                population_size: alloc.population_size,
                sample_size: n_h,
                weight: w_h,
                inclusion_probability: pi_h,
                observed_count,
                missing_count,
                is_census,
                mean_loss_observed: mean_observed,
                mean_loss_lower: mean_lower,
                mean_loss_upper: mean_upper,
                upper_bound_uh,
            },
        );
    }

    let r_hat_observed = if all_strata_have_observed {
        Some(r_hat_observed_acc)
    } else {
        None
    };

    let point_estimate_guaranteed = total_missing_labels == 0 && all_probabilities_known;

    Ok(DesignWeightedLossReport {
        total_frame_cases,
        total_sampled_cases,
        num_strata,
        total_missing_labels,
        alpha,
        min_inclusion_probability: min_pi,
        max_inclusion_probability: max_pi,
        weights_clipped: false,
        point_estimate_guaranteed,
        r_hat_observed,
        r_hat_lower,
        r_hat_upper,
        conservative_upper_bound: conservative_upper_bound.min(1.0),
        strata: stratum_reports,
    })
}

/// Convenience helper to compute design-weighted loss directly from a frozen manifest
/// and a map of case keys to their evaluated losses.
/// The caller must first verify the manifest against its source frame; this
/// adapter does not authenticate provenance or establish frame membership.
pub fn compute_design_weighted_loss_from_manifest(
    manifest: &FrozenSampleManifest,
    losses_by_case: &BTreeMap<CaseKey, SampledCaseLoss>,
    alpha: f64,
) -> Result<DesignWeightedLossReport, DesignWeightedError> {
    match manifest.design_status {
        DesignStatus::DiagnosticFixed => return Err(DesignWeightedError::UnsupportedDesign),
        DesignStatus::StratifiedProbabilitySample => {
            if !matches!(
                manifest.randomization_provenance,
                RandomizationProvenance::OsRandom { .. }
            ) {
                return Err(DesignWeightedError::UnsupportedDesign);
            }
        }
        DesignStatus::FullCensus => {
            if manifest.total_sampled_families != manifest.total_frame_families
                || manifest
                    .strata
                    .values()
                    .any(|s| s.sample_size != s.population_size)
            {
                return Err(DesignWeightedError::UnsupportedDesign);
            }
        }
    }
    let mut case_losses = Vec::with_capacity(manifest.selected_cases.len());
    for entry in &manifest.selected_cases {
        let loss = losses_by_case
            .get(&entry.case_key)
            .copied()
            .unwrap_or(SampledCaseLoss::Missing);
        case_losses.push(StratumCaseLoss {
            case_key: Some(entry.case_key.clone()),
            stratum_key: entry.stratum_key.clone(),
            loss,
        });
    }

    compute_design_weighted_loss(&manifest.strata, &case_losses, alpha)
}

/// Compute a design-weighted non-linear ratio estimator $\hat{R} = \hat{T}_Y / \hat{T}_X$.
///
/// Invariant: Ratio estimators are non-linear; ordinary Wilson confidence intervals
/// must never be applied to weighted pseudo-counts.
pub fn compute_design_weighted_ratio(
    strata_allocations: &BTreeMap<String, StratumAllocation>,
    case_pairs: &[StratumCaseRatioPair],
) -> Result<DesignWeightedRatio, DesignWeightedError> {
    if strata_allocations.is_empty() {
        return Err(DesignWeightedError::EmptyFrame);
    }

    let mut pairs_by_stratum: BTreeMap<&str, Vec<&StratumCaseRatioPair>> = BTreeMap::new();
    for key in strata_allocations.keys() {
        pairs_by_stratum.insert(key.as_str(), Vec::new());
    }

    for pair in case_pairs {
        pair.numerator.validate()?;
        pair.denominator.validate()?;
        let list = pairs_by_stratum
            .get_mut(pair.stratum_key.as_str())
            .ok_or_else(|| DesignWeightedError::UnknownStratum {
                stratum: pair.stratum_key.clone(),
            })?;
        list.push(pair);
    }

    let mut numerator_hat = 0.0f64;
    let mut denominator_hat = 0.0f64;
    let mut missing_num = 0usize;
    let mut missing_den = 0usize;

    for (stratum_key, alloc) in strata_allocations {
        let pairs = &pairs_by_stratum[stratum_key.as_str()];
        if pairs.len() != alloc.sample_size {
            return Err(DesignWeightedError::InconsistentSampleCount {
                stratum: stratum_key.clone(),
                expected: alloc.sample_size,
                actual: pairs.len(),
            });
        }

        let n_h_f = alloc.sample_size as f64;
        let mut num_sum = 0.0f64;
        let mut den_sum = 0.0f64;

        for pair in pairs {
            if pair.numerator.is_missing() {
                missing_num += 1;
            } else {
                num_sum += pair.numerator.lower_assignment();
            }
            if pair.denominator.is_missing() {
                missing_den += 1;
            } else {
                den_sum += pair.denominator.lower_assignment();
            }
        }

        let w_h = alloc.weight;
        numerator_hat += w_h * (num_sum / n_h_f);
        denominator_hat += w_h * (den_sum / n_h_f);
    }

    let ratio = if denominator_hat > 0.0 {
        numerator_hat / denominator_hat
    } else {
        0.0
    };

    Ok(DesignWeightedRatio {
        numerator_hat,
        denominator_hat,
        ratio,
        missing_numerator_count: missing_num,
        missing_denominator_count: missing_den,
    })
}
