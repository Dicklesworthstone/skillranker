//! Per-stage validated response cache and truthful freshness.
//!
//! Enforces:
//! 1. Per-stage isolation: Wide and Rerank responses are keyed by their exact request fingerprints.
//! 2. Shortlist identity: Stage 2 is keyed by its actual shortlist candidates, not just Stage 1.
//! 3. Truthful freshness: TTL starts at provider receipt; negative age (clock rollback) is rejected.
//! 4. Non-renewal on read: Reading an entry never extends or renews its TTL.
//! 5. Offline & unversioned alias safety: An unversioned model alias prevents mixing cached wide with fresh rerank;
//!    offline mode with missing rerank fails as cache miss rather than reusing an alien candidate set.
//! 6. Zero-cost accounting: Wholly cached runs report 0 new requests and 0 new tokens.
//! 7. Stale inspection non-actionable: Expired entries can be viewed with `stale: true` and `is_actionable: false`,
//!    and are never used as live hook or timeout fallbacks.

use crate::cache::fingerprint::{CacheKey, CacheNamespace, RequestFingerprint, RequestStage};
use crate::jev::codec::Usage;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, RwLock};

/// Default and maximum time-to-live: 10 minutes (600 seconds). A longer stored
/// TTL never extends freshness beyond this.
pub const DEFAULT_CACHE_TTL_SECS: u32 = 600;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheError {
    CorruptEntry,
    OfflineCacheMiss,
    UnversionedPairMismatch,
    LockPoisoned,
}

impl fmt::Display for CacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::CorruptEntry => "cache entry payload is corrupt or invalid",
            Self::OfflineCacheMiss => "missing complete offline cache result",
            Self::UnversionedPairMismatch => {
                "cannot pair cached stage with fresh stage under unversioned model alias"
            }
            Self::LockPoisoned => "internal cache lock poisoned",
        })
    }
}

impl std::error::Error for CacheError {}

/// Provenance of a single pipeline stage execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum StageProvenance {
    /// Newly obtained from live provider call.
    Fresh,
    /// Exact match served from unexpired cache.
    Cached,
}

/// Overall provenance of a two-stage recommendation pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PipelineCacheProvenance {
    /// Both stages were freshly executed via provider.
    WhollyFresh,
    /// Exactly one stage was served from cache and one from provider.
    PartiallyCached {
        wide: StageProvenance,
        rerank: StageProvenance,
    },
    /// Both stages served from cache with zero new provider usage.
    WhollyCached,
}

/// Detailed freshness evaluation for a cached response entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FreshnessStatus {
    /// Valid and fresh within its TTL.
    Fresh { age_ms: u64, remaining_ttl_ms: u64 },
    /// Expired: age strictly exceeds TTL.
    Expired { age_ms: u64 },
    /// Wall-clock rollback detected: current time predates receipt timestamp.
    ClockRollback {
        received_at_unix_ms: u64,
        current_unix_ms: u64,
    },
    /// Model identity or pinned revision mismatch.
    RevisionMismatch,
}

impl FreshnessStatus {
    #[inline]
    pub fn is_fresh(&self) -> bool {
        matches!(self, Self::Fresh { .. })
    }

    #[inline]
    pub fn is_stale(&self) -> bool {
        !self.is_fresh()
    }
}

/// A stored response entry with truthful receipt timestamp and provenance metadata.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CachedResponseEntry {
    pub stage: RequestStage,
    pub request_fingerprint: RequestFingerprint,
    pub response_bytes: Vec<u8>,
    pub received_at_unix_ms: u64,
    pub ttl_seconds: u32,
    pub model: String,
    pub model_revision: Option<String>,
    pub original_usage: Usage,
    pub attempt_id: Option<String>,
}

impl CachedResponseEntry {
    /// Evaluates freshness against the current wall-clock time and active model revision.
    ///
    /// Truthfully rejects clock rollback (negative ages) and revision mismatches.
    pub fn evaluate_freshness(
        &self,
        now_unix_ms: u64,
        active_model: &str,
        active_revision: Option<&str>,
    ) -> FreshnessStatus {
        // Wall-clock rollback: current timestamp is in the past relative to receipt
        if now_unix_ms < self.received_at_unix_ms {
            return FreshnessStatus::ClockRollback {
                received_at_unix_ms: self.received_at_unix_ms,
                current_unix_ms: now_unix_ms,
            };
        }

        // Model name mismatch
        if self.model != active_model {
            return FreshnessStatus::RevisionMismatch;
        }

        // Model revision mismatch (if either party pins a revision)
        if self.model_revision.as_deref() != active_revision {
            return FreshnessStatus::RevisionMismatch;
        }

        let age_ms = now_unix_ms.saturating_sub(self.received_at_unix_ms);
        // Ten minutes from receipt is a maximum, whatever TTL was stored.
        let ttl_ms = u64::from(self.ttl_seconds.min(DEFAULT_CACHE_TTL_SECS)).saturating_mul(1000);

        if age_ms >= ttl_ms {
            FreshnessStatus::Expired { age_ms }
        } else {
            FreshnessStatus::Fresh {
                age_ms,
                remaining_ttl_ms: ttl_ms.saturating_sub(age_ms),
            }
        }
    }
}

