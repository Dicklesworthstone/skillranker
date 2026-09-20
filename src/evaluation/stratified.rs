//! Stratified family sampling and randomization provenance (P5, sr-roadmap-l1i.6.21).
//!
//! Provides:
//! 1. Outcome-independent family representative selection and deduplication within splits.
//! 2. Split isolation verification preventing cross-split family contamination.
//! 3. Observable stratum classification (retrieval profile, input quality, unknown metadata,
//!    with gate-abstained and operational-failure cases preserved in the frame).
//! 4. Stratum allocation methods (proportional, equal, explicit, full census) with stratum floor enforcement.
//! 5. Audit-grade randomization provenance (trusted OS randomness, manual diagnostic seed,
//!    imported frozen manifest replay, census) and design status tracking.
//! 6. Exact design inclusion probabilities (\pi_i = n_h / N_h) for probability samples,
//!    refusing to fabricate design-based claims for unverified or diagnostic manual seeds.
//! 7. Frozen sample manifest creation, BLAKE3 frame digests, deterministic replay,
//!    and frame change detection.

use crate::evaluation::sampling::{Pcg64Dxsm, SAMPLING_VERSION};
use crate::evaluation::{CaseKey, EvaluationCaseRecord, EvaluationError, EvaluationSplit};
use crate::output::SCHEMA_VERSION;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Rule for selecting a single representative case per task family,
/// strictly independent of evaluated outcomes, decisions, or labels.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FamilyRepresentativeRule {
    /// Select the case with lexicographically smallest `(case_id, replicate)`.
    FirstByCaseIdReplicate,
    /// Select the case with lowest replicate number, breaking ties by case_id.
    LowestReplicateFirst,
}

/// Retrieval profile of an evaluation case candidate set.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RetrievalProfile {
    Normal,
    Overflow,
    Unknown,
}

/// Input quality indicator of an evaluation case.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputQuality {
    Complete,
    Degraded,
    Unknown,
}

/// Observable stratum formed before observing outcomes or labels.
///
/// Invariant: Gate-abstained and operationally failed cases remain in the frame
/// and in their respective observable strata rather than being dropped or
/// grouped retrospectively.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct ObservableStratum {
    pub retrieval: RetrievalProfile,
    pub input_quality: InputQuality,
}

impl ObservableStratum {
    /// Classify an evaluation case record into its observable stratum.
    #[must_use]
    pub fn classify(case: &EvaluationCaseRecord) -> Self {
        let retrieval = if case.roster_skills.is_empty() && case.operational_failure {
            RetrievalProfile::Unknown
        } else if case.roster_skills.len() > 254 {
            RetrievalProfile::Overflow
        } else {
            RetrievalProfile::Normal
        };

        let input_quality = match &case.prompt_summary {
            Some(p) if !p.trim().is_empty() => InputQuality::Complete,
            Some(_) => InputQuality::Degraded,
            None => {
                if case.operational_failure && case.roster_skills.is_empty() {
                    InputQuality::Unknown
                } else {
                    InputQuality::Degraded
                }
            }
        };

        Self {
            retrieval,
            input_quality,
        }
    }

    /// Canonical string identifier for this stratum (e.g. "normal:complete").
    #[must_use]
    pub fn key(&self) -> String {
        format!("{}:{}", self.retrieval_str(), self.input_quality_str())
    }

    #[must_use]
    pub const fn retrieval_str(&self) -> &'static str {
        match self.retrieval {
            RetrievalProfile::Normal => "normal",
            RetrievalProfile::Overflow => "overflow",
            RetrievalProfile::Unknown => "unknown",
        }
    }

    #[must_use]
    pub const fn input_quality_str(&self) -> &'static str {
        match self.input_quality {
            InputQuality::Complete => "complete",
            InputQuality::Degraded => "degraded",
            InputQuality::Unknown => "unknown",
        }
    }
}

