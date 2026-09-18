//! Protected cache namespace, request, and decision fingerprints.
//!
//! Uses keyed BLAKE3 hashing with a local random secret key to prevent
//! low-entropy prompt guessing while maintaining exact deterministic
//! cache lookup, namespace separation, and duplicate delivery tracking.

use crate::identity::{
    AdapterId, AdapterVersion, BranchId, ContentHash, ContextEpoch, HarnessId, SessionId, SkillId,
    WorkspaceId,
};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::Read;

/// Domain separation tags to prevent cross-type fingerprint collision.
const DOMAIN_REQUEST: &[u8] = b"SR_REQ_FP_V1\0";
const DOMAIN_DECISION: &[u8] = b"SR_DEC_FP_V1\0";
const DOMAIN_DELIVERY: &[u8] = b"SR_EVT_DEL_V1\0";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FingerprintError {
    NonFiniteWeight,
    InvalidCandidateOrder,
}

impl fmt::Display for FingerprintError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NonFiniteWeight => "ranking policy contains a non-finite weight or threshold",
            Self::InvalidCandidateOrder => {
                "candidate list violates canonical ordering requirements"
            }
        })
    }
}

impl std::error::Error for FingerprintError {}

/// Secret 32-byte key for keyed BLAKE3 hashing.
///
/// Kept strictly private in local memory; debug formatting and display
/// never disclose the raw key bytes.
#[derive(Clone, Eq, PartialEq)]
pub struct CacheKey([u8; 32]);

impl CacheKey {
    /// Creates a key directly from a 32-byte array.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Generates a fresh random key from the operating system's CSPRNG.
    pub fn generate() -> Result<Self, std::io::Error> {
        let mut bytes = [0u8; 32];
        let mut file = std::fs::File::open("/dev/urandom")?;
        file.read_exact(&mut bytes)?;
        Ok(Self(bytes))
    }

    /// Borrows the raw key bytes internally for keyed hashing.
    #[inline]
    pub(crate) fn as_raw_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for CacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CacheKey(<secret-key>)")
    }
}

/// Request stage discriminator.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum RequestStage {
    Wide,
    Rerank,
}

impl RequestStage {
    #[inline]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Wide => "wide",
            Self::Rerank => "rerank",
        }
    }
}

/// Cache namespace binding decisions and requests to a specific session and context epoch.
///
/// Different sessions or key generations never share cache entries, even if
/// the redacted text or queries happen to be identical.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CacheNamespace {
    pub workspace_id: Option<WorkspaceId>,
    pub session_id: Option<SessionId>,
    pub branch_id: Option<BranchId>,
    pub context_epoch: Option<ContextEpoch>,
    pub adapter_id: Option<AdapterId>,
    pub adapter_version: Option<AdapterVersion>,
    pub harness_id: HarnessId,
    pub key_generation: u64,
}

impl CacheNamespace {
    pub fn new(harness_id: HarnessId, key_generation: u64) -> Self {
        Self {
            workspace_id: None,
            session_id: None,
            branch_id: None,
            context_epoch: None,
            adapter_id: None,
            adapter_version: None,
            harness_id,
            key_generation,
        }
    }

    pub fn with_workspace(mut self, id: WorkspaceId) -> Self {
        self.workspace_id = Some(id);
        self
    }

    pub fn with_session(mut self, id: SessionId) -> Self {
        self.session_id = Some(id);
        self
    }

    pub fn with_branch(mut self, id: BranchId) -> Self {
        self.branch_id = Some(id);
        self
    }

    pub fn with_context_epoch(mut self, epoch: ContextEpoch) -> Self {
        self.context_epoch = Some(epoch);
        self
    }

    pub fn with_adapter(mut self, id: AdapterId, version: AdapterVersion) -> Self {
        self.adapter_id = Some(id);
        self.adapter_version = Some(version);
        self
    }

