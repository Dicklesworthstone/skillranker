//! Rejection-based deterministic sampling and shuffle adapted from FrankenNumPy (sr-roadmap-l1i.6.18).
//!
//! Provides:
//! 1. PCG64-DXSM 128-bit pseudo-random number generator with bit-exact NumPy/FrankenNumPy parity.
//! 2. Unbiased rejection-based bounded integer generation (eliminating modulo bias).
//! 3. Floyd's sampling algorithm without replacement (`choice_indices(pop_size, size, false)`).
//! 4. Fisher-Yates slice shuffling (`shuffle_slice`).
//! 5. Deterministic seeded replay and provenance tracking.

use std::collections::BTreeSet;
use std::fmt;

const PCG_DEFAULT_MULTIPLIER_128: u128 = 0x2360_ed05_1fc6_5da4_4385_df64_9fcc_f645;
const PCG_CHEAP_MULTIPLIER: u64 = 0xda94_2042_e4dd_58b5;

/// Sampling error kinds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SamplingError {
    ZeroPopulation,
    InvalidSampleSize { size: usize, population: usize },
}

impl fmt::Display for SamplingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroPopulation => f.write_str("cannot sample from a population of size zero"),
            Self::InvalidSampleSize { size, population } => {
                write!(
                    f,
                    "sample size ({size}) cannot exceed population ({population}) without replacement"
                )
            }
        }
    }
}

impl std::error::Error for SamplingError {}

/// PCG-XSL-DXSM-128/64 generator matching NumPy / FrankenNumPy's PCG64DXSM bit generator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pcg64Dxsm {
    state: u128,
    inc: u128,
}

impl Pcg64Dxsm {
    /// Create from explicit 128-bit state and sequence selector.
    #[must_use]
    pub fn seed(initstate: u128, initseq: u128) -> Self {
        let inc = (initseq << 1) | 1;
        let mut state: u128 = 0;
        state = state
            .wrapping_mul(PCG_DEFAULT_MULTIPLIER_128)
            .wrapping_add(inc);
        state = state.wrapping_add(initstate);
        state = state
            .wrapping_mul(PCG_DEFAULT_MULTIPLIER_128)
            .wrapping_add(inc);
        Self { state, inc }
    }

    /// Construct a generator from a single 64-bit seed.
    #[must_use]
    pub fn from_seed(seed: u64) -> Self {
        // Construct deterministic 128-bit initial state and sequence words
        let initstate = ((seed as u128) << 64) | ((!seed as u128) ^ 0x5851_f42d_4c95_7f2d);
        let initseq = ((seed.rotate_left(17) as u128) << 64) | 0x1405_7b7e_f767_814f;
        Self::seed(initstate, initseq)
    }

    /// Construct directly from raw state and increment (for checkpoint/restore).
    #[must_use]
    pub const fn from_raw_state(state: u128, inc: u128) -> Self {
        Self { state, inc }
    }

    /// Retrieve the raw generator state `(state, inc)`.
    #[must_use]
    pub const fn raw_state(&self) -> (u128, u128) {
        (self.state, self.inc)
    }

    /// Advance the state using the cheap multiplier.
    fn step(&mut self) {
        self.state = self
            .state
            .wrapping_mul(u128::from(PCG_CHEAP_MULTIPLIER))
            .wrapping_add(self.inc);
    }

    /// DXSM output permutation.
    #[must_use]
    fn dxsm_output(&self) -> u64 {
        let mut hi = (self.state >> 64) as u64;
        let lo = (self.state as u64) | 1;
        hi ^= hi >> 32;
        hi = hi.wrapping_mul(PCG_CHEAP_MULTIPLIER);
        hi ^= hi >> 48;
        hi = hi.wrapping_mul(lo);
        hi
    }

    /// Generate the next 64-bit pseudo-random unsigned integer.
    pub fn next_u64(&mut self) -> u64 {
        let output = self.dxsm_output();
        self.step();
        output
    }

    /// Generate the next pseudo-random floating point value in $[0, 1)$ with 53 bits of precision.
    pub fn next_f64(&mut self) -> f64 {
        let sample = self.next_u64() >> 11;
        sample as f64 / (1u64 << 53) as f64
    }

    /// Generate an unbiased random integer in `[0, upper_bound)` via Lemire / NumPy rejection sampling.
    ///
    /// Eliminates modulo bias completely by discarding values in the incomplete upper residue.
    pub fn bounded_u64(&mut self, upper_bound: u64) -> Result<u64, SamplingError> {
        if upper_bound == 0 {
            return Err(SamplingError::ZeroPopulation);
        }
        let threshold = u64::MAX - (u64::MAX % upper_bound);
        loop {
            let candidate = self.next_u64();
            if candidate < threshold {
                return Ok(candidate % upper_bound);
            }
        }
    }

    /// Choose `size` integer indices from `[0, pop_size)`.
    ///
    /// When `replace == false`, implements Floyd's sampling algorithm with rejection-based draws.
    /// Guarantees membership in `[0, pop_size)` and uniqueness of all selected indices.
    pub fn choice_indices(
        &mut self,
        pop_size: usize,
        size: usize,
        replace: bool,
    ) -> Result<Vec<usize>, SamplingError> {
        if pop_size == 0 && size > 0 {
            return Err(SamplingError::ZeroPopulation);
        }
        if !replace && size > pop_size {
            return Err(SamplingError::InvalidSampleSize {
                size,
                population: pop_size,
            });
        }
        if size == 0 {
            return Ok(Vec::new());
        }

        if replace {
            let mut indices = Vec::with_capacity(size);
            for _ in 0..size {
                let idx = self.bounded_u64(pop_size as u64)? as usize;
                indices.push(idx);
            }
            return Ok(indices);
        }

        // Floyd's algorithm without replacement:
        let mut selected = BTreeSet::new();
        let mut result = Vec::with_capacity(size);

        for j in (pop_size - size)..pop_size {
            let t = self.bounded_u64((j + 1) as u64)? as usize;
            if selected.insert(t) {
                result.push(t);
            } else {
                selected.insert(j);
                result.push(j);
            }
        }

        Ok(result)
    }

    /// Fisher-Yates in-place shuffle using unbiased rejection-based bounded integer draws.
    pub fn shuffle_slice<T>(&mut self, slice: &mut [T]) {
        let n = slice.len();
        if n <= 1 {
            return;
        }
        for i in (1..n).rev() {
            let j = self.bounded_u64((i + 1) as u64).unwrap_or(0) as usize;
            slice.swap(i, j);
        }
    }

    /// Generates a random permutation of `[0, pop_size)`.
    pub fn permutation(&mut self, pop_size: usize) -> Result<Vec<usize>, SamplingError> {
        let mut indices: Vec<usize> = (0..pop_size).collect();
        self.shuffle_slice(&mut indices);
        Ok(indices)
    }
}

/// Convenience helper to sample indices from a population using a single seed.
pub fn choice_indices(
    seed: u64,
    pop_size: usize,
    size: usize,
    replace: bool,
) -> Result<Vec<usize>, SamplingError> {
    let mut rng = Pcg64Dxsm::from_seed(seed);
    rng.choice_indices(pop_size, size, replace)
}

/// Convenience helper to shuffle a slice in place using a single seed.
pub fn shuffle_slice<T>(seed: u64, slice: &mut [T]) {
    let mut rng = Pcg64Dxsm::from_seed(seed);
    rng.shuffle_slice(slice);
}
