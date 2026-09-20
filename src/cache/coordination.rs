//! Single-flight response coordination with fenced bounded leases (sr-roadmap-l1i.5.10).
//!
//! Provides single-flight coordination keyed by namespace and exact request fingerprint.
//! Ensures that:
//! 1. Only a validated provider response is shared (no decisions, event identities, or exposures are shared).
//! 2. Lease ownership is tracked via a unique `OwnerToken` and strictly monotonic `FencingGeneration`.
//! 3. An expired leader cannot publish after a successor acquires the lease (superseded results become quiet fallback).
//! 4. Follower wait is strictly bounded by remaining deadline, returning quiet fallback on timeout.
//! 5. SQLite/coordination write transactions cover bounded local work, never HTTP calls.
//! 6. Coordination state stores NO response bodies; `--no-cache` disables cross-process response sharing.
//! 7. Request owner alone records provider attempts/usage; followers incur zero new requests and zero new tokens.
//! 8. Stage-aware coordination distinguishes Wide and Rerank stages.
//! 9. Responses are published atomically with lease completion, eliminating completion/body races and preventing stale overwrite.
//! 10. Expired or lost cache entries can reacquire leadership for fresh retrieval.
//! 11. SQLite connection opening is verified against qualified engine version and safe directory permissions.

use super::fingerprint::{CacheKey, CacheNamespace, RequestFingerprint, RequestStage};
use super::response::{
    CacheError, CacheLookupQuery, CacheLookupResult, CachedResponseEntry, FreshnessStatus,
    MemoryResponseCache, ResponseCache,
};
use crate::jev::codec::Usage;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Duration;

/// Default lease TTL in milliseconds (5,000 ms).
pub const DEFAULT_LEASE_TTL_MS: u64 = 5_000;

/// Error kinds during single-flight coordination.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CoordinationError {
    LockPoisoned,
    StorageBusy,
    StorageError(String),
    CacheError(CacheError),
    InvalidTimestamp,
}

impl fmt::Display for CoordinationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LockPoisoned => f.write_str("coordination synchronization lock was poisoned"),
            Self::StorageBusy => f.write_str("coordination storage is busy"),
            Self::StorageError(e) => write!(f, "coordination storage error: {e}"),
            Self::CacheError(e) => write!(f, "response cache error: {e}"),
            Self::InvalidTimestamp => {
                f.write_str("invalid or rolling-back timestamp during coordination")
            }
        }
    }
}

impl std::error::Error for CoordinationError {}

impl From<CacheError> for CoordinationError {
    fn from(err: CacheError) -> Self {
        Self::CacheError(err)
    }
}

impl From<rusqlite::Error> for CoordinationError {
    fn from(err: rusqlite::Error) -> Self {
        if matches!(
            err.sqlite_error_code(),
            Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
        ) {
            return Self::StorageBusy;
        }
        Self::StorageError(err.to_string())
    }
}

/// Unique owner token generated from OS CSPRNG.
///
/// Debug representation strictly redacts raw token bytes.
#[derive(Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct OwnerToken([u8; 16]);

impl OwnerToken {
    /// Creates an owner token directly from 16 bytes.
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Generates a fresh random token from the operating system CSPRNG.
    pub fn generate() -> Result<Self, std::io::Error> {
        let mut bytes = [0u8; 16];
        let mut file = std::fs::File::open("/dev/urandom")?;
        file.read_exact(&mut bytes)?;
        Ok(Self(bytes))
    }

    /// Returns the raw 16 bytes for storage.
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Debug for OwnerToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OwnerToken(<secret>)")
    }
}

/// Monotonically increasing fencing generation counter for lease leadership.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub struct FencingGeneration(pub u64);

impl FencingGeneration {
    pub const fn initial() -> Self {
        Self(1)
    }

    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// Coordination key derived from namespace and request fingerprint.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub struct CoordinationKey([u8; 32]);

impl CoordinationKey {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Computes the keyed BLAKE3 coordination key binding namespace and request fingerprint.
    pub fn compute(
        key: &CacheKey,
        namespace: &CacheNamespace,
        request_fingerprint: &RequestFingerprint,
    ) -> Self {
        let mut hasher = blake3::Hasher::new_keyed(key.as_raw_bytes());
        hasher.update(b"SR_COORDINATION_KEY_V2\0");
        namespace.feed_into(&mut hasher);
        hasher.update(request_fingerprint.as_bytes());
        Self(*hasher.finalize().as_bytes())
    }
}

/// Persistent metadata for an active or completed lease.
///
/// MUST NOT contain response bodies.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LeaseRecord {
    pub owner_token: OwnerToken,
    pub fencing_generation: FencingGeneration,
    pub acquired_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub attempt_id: String,
    pub is_completed: bool,
}

/// Policy governing single-flight lease coordination.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CoordinationPolicy {
    /// Whether response cache is enabled. If false, cross-process response sharing is disabled.
    pub cache_enabled: bool,
    /// Whether cross-process persistence is allowed.
    pub cross_process_allowed: bool,
    /// Lease duration in milliseconds.
    pub lease_ttl_ms: u64,
}

impl Default for CoordinationPolicy {
    fn default() -> Self {
        Self {
            cache_enabled: true,
            cross_process_allowed: true,
            lease_ttl_ms: DEFAULT_LEASE_TTL_MS,
        }
    }
}

impl CoordinationPolicy {
    pub fn stateless() -> Self {
        Self {
            cache_enabled: false,
            cross_process_allowed: false,
            lease_ttl_ms: DEFAULT_LEASE_TTL_MS,
        }
    }

    pub fn no_cache() -> Self {
        Self {
            cache_enabled: false,
            cross_process_allowed: false,
            lease_ttl_ms: DEFAULT_LEASE_TTL_MS,
        }
    }
}

/// Context for the leader that acquired the lease.
#[derive(Clone, Debug)]
pub struct LeaderContext {
    pub key: CoordinationKey,
    pub owner_token: OwnerToken,
    pub fencing_generation: FencingGeneration,
    pub lease_expires_at_unix_ms: u64,
    pub attempt_id: String,
}

/// Context for a follower waiting for an active leader.
#[derive(Clone, Debug)]
pub struct FollowerContext {
    pub key: CoordinationKey,
    pub leader_generation: FencingGeneration,
    pub lease_expires_at_unix_ms: u64,
}

/// Outcome of attempting to acquire a lease.
#[derive(Debug)]
pub enum LeaseAcquisition {
    /// Caller won the lease and must execute the provider call.
    Leading(LeaderContext),
    /// Another leader currently owns an active, unexpired lease.
    Following(FollowerContext),
    /// The request was already completed and published by an earlier leader.
    AlreadyCompleted,
}

/// Outcome of a leader's attempt to publish a response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishOutcome {
    /// Validated response was committed successfully.
    Published,
    /// The lease expired or was superseded by a successor with a higher fencing generation.
    /// The response was dropped to prevent overwriting newer state (quiet fallback).
    Superseded {
        expected_generation: FencingGeneration,
        current_generation: Option<FencingGeneration>,
    },
}

/// Outcome of a follower waiting for a leader to complete.
#[derive(Clone, Debug)]
pub enum FollowerResolution {
    /// Leader completed and published to cache.
    Completed,
    /// Leader completed and response was safely retrieved from cache.
    Reused(CachedResponseEntry),
    /// Follower's remaining deadline was reached before completion (quiet fallback).
    DeadlineExceeded,
    /// Leader's lease expired without publication.
    LeaseExpired,
    /// Response cache is disabled; no cross-process response sharing possible.
    CacheDisabled,
    /// Leader failed or cancelled without completing.
    LeaderFailed,
}