/// Select a single representative case per family for the given split.
///
/// Enforces split isolation: if any family appears across multiple splits,
/// a `SplitContamination` error is returned.
pub fn select_family_representatives(
    cases: &[EvaluationCaseRecord],
    split: EvaluationSplit,
    rule: FamilyRepresentativeRule,
) -> Result<Vec<EvaluationCaseRecord>, EvaluationError> {
    // 1. Verify split isolation across all cases in the frame
    let mut family_to_split: BTreeMap<&str, EvaluationSplit> = BTreeMap::new();
    for case in cases {
        let fam = case.key.family_id.as_str();
        if let Some(&existing_split) = family_to_split.get(fam) {
            if existing_split != case.split {
                return Err(EvaluationError::SplitContamination(format!(
                    "family '{fam}' appears in multiple splits: {existing_split} and {}",
                    case.split
                )));
            }
        } else {
            family_to_split.insert(fam, case.split);
        }
    }

    // 2. Filter cases for the requested split and group by family_id
    let mut family_groups: BTreeMap<&str, Vec<&EvaluationCaseRecord>> = BTreeMap::new();
    for case in cases {
        if case.split == split {
            family_groups
                .entry(case.key.family_id.as_str())
                .or_default()
                .push(case);
        }
    }

    // 3. For each family, pick the single representative by the outcome-independent rule
    let mut representatives = Vec::with_capacity(family_groups.len());
    for (_fam, mut group) in family_groups {
        match rule {
            FamilyRepresentativeRule::FirstByCaseIdReplicate => {
                group.sort_by(|a, b| {
                    a.key
                        .case_id
                        .cmp(&b.key.case_id)
                        .then_with(|| a.key.replicate.cmp(&b.key.replicate))
                });
            }
            FamilyRepresentativeRule::LowestReplicateFirst => {
                group.sort_by(|a, b| {
                    a.key
                        .replicate
                        .cmp(&b.key.replicate)
                        .then_with(|| a.key.case_id.cmp(&b.key.case_id))
                });
            }
        }
        if let Some(&rep) = group.first() {
            representatives.push(rep.clone());
        }
    }

    // Sort representatives canonically by family_id
    representatives.sort_by(|a, b| a.key.family_id.cmp(&b.key.family_id));
    Ok(representatives)
}

/// Compute a canonical BLAKE3 frame digest over evaluation cases.
///
/// Any change in case keys, splits, or observable metadata yields a changed digest,
/// preventing silent replay against modified evaluation frames.
#[must_use]
pub fn compute_frame_digest(cases: &[EvaluationCaseRecord]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"skillranker:evaluation:frame:v1\n");

    let mut sorted_keys: Vec<(&CaseKey, EvaluationSplit, usize, bool, bool)> = cases
        .iter()
        .map(|c| {
            (
                &c.key,
                c.split,
                c.roster_skills.len(),
                c.prompt_summary.is_some(),
                c.operational_failure,
            )
        })
        .collect();
    sorted_keys.sort_by(|a, b| a.0.cmp(b.0));

    hasher.update(&(sorted_keys.len() as u64).to_le_bytes());
    for (key, split, roster_len, has_prompt, op_fail) in sorted_keys {
        hasher.update(key.frame_id.as_bytes());
        hasher.update(b":");
        hasher.update(key.family_id.as_bytes());
        hasher.update(b":");
        hasher.update(key.case_id.as_bytes());
        hasher.update(b":");
        hasher.update(&key.replicate.to_le_bytes());
        hasher.update(b":");
        hasher.update(key.policy_id.as_bytes());
        hasher.update(b":");
        hasher.update(split.to_string().as_bytes());
        hasher.update(b":");
        hasher.update(&(roster_len as u64).to_le_bytes());
        hasher.update(b":");
        hasher.update(&[if has_prompt { 1 } else { 0 }]);
        hasher.update(b":");
        hasher.update(&[if op_fail { 1 } else { 0 }]);
        hasher.update(b"\n");
    }
    hasher.finalize().to_hex().to_string()
}

