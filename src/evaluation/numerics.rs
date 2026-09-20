//! Narrow numerical evaluation primitives adapted from FrankenSciPy (sr-roadmap-l1i.6.18).
//!
//! Provides:
//! 1. Wilson score confidence interval (two-sided).
//! 2. Exact Clopper-Pearson binomial confidence interval (two-sided and one-sided upper).
//! 3. Beta distribution inverse CDF (PPF) via bracketed root-finding on the regularized incomplete beta.
//! 4. Stable closed-form zero-event upper bound `-expm1(log(1 - alpha) / n)`.
//! 5. Max-shifted stable log-sum-exp for safe probability tail summation.
//! 6. Backend provenance metadata recording source repository and commit revisions.

use std::f64::consts::PI;
use std::fmt;

/// Mathematical and domain errors for evaluation numerics.
#[derive(Clone, Debug, PartialEq)]
pub enum NumericsError {
    DomainError(String),
    InvalidConfidence(f64),
    InvalidSampleSize { k: usize, n: usize },
    EmptyTrials,
    NonFiniteInput,
}

impl fmt::Display for NumericsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DomainError(msg) => write!(f, "numerical domain error: {msg}"),
            Self::InvalidConfidence(c) => {
                write!(f, "confidence level must be strictly in (0, 1), got {c}")
            }
            Self::InvalidSampleSize { k, n } => {
                write!(f, "successes k ({k}) cannot exceed total trials n ({n})")
            }
            Self::EmptyTrials => f.write_str("confidence interval is undefined for n = 0 trials"),
            Self::NonFiniteInput => f.write_str("numerical input must be finite"),
        }
    }
}

impl std::error::Error for NumericsError {}

/// Provenance metadata tracking the exact backend source and commit revisions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BackendProvenance {
    pub backend_name: &'static str,
    pub frankenscipy_revision: &'static str,
    pub franken_numpy_revision: &'static str,
    pub frankenpandas_revision: &'static str,
}

pub const BACKEND_PROVENANCE: BackendProvenance = BackendProvenance {
    backend_name: "frankensuite-narrow",
    frankenscipy_revision: "213a417c739025ed865a3875c89a7d86726ecb5a",
    franken_numpy_revision: "52700bc2a6d2ab48dab5e608b1cfa34d781a7bb1",
    frankenpandas_revision: "ab7bc5a4b5a59a5923987622a08d0f391093a8db",
};

// ---------------------------------------------------------------------------
// Standard Normal Quantile (Inverse CDF) via Wichura (1988) AS241
// ---------------------------------------------------------------------------

