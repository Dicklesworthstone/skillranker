//! Response cache and fingerprint contracts.
//!
//! Provides namespace-scoped, keyed-BLAKE3 request and decision fingerprints,
//! preventing low-entropy prompt inversion while guaranteeing exact cache lookup
//! and duplicate hook delivery detection.

pub mod coordination;
pub mod fingerprint;
pub mod response;

pub use coordination::{
    CoordinateRequestQuery, CoordinatedResponse, CoordinationError, CoordinationKey,
    CoordinationPolicy, DEFAULT_LEASE_TTL_MS, FencingGeneration, FollowerContext,
    FollowerResolution, LeaderContext, LeaseAcquisition, LeaseCoordinator, LeaseRecord,
    MemoryCoordinator, OwnerToken, PublishOutcome, SingleFlightCoordinator, SqliteLeaseCoordinator,
};
pub use fingerprint::{
    CacheKey, CacheNamespace, CandidateDigest, DecisionFingerprint, DecisionFingerprintInput,
    EventDeliveryInput, EventDeliveryKey, FingerprintError, LoadedReferenceDigest,
    RankingPolicySnapshot, RequestFingerprint, RequestFingerprintInput, RequestStage, SnoozeDigest,
    compute_decision_fingerprint, compute_delivery_key, compute_request_fingerprint,
};
pub use response::{
    CacheError, CacheLookupQuery, CacheLookupResult, CacheStorageKey, CacheStorageMap,
    CachedResponseEntry, DEFAULT_CACHE_TTL_SECS, ExecutionAccounting, FreshnessStatus,
    InspectionView, MemoryResponseCache, PipelineCacheProvenance, StageProvenance,
    validate_stage_pair_coherence,
};