/// Partition cases into observable strata before observing outcomes or labels.
#[must_use]
pub fn partition_strata(
    representatives: &[EvaluationCaseRecord],
) -> BTreeMap<String, Vec<EvaluationCaseRecord>> {
    let mut strata: BTreeMap<String, Vec<EvaluationCaseRecord>> = BTreeMap::new();
    for case in representatives {
        let stratum_key = ObservableStratum::classify(case).key();
        strata.entry(stratum_key).or_default().push(case.clone());
    }
    for cases in strata.values_mut() {
        cases.sort_by(|a, b| {
            a.key
                .family_id
                .cmp(&b.key.family_id)
                .then_with(|| a.key.case_id.cmp(&b.key.case_id))
                .then_with(|| a.key.replicate.cmp(&b.key.replicate))
        });
    }
    strata
}

/// Strategy for allocating a total sample size across observable strata.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum AllocationMethod {
    /// Proportional to stratum population size N_h / N, respecting stratum floors.
    Proportional { min_floor: usize },
    /// Equal allocation per stratum (n / H), clamped to N_h.
    EqualPerStratum { min_floor: usize },
    /// Explicit target allocations per stratum key.
    Explicit {
        allocations: BTreeMap<String, usize>,
    },
}

/// Allocate sample sizes n_h across represented strata.
///
/// The target is the requested size capped by the total population. Explicit
/// allocations must name exactly the represented strata and sum to that target.
/// Proportional allocation fixes binding population-capped floors, then uses
/// largest remainders for the remaining proportional quotas. Ties use stratum
/// keys. Equal allocation fills a common level with population caps.
pub fn allocate_sample_sizes(
    strata_sizes: &BTreeMap<String, usize>,
    total_sample_size: usize,
    method: &AllocationMethod,
) -> Result<BTreeMap<String, usize>, EvaluationError> {
    let invalid = |message: &str| EvaluationError::InvalidStratumAllocation(message.into());
    if strata_sizes.is_empty() || strata_sizes.values().any(|&size| size == 0) {
        return Err(invalid("represented strata must have positive populations"));
    }
    let total_population = strata_sizes.values().try_fold(0usize, |sum, size| {
        sum.checked_add(*size)
            .ok_or_else(|| invalid("total stratum population overflows usize"))
    })?;
    let target = total_sample_size.min(total_population);

    // Explicit allocations are an exact design, even when the budget permits a
    // census. Never silently replace a malformed design with a different one.
    if let AllocationMethod::Explicit { allocations } = method {
        if !allocations.keys().eq(strata_sizes.keys()) {
            return Err(invalid(
                "explicit allocation keys must match represented strata",
            ));
        }
        let mut total = 0usize;
        for (key, &size) in allocations {
            if size == 0 || size > strata_sizes[key] {
                return Err(invalid(
                    "explicit allocations must be positive and within population",
                ));
            }
            total = total
                .checked_add(size)
                .ok_or_else(|| invalid("total explicit allocation overflows usize"))?;
        }
        if total != target {
            return Err(invalid(
                "explicit allocations must sum to the population-capped sample budget",
            ));
        }
        return Ok(allocations.clone());
    }
    let floor = match method {
        AllocationMethod::Proportional { min_floor }
        | AllocationMethod::EqualPerStratum { min_floor } => (*min_floor).max(1),
        AllocationMethod::Explicit { .. } => unreachable!("handled above"),
    };
    // This sum cannot exceed the already checked total population.
    let required_floor: usize = strata_sizes.values().map(|&pop| pop.min(floor)).sum();
    if target < required_floor {
        return Err(EvaluationError::InsufficientSampleBudgetForStrata {
            budget: total_sample_size,
            required_floor,
        });
    }
    if target == total_population {
        return Ok(strata_sizes.clone());
    }

    if matches!(method, AllocationMethod::EqualPerStratum { .. }) {
        // Find the largest common level that fits, with small strata capped at
        // their population. Binary search avoids a loop per sampled family.
        let mut low = floor.min(*strata_sizes.values().max().expect("nonempty"));
        let mut high = *strata_sizes.values().max().expect("nonempty");
        while low < high {
            let gap = high - low;
            let mid = low + gap / 2 + gap % 2;
            let count: usize = strata_sizes.values().map(|&pop| pop.min(mid)).sum();
            if count <= target {
                low = mid;
            } else {
                high = mid - 1;
            }
        }
        let mut allocated: BTreeMap<String, usize> = strata_sizes
            .iter()
            .map(|(key, &pop)| (key.clone(), pop.min(low)))
            .collect();
        let mut remaining = target - allocated.values().sum::<usize>();
        for (key, count) in &mut allocated {
            if remaining > 0 && *count < strata_sizes[key] {
                *count += 1;
                remaining -= 1;
            }
        }
        return Ok(allocated);
    }

    // Constrained proportional apportionment: fix strata whose proportional
    // quota falls below their capped floor, then recompute quotas for the rest.
    // Products of two usize values fit u128 on supported 32/64-bit platforms.
    let mut allocated = BTreeMap::new();
    let mut active = strata_sizes.clone();
    let mut remaining = target;
    let mut active_population = total_population;
    loop {
        let constrained: Vec<String> = active
            .iter()
            .filter(|(_, pop)| {
                (remaining as u128) * (**pop as u128)
                    < (floor.min(**pop) as u128) * (active_population as u128)
            })
            .map(|(key, _)| key.clone())
            .collect();
        if constrained.is_empty() {
            break;
        }
        for key in constrained {
            let pop = active.remove(&key).expect("active stratum");
            let count = floor.min(pop);
            allocated.insert(key, count);
            remaining -= count;
            active_population -= pop;
        }
    }
    // Hamilton largest-remainder allocation within the unconstrained strata.
    // Exact integer remainders avoid floating point and key-dependent rounding.
    let mut remainders = Vec::with_capacity(active.len());
    let mut assigned = 0usize;
    for (key, pop) in active {
        let numerator = (remaining as u128) * (pop as u128);
        let count = (numerator / active_population as u128) as usize;
        remainders.push((key.clone(), numerator % active_population as u128));
        allocated.insert(key, count);
        assigned += count;
    }
    remainders.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    for (key, _) in remainders.into_iter().take(remaining - assigned) {
        *allocated.get_mut(&key).expect("allocated active stratum") += 1;
    }
    Ok(allocated)
}