/// Inverse CDF (quantile function) of the standard normal distribution $\Phi^{-1}(p)$.
///
/// Implements the Wichura (1988) Algorithm AS 241 rational approximation.
/// Domain: $0 < p < 1$.
#[allow(clippy::excessive_precision)]
pub fn standard_normal_ppf(p: f64) -> Result<f64, NumericsError> {
    if !p.is_finite() {
        return Err(NumericsError::NonFiniteInput);
    }
    if p <= 0.0 || p >= 1.0 {
        return Err(NumericsError::DomainError(format!(
            "normal ppf requires probability in (0, 1), got {p}"
        )));
    }

    let q = p - 0.5;
    if q.abs() <= 0.425 {
        let r = 0.180_625 - q * q;
        let num = q
            * (((((((2.509_080_928_730_122_7e3 * r + 3.343_054_881_277_247_5e4) * r
                + 6.726_577_092_700_87e4)
                * r
                + 4.592_195_393_154_987e4)
                * r
                + 1.373_169_376_550_946e4)
                * r
                + 1.971_590_950_306_551_4e3)
                * r
                + 1.331_416_678_917_843_8e2)
                * r
                + 3.387_132_872_796_366_5);
        let den = ((((((5.226_495_278_852_854e3 * r + 2.872_908_573_572_194_3e4) * r
            + 3.930_789_580_009_271e4)
            * r
            + 2.121_379_430_158_993_7e4)
            * r
            + 5.394_196_021_424_751e3)
            * r
            + 6.871_870_741_484_02e2)
            * r
            + 4.231_333_070_160_091e1)
            * r
            + 1.0;
        return Ok(num / den);
    }

    let r = if q < 0.0 { p } else { 1.0 - p };
    let r = (-r.ln()).sqrt();

    let val = if r <= 5.0 {
        let r_adj = r - 1.6;
        let num = ((((((7.745_450_142_772_702_5e-4 * r_adj + 2.272_388_412_100_731_05e-2)
            * r_adj
            + 2.417_807_251_774_506_12e-1)
            * r_adj
            + 1.270_458_252_452_368_38)
            * r_adj
            + 3.647_848_324_763_204_60)
            * r_adj
            + 5.769_497_221_460_691_41)
            * r_adj
            + 4.630_337_846_156_545_30)
            * r_adj
            + 1.423_437_110_749_683_58;
        let den = ((((((1.050_750_071_644_416_87e-9 * r_adj + 5.475_938_084_995_344_95e-4)
            * r_adj
            + 1.519_866_656_361_645_71e-2)
            * r_adj
            + 1.481_039_764_274_802_97e-1)
            * r_adj
            + 6.897_673_349_851_000_04e-1)
            * r_adj
            + 1.676_384_830_183_803_84)
            * r_adj
            + 2.053_191_626_637_758_82)
            * r_adj
            + 1.0;
        num / den
    } else {
        let r_adj = r - 5.0;
        let num = ((((((2.010_334_399_292_288_13e-7 * r_adj + 2.711_555_568_743_487_57e-5)
            * r_adj
            + 1.242_660_947_388_078_43e-3)
            * r_adj
            + 2.653_218_952_657_612_30e-2)
            * r_adj
            + 2.965_605_718_285_048_94e-1)
            * r_adj
            + 1.784_826_539_917_291_30)
            * r_adj
            + 5.463_784_911_164_114_37)
            * r_adj
            + 6.657_904_643_501_103_78;
        let den = ((((((2.044_263_106_006_757_53e-15 * r_adj + 1.421_511_758_316_445_88e-7)
            * r_adj
            + 1.846_318_317_510_054_68e-5)
            * r_adj
            + 7.868_691_316_776_138_71e-4)
            * r_adj
            + 1.487_536_129_085_061_49e-2)
            * r_adj
            + 1.369_298_809_227_358_05e-1)
            * r_adj
            + 5.998_322_065_558_879_38e-1)
            * r_adj
            + 1.0;
        num / den
    };

    if q < 0.0 { Ok(-val) } else { Ok(val) }
}

// ---------------------------------------------------------------------------
// Log-Gamma and Lanczos Approximation (fsci-special parity)
// ---------------------------------------------------------------------------

#[allow(clippy::excessive_precision)]
const LANCZOS_COEFFS: [f64; 9] = [
    0.999_999_999_999_809_9,
    676.520_368_121_885_1,
    -1_259.139_216_722_402_8,
    771.323_428_777_653_1,
    -176.615_029_162_140_6,
    12.507_343_278_686_905,
    -0.138_571_095_265_720_12,
    0.000_009_984_369_578_019_572,
    0.000_000_150_563_273_514_931_16,
];

/// Log-gamma function $\ln \Gamma(x)$ for positive real $x$.
pub fn log_gamma(x: f64) -> Result<f64, NumericsError> {
    if !x.is_finite() || x <= 0.0 {
        return Err(NumericsError::DomainError(format!(
            "log_gamma requires positive finite argument, got {x}"
        )));
    }

    // Exact values for x = 1.0 and x = 2.0: ln Gamma(1) = ln Gamma(2) = 0
    if (x - 1.0).abs() < 1e-15 || (x - 2.0).abs() < 1e-15 {
        return Ok(0.0);
    }

    if x < 0.5 {
        // Recurrence: ln Gamma(x) = ln Gamma(x + 1) - ln(x)
        let g1 = log_gamma(x + 1.0)?;
        return Ok(g1 - x.ln());
    }

    let mut coeff_sum = LANCZOS_COEFFS[0];
    for (idx, &coeff) in LANCZOS_COEFFS.iter().enumerate().skip(1) {
        coeff_sum += coeff / (x + (idx as f64 - 1.0));
    }

    let t = x + 6.5;
    let val = 0.5 * (2.0 * PI).ln() + (x - 0.5) * t.ln() - t + coeff_sum.ln();
    Ok(val)
}