/// Trait defining the contract for lease coordination.
pub trait LeaseCoordinator: Send + Sync {
    /// Attempts to acquire a lease for the given coordination key.
    fn acquire(
        &self,
        key: CoordinationKey,
        now_unix_ms: u64,
        policy: &CoordinationPolicy,
    ) -> Result<LeaseAcquisition, CoordinationError>;

    /// Reacquires leadership for a key, bumping the fencing generation,
    /// or returns Following if an active unexpired leader is already refreshing the key.
    fn force_reacquire(
        &self,
        key: CoordinationKey,
        now_unix_ms: u64,
        policy: &CoordinationPolicy,
    ) -> Result<LeaseAcquisition, CoordinationError>;

    /// Completes and releases a lease if the fencing generation and owner match.
    fn complete(
        &self,
        key: CoordinationKey,
        owner_token: OwnerToken,
        generation: FencingGeneration,
        now_unix_ms: u64,
    ) -> Result<PublishOutcome, CoordinationError>;

    /// Checks the current state of a lease without mutating it.
    fn check_lease(&self, key: CoordinationKey) -> Result<Option<LeaseRecord>, CoordinationError>;
}

/// In-memory single-flight coordinator for intra-process thread coordination.
#[derive(Default)]
pub struct MemoryCoordinator {
    leases: Arc<RwLock<BTreeMap<CoordinationKey, LeaseRecord>>>,
    notify: Arc<(Mutex<()>, Condvar)>,
}

impl MemoryCoordinator {
    pub fn new() -> Self {
        Self {
            leases: Arc::new(RwLock::new(BTreeMap::new())),
            notify: Arc::new((Mutex::new(()), Condvar::new())),
        }
    }

    /// Atomically verifies lease ownership/fencing before executing the publication closure and marking the lease completed.
    ///
    /// If the leader was superseded or expired, `publish` is NEVER invoked and `PublishOutcome::Superseded` is returned.
    pub fn complete_and_publish<F>(
        &self,
        key: CoordinationKey,
        owner_token: OwnerToken,
        generation: FencingGeneration,
        now_unix_ms: u64,
        publish: F,
    ) -> Result<PublishOutcome, CoordinationError>
    where
        F: FnOnce() -> Result<(), CoordinationError>,
    {
        let mut map = self
            .leases
            .write()
            .map_err(|_| CoordinationError::LockPoisoned)?;

        let Some(existing) = map.get_mut(&key) else {
            return Ok(PublishOutcome::Superseded {
                expected_generation: generation,
                current_generation: None,
            });
        };

        if existing.owner_token != owner_token || existing.fencing_generation != generation {
            return Ok(PublishOutcome::Superseded {
                expected_generation: generation,
                current_generation: Some(existing.fencing_generation),
            });
        }

        if now_unix_ms >= existing.expires_at_unix_ms {
            return Ok(PublishOutcome::Superseded {
                expected_generation: generation,
                current_generation: Some(existing.fencing_generation),
            });
        }

        publish()?;
        existing.is_completed = true;

        // Wake waiting followers
        let (_, cvar) = &*self.notify;
        cvar.notify_all();

        Ok(PublishOutcome::Published)
    }
}

impl LeaseCoordinator for MemoryCoordinator {
    fn acquire(
        &self,
        key: CoordinationKey,
        now_unix_ms: u64,
        policy: &CoordinationPolicy,
    ) -> Result<LeaseAcquisition, CoordinationError> {
        let mut map = self
            .leases
            .write()
            .map_err(|_| CoordinationError::LockPoisoned)?;

        if let Some(existing) = map.get_mut(&key) {
            // First check if lease is unexpired
            if now_unix_ms < existing.expires_at_unix_ms {
                if existing.is_completed {
                    return Ok(LeaseAcquisition::AlreadyCompleted);
                }
                return Ok(LeaseAcquisition::Following(FollowerContext {
                    key,
                    leader_generation: existing.fencing_generation,
                    lease_expires_at_unix_ms: existing.expires_at_unix_ms,
                }));
            }

            // Existing lease expired; successor reacquires with bumped generation
            let new_gen = existing.fencing_generation.next();
            let new_token = OwnerToken::generate()
                .map_err(|e| CoordinationError::StorageError(e.to_string()))?;
            let expires_at = now_unix_ms.saturating_add(policy.lease_ttl_ms);
            let attempt_id = format!("att-inmem-{}", new_gen.as_u64());

            *existing = LeaseRecord {
                owner_token: new_token,
                fencing_generation: new_gen,
                acquired_at_unix_ms: now_unix_ms,
                expires_at_unix_ms: expires_at,
                attempt_id: attempt_id.clone(),
                is_completed: false,
            };

            return Ok(LeaseAcquisition::Leading(LeaderContext {
                key,
                owner_token: new_token,
                fencing_generation: new_gen,
                lease_expires_at_unix_ms: expires_at,
                attempt_id,
            }));
        }

        // New lease
        let token =
            OwnerToken::generate().map_err(|e| CoordinationError::StorageError(e.to_string()))?;
        let fence_gen = FencingGeneration::initial();
        let expires_at = now_unix_ms.saturating_add(policy.lease_ttl_ms);
        let attempt_id = format!("att-inmem-{}", fence_gen.as_u64());

        map.insert(
            key,
            LeaseRecord {
                owner_token: token,
                fencing_generation: fence_gen,
                acquired_at_unix_ms: now_unix_ms,
                expires_at_unix_ms: expires_at,
                attempt_id: attempt_id.clone(),
                is_completed: false,
            },
        );

        Ok(LeaseAcquisition::Leading(LeaderContext {
            key,
            owner_token: token,
            fencing_generation: fence_gen,
            lease_expires_at_unix_ms: expires_at,
            attempt_id,
        }))
    }

    fn force_reacquire(
        &self,
        key: CoordinationKey,
        now_unix_ms: u64,
        policy: &CoordinationPolicy,
    ) -> Result<LeaseAcquisition, CoordinationError> {
        let mut map = self
            .leases
            .write()
            .map_err(|_| CoordinationError::LockPoisoned)?;

        if let Some(existing) = map.get_mut(&key) {
            // If another leader already reacquired to refresh and is currently unexpired, follow them
            if !existing.is_completed && now_unix_ms < existing.expires_at_unix_ms {
                return Ok(LeaseAcquisition::Following(FollowerContext {
                    key,
                    leader_generation: existing.fencing_generation,
                    lease_expires_at_unix_ms: existing.expires_at_unix_ms,
                }));
            }

            let new_gen = existing.fencing_generation.next();
            let new_token = OwnerToken::generate()
                .map_err(|e| CoordinationError::StorageError(e.to_string()))?;
            let expires_at = now_unix_ms.saturating_add(policy.lease_ttl_ms);
            let attempt_id = format!("att-inmem-{}", new_gen.as_u64());

            *existing = LeaseRecord {
                owner_token: new_token,
                fencing_generation: new_gen,
                acquired_at_unix_ms: now_unix_ms,
                expires_at_unix_ms: expires_at,
                attempt_id: attempt_id.clone(),
                is_completed: false,
            };

            return Ok(LeaseAcquisition::Leading(LeaderContext {
                key,
                owner_token: new_token,
                fencing_generation: new_gen,
                lease_expires_at_unix_ms: expires_at,
                attempt_id,
            }));
        }

        // New lease
        let token =
            OwnerToken::generate().map_err(|e| CoordinationError::StorageError(e.to_string()))?;
        let fence_gen = FencingGeneration::initial();
        let expires_at = now_unix_ms.saturating_add(policy.lease_ttl_ms);
        let attempt_id = format!("att-inmem-{}", fence_gen.as_u64());

        map.insert(
            key,
            LeaseRecord {
                owner_token: token,
                fencing_generation: fence_gen,
                acquired_at_unix_ms: now_unix_ms,
                expires_at_unix_ms: expires_at,
                attempt_id: attempt_id.clone(),
                is_completed: false,
            },
        );

        Ok(LeaseAcquisition::Leading(LeaderContext {
            key,
            owner_token: token,
            fencing_generation: fence_gen,
            lease_expires_at_unix_ms: expires_at,
            attempt_id,
        }))
    }