/// Lookup result distinguishing fresh hits, stale entries, and cache misses.
#[derive(Clone, Debug)]
pub enum CacheLookupResult {
    /// Exact match, fresh within TTL.
    Hit {
        entry: CachedResponseEntry,
        age_ms: u64,
        remaining_ttl_ms: u64,
    },
    /// Entry found but expired, rolled back, or revision-mismatched.
    Stale {
        entry: CachedResponseEntry,
        status: FreshnessStatus,
    },
    /// No entry exists under this namespace and request fingerprint.
    Miss,
}

impl CacheLookupResult {
    /// Returns the entry if and only if it is fresh.
    pub fn fresh_entry(&self) -> Option<&CachedResponseEntry> {
        match self {
            Self::Hit { entry, .. } => Some(entry),
            _ => None,
        }
    }

    /// Prepares an inspection view. Expired entries are marked `stale: true` and `is_actionable: false`.
    pub fn to_inspection_view(&self) -> InspectionView {
        match self {
            Self::Hit { entry, age_ms, .. } => InspectionView {
                is_stale: false,
                is_actionable: true,
                age_ms: Some(*age_ms),
                entry: Some(entry.clone()),
            },
            Self::Stale { entry, status } => {
                let age_ms = match status {
                    FreshnessStatus::Expired { age_ms } => Some(*age_ms),
                    _ => None,
                };
                InspectionView {
                    is_stale: true,
                    is_actionable: false,
                    age_ms,
                    entry: Some(entry.clone()),
                }
            }
            Self::Miss => InspectionView {
                is_stale: false,
                is_actionable: false,
                age_ms: None,
                entry: None,
            },
        }
    }
}

/// Inspection presentation view guaranteeing that stale results cannot provide actionable recommendations.
#[derive(Clone, Debug)]
pub struct InspectionView {
    pub is_stale: bool,
    pub is_actionable: bool,
    pub age_ms: Option<u64>,
    pub entry: Option<CachedResponseEntry>,
}

/// Internal cache key type: (namespace_hash, stage_name, request_fingerprint_bytes).
pub type CacheStorageKey = ([u8; 32], &'static str, [u8; 32]);
/// Storage map type for in-memory response cache.
pub type CacheStorageMap = BTreeMap<CacheStorageKey, CachedResponseEntry>;

/// Query parameters for looking up a response in the cache.
#[derive(Debug, Clone, Copy)]
pub struct CacheLookupQuery<'a> {
    pub key: &'a CacheKey,
    pub namespace: &'a CacheNamespace,
    pub stage: RequestStage,
    pub fingerprint: &'a RequestFingerprint,
    pub now_unix_ms: u64,
    pub active_model: &'a str,
    pub active_revision: Option<&'a str>,
}

/// Thread-safe in-memory response cache.
#[derive(Clone, Default)]
pub struct MemoryResponseCache {
    entries: Arc<RwLock<CacheStorageMap>>,
}