/// Natural log of the beta function $\ln B(a, b) = \ln \Gamma(a) + \ln \Gamma(b) - \ln \Gamma(a + b)$.
pub fn ln_beta(a: f64, b: f64) -> Result<f64, NumericsError> {
    if a <= 0.0 || b <= 0.0 || !a.is_finite() || !b.is_finite() {
        return Err(NumericsError::DomainError(format!(
            "ln_beta requires positive finite shape parameters, got a={a}, b={b}"
        )));
    }
    let lga = log_gamma(a)?;
    let lgb = log_gamma(b)?;
    let lgab = log_gamma(a + b)?;
    Ok(lga + lgb - lgab)
}

// ---------------------------------------------------------------------------
// Regularized Incomplete Beta and Inverse (fsci-special / fsci-stats parity)
// ---------------------------------------------------------------------------

/// Continued fraction evaluation for the incomplete beta function (Lentz's method).
fn betacf(a: f64, b: f64, x: f64) -> f64 {
    const MAX_ITERS: usize = 200;
    const EPS: f64 = 3.0e-14;
    const MIN_NUM: f64 = 1.0e-300;

    let qab = a + b;
    let qap = a + 1.0;
    let qam = a - 1.0;
    let mut c = 1.0;
    let mut d = 1.0 - qab * x / qap;
    if d.abs() < MIN_NUM {
        d = MIN_NUM;
    }
    d = 1.0 / d;
    let mut h = d;

    for m in 1..=MAX_ITERS {
        let m_f = m as f64;
        let m2 = 2.0 * m_f;

        let aa = m_f * (b - m_f) * x / ((qam + m2) * (a + m2));
        d = 1.0 + aa * d;
        if d.abs() < MIN_NUM {
            d = MIN_NUM;
        }
        c = 1.0 + aa / c;
        if c.abs() < MIN_NUM {
            c = MIN_NUM;
        }
        d = 1.0 / d;
        h *= d * c;

        let aa2 = -(a + m_f) * (qab + m_f) * x / ((a + m2) * (qap + m2));
        d = 1.0 + aa2 * d;
        if d.abs() < MIN_NUM {
            d = MIN_NUM;
        }
        c = 1.0 + aa2 / c;
        if c.abs() < MIN_NUM {
            c = MIN_NUM;
        }
        d = 1.0 / d;
        let delta = d * c;
        h *= delta;
        if (delta - 1.0).abs() <= EPS {
            break;
        }
    }

    h
}

/// Regularized incomplete beta function $I_x(a, b) \in [0, 1]$.
pub fn betainc(a: f64, b: f64, x: f64) -> Result<f64, NumericsError> {
    if !a.is_finite() || !b.is_finite() || !x.is_finite() {
        return Err(NumericsError::NonFiniteInput);
    }
    if a <= 0.0 || b <= 0.0 {
        return Err(NumericsError::DomainError(format!(
            "betainc requires positive shapes a={a}, b={b}"
        )));
    }
    if !(0.0..=1.0).contains(&x) {
        return Err(NumericsError::DomainError(format!(
            "betainc requires x in [0, 1], got {x}"
        )));
    }
    if x == 0.0 {
        return Ok(0.0);
    }
    if x == 1.0 {
        return Ok(1.0);
    }

    let lb = ln_beta(a, b)?;
    let front = (a * x.ln() + b * (1.0 - x).ln() - lb).exp();

    if x < (a + 1.0) / (a + b + 2.0) {
        let val = front * betacf(a, b, x) / a;
        Ok(val.clamp(0.0, 1.0))
    } else {
        let val = 1.0 - front * betacf(b, a, 1.0 - x) / b;
        Ok(val.clamp(0.0, 1.0))
    }
}