/// Audit-grade provenance describing how sampling randomness was obtained.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "kebab-case")]
pub enum RandomizationProvenance {
    /// Fresh seed drawn from trusted OS randomness before selection.
    OsRandom { entropy_source: String, seed: u64 },
    /// Seed supplied manually via command line or config for deterministic diagnostics.
    SuppliedManual { seed: u64 },
    /// Replaying a matching, previously frozen manifest imported from an artifact.
    SuppliedImported { seed: u64, manifest_digest: String },
    /// Complete enumeration of the entire finite frame; no sampling RNG needed.
    Census,
}

/// Inferential status of the sampling design.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DesignStatus {
    /// True probability sample with verified design inclusion probabilities pi_i = n_h / N_h.
    StratifiedProbabilitySample,
    /// Diagnostic fixed selection; inclusion probabilities are not asserted as design-based coverage.
    DiagnosticFixed,
    /// Complete enumeration of the entire finite frame (n = N), exact finite-frame mean.
    FullCensus,
}

/// Declared estimand weighting.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EstimandWeighting {
    /// Each task family is treated as an equal unit (v1 default).
    FamilyWeighted,
    /// Traffic-weighted estimation.
    TrafficWeighted,
}

/// Allocation and weighting parameters for a single observable stratum.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StratumAllocation {
    pub stratum_key: String,
    pub population_size: usize,
    pub sample_size: usize,
    pub weight: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inclusion_probability: Option<f64>,
}