    /// Serializes namespace into the hasher using length framing.
    pub(super) fn feed_into(&self, hasher: &mut blake3::Hasher) {
        feed_length_prefixed(hasher, self.harness_id.as_str().as_bytes());
        hasher.update(&self.key_generation.to_le_bytes());

        feed_opt_id(hasher, self.workspace_id.as_ref().map(|id| id.as_str()));
        feed_opt_id(hasher, self.session_id.as_ref().map(|id| id.as_str()));
        feed_opt_id(hasher, self.branch_id.as_ref().map(|id| id.as_str()));
        feed_opt_id(hasher, self.context_epoch.as_ref().map(|id| id.as_str()));
        feed_opt_id(hasher, self.adapter_id.as_ref().map(|id| id.as_str()));
        feed_opt_id(hasher, self.adapter_version.as_ref().map(|id| id.as_str()));
    }
}

/// A digest of candidate skill identity, content, and wide/rerank excerpt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateDigest {
    pub skill_id: SkillId,
    pub content_hash: ContentHash,
    pub excerpt_hash: Option<ContentHash>,
}

/// Request fingerprint representing a canonical provider call.
#[derive(Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct RequestFingerprint([u8; 32]);

impl RequestFingerprint {
    #[inline]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[inline]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        blake3::Hash::from(self.0).to_hex().to_string()
    }
}

impl fmt::Debug for RequestFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("RequestFingerprint")
            .field(&self.to_hex())
            .finish()
    }
}

impl fmt::Display for RequestFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// Inputs for computing a `RequestFingerprint`.
#[derive(Clone, Debug)]
pub struct RequestFingerprintInput<'a> {
    pub stage: RequestStage,
    pub canonical_redacted_state: &'a [u8],
    pub candidates: &'a [CandidateDigest],
    pub questions_digest: [u8; 32],
    pub endpoint_url: &'a str,
    pub model: &'a str,
    pub prompt_version: &'a str,
    pub adapter_version: &'a str,
    pub privacy_policy_version: &'a str,
    pub excerpt_strategy: &'a str,
}

/// Computes a keyed `RequestFingerprint`.
pub fn compute_request_fingerprint(
    key: &CacheKey,
    namespace: &CacheNamespace,
    input: &RequestFingerprintInput<'_>,
) -> RequestFingerprint {
    let mut hasher = blake3::Hasher::new_keyed(key.as_raw_bytes());
    hasher.update(DOMAIN_REQUEST);

    // Bind to the exact cache namespace
    namespace.feed_into(&mut hasher);

    // Request-specific fields
    feed_length_prefixed(&mut hasher, input.stage.as_str().as_bytes());
    feed_length_prefixed(&mut hasher, input.canonical_redacted_state);

    // Candidates count + items
    hasher.update(&(input.candidates.len() as u64).to_le_bytes());
    for c in input.candidates {
        feed_length_prefixed(&mut hasher, c.skill_id.as_str().as_bytes());
        feed_length_prefixed(&mut hasher, c.content_hash.as_str().as_bytes());
        match &c.excerpt_hash {
            Some(eh) => {
                hasher.update(&[1u8]);
                feed_length_prefixed(&mut hasher, eh.as_str().as_bytes());
            }
            None => {
                hasher.update(&[0u8]);
            }
        }
    }

    // Questions digest
    hasher.update(&input.questions_digest);

    // Endpoint, model, versions
    feed_length_prefixed(&mut hasher, input.endpoint_url.as_bytes());
    feed_length_prefixed(&mut hasher, input.model.as_bytes());
    feed_length_prefixed(&mut hasher, input.prompt_version.as_bytes());
    feed_length_prefixed(&mut hasher, input.adapter_version.as_bytes());
    feed_length_prefixed(&mut hasher, input.privacy_policy_version.as_bytes());
    feed_length_prefixed(&mut hasher, input.excerpt_strategy.as_bytes());

    let hash = hasher.finalize();
    RequestFingerprint(*hash.as_bytes())
}