/// Inverse of the regularized incomplete beta function $x$ such that $I_x(a, b) = y$.
pub fn betaincinv(a: f64, b: f64, y: f64) -> Result<f64, NumericsError> {
    if !a.is_finite() || !b.is_finite() || !y.is_finite() {
        return Err(NumericsError::NonFiniteInput);
    }
    if a <= 0.0 || b <= 0.0 {
        return Err(NumericsError::DomainError(format!(
            "betaincinv requires positive shapes a={a}, b={b}"
        )));
    }
    if !(0.0..=1.0).contains(&y) {
        return Err(NumericsError::DomainError(format!(
            "betaincinv requires y in [0, 1], got {y}"
        )));
    }
    if y == 0.0 {
        return Ok(0.0);
    }
    if y == 1.0 {
        return Ok(1.0);
    }

    // Symmetry: I_x(a, b) = 1 - I_{1-x}(b, a)
    if y > 0.5 {
        let inv = betaincinv(b, a, 1.0 - y)?;
        return Ok(1.0 - inv);
    }

    let lb = ln_beta(a, b)?;
    let mut x = (y * a * lb.exp()).powf(1.0 / a);
    if !(x > 0.0 && x < 1.0) {
        x = a / (a + b);
    }

    let mut lo = 0.0_f64;
    let mut hi = 1.0_f64;

    for _ in 0..120 {
        let val = betainc(a, b, x)?;
        let err = val - y;
        if err.abs() <= 1e-15 * y.max(1e-300) {
            break;
        }
        if val < y {
            lo = x;
        } else {
            hi = x;
        }

        // Newton step using Beta PDF derivative
        let pdf = ((a - 1.0) * x.ln() + (b - 1.0) * (1.0 - x).ln() - lb).exp();
        if pdf > 1e-30 {
            let next_x = x - err / pdf;
            if next_x > lo && next_x < hi {
                x = next_x;
                continue;
            }
        }
        // Bisection fallback
        x = 0.5 * (lo + hi);
    }

    Ok(x.clamp(0.0, 1.0))
}

/// Beta distribution representation for statistical evaluation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BetaDist {
    pub a: f64,
    pub b: f64,
}

impl BetaDist {
    pub fn new(a: f64, b: f64) -> Result<Self, NumericsError> {
        if !a.is_finite() || !b.is_finite() || a <= 0.0 || b <= 0.0 {
            return Err(NumericsError::DomainError(format!(
                "Beta distribution requires positive finite parameters a={a}, b={b}"
            )));
        }
        Ok(Self { a, b })
    }

    pub fn cdf(&self, x: f64) -> Result<f64, NumericsError> {
        betainc(self.a, self.b, x)
    }

    pub fn ppf(&self, q: f64) -> Result<f64, NumericsError> {
        betaincinv(self.a, self.b, q)
    }
}

// ---------------------------------------------------------------------------
// Confidence Intervals and Tail Arithmetic
// ---------------------------------------------------------------------------

/// Wilson score confidence interval for a binomial proportion $k / n$.
///
/// Returns a two-sided confidence interval `(lower, upper)`.
pub fn wilson_ci(k: usize, n: usize, confidence: f64) -> Result<(f64, f64), NumericsError> {
    if !confidence.is_finite() || confidence <= 0.0 || confidence >= 1.0 {
        return Err(NumericsError::InvalidConfidence(confidence));
    }
    if n == 0 {
        return Err(NumericsError::EmptyTrials);
    }
    if k > n {
        return Err(NumericsError::InvalidSampleSize { k, n });
    }

    let p = k as f64 / n as f64;
    let n_f = n as f64;
    let alpha = 1.0 - confidence;
    let z = standard_normal_ppf(1.0 - alpha / 2.0)?;
    let z2 = z * z;

    let denom = 1.0 + z2 / n_f;
    let center = (p + z2 / (2.0 * n_f)) / denom;
    let margin = z * (p * (1.0 - p) / n_f + z2 / (4.0 * n_f * n_f)).sqrt() / denom;

    Ok(((center - margin).max(0.0), (center + margin).min(1.0)))
}

