//! Local utility and normalization (P4). Scoring only orders candidates that
//! local eligibility already admitted; it cannot add, restore or remove one.
//! Scores are normalized over every eligible candidate before top-K
//! truncation, so a returned subset reports the mass it omits. A score is a
//! relative local weight, never a calibrated probability of usefulness.

use crate::eligibility::Eligible;
use crate::identity::SkillId;
use std::collections::BTreeSet;
use std::fmt;

/// Probabilities are clipped to `[EPSILON, 1 - EPSILON]` before any logarithm.
pub const EPSILON: f64 = 1e-6;
/// The largest top-K the output contract allows.
pub const MAX_TOP_K: usize = 32;

pub const W_FIT_MAX: f64 = 4.0;
pub const W_PRIOR_MAX: f64 = 0.5;
pub const W_PHASE_MAX: f64 = 1.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScoringError {
    InvalidWeight,
    InvalidEstimate,
    InvalidAdjustment,
    InvalidTopK,
    DuplicateCandidate,
    NoCandidates,
}

impl fmt::Display for ScoringError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "local scoring: {self:?}")
    }
}

impl std::error::Error for ScoringError {}

pub fn clip(x: f64) -> f64 {
    x.clamp(EPSILON, 1.0 - EPSILON)
}

pub fn log_odds(x: f64) -> f64 {
    let c = clip(x);
    (c / (1.0 - c)).ln()
}

/// Validated blend weights. Defaults are `w_fit = 1`, `w_prior = 0`,
/// `w_phase = 0`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Weights {
    fit: f64,
    prior: f64,
    phase: f64,
}

impl Default for Weights {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl Weights {
    pub const DEFAULT: Self = Self {
        fit: 1.0,
        prior: 0.0,
        phase: 0.0,
    };

    pub fn new(fit: f64, prior: f64, phase: f64) -> Result<Self, ScoringError> {
        let within = |value: f64, max: f64| (0.0..=max).contains(&value);
        if within(fit, W_FIT_MAX) && within(prior, W_PRIOR_MAX) && within(phase, W_PHASE_MAX) {
            Ok(Self { fit, prior, phase })
        } else {
            Err(ScoringError::InvalidWeight)
        }
    }
    pub const fn fit(self) -> f64 {
        self.fit
    }
    pub const fn prior(self) -> f64 {
        self.prior
    }
    pub const fn phase(self) -> f64 {
        self.phase
    }
}

/// One eligible candidate's scoring inputs. The stable skill ID is the only
/// tie key; provider option handles are resolved before scoring.
#[derive(Clone, Copy, Debug)]
pub struct Input<'a> {
    pub id: &'a SkillId,
    /// The candidate's own raw rerank Choice probability.
    pub rerank: f64,
    pub fit: f64,
    /// Finite centered prior adjustment; zero when priors are disabled.
    pub prior_delta: f64,
    /// Phase-distribution mass over the skill's declared phases, in `[0, 1]`.
    pub phase_match: f64,
}

impl<'a> Input<'a> {
    /// Inputs for an eligible candidate with priors and phase disabled.
    pub fn from_eligible(eligible: &Eligible<'a>) -> Self {
        Self {
            id: &eligible.skill.binding.id,
            rerank: eligible.rerank,
            fit: eligible.fit,
            prior_delta: 0.0,
            phase_match: 0.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scored {
    /// Position of the candidate in the caller's input slice.
    pub index: usize,
    pub utility: f64,
    pub rank_score: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Ranking {
    /// At most `top_k` candidates, by descending score then ascending stable ID.
    pub returned: Vec<Scored>,
    /// Summed score of every eligible candidate beyond `top_k`.
    pub omitted_mass: f64,
    /// Number of eligible candidates normalized, including omitted ones.
    pub eligible: usize,
}

pub fn utility(input: &Input<'_>, weights: Weights) -> f64 {
    clip(input.rerank).ln()
        + weights.fit * log_odds(input.fit)
        + weights.prior * input.prior_delta
        + weights.phase * input.phase_match
}

fn validate(inputs: &[Input<'_>], top_k: usize) -> Result<(), ScoringError> {
    if inputs.is_empty() {
        return Err(ScoringError::NoCandidates);
    }
    if !(1..=MAX_TOP_K).contains(&top_k) {
        return Err(ScoringError::InvalidTopK);
    }
    let probability = |x: f64| (0.0..=1.0).contains(&x);
    let mut ids = BTreeSet::new();
    for input in inputs {
        if !probability(input.rerank) || !probability(input.fit) {
            return Err(ScoringError::InvalidEstimate);
        }
        if !input.prior_delta.is_finite() || !probability(input.phase_match) {
            return Err(ScoringError::InvalidAdjustment);
        }
        if !ids.insert(input.id) {
            return Err(ScoringError::DuplicateCandidate);
        }
    }
    Ok(())
}

/// Score every eligible candidate with a max-shifted softmax over their
/// utilities, then return the top `top_k` and the mass left out.
pub fn rank(inputs: &[Input<'_>], weights: Weights, top_k: usize) -> Result<Ranking, ScoringError> {
    validate(inputs, top_k)?;
    let utilities: Vec<f64> = inputs.iter().map(|input| utility(input, weights)).collect();
    if !utilities.iter().all(|u| u.is_finite()) {
        return Err(ScoringError::InvalidAdjustment);
    }
    let max = utilities.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let exps: Vec<f64> = utilities.iter().map(|u| (u - max).exp()).collect();
    let total: f64 = exps.iter().sum();
    let mut scored: Vec<Scored> = utilities
        .iter()
        .zip(&exps)
        .enumerate()
        .map(|(index, (&utility, &exp))| Scored {
            index,
            utility,
            rank_score: exp / total,
        })
        .collect();
    scored.sort_by(|a, b| {
        b.rank_score
            .total_cmp(&a.rank_score)
            .then_with(|| inputs[a.index].id.cmp(inputs[b.index].id))
    });
    let omitted = scored.split_off(top_k.min(scored.len()));
    Ok(Ranking {
        omitted_mass: omitted.iter().map(|s| s.rank_score).sum(),
        returned: scored,
        eligible: inputs.len(),
    })
}