/// Entry for a sampled case in the frozen manifest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SampledCaseEntry {
    pub case_key: CaseKey,
    pub stratum_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inclusion_probability: Option<f64>,
    pub sample_order: usize,
}

/// Immutable frozen manifest documenting a stratified evaluation sample.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FrozenSampleManifest {
    pub schema_version: u64,
    pub manifest_id: String,
    pub frame_digest: String,
    pub split: EvaluationSplit,
    pub design_status: DesignStatus,
    pub randomization_provenance: RandomizationProvenance,
    pub sampling_algorithm_version: String,
    pub estimand_weighting: EstimandWeighting,
    pub total_frame_families: usize,
    pub total_sampled_families: usize,
    pub strata: BTreeMap<String, StratumAllocation>,
    pub selected_cases: Vec<SampledCaseEntry>,
    pub policy_id: String,
    pub created_at_unix_ms: u64,
}

impl FrozenSampleManifest {
    /// Compute a canonical BLAKE3 content digest over the manifest.
    #[must_use]
    pub fn compute_manifest_digest(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"skillranker:sample_manifest:v1\n");
        hasher.update(self.frame_digest.as_bytes());
        hasher.update(b":");
        hasher.update(self.split.to_string().as_bytes());
        hasher.update(b":");
        hasher.update(&(self.total_frame_families as u64).to_le_bytes());
        hasher.update(b":");
        hasher.update(&(self.total_sampled_families as u64).to_le_bytes());
        hasher.update(b":");
        hasher.update(self.policy_id.as_bytes());
        for entry in &self.selected_cases {
            hasher.update(entry.case_key.family_id.as_bytes());
            hasher.update(b":");
            hasher.update(entry.case_key.case_id.as_bytes());
            hasher.update(b":");
            hasher.update(&entry.case_key.replicate.to_le_bytes());
            hasher.update(b":");
            hasher.update(entry.stratum_key.as_bytes());
            hasher.update(b"\n");
        }
        hasher.finalize().to_hex().to_string()
    }
}

/// Draw a fresh 64-bit seed from trusted OS randomness (/dev/urandom).
pub fn draw_os_seed() -> Result<u64, EvaluationError> {
    use std::io::Read;
    let mut file = std::fs::File::open("/dev/urandom").map_err(|e| {
        EvaluationError::SamplingFailure(format!("failed to open /dev/urandom: {e}"))
    })?;
    let mut buf = [0u8; 8];
    file.read_exact(&mut buf).map_err(|e| {
        EvaluationError::SamplingFailure(format!("failed to read OS randomness: {e}"))
    })?;
    Ok(u64::from_le_bytes(buf))
}