    fn complete(
        &self,
        key: CoordinationKey,
        owner_token: OwnerToken,
        generation: FencingGeneration,
        now_unix_ms: u64,
    ) -> Result<PublishOutcome, CoordinationError> {
        self.complete_and_publish(key, owner_token, generation, now_unix_ms, || Ok(()))
    }

    fn check_lease(&self, key: CoordinationKey) -> Result<Option<LeaseRecord>, CoordinationError> {
        let map = self
            .leases
            .read()
            .map_err(|_| CoordinationError::LockPoisoned)?;
        Ok(map.get(&key).cloned())
    }
}

impl MemoryCoordinator {
    /// Waits for a completed response or deadline expiration.
    pub fn wait_for_completion(
        &self,
        key: CoordinationKey,
        now_fn: impl Fn() -> u64,
        deadline_unix_ms: u64,
    ) -> FollowerResolution {
        let (lock, cvar) = &*self.notify;
        let mut guard = lock.lock().unwrap();

        loop {
            let now = now_fn();
            if now >= deadline_unix_ms {
                return FollowerResolution::DeadlineExceeded;
            }

            if let Ok(Some(record)) = self.check_lease(key) {
                if record.is_completed {
                    return FollowerResolution::Completed;
                }
                if now >= record.expires_at_unix_ms {
                    return FollowerResolution::LeaseExpired;
                }
            } else {
                return FollowerResolution::LeaderFailed;
            }

            let wait_budget = Duration::from_millis((deadline_unix_ms.saturating_sub(now)).min(25));

            let (new_guard, _) = cvar.wait_timeout(guard, wait_budget).unwrap();
            guard = new_guard;
        }
    }
}

fn validate_sqlite_path(path: &Path) -> Result<(), CoordinationError> {
    if !path.is_absolute() {
        return Err(CoordinationError::StorageError(
            "coordination database path must be absolute".to_string(),
        ));
    }
    for component in path.components() {
        if let std::path::Component::ParentDir = component {
            return Err(CoordinationError::StorageError(
                "coordination database path must not contain '..'".to_string(),
            ));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Some(parent) = path.parent()
            && let Ok(meta) = std::fs::symlink_metadata(parent)
        {
            if !meta.is_dir() {
                return Err(CoordinationError::StorageError(
                    "coordination parent is not a directory".to_string(),
                ));
            }
            let uid = nix::unistd::geteuid().as_raw();
            if meta.uid() != uid && meta.uid() != 0 {
                return Err(CoordinationError::StorageError(
                    "coordination parent directory not owned by current user or root".to_string(),
                ));
            }
            let mode = meta.mode();
            if (mode & 0o022 != 0) && !(meta.uid() == 0 && (mode & 0o1000 != 0)) {
                return Err(CoordinationError::StorageError(
                    "coordination parent directory has unsafe permissions".to_string(),
                ));
            }
        }
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            if meta.file_type().is_symlink() {
                return Err(CoordinationError::StorageError(
                    "coordination database path must not be a symlink".to_string(),
                ));
            }
            let uid = nix::unistd::geteuid().as_raw();
            if meta.uid() != uid {
                return Err(CoordinationError::StorageError(
                    "coordination database file not owned by current user".to_string(),
                ));
            }
            if meta.mode() & 0o077 != 0 {
                return Err(CoordinationError::StorageError(
                    "coordination database file has unsafe permissions (must be owner-only)"
                        .to_string(),
                ));
            }
        }
    }
    Ok(())
}

fn open_qualified_connection(path: &Path) -> Result<Connection, CoordinationError> {
    open_qualified_connection_with_budget(path, Duration::from_millis(25))
}

fn open_qualified_connection_with_budget(
    path: &Path,
    busy_budget: Duration,
) -> Result<Connection, CoordinationError> {
    let normalized = crate::storage::storage_path(path.to_path_buf());
    let path = normalized.as_path();
    crate::sqlite_engine::linked_engine()
        .map_err(|e| CoordinationError::StorageError(e.to_string()))?;
    validate_sqlite_path(path)?;

    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;

    let conn = Connection::open_with_flags(path, flags)?;
    conn.busy_timeout(busy_budget.min(Duration::from_millis(25)))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.set_limit(
        rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH,
        2 * 1024 * 1024,
    )?;
    conn.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_SQL_LENGTH, 64 * 1024)?;
    conn.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_ATTACHED, 0)?;
    conn.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_WORKER_THREADS, 0)?;
    conn.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    conn.set_db_config(
        rusqlite::config::DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA,
        false,
    )?;
    conn.set_db_config(
        rusqlite::config::DbConfig::SQLITE_DBCONFIG_ENABLE_TRIGGER,
        false,
    )?;
    conn.set_db_config(
        rusqlite::config::DbConfig::SQLITE_DBCONFIG_ENABLE_VIEW,
        false,
    )?;
    conn.set_db_config(
        rusqlite::config::DbConfig::SQLITE_DBCONFIG_ENABLE_FKEY,
        true,
    )?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mut perms = meta.permissions();
            if perms.mode() & 0o777 != 0o600 {
                perms.set_mode(0o600);
                let _ = std::fs::set_permissions(path, perms);
            }
        }
    }

    Ok(conn)
}

/// SQLite-backed lease coordinator for cross-process coordination.
///
/// Guarantees:
/// 1. SQLite write locks cover lease check/insert/update and bounded cache
///    publication callbacks, NEVER an HTTP request.
/// 2. Zero response bodies: table `sr_coordination_leases` contains only tokens, generation, and timestamps.
pub struct SqliteLeaseCoordinator {
    db_path: PathBuf,
}