impl MemoryResponseCache {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }

    fn namespace_hash(key: &CacheKey, namespace: &CacheNamespace) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new_keyed(key.as_raw_bytes());
        hasher.update(b"SR_CACHE_NS_HASH_V2\0");
        namespace.feed_into(&mut hasher);
        *hasher.finalize().as_bytes()
    }

    /// Looks up a cached entry and evaluates freshness.
    ///
    /// Reading from cache NEVER updates `received_at_unix_ms` or extends TTL.
    pub fn get(&self, query: &CacheLookupQuery<'_>) -> Result<CacheLookupResult, CacheError> {
        let ns_hash = Self::namespace_hash(query.key, query.namespace);
        let stage_str = query.stage.as_str();
        let map = self.entries.read().map_err(|_| CacheError::LockPoisoned)?;

        let Some(entry) = map.get(&(ns_hash, stage_str, *query.fingerprint.as_bytes())) else {
            return Ok(CacheLookupResult::Miss);
        };

        let freshness =
            entry.evaluate_freshness(query.now_unix_ms, query.active_model, query.active_revision);
        match freshness {
            FreshnessStatus::Fresh {
                age_ms,
                remaining_ttl_ms,
            } => Ok(CacheLookupResult::Hit {
                entry: entry.clone(),
                age_ms,
                remaining_ttl_ms,
            }),
            other => Ok(CacheLookupResult::Stale {
                entry: entry.clone(),
                status: other,
            }),
        }
    }

    /// Stores a validated response entry in cache.
    pub fn put(
        &self,
        key: &CacheKey,
        namespace: &CacheNamespace,
        entry: CachedResponseEntry,
    ) -> Result<(), CacheError> {
        let ns_hash = Self::namespace_hash(key, namespace);
        let stage_str = entry.stage.as_str();
        let fp_bytes = *entry.request_fingerprint.as_bytes();
        let mut map = self.entries.write().map_err(|_| CacheError::LockPoisoned)?;
        map.insert((ns_hash, stage_str, fp_bytes), entry);
        Ok(())
    }

    /// Evicts all entries for a specific namespace (e.g. on session termination or key generation change).
    pub fn evict_namespace(
        &self,
        key: &CacheKey,
        namespace: &CacheNamespace,
    ) -> Result<usize, CacheError> {
        let ns_hash = Self::namespace_hash(key, namespace);
        let mut map = self.entries.write().map_err(|_| CacheError::LockPoisoned)?;
        let before = map.len();
        map.retain(|(h, _, _), _| *h != ns_hash);
        Ok(before.saturating_sub(map.len()))
    }
}

/// Evaluates pairing safety when combining stage 1 (Wide) and stage 2 (Rerank).
///
/// If an unversioned model alias is used, combining an old cached wide stage with a fresh
/// rerank stage is forbidden (must refresh the pair or remain unavailable).
pub fn validate_stage_pair_coherence(
    wide_provenance: StageProvenance,
    rerank_provenance: StageProvenance,
    is_unversioned_alias: bool,
) -> Result<PipelineCacheProvenance, CacheError> {
    match (wide_provenance, rerank_provenance) {
        (StageProvenance::Cached, StageProvenance::Cached) => {
            Ok(PipelineCacheProvenance::WhollyCached)
        }
        (StageProvenance::Fresh, StageProvenance::Fresh) => {
            Ok(PipelineCacheProvenance::WhollyFresh)
        }
        (wide, rerank) => {
            if is_unversioned_alias {
                // Cannot combine cached wide with fresh rerank under unversioned model alias
                Err(CacheError::UnversionedPairMismatch)
            } else {
                Ok(PipelineCacheProvenance::PartiallyCached { wide, rerank })
            }
        }
    }
}

/// Calculates net new provider costs for a pipeline execution.
///
/// Wholly cached passes guarantee exactly zero new requests and zero new tokens.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExecutionAccounting {
    pub new_requests: usize,
    pub new_tokens: u64,
    pub original_usage: Usage,
    pub served_from_cache: bool,
}

impl ExecutionAccounting {
    pub fn compute(
        provenance: PipelineCacheProvenance,
        wide_usage: Usage,
        rerank_usage: Option<Usage>,
    ) -> Self {
        let total_orig_tokens = wide_usage
            .total_tokens()
            .saturating_add(rerank_usage.as_ref().map_or(0, |u| u.total_tokens()));
        let orig_usage = Usage {
            input_tokens: wide_usage
                .input_tokens
                .saturating_add(rerank_usage.as_ref().map_or(0, |u| u.input_tokens)),
            output_tokens: wide_usage
                .output_tokens
                .saturating_add(rerank_usage.as_ref().map_or(0, |u| u.output_tokens)),
        };

        match provenance {
            PipelineCacheProvenance::WhollyCached => Self {
                new_requests: 0,
                new_tokens: 0,
                original_usage: orig_usage,
                served_from_cache: true,
            },
            PipelineCacheProvenance::WhollyFresh => {
                let requests = if rerank_usage.is_some() { 2 } else { 1 };
                Self {
                    new_requests: requests,
                    new_tokens: total_orig_tokens,
                    original_usage: orig_usage,
                    served_from_cache: false,
                }
            }
            PipelineCacheProvenance::PartiallyCached { wide, rerank } => {
                let mut new_requests = 0;
                let mut new_tokens: u64 = 0;
                if wide == StageProvenance::Fresh {
                    new_requests += 1;
                    new_tokens = new_tokens.saturating_add(wide_usage.total_tokens());
                }
                if rerank == StageProvenance::Fresh {
                    new_requests += 1;
                    if let Some(u) = rerank_usage {
                        new_tokens = new_tokens.saturating_add(u.total_tokens());
                    }
                }
                Self {
                    new_requests,
                    new_tokens,
                    original_usage: orig_usage,
                    served_from_cache: false,
                }
            }
        }
    }
}