/// Draw a stratified sample of family representatives, freezing the resulting manifest.
pub fn draw_stratified_sample(
    representatives: &[EvaluationCaseRecord],
    split: EvaluationSplit,
    total_sample_size: usize,
    allocation_method: &AllocationMethod,
    provenance: RandomizationProvenance,
    policy_id: &str,
    created_at_unix_ms: u64,
) -> Result<FrozenSampleManifest, EvaluationError> {
    if representatives.is_empty() {
        return Err(EvaluationError::SamplingFailure(
            "cannot sample from an empty representative set".into(),
        ));
    }

    let frame_digest = compute_frame_digest(representatives);
    let total_frame_families = representatives.len();

    let strata_cases = partition_strata(representatives);
    let strata_sizes: BTreeMap<String, usize> = strata_cases
        .iter()
        .map(|(k, v)| (k.clone(), v.len()))
        .collect();

    let allocations = allocate_sample_sizes(&strata_sizes, total_sample_size, allocation_method)?;

    let is_full_census = allocations
        .iter()
        .all(|(k, &alloc)| alloc == strata_sizes[k]);

    let design_status = if is_full_census {
        DesignStatus::FullCensus
    } else {
        match provenance {
            RandomizationProvenance::OsRandom { .. } => DesignStatus::StratifiedProbabilitySample,
            RandomizationProvenance::SuppliedManual { .. } => DesignStatus::DiagnosticFixed,
            RandomizationProvenance::SuppliedImported { .. } => DesignStatus::DiagnosticFixed,
            RandomizationProvenance::Census => DesignStatus::FullCensus,
        }
    };

    let seed = match &provenance {
        RandomizationProvenance::OsRandom { seed, .. }
        | RandomizationProvenance::SuppliedManual { seed }
        | RandomizationProvenance::SuppliedImported { seed, .. } => *seed,
        RandomizationProvenance::Census => 0,
    };

    let mut rng = if design_status != DesignStatus::FullCensus {
        Some(Pcg64Dxsm::from_seed(seed))
    } else {
        None
    };

    let mut manifest_strata: BTreeMap<String, StratumAllocation> = BTreeMap::new();
    let mut selected_cases: Vec<SampledCaseEntry> = Vec::new();

    for (stratum_key, cases) in strata_cases {
        let pop_size = cases.len();
        let alloc_size = allocations.get(&stratum_key).copied().unwrap_or(0);
        let weight = (pop_size as f64) / (total_frame_families as f64);

        let inclusion_probability = match design_status {
            DesignStatus::StratifiedProbabilitySample | DesignStatus::FullCensus => {
                Some((alloc_size as f64) / (pop_size as f64))
            }
            DesignStatus::DiagnosticFixed => None,
        };

        manifest_strata.insert(
            stratum_key.clone(),
            StratumAllocation {
                stratum_key: stratum_key.clone(),
                population_size: pop_size,
                sample_size: alloc_size,
                weight,
                inclusion_probability,
            },
        );

        if alloc_size == pop_size {
            for case in cases {
                selected_cases.push(SampledCaseEntry {
                    case_key: case.key.clone(),
                    stratum_key: stratum_key.clone(),
                    inclusion_probability,
                    sample_order: selected_cases.len() + 1,
                });
            }
        } else if let Some(ref mut generator) = rng {
            let sampled_indices = generator
                .choice_indices(pop_size, alloc_size, false)
                .map_err(|e| EvaluationError::SamplingFailure(e.to_string()))?;
            for idx in sampled_indices {
                let case = &cases[idx];
                selected_cases.push(SampledCaseEntry {
                    case_key: case.key.clone(),
                    stratum_key: stratum_key.clone(),
                    inclusion_probability,
                    sample_order: selected_cases.len() + 1,
                });
            }
        }
    }

    let total_sampled_families = selected_cases.len();

    let mut manifest = FrozenSampleManifest {
        schema_version: SCHEMA_VERSION,
        manifest_id: String::new(),
        frame_digest,
        split,
        design_status,
        randomization_provenance: provenance,
        sampling_algorithm_version: SAMPLING_VERSION.to_string(),
        estimand_weighting: EstimandWeighting::FamilyWeighted,
        total_frame_families,
        total_sampled_families,
        strata: manifest_strata,
        selected_cases,
        policy_id: policy_id.to_string(),
        created_at_unix_ms,
    };

    let digest = manifest.compute_manifest_digest();
    manifest.manifest_id = format!("man-{}", &digest[..16]);
    Ok(manifest)
}