impl SqliteLeaseCoordinator {
    /// Creates or connects to a SQLite lease coordinator at `db_path`.
    pub fn open(db_path: impl AsRef<Path>) -> Result<Self, CoordinationError> {
        let db_path = db_path.as_ref().to_path_buf();
        let conn = open_qualified_connection(&db_path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sr_coordination_leases (
                 coordination_key BLOB PRIMARY KEY CHECK(length(coordination_key) = 32),
                 owner_token BLOB NOT NULL CHECK(length(owner_token) = 16),
                 fencing_generation INTEGER NOT NULL CHECK(fencing_generation >= 1),
                 acquired_at_unix_ms INTEGER NOT NULL,
                 expires_at_unix_ms INTEGER NOT NULL,
                 attempt_id TEXT NOT NULL,
                 is_completed INTEGER NOT NULL CHECK(is_completed IN (0, 1))
             ) STRICT;
             CREATE TABLE IF NOT EXISTS sr_response_cache (
                 namespace_hash BLOB NOT NULL CHECK(length(namespace_hash) = 32),
                 stage TEXT NOT NULL CHECK(stage IN ('wide', 'rerank')),
                 request_fingerprint BLOB NOT NULL CHECK(length(request_fingerprint) = 32),
                 response_bytes BLOB NOT NULL,
                 received_at_unix_ms INTEGER NOT NULL,
                 ttl_seconds INTEGER NOT NULL,
                 model TEXT NOT NULL,
                 model_revision TEXT,
                 input_tokens INTEGER NOT NULL,
                 output_tokens INTEGER NOT NULL,
                 attempt_id TEXT,
                 PRIMARY KEY (namespace_hash, stage, request_fingerprint)
             ) STRICT;",
        )?;
        Ok(Self { db_path })
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Hold the lease writer lock from current-owner validation through a
    /// bounded external cache write. The callback must not perform network
    /// work. This serializes successor acquisition with the actual cache
    /// commit without storing response bodies in the coordination database.
    pub fn with_active_lease<T>(
        &self,
        leader: &LeaderContext,
        busy_wait: Duration,
        now: impl FnOnce() -> u64,
        publish: impl FnOnce() -> T,
    ) -> Result<Option<T>, CoordinationError> {
        let mut conn = open_qualified_connection(&self.db_path)?;
        conn.busy_timeout(busy_wait.min(Duration::from_millis(25)))?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let valid = Self::active_lease_in_transaction(&tx, leader, now())?;
        if !valid {
            return Ok(None);
        }
        let result = publish();
        // No lease mutation: dropping the read/write transaction releases the
        // writer lock only after the external cache transaction has finished.
        drop(tx);
        Ok(Some(result))
    }

    /// Atomically verifies lease ownership/fencing, optionally publishes response bytes into `sr_response_cache`,
    /// and marks the lease completed within a single immediate SQLite transaction.
    ///
    /// If the leader was superseded or expired, `sr_response_cache` is NEVER modified and `PublishOutcome::Superseded` is returned.
    pub fn complete_and_publish(
        &self,
        key: CoordinationKey,
        owner_token: OwnerToken,
        generation: FencingGeneration,
        now_unix_ms: u64,
        cache_entry: Option<(&CacheKey, &CacheNamespace, &CachedResponseEntry)>,
    ) -> Result<PublishOutcome, CoordinationError> {
        let mut conn = open_qualified_connection(&self.db_path)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let result = Self::complete_in_transaction(
            &tx,
            key,
            owner_token,
            generation,
            now_unix_ms,
            cache_entry,
        )?;
        tx.commit()?;
        Ok(result)
    }
}

impl LeaseCoordinator for SqliteLeaseCoordinator {
    fn acquire(
        &self,
        key: CoordinationKey,
        now_unix_ms: u64,
        policy: &CoordinationPolicy,
    ) -> Result<LeaseAcquisition, CoordinationError> {
        let mut conn = open_qualified_connection(&self.db_path)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let outcome = Self::acquire_in_transaction(&tx, key, now_unix_ms, policy)?;
        tx.commit()?;
        Ok(outcome)
    }

    fn force_reacquire(
        &self,
        key: CoordinationKey,
        now_unix_ms: u64,
        policy: &CoordinationPolicy,
    ) -> Result<LeaseAcquisition, CoordinationError> {
        let mut conn = open_qualified_connection(&self.db_path)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let outcome = Self::force_reacquire_in_transaction(&tx, key, now_unix_ms, policy)?;
        tx.commit()?;
        Ok(outcome)
    }

    fn complete(
        &self,
        key: CoordinationKey,
        owner_token: OwnerToken,
        generation: FencingGeneration,
        now_unix_ms: u64,
    ) -> Result<PublishOutcome, CoordinationError> {
        self.complete_and_publish(key, owner_token, generation, now_unix_ms, None)
    }

    fn check_lease(&self, key: CoordinationKey) -> Result<Option<LeaseRecord>, CoordinationError> {
        self.check_lease_with_budget(key, Duration::from_millis(25))
    }
}

impl SqliteLeaseCoordinator {
    fn check_lease_with_budget(
        &self,
        key: CoordinationKey,
        busy_budget: Duration,
    ) -> Result<Option<LeaseRecord>, CoordinationError> {
        let conn = open_qualified_connection_with_budget(&self.db_path, busy_budget)?;
        Self::check_lease_on_connection(&conn, key)
    }
}

impl SqliteLeaseCoordinator {
    /// Bounded poll for a cross-process lease completion.
    ///
    /// Polls the SQLite table every `poll_interval` until completion, lease expiration,
    /// or follower deadline expiration.
    pub fn wait_for_completion(
        &self,
        key: CoordinationKey,
        now_fn: impl Fn() -> u64,
        deadline_unix_ms: u64,
        poll_interval: Duration,
    ) -> Result<FollowerResolution, CoordinationError> {
        loop {
            let now = now_fn();
            if now >= deadline_unix_ms {
                return Ok(FollowerResolution::DeadlineExceeded);
            }

            let lease = match self.check_lease_with_budget(
                key,
                Duration::from_millis(deadline_unix_ms.saturating_sub(now)),
            ) {
                Ok(lease) => lease,
                Err(CoordinationError::StorageBusy) => {
                    let remaining = deadline_unix_ms.saturating_sub(now_fn());
                    if remaining == 0 {
                        return Ok(FollowerResolution::DeadlineExceeded);
                    }
                    std::thread::sleep(poll_interval.min(Duration::from_millis(remaining)));
                    continue;
                }
                Err(error) => return Err(error),
            };
            let now = now_fn();
            if now >= deadline_unix_ms {
                return Ok(FollowerResolution::DeadlineExceeded);
            }
            let Some(record) = lease else {
                return Ok(FollowerResolution::LeaderFailed);
            };

            if record.is_completed {
                return Ok(FollowerResolution::Completed);
            }

            if now >= record.expires_at_unix_ms {
                return Ok(FollowerResolution::LeaseExpired);
            }

            let remaining_ms = deadline_unix_ms.saturating_sub(now);
            let sleep_dur = poll_interval.min(Duration::from_millis(remaining_ms));
            std::thread::sleep(sleep_dur);
        }
    }
}

/// Persistent SQLite-backed response cache for cross-process response sharing.
///
/// Stores validated provider responses separately from coordination lease metadata.
pub struct SqliteResponseCache {
    db_path: PathBuf,
}

impl SqliteResponseCache {
    /// Creates or connects to a SQLite response cache at `db_path`.
    pub fn open(db_path: impl AsRef<Path>) -> Result<Self, CoordinationError> {
        let db_path = db_path.as_ref().to_path_buf();
        let conn = open_qualified_connection(&db_path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sr_response_cache (
                namespace_hash BLOB NOT NULL CHECK(length(namespace_hash) = 32),
                stage TEXT NOT NULL CHECK(stage IN ('wide', 'rerank')),
                request_fingerprint BLOB NOT NULL CHECK(length(request_fingerprint) = 32),
                response_bytes BLOB NOT NULL,
                received_at_unix_ms INTEGER NOT NULL,
                ttl_seconds INTEGER NOT NULL,
                model TEXT NOT NULL,
                model_revision TEXT,
                input_tokens INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL,
                attempt_id TEXT,
                PRIMARY KEY (namespace_hash, stage, request_fingerprint)
            ) STRICT;",
        )?;
        Ok(Self { db_path })
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    pub fn get(&self, query: &CacheLookupQuery<'_>) -> Result<CacheLookupResult, CacheError> {
        <Self as ResponseCache>::get(self, query)
    }

    pub fn put(
        &self,
        key: &CacheKey,
        namespace: &CacheNamespace,
        entry: CachedResponseEntry,
    ) -> Result<(), CacheError> {
        <Self as ResponseCache>::put(self, key, namespace, entry)
    }

    pub fn evict_namespace(
        &self,
        key: &CacheKey,
        namespace: &CacheNamespace,
    ) -> Result<usize, CacheError> {
        <Self as ResponseCache>::evict_namespace(self, key, namespace)
    }
}

type CachedResponseRow = (
    Vec<u8>,
    i64,
    i64,
    String,
    Option<String>,
    i64,
    i64,
    Option<String>,
);

impl ResponseCache for SqliteResponseCache {
    fn sqlite_path(&self) -> Option<&Path> {
        Some(&self.db_path)
    }