/// Loaded reference record observed in local session history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedReferenceDigest {
    pub skill_id: SkillId,
    pub content_hash: ContentHash,
}

/// Active advisory snooze record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnoozeDigest {
    pub skill_id: SkillId,
    pub until_unix_ms: u64,
}

/// Bounded ranking policy parameters with strictly finite floats.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RankingPolicySnapshot {
    pub w_fit: f64,
    pub w_prior: f64,
    pub w_phase: f64,
    pub gate_threshold: f64,
    pub fit_threshold: f64,
    pub top_k: usize,
    pub max_shortlist_m: usize,
}

impl RankingPolicySnapshot {
    pub fn validate(&self) -> Result<(), FingerprintError> {
        if !self.w_fit.is_finite()
            || !self.w_prior.is_finite()
            || !self.w_phase.is_finite()
            || !self.gate_threshold.is_finite()
            || !self.fit_threshold.is_finite()
        {
            return Err(FingerprintError::NonFiniteWeight);
        }
        Ok(())
    }

    fn feed_into(&self, hasher: &mut blake3::Hasher) {
        hasher.update(&self.w_fit.to_bits().to_le_bytes());
        hasher.update(&self.w_prior.to_bits().to_le_bytes());
        hasher.update(&self.w_phase.to_bits().to_le_bytes());
        hasher.update(&self.gate_threshold.to_bits().to_le_bytes());
        hasher.update(&self.fit_threshold.to_bits().to_le_bytes());
        hasher.update(&(self.top_k as u64).to_le_bytes());
        hasher.update(&(self.max_shortlist_m as u64).to_le_bytes());
    }
}

/// Decision fingerprint representing the output recommendation context.
#[derive(Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct DecisionFingerprint([u8; 32]);

impl DecisionFingerprint {
    #[inline]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[inline]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        blake3::Hash::from(self.0).to_hex().to_string()
    }
}

impl fmt::Debug for DecisionFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("DecisionFingerprint")
            .field(&self.to_hex())
            .finish()
    }
}

impl fmt::Display for DecisionFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// Inputs for computing a `DecisionFingerprint`.
#[derive(Clone, Debug)]
pub struct DecisionFingerprintInput<'a> {
    pub request_fingerprint: RequestFingerprint,
    pub loaded_references: &'a [LoadedReferenceDigest],
    pub explicit_exclusions: &'a [SkillId],
    pub ranking_policy: RankingPolicySnapshot,
    pub prior_snapshot_id: Option<ContentHash>,
    pub effective_snoozes: &'a [SnoozeDigest],
    pub visibility_metadata: &'a str,
}

/// Computes a keyed `DecisionFingerprint`.
pub fn compute_decision_fingerprint(
    key: &CacheKey,
    namespace: &CacheNamespace,
    input: &DecisionFingerprintInput<'_>,
) -> Result<DecisionFingerprint, FingerprintError> {
    input.ranking_policy.validate()?;

    let mut hasher = blake3::Hasher::new_keyed(key.as_raw_bytes());
    hasher.update(DOMAIN_DECISION);

    // Bind to namespace
    namespace.feed_into(&mut hasher);

    // Request fingerprint link
    hasher.update(input.request_fingerprint.as_bytes());

    // Loaded references
    hasher.update(&(input.loaded_references.len() as u64).to_le_bytes());
    for lr in input.loaded_references {
        feed_length_prefixed(&mut hasher, lr.skill_id.as_str().as_bytes());
        feed_length_prefixed(&mut hasher, lr.content_hash.as_str().as_bytes());
    }

    // Explicit exclusions
    hasher.update(&(input.explicit_exclusions.len() as u64).to_le_bytes());
    for ex in input.explicit_exclusions {
        feed_length_prefixed(&mut hasher, ex.as_str().as_bytes());
    }

    // Policy parameters
    input.ranking_policy.feed_into(&mut hasher);

    // Prior snapshot ID
    match &input.prior_snapshot_id {
        Some(ps) => {
            hasher.update(&[1u8]);
            feed_length_prefixed(&mut hasher, ps.as_str().as_bytes());
        }
        None => {
            hasher.update(&[0u8]);
        }
    }

    // Snoozes
    hasher.update(&(input.effective_snoozes.len() as u64).to_le_bytes());
    for sn in input.effective_snoozes {
        feed_length_prefixed(&mut hasher, sn.skill_id.as_str().as_bytes());
        hasher.update(&sn.until_unix_ms.to_le_bytes());
    }

    // Visibility metadata
    feed_length_prefixed(&mut hasher, input.visibility_metadata.as_bytes());

    let hash = hasher.finalize();
    Ok(DecisionFingerprint(*hash.as_bytes()))
}