/// Exact Clopper-Pearson two-sided confidence interval for a binomial proportion $k / n$.
pub fn clopper_pearson_ci(
    k: usize,
    n: usize,
    confidence: f64,
) -> Result<(f64, f64), NumericsError> {
    if !confidence.is_finite() || confidence <= 0.0 || confidence >= 1.0 {
        return Err(NumericsError::InvalidConfidence(confidence));
    }
    if n == 0 {
        return Err(NumericsError::EmptyTrials);
    }
    if k > n {
        return Err(NumericsError::InvalidSampleSize { k, n });
    }

    let alpha = 1.0 - confidence;
    let n_f = n as f64;
    let k_f = k as f64;

    let lower = if k == 0 {
        0.0
    } else {
        betaincinv(k_f, n_f - k_f + 1.0, alpha / 2.0)?
    };

    let upper = if k == n {
        1.0
    } else {
        betaincinv(k_f + 1.0, n_f - k_f, 1.0 - alpha / 2.0)?
    };

    Ok((lower, upper))
}

/// Stable closed-form upper confidence bound for zero observed events ($k = 0$ in $n$ trials).
///
/// Computes $- \text{expm1}(\ln(1 - \text{confidence}) / n) = 1 - (1 - \text{confidence})^{1/n}$.
/// For 95% confidence, this evaluates $- \text{expm1}(\ln(0.05) / n)$.
pub fn zero_event_upper_bound(n: usize, confidence: f64) -> Result<f64, NumericsError> {
    if !confidence.is_finite() || confidence <= 0.0 || confidence >= 1.0 {
        return Err(NumericsError::InvalidConfidence(confidence));
    }
    if n == 0 {
        return Err(NumericsError::EmptyTrials);
    }

    let alpha = 1.0 - confidence;
    let bound = -(alpha.ln() / n as f64).exp_m1();
    Ok(bound.clamp(0.0, 1.0))
}

/// Exact Clopper-Pearson one-sided upper confidence bound at level `confidence`.
///
/// When $k = 0$: evaluated via the stable closed form `-expm1(log(1 - confidence)/n)`.
/// When $k = n$: 1.0.
/// When $0 < k < n$: `Beta(k + 1, n - k).ppf(confidence)`.
pub fn clopper_pearson_one_sided_upper(
    k: usize,
    n: usize,
    confidence: f64,
) -> Result<f64, NumericsError> {
    if !confidence.is_finite() || confidence <= 0.0 || confidence >= 1.0 {
        return Err(NumericsError::InvalidConfidence(confidence));
    }
    if n == 0 {
        return Err(NumericsError::EmptyTrials);
    }
    if k > n {
        return Err(NumericsError::InvalidSampleSize { k, n });
    }

    if k == 0 {
        return zero_event_upper_bound(n, confidence);
    }
    if k == n {
        return Ok(1.0);
    }

    let beta = BetaDist::new((k + 1) as f64, (n - k) as f64)?;
    let upper = beta.ppf(confidence)?;
    Ok(upper.clamp(0.0, 1.0))
}

// ---------------------------------------------------------------------------
// Stable Log-Sum-Exp
// ---------------------------------------------------------------------------

/// Stable log-sum-exp over a slice of floating point values: $\ln \sum_i \exp(x_i)$.
///
/// Uses max-subtraction to avoid catastrophic overflow or loss of precision.
/// Correctly handles empty slices ($-\infty$), all $-\infty$ ($-\infty$), $+\infty$ ($+\infty$),
/// and NaN propagation.
pub fn logsumexp(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return f64::NEG_INFINITY;
    }

    let mut max_val = f64::NEG_INFINITY;
    let mut has_pos_inf = false;

    for &x in xs {
        if x.is_nan() {
            return f64::NAN;
        }
        if x == f64::INFINITY {
            has_pos_inf = true;
        }
        if x > max_val {
            max_val = x;
        }
    }

    if has_pos_inf {
        return f64::INFINITY;
    }
    if max_val == f64::NEG_INFINITY {
        return f64::NEG_INFINITY;
    }

    let mut sum_exp = 0.0_f64;
    for &x in xs {
        if x != f64::NEG_INFINITY {
            sum_exp += (x - max_val).exp();
        }
    }

    max_val + sum_exp.ln()
}