    fn get(&self, query: &CacheLookupQuery<'_>) -> Result<CacheLookupResult, CacheError> {
        let conn = open_qualified_connection(&self.db_path)
            .map_err(|e| CacheError::StorageError(e.to_string()))?;
        let ns_hash = MemoryResponseCache::namespace_hash(query.key, query.namespace);
        let stage_str = query.stage.as_str();
        let fp_bytes = query.fingerprint.as_bytes();

        let row: Option<CachedResponseRow> = conn
            .query_row(
                "SELECT response_bytes, received_at_unix_ms, ttl_seconds, model, model_revision, input_tokens, output_tokens, attempt_id
                 FROM sr_response_cache
                 WHERE namespace_hash = ?1 AND stage = ?2 AND request_fingerprint = ?3",
                params![&ns_hash[..], stage_str, &fp_bytes[..]],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| CacheError::StorageError(e.to_string()))?;

        let Some((resp_bytes, rec_ms, ttl_s, model, rev, in_tok, out_tok, att_id)) = row else {
            return Ok(CacheLookupResult::Miss);
        };

        let entry = CachedResponseEntry {
            stage: query.stage,
            request_fingerprint: *query.fingerprint,
            response_bytes: resp_bytes,
            received_at_unix_ms: rec_ms as u64,
            ttl_seconds: ttl_s as u32,
            model,
            model_revision: rev,
            original_usage: Usage {
                input_tokens: in_tok as u64,
                output_tokens: out_tok as u64,
            },
            attempt_id: att_id,
        };

        let freshness =
            entry.evaluate_freshness(query.now_unix_ms, query.active_model, query.active_revision);

        match freshness {
            FreshnessStatus::Fresh {
                age_ms,
                remaining_ttl_ms,
            } => Ok(CacheLookupResult::Hit {
                entry,
                age_ms,
                remaining_ttl_ms,
            }),
            status => Ok(CacheLookupResult::Stale { entry, status }),
        }
    }

    fn put(
        &self,
        key: &CacheKey,
        namespace: &CacheNamespace,
        entry: CachedResponseEntry,
    ) -> Result<(), CacheError> {
        let conn = open_qualified_connection(&self.db_path)
            .map_err(|e| CacheError::StorageError(e.to_string()))?;
        let ns_hash = MemoryResponseCache::namespace_hash(key, namespace);
        let stage_str = entry.stage.as_str();
        let fp_bytes = entry.request_fingerprint.as_bytes();

        conn.execute(
            "INSERT INTO sr_response_cache (
                namespace_hash, stage, request_fingerprint, response_bytes,
                received_at_unix_ms, ttl_seconds, model, model_revision,
                input_tokens, output_tokens, attempt_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(namespace_hash, stage, request_fingerprint) DO UPDATE SET
                response_bytes = excluded.response_bytes,
                received_at_unix_ms = excluded.received_at_unix_ms,
                ttl_seconds = excluded.ttl_seconds,
                model = excluded.model,
                model_revision = excluded.model_revision,
                input_tokens = excluded.input_tokens,
                output_tokens = excluded.output_tokens,
                attempt_id = excluded.attempt_id",
            params![
                &ns_hash[..],
                stage_str,
                &fp_bytes[..],
                &entry.response_bytes[..],
                entry.received_at_unix_ms as i64,
                entry.ttl_seconds as i64,
                &entry.model,
                &entry.model_revision,
                entry.original_usage.input_tokens as i64,
                entry.original_usage.output_tokens as i64,
                &entry.attempt_id
            ],
        )
        .map_err(|e| CacheError::StorageError(e.to_string()))?;

        Ok(())
    }

    fn evict_namespace(
        &self,
        key: &CacheKey,
        namespace: &CacheNamespace,
    ) -> Result<usize, CacheError> {
        let conn = open_qualified_connection(&self.db_path)
            .map_err(|e| CacheError::StorageError(e.to_string()))?;
        let ns_hash = MemoryResponseCache::namespace_hash(key, namespace);
        let deleted = conn
            .execute(
                "DELETE FROM sr_response_cache WHERE namespace_hash = ?1",
                params![&ns_hash[..]],
            )
            .map_err(|e| CacheError::StorageError(e.to_string()))?;
        Ok(deleted)
    }
}

/// Helper for coordinating response execution and usage accounting.
pub struct SingleFlightCoordinator {
    policy: CoordinationPolicy,
    memory: MemoryCoordinator,
    sqlite: Option<SqliteLeaseCoordinator>,
}

/// Query parameters for coordinating an exact provider request.
#[derive(Debug, Clone, Copy)]
pub struct CoordinateRequestQuery<'a> {
    pub key: &'a CacheKey,
    pub namespace: &'a CacheNamespace,
    pub stage: RequestStage,
    pub request_fingerprint: &'a RequestFingerprint,
    pub deadline_unix_ms: u64,
    pub active_model: &'a str,
    pub active_revision: Option<&'a str>,
}

impl LeaseCoordinator for SingleFlightCoordinator {
    fn acquire(
        &self,
        key: CoordinationKey,
        now_unix_ms: u64,
        policy: &CoordinationPolicy,
    ) -> Result<LeaseAcquisition, CoordinationError> {
        if let Some(sql) = &self.sqlite {
            sql.acquire(key, now_unix_ms, policy)
        } else {
            self.memory.acquire(key, now_unix_ms, policy)
        }
    }

    fn force_reacquire(
        &self,
        key: CoordinationKey,
        now_unix_ms: u64,
        policy: &CoordinationPolicy,
    ) -> Result<LeaseAcquisition, CoordinationError> {
        if let Some(sql) = &self.sqlite {
            sql.force_reacquire(key, now_unix_ms, policy)
        } else {
            self.memory.force_reacquire(key, now_unix_ms, policy)
        }
    }

    fn complete(
        &self,
        key: CoordinationKey,
        owner_token: OwnerToken,
        generation: FencingGeneration,
        now_unix_ms: u64,
    ) -> Result<PublishOutcome, CoordinationError> {
        if let Some(sql) = &self.sqlite {
            sql.complete(key, owner_token, generation, now_unix_ms)
        } else {
            self.memory
                .complete(key, owner_token, generation, now_unix_ms)
        }
    }

    fn check_lease(&self, key: CoordinationKey) -> Result<Option<LeaseRecord>, CoordinationError> {
        if let Some(sql) = &self.sqlite {
            sql.check_lease(key)
        } else {
            self.memory.check_lease(key)
        }
    }
}

impl SingleFlightCoordinator {
    /// Creates an in-process-only coordinator (memory).
    pub fn memory_only(policy: CoordinationPolicy) -> Self {
        Self {
            policy,
            memory: MemoryCoordinator::new(),
            sqlite: None,
        }
    }

    /// Creates a coordinator with optional cross-process SQLite backing.
    pub fn new(
        policy: CoordinationPolicy,
        db_path: Option<&Path>,
    ) -> Result<Self, CoordinationError> {
        let sqlite = if policy.cross_process_allowed {
            if let Some(path) = db_path {
                Some(SqliteLeaseCoordinator::open(path)?)
            } else {
                None
            }
        } else {
            None
        };

        Ok(Self {
            policy,
            memory: MemoryCoordinator::new(),
            sqlite,
        })
    }