/// Inputs for event delivery duplicate detection.
#[derive(Clone, Debug)]
pub struct EventDeliveryInput<'a> {
    pub delivery_id: Option<&'a str>,
    pub session_id: &'a SessionId,
    pub branch_id: Option<&'a BranchId>,
    pub event_type: &'a str,
    pub transcript_generation: u64,
    pub cursor_offset: u64,
    pub prompt_fingerprint: &'a ContentHash,
}

/// Unique event delivery key with ambiguity flag.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct EventDeliveryKey {
    digest: [u8; 32],
    is_ambiguous: bool,
}

impl EventDeliveryKey {
    #[inline]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.digest
    }

    #[inline]
    pub const fn is_ambiguous(&self) -> bool {
        self.is_ambiguous
    }

    pub fn to_hex(&self) -> String {
        blake3::Hash::from(self.digest).to_hex().to_string()
    }
}

impl fmt::Debug for EventDeliveryKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventDeliveryKey")
            .field("digest", &self.to_hex())
            .field("is_ambiguous", &self.is_ambiguous)
            .finish()
    }
}

/// Computes a delivery key for deduplicating hook invocations.
///
/// If the harness provides no unique delivery ID and delivery coordinates
/// are unversioned, the result is marked `is_ambiguous = true` so exposure-based
/// training excludes it.
pub fn compute_delivery_key(key: &CacheKey, input: &EventDeliveryInput<'_>) -> EventDeliveryKey {
    let mut hasher = blake3::Hasher::new_keyed(key.as_raw_bytes());
    hasher.update(DOMAIN_DELIVERY);

    feed_length_prefixed(&mut hasher, input.session_id.as_str().as_bytes());
    feed_opt_id(&mut hasher, input.branch_id.map(|b| b.as_str()));
    feed_length_prefixed(&mut hasher, input.event_type.as_bytes());
    hasher.update(&input.transcript_generation.to_le_bytes());
    hasher.update(&input.cursor_offset.to_le_bytes());
    feed_length_prefixed(&mut hasher, input.prompt_fingerprint.as_str().as_bytes());

    let is_ambiguous = match input.delivery_id {
        Some(did) => {
            hasher.update(&[1u8]);
            feed_length_prefixed(&mut hasher, did.as_bytes());
            false
        }
        None => {
            hasher.update(&[0u8]);
            // Without delivery ID, if generation and cursor offset are zero,
            // identical prompt text alone is ambiguous.
            input.transcript_generation == 0 && input.cursor_offset == 0
        }
    };

    let hash = hasher.finalize();
    EventDeliveryKey {
        digest: *hash.as_bytes(),
        is_ambiguous,
    }
}

#[inline]
fn feed_length_prefixed(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

#[inline]
fn feed_opt_id(hasher: &mut blake3::Hasher, opt: Option<&str>) {
    match opt {
        Some(val) => {
            hasher.update(&[1u8]);
            feed_length_prefixed(hasher, val.as_bytes());
        }
        None => {
            hasher.update(&[0u8]);
        }
    }
}