/// Verify an existing frozen sample manifest against an evaluation frame.
///
/// Invariants verified:
/// 1. Frame digest matches exactly.
/// 2. Split matches exactly.
/// 3. Every selected case exists in the frame and classifies into the declared stratum.
/// 4. No duplicate family IDs among selected cases.
/// 5. Stratum sample sizes match declared counts.
pub fn verify_manifest_against_frame(
    manifest: &FrozenSampleManifest,
    cases: &[EvaluationCaseRecord],
) -> Result<(), EvaluationError> {
    let reps = select_family_representatives(
        cases,
        manifest.split,
        FamilyRepresentativeRule::FirstByCaseIdReplicate,
    )?;

    let actual_frame_digest = compute_frame_digest(&reps);
    if actual_frame_digest != manifest.frame_digest {
        return Err(EvaluationError::FrameDigestMismatch {
            expected: manifest.frame_digest.clone(),
            actual: actual_frame_digest,
        });
    }

    if manifest.total_frame_families != reps.len() {
        return Err(EvaluationError::ManifestVerificationFailure(format!(
            "manifest frame family count ({}) does not match frame ({})",
            manifest.total_frame_families,
            reps.len()
        )));
    }

    if manifest.total_sampled_families != manifest.selected_cases.len() {
        return Err(EvaluationError::ManifestVerificationFailure(format!(
            "manifest total sampled count ({}) does not match entries ({})",
            manifest.total_sampled_families,
            manifest.selected_cases.len()
        )));
    }

    let rep_lookup: BTreeMap<(&str, &str), &EvaluationCaseRecord> = reps
        .iter()
        .map(|c| ((c.key.family_id.as_str(), c.key.case_id.as_str()), c))
        .collect();

    let mut seen_families: BTreeSet<&str> = BTreeSet::new();
    let mut observed_strata_counts: BTreeMap<&str, usize> = BTreeMap::new();

    for entry in &manifest.selected_cases {
        let fam = entry.case_key.family_id.as_str();
        if !seen_families.insert(fam) {
            return Err(EvaluationError::ManifestVerificationFailure(format!(
                "duplicate family '{fam}' selected in manifest"
            )));
        }

        let Some(case) = rep_lookup.get(&(fam, entry.case_key.case_id.as_str())) else {
            return Err(EvaluationError::ManifestVerificationFailure(format!(
                "selected case '{}:{}' not found in frame representatives",
                fam, entry.case_key.case_id
            )));
        };

        let classified_stratum = ObservableStratum::classify(case).key();
        if classified_stratum != entry.stratum_key {
            return Err(EvaluationError::ManifestVerificationFailure(format!(
                "selected case '{}:{}' stratum mismatch: declared '{}', classified '{}'",
                fam, entry.case_key.case_id, entry.stratum_key, classified_stratum
            )));
        }

        *observed_strata_counts
            .entry(entry.stratum_key.as_str())
            .or_default() += 1;
    }

    for (stratum_key, alloc) in &manifest.strata {
        let actual_count = observed_strata_counts
            .get(stratum_key.as_str())
            .copied()
            .unwrap_or(0);
        if actual_count != alloc.sample_size {
            return Err(EvaluationError::ManifestVerificationFailure(format!(
                "stratum '{stratum_key}' count mismatch: expected {}, actual {actual_count}",
                alloc.sample_size
            )));
        }
    }

    Ok(())
}

/// Replay a frozen manifest against an evaluation frame without generating a new independent sample.
///
/// Replaying an existing frozen manifest preserves that original draw and counts as
/// no new independent sample; imported provenance is verified rather than inferred.
pub fn replay_manifest_sample(
    manifest: &FrozenSampleManifest,
    cases: &[EvaluationCaseRecord],
) -> Result<Vec<EvaluationCaseRecord>, EvaluationError> {
    verify_manifest_against_frame(manifest, cases)?;

    let reps = select_family_representatives(
        cases,
        manifest.split,
        FamilyRepresentativeRule::FirstByCaseIdReplicate,
    )?;

    let rep_lookup: BTreeMap<(&str, &str), &EvaluationCaseRecord> = reps
        .iter()
        .map(|c| ((c.key.family_id.as_str(), c.key.case_id.as_str()), c))
        .collect();

    let mut sampled_records = Vec::with_capacity(manifest.selected_cases.len());
    for entry in &manifest.selected_cases {
        let fam = entry.case_key.family_id.as_str();
        let cid = entry.case_key.case_id.as_str();
        if let Some(&case) = rep_lookup.get(&(fam, cid)) {
            sampled_records.push(case.clone());
        } else {
            return Err(EvaluationError::ManifestVerificationFailure(format!(
                "failed to retrieve case '{fam}:{cid}' during replay"
            )));
        }
    }

    Ok(sampled_records)
}