    fn execute_as_leader<C, F>(
        &self,
        query: &CoordinateRequestQuery<'_>,
        coord_key: CoordinationKey,
        leader: LeaderContext,
        cache: &C,
        now_fn: &impl Fn() -> u64,
        execute_provider: F,
    ) -> Result<CoordinatedResponse, CoordinationError>
    where
        C: ResponseCache + ?Sized,
        F: FnOnce(&str) -> Result<(CachedResponseEntry, Usage), String>,
    {
        check_request_deadline(query, now_fn())?;

        let attempt_id = leader.attempt_id.clone();
        let (response_entry, usage) = execute_provider(&attempt_id).map_err(|e| {
            CoordinationError::StorageError(format!("provider execution failed: {e}"))
        })?;

        let finish_now = now_fn();
        check_request_deadline(query, finish_now)?;

        let outcome = if let Some(sql) = &self.sqlite {
            let cache_entry = if self.policy.cache_enabled {
                Some((query.key, query.namespace, &response_entry))
            } else {
                None
            };
            sql.complete_and_publish(
                coord_key,
                leader.owner_token,
                leader.fencing_generation,
                finish_now,
                cache_entry,
            )?
        } else {
            self.memory.complete_and_publish(
                coord_key,
                leader.owner_token,
                leader.fencing_generation,
                finish_now,
                || {
                    if self.policy.cache_enabled {
                        cache
                            .put(query.key, query.namespace, response_entry.clone())
                            .map_err(CoordinationError::CacheError)?;
                    }
                    Ok(())
                },
            )?
        };

        check_request_deadline(query, now_fn())?;

        match outcome {
            PublishOutcome::Published => Ok(CoordinatedResponse {
                entry: response_entry,
                served_from_cache: false,
                new_requests: 1,
                new_tokens: usage.total_tokens(),
                attempt_id: Some(attempt_id),
                is_follower: false,
            }),
            PublishOutcome::Superseded {
                expected_generation,
                current_generation,
            } => Err(CoordinationError::StorageError(format!(
                "leader superseded (gen {:?}, current {:?}); quiet fallback",
                expected_generation, current_generation
            ))),
        }
    }

    fn wait_as_follower<C>(
        &self,
        query: &CoordinateRequestQuery<'_>,
        coord_key: CoordinationKey,
        cache: &C,
        now_fn: &impl Fn() -> u64,
    ) -> Result<CoordinatedResponse, CoordinationError>
    where
        C: ResponseCache + ?Sized,
    {
        if !self.policy.cache_enabled {
            // With cache disabled, cross-process response sharing is forbidden
            return Err(CoordinationError::StorageError(
                "--no-cache disables response sharing; coordination stores no bodies".to_string(),
            ));
        }

        // Follower wait loop
        let resolution = if let Some(sql) = &self.sqlite {
            sql.wait_for_completion(
                coord_key,
                now_fn,
                query.deadline_unix_ms,
                Duration::from_millis(15),
            )?
        } else {
            self.memory
                .wait_for_completion(coord_key, now_fn, query.deadline_unix_ms)
        };

        match resolution {
            FollowerResolution::Completed => {
                let lookup = cache.get(&CacheLookupQuery {
                    key: query.key,
                    namespace: query.namespace,
                    stage: query.stage,
                    fingerprint: query.request_fingerprint,
                    now_unix_ms: now_fn(),
                    active_model: query.active_model,
                    active_revision: query.active_revision,
                })?;
                check_request_deadline(query, now_fn())?;
                if let CacheLookupResult::Hit { entry, .. } = lookup {
                    Ok(CoordinatedResponse {
                        entry,
                        served_from_cache: true,
                        new_requests: 0,
                        new_tokens: 0,
                        attempt_id: None,
                        is_follower: true,
                    })
                } else {
                    Err(CoordinationError::StorageError(
                        "leader completed but no cached response found".to_string(),
                    ))
                }
            }
            FollowerResolution::DeadlineExceeded => Err(CoordinationError::StorageError(
                "follower deadline exceeded; quiet fallback".to_string(),
            )),
            FollowerResolution::LeaseExpired => Err(CoordinationError::StorageError(
                "leader lease expired without completion".to_string(),
            )),
            FollowerResolution::CacheDisabled => Err(CoordinationError::StorageError(
                "cache disabled; cannot read response body".to_string(),
            )),
            FollowerResolution::LeaderFailed => Err(CoordinationError::StorageError(
                "leader failed or cancelled without completing".to_string(),
            )),
            FollowerResolution::Reused(entry) => Ok(CoordinatedResponse {
                entry,
                served_from_cache: true,
                new_requests: 0,
                new_tokens: 0,
                attempt_id: None,
                is_follower: true,
            }),
        }
    }

    /// Primary entry point: coordinates execution of an exact provider request.
    ///
    /// Shares ONLY the validated response. Decisions, identity, and exposures remain distinct.
    pub fn coordinate_request<C, F>(
        &self,
        query: &CoordinateRequestQuery<'_>,
        cache: &C,
        now_fn: impl Fn() -> u64,
        execute_provider: F,
    ) -> Result<CoordinatedResponse, CoordinationError>
    where
        C: ResponseCache + ?Sized,
        F: FnOnce(&str) -> Result<(CachedResponseEntry, Usage), String>,
    {
        let coord_key =
            CoordinationKey::compute(query.key, query.namespace, query.request_fingerprint);
        let now = now_fn();
        check_request_deadline(query, now)?;

        // Validate that if cross-process SQLite coordination is active and caching is enabled,
        // the supplied cache backend is bound to the exact same SQLite database.
        if self.policy.cache_enabled
            && let Some(sql) = &self.sqlite
        {
            match cache.sqlite_path() {
                Some(path)
                    if crate::storage::storage_path(path.to_path_buf())
                        == crate::storage::storage_path(sql.db_path().to_path_buf()) => {}
                Some(other) => {
                    return Err(CoordinationError::StorageError(format!(
                        "response cache store mismatch: coordinator bound to SQLite {:?}, supplied cache bound to {:?}",
                        sql.db_path(),
                        other
                    )));
                }
                None => {
                    return Err(CoordinationError::StorageError(format!(
                        "response cache store mismatch: coordinator bound to SQLite {:?}, supplied cache is in-memory",
                        sql.db_path()
                    )));
                }
            }
        }

        // 1. Initial cache check
        if self.policy.cache_enabled {
            let lookup = cache.get(&CacheLookupQuery {
                key: query.key,
                namespace: query.namespace,
                stage: query.stage,
                fingerprint: query.request_fingerprint,
                now_unix_ms: now,
                active_model: query.active_model,
                active_revision: query.active_revision,
            })?;
            check_request_deadline(query, now_fn())?;
            if let CacheLookupResult::Hit { entry, .. } = lookup {
                return Ok(CoordinatedResponse {
                    entry,
                    served_from_cache: true,
                    new_requests: 0,
                    new_tokens: 0,
                    attempt_id: None,
                    is_follower: false,
                });
            }
        }

        // 2. Select backend
        let coordinator: &dyn LeaseCoordinator = if let Some(sql) = &self.sqlite {
            sql
        } else {
            &self.memory
        };

        // 3. Acquire lease
        let acquire_now = now_fn();
        check_request_deadline(query, acquire_now)?;
        let acq = coordinator.acquire(coord_key, acquire_now, &self.policy)?;
        check_request_deadline(query, now_fn())?;
        match acq {
            LeaseAcquisition::AlreadyCompleted => {
                if self.policy.cache_enabled {
                    let lookup = cache.get(&CacheLookupQuery {
                        key: query.key,
                        namespace: query.namespace,
                        stage: query.stage,
                        fingerprint: query.request_fingerprint,
                        now_unix_ms: now_fn(),
                        active_model: query.active_model,
                        active_revision: query.active_revision,
                    })?;
                    check_request_deadline(query, now_fn())?;
                    if let CacheLookupResult::Hit { entry, .. } = lookup {
                        return Ok(CoordinatedResponse {
                            entry,
                            served_from_cache: true,
                            new_requests: 0,
                            new_tokens: 0,
                            attempt_id: None,
                            is_follower: true,
                        });
                    }

                    // Cache entry missing or stale despite completed lease flag.
                    // Reacquire or follow without preemption storm!
                    let reacquired =
                        coordinator.force_reacquire(coord_key, now_fn(), &self.policy)?;
                    match reacquired {
                        LeaseAcquisition::Leading(leader) => self.execute_as_leader(
                            query,
                            coord_key,
                            leader,
                            cache,
                            &now_fn,
                            execute_provider,
                        ),
                        LeaseAcquisition::Following(_follower) => {
                            self.wait_as_follower(query, coord_key, cache, &now_fn)
                        }
                        LeaseAcquisition::AlreadyCompleted => {
                            let lookup2 = cache.get(&CacheLookupQuery {
                                key: query.key,
                                namespace: query.namespace,
                                stage: query.stage,
                                fingerprint: query.request_fingerprint,
                                now_unix_ms: now_fn(),
                                active_model: query.active_model,
                                active_revision: query.active_revision,
                            })?;
                            check_request_deadline(query, now_fn())?;
                            if let CacheLookupResult::Hit { entry, .. } = lookup2 {
                                Ok(CoordinatedResponse {
                                    entry,
                                    served_from_cache: true,
                                    new_requests: 0,
                                    new_tokens: 0,
                                    attempt_id: None,
                                    is_follower: true,
                                })
                            } else {
                                Err(CoordinationError::StorageError(
                                    "completed lease missing cache entry; quiet fallback"
                                        .to_string(),
                                ))
                            }
                        }
                    }
                } else {
                    // If cache disabled, cannot read shared body
                    Err(CoordinationError::StorageError(
                        "cache disabled; cannot share response body".to_string(),
                    ))
                }
            }
            LeaseAcquisition::Leading(leader) => {
                self.execute_as_leader(query, coord_key, leader, cache, &now_fn, execute_provider)
            }
            LeaseAcquisition::Following(_follower) => {
                self.wait_as_follower(query, coord_key, cache, &now_fn)
            }
        }
    }
}

fn check_request_deadline(
    query: &CoordinateRequestQuery<'_>,
    now_unix_ms: u64,
) -> Result<(), CoordinationError> {
    if now_unix_ms >= query.deadline_unix_ms {
        return Err(CoordinationError::StorageError(
            "coordinated request deadline exceeded; quiet fallback".to_string(),
        ));
    }
    Ok(())
}

/// Final outcome of a coordinated request execution.
#[derive(Clone, Debug)]
pub struct CoordinatedResponse {
    pub entry: CachedResponseEntry,
    pub served_from_cache: bool,
    pub new_requests: u64,
    pub new_tokens: u64,
    pub attempt_id: Option<String>,
    pub is_follower: bool,
}

impl SqliteLeaseCoordinator {
    pub(crate) fn acquire_in_transaction(
        tx: &rusqlite::Transaction<'_>,
        key: CoordinationKey,
        now_unix_ms: u64,
        policy: &CoordinationPolicy,
    ) -> Result<LeaseAcquisition, CoordinationError> {
        let row: Option<(Vec<u8>, i64, i64, i64, String, i64)> = tx
            .query_row(
                "SELECT owner_token, fencing_generation, acquired_at_unix_ms, expires_at_unix_ms, attempt_id, is_completed
                 FROM sr_coordination_leases WHERE coordination_key = ?1",
                params![key.as_bytes()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .optional()?;

        if let Some((_raw_token, gen_i64, _acq, exp_i64, _att, completed_i64)) = row {
            let cur_fence_gen = FencingGeneration(gen_i64 as u64);
            let expires_at = exp_i64 as u64;
            let is_completed = completed_i64 == 1;

            if now_unix_ms < expires_at {
                if is_completed {
                    return Ok(LeaseAcquisition::AlreadyCompleted);
                }
                return Ok(LeaseAcquisition::Following(FollowerContext {
                    key,
                    leader_generation: cur_fence_gen,
                    lease_expires_at_unix_ms: expires_at,
                }));
            }

            // Expired lease -> successor reacquires with bumped fencing generation
            let new_gen = cur_fence_gen.next();
            let new_token = OwnerToken::generate()
                .map_err(|e| CoordinationError::StorageError(e.to_string()))?;
            let new_expires_at = now_unix_ms.saturating_add(policy.lease_ttl_ms);
            let new_attempt_id = format!("att-proc-{}", new_gen.as_u64());

            tx.execute(
                "UPDATE sr_coordination_leases SET
                    owner_token = ?1,
                    fencing_generation = ?2,
                    acquired_at_unix_ms = ?3,
                    expires_at_unix_ms = ?4,
                    attempt_id = ?5,
                    is_completed = 0
                 WHERE coordination_key = ?6",
                params![
                    new_token.as_bytes(),
                    new_gen.as_u64() as i64,
                    now_unix_ms as i64,
                    new_expires_at as i64,
                    new_attempt_id,
                    key.as_bytes()
                ],
            )?;

            return Ok(LeaseAcquisition::Leading(LeaderContext {
                key,
                owner_token: new_token,
                fencing_generation: new_gen,
                lease_expires_at_unix_ms: new_expires_at,
                attempt_id: new_attempt_id,
            }));
        }

        // New lease row
        let new_token =
            OwnerToken::generate().map_err(|e| CoordinationError::StorageError(e.to_string()))?;
        let init_gen = FencingGeneration::initial();
        let new_expires_at = now_unix_ms.saturating_add(policy.lease_ttl_ms);
        let new_attempt_id = format!("att-proc-{}", init_gen.as_u64());

        tx.execute(
            "INSERT INTO sr_coordination_leases (
                coordination_key, owner_token, fencing_generation, acquired_at_unix_ms, expires_at_unix_ms, attempt_id, is_completed
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
            params![
                key.as_bytes(),
                new_token.as_bytes(),
                init_gen.as_u64() as i64,
                now_unix_ms as i64,
                new_expires_at as i64,
                new_attempt_id
            ],
        )?;

        Ok(LeaseAcquisition::Leading(LeaderContext {
            key,
            owner_token: new_token,
            fencing_generation: init_gen,
            lease_expires_at_unix_ms: new_expires_at,
            attempt_id: new_attempt_id,
        }))
    }
}

impl SqliteLeaseCoordinator {
    pub(crate) fn force_reacquire_in_transaction(
        tx: &rusqlite::Transaction<'_>,
        key: CoordinationKey,
        now_unix_ms: u64,
        policy: &CoordinationPolicy,
    ) -> Result<LeaseAcquisition, CoordinationError> {
        let row: Option<(Vec<u8>, i64, i64, i64, String, i64)> = tx
            .query_row(
                "SELECT owner_token, fencing_generation, acquired_at_unix_ms, expires_at_unix_ms, attempt_id, is_completed
                 FROM sr_coordination_leases WHERE coordination_key = ?1",
                params![key.as_bytes()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .optional()?;

        if let Some((_raw_token, gen_i64, _acq, exp_i64, _att, completed_i64)) = row {
            let cur_fence_gen = FencingGeneration(gen_i64 as u64);
            let expires_at = exp_i64 as u64;
            let is_completed = completed_i64 == 1;

            // If another leader already reacquired to refresh and is currently unexpired, follow them
            if !is_completed && now_unix_ms < expires_at {
                return Ok(LeaseAcquisition::Following(FollowerContext {
                    key,
                    leader_generation: cur_fence_gen,
                    lease_expires_at_unix_ms: expires_at,
                }));
            }

            let new_gen = cur_fence_gen.next();
            let new_token = OwnerToken::generate()
                .map_err(|e| CoordinationError::StorageError(e.to_string()))?;
            let new_expires_at = now_unix_ms.saturating_add(policy.lease_ttl_ms);
            let new_attempt_id = format!("att-proc-{}", new_gen.as_u64());

            tx.execute(
                "UPDATE sr_coordination_leases SET
                    owner_token = ?1,
                    fencing_generation = ?2,
                    acquired_at_unix_ms = ?3,
                    expires_at_unix_ms = ?4,
                    attempt_id = ?5,
                    is_completed = 0
                 WHERE coordination_key = ?6",
                params![
                    new_token.as_bytes(),
                    new_gen.as_u64() as i64,
                    now_unix_ms as i64,
                    new_expires_at as i64,
                    new_attempt_id,
                    key.as_bytes()
                ],
            )?;

            return Ok(LeaseAcquisition::Leading(LeaderContext {
                key,
                owner_token: new_token,
                fencing_generation: new_gen,
                lease_expires_at_unix_ms: new_expires_at,
                attempt_id: new_attempt_id,
            }));
        }

        let new_token =
            OwnerToken::generate().map_err(|e| CoordinationError::StorageError(e.to_string()))?;
        let init_gen = FencingGeneration::initial();
        let new_expires_at = now_unix_ms.saturating_add(policy.lease_ttl_ms);
        let new_attempt_id = format!("att-proc-{}", init_gen.as_u64());

        tx.execute(
            "INSERT INTO sr_coordination_leases (
                coordination_key, owner_token, fencing_generation, acquired_at_unix_ms, expires_at_unix_ms, attempt_id, is_completed
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
            params![
                key.as_bytes(),
                new_token.as_bytes(),
                init_gen.as_u64() as i64,
                now_unix_ms as i64,
                new_expires_at as i64,
                new_attempt_id
            ],
        )?;

        Ok(LeaseAcquisition::Leading(LeaderContext {
            key,
            owner_token: new_token,
            fencing_generation: init_gen,
            lease_expires_at_unix_ms: new_expires_at,
            attempt_id: new_attempt_id,
        }))
    }
}

impl SqliteLeaseCoordinator {
    pub(crate) fn active_lease_in_transaction(
        tx: &rusqlite::Transaction<'_>,
        leader: &LeaderContext,
        now: u64,
    ) -> Result<bool, CoordinationError> {
        let row: Option<(Vec<u8>, i64, i64, i64, i64)> = tx
            .query_row(
                "SELECT owner_token, fencing_generation, expires_at_unix_ms, is_completed, acquired_at_unix_ms \
                 FROM sr_coordination_leases WHERE coordination_key=?1",
                params![leader.key.as_bytes()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        Ok(
            row.is_some_and(|(token, generation, expiry, completed, acquired)| {
                token.as_slice() == leader.owner_token.as_bytes()
                    && u64::try_from(generation).ok() == Some(leader.fencing_generation.as_u64())
                    && u64::try_from(expiry).ok() == Some(leader.lease_expires_at_unix_ms)
                    && completed == 0
                    && u64::try_from(acquired).is_ok_and(|acquired| now >= acquired)
                    && now < leader.lease_expires_at_unix_ms
            }),
        )
    }
}

impl SqliteLeaseCoordinator {
    pub(crate) fn complete_in_transaction(
        tx: &rusqlite::Transaction<'_>,
        key: CoordinationKey,
        owner_token: OwnerToken,
        generation: FencingGeneration,
        now_unix_ms: u64,
        cache_entry: Option<(&CacheKey, &CacheNamespace, &CachedResponseEntry)>,
    ) -> Result<PublishOutcome, CoordinationError> {
        let row: Option<(Vec<u8>, i64, i64, i64)> = tx
            .query_row(
                "SELECT owner_token, fencing_generation, expires_at_unix_ms, is_completed
                 FROM sr_coordination_leases WHERE coordination_key = ?1",
                params![key.as_bytes()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;

        let Some((cur_token, cur_gen_i64, exp_i64, _completed)) = row else {
            return Ok(PublishOutcome::Superseded {
                expected_generation: generation,
                current_generation: None,
            });
        };

        let cur_gen = FencingGeneration(cur_gen_i64 as u64);
        let expires_at = exp_i64 as u64;

        if cur_token.as_slice() != owner_token.as_bytes() || cur_gen != generation {
            return Ok(PublishOutcome::Superseded {
                expected_generation: generation,
                current_generation: Some(cur_gen),
            });
        }

        if now_unix_ms >= expires_at {
            return Ok(PublishOutcome::Superseded {
                expected_generation: generation,
                current_generation: Some(cur_gen),
            });
        }

        if let Some((cache_key, cache_ns, entry)) = cache_entry {
            let ns_hash = MemoryResponseCache::namespace_hash(cache_key, cache_ns);
            let stage_str = entry.stage.as_str();
            let fp_bytes = entry.request_fingerprint.as_bytes();

            tx.execute(
                "INSERT INTO sr_response_cache (
                    namespace_hash, stage, request_fingerprint, response_bytes,
                    received_at_unix_ms, ttl_seconds, model, model_revision,
                    input_tokens, output_tokens, attempt_id
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(namespace_hash, stage, request_fingerprint) DO UPDATE SET
                    response_bytes = excluded.response_bytes,
                    received_at_unix_ms = excluded.received_at_unix_ms,
                    ttl_seconds = excluded.ttl_seconds,
                    model = excluded.model,
                    model_revision = excluded.model_revision,
                    input_tokens = excluded.input_tokens,
                    output_tokens = excluded.output_tokens,
                    attempt_id = excluded.attempt_id",
                params![
                    &ns_hash[..],
                    stage_str,
                    &fp_bytes[..],
                    &entry.response_bytes[..],
                    entry.received_at_unix_ms as i64,
                    entry.ttl_seconds as i64,
                    &entry.model,
                    &entry.model_revision,
                    entry.original_usage.input_tokens as i64,
                    entry.original_usage.output_tokens as i64,
                    &entry.attempt_id
                ],
            )?;
        }

        tx.execute(
            "UPDATE sr_coordination_leases SET is_completed = 1 WHERE coordination_key = ?1",
            params![key.as_bytes()],
        )?;

        Ok(PublishOutcome::Published)
    }
}

impl SqliteLeaseCoordinator {
    pub(crate) fn check_lease_on_connection(
        conn: &Connection,
        key: CoordinationKey,
    ) -> Result<Option<LeaseRecord>, CoordinationError> {
        let row: Option<(Vec<u8>, i64, i64, i64, String, i64)> = conn
            .query_row(
                "SELECT owner_token, fencing_generation, acquired_at_unix_ms, expires_at_unix_ms, attempt_id, is_completed
                 FROM sr_coordination_leases WHERE coordination_key = ?1",
                params![key.as_bytes()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .optional()?;

        let Some((token_bytes, gen_i64, acq, exp, att, comp)) = row else {
            return Ok(None);
        };

        let mut token_arr = [0u8; 16];
        if token_bytes.len() == 16 {
            token_arr.copy_from_slice(&token_bytes);
        }

        Ok(Some(LeaseRecord {
            owner_token: OwnerToken::from_bytes(token_arr),
            fencing_generation: FencingGeneration(gen_i64 as u64),
            acquired_at_unix_ms: acq as u64,
            expires_at_unix_ms: exp as u64,
            attempt_id: att,
            is_completed: comp == 1,
        }))
    }
}
