//! Single-flight response coordination with fenced bounded leases (sr-roadmap-l1i.5.10).
//!
//! Provides single-flight coordination keyed by namespace and exact request fingerprint.
//! Ensures that:
//! 1. Only a validated provider response is shared (no decisions, event identities, or exposures are shared).
//! 2. Lease ownership is tracked via a unique `OwnerToken` and strictly monotonic `FencingGeneration`.
//! 3. An expired leader cannot publish after a successor acquires the lease (superseded results become quiet fallback).
//! 4. Follower wait is strictly bounded by remaining deadline, returning quiet fallback on timeout.
//! 5. SQLite/coordination write transactions are short (< 1ms) and never held across HTTP calls.
//! 6. Coordination state stores NO response bodies; `--no-cache` disables cross-process response sharing.
//! 7. Request owner alone records provider attempts/usage; followers incur zero new requests and zero new tokens.

use super::fingerprint::{CacheKey, CacheNamespace, RequestFingerprint};
use super::response::{
    CacheError, CacheLookupQuery, CacheLookupResult, CachedResponseEntry, MemoryResponseCache,
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
    StorageError(String),
    CacheError(CacheError),
    InvalidTimestamp,
}

impl fmt::Display for CoordinationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LockPoisoned => f.write_str("coordination synchronization lock was poisoned"),
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
        hasher.update(b"SR_COORDINATION_KEY_V1\0");
        hasher.update(namespace.harness_id.as_str().as_bytes());
        hasher.update(&namespace.key_generation.to_le_bytes());
        if let Some(ws) = &namespace.workspace_id {
            hasher.update(ws.as_str().as_bytes());
        }
        if let Some(sess) = &namespace.session_id {
            hasher.update(sess.as_str().as_bytes());
        }
        if let Some(branch) = &namespace.branch_id {
            hasher.update(branch.as_str().as_bytes());
        }
        if let Some(epoch) = &namespace.context_epoch {
            hasher.update(epoch.as_str().as_bytes());
        }
        if let Some(adapter) = &namespace.adapter_id {
            hasher.update(adapter.as_str().as_bytes());
        }
        if let Some(ver) = &namespace.adapter_version {
            hasher.update(ver.as_str().as_bytes());
        }
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
            if existing.is_completed {
                return Ok(LeaseAcquisition::AlreadyCompleted);
            }

            if now_unix_ms < existing.expires_at_unix_ms {
                // Active lease exists; caller is a follower
                return Ok(LeaseAcquisition::Following(FollowerContext {
                    key,
                    leader_generation: existing.fencing_generation,
                    lease_expires_at_unix_ms: existing.expires_at_unix_ms,
                }));
            }

            // Existing lease expired without completion; successor reacquires with bumped generation
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

        if now_unix_ms > existing.expires_at_unix_ms {
            return Ok(PublishOutcome::Superseded {
                expected_generation: generation,
                current_generation: Some(existing.fencing_generation),
            });
        }

        existing.is_completed = true;

        // Wake waiting followers
        let (_, cvar) = &*self.notify;
        cvar.notify_all();

        Ok(PublishOutcome::Published)
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
                    return FollowerResolution::LeaderFailed; // caller will inspect cache
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

/// SQLite-backed lease coordinator for cross-process coordination.
///
/// Guarantees:
/// 1. Short transactions: SQLite write lock is held only for atomic lease check/insert/update (~1ms),
///    NEVER across an HTTP request.
/// 2. Zero response bodies: table `sr_coordination_leases` contains only tokens, generation, and timestamps.
pub struct SqliteLeaseCoordinator {
    db_path: PathBuf,
}

impl SqliteLeaseCoordinator {
    /// Creates or connects to a SQLite lease coordinator at `db_path`.
    pub fn open(db_path: impl AsRef<Path>) -> Result<Self, CoordinationError> {
        let db_path = db_path.as_ref().to_path_buf();
        let conn = Self::open_connection(&db_path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA busy_timeout = 25;
             CREATE TABLE IF NOT EXISTS sr_coordination_leases (
                 coordination_key BLOB PRIMARY KEY CHECK(length(coordination_key) = 32),
                 owner_token BLOB NOT NULL CHECK(length(owner_token) = 16),
                 fencing_generation INTEGER NOT NULL CHECK(fencing_generation >= 1),
                 acquired_at_unix_ms INTEGER NOT NULL,
                 expires_at_unix_ms INTEGER NOT NULL,
                 attempt_id TEXT NOT NULL,
                 is_completed INTEGER NOT NULL CHECK(is_completed IN (0, 1))
             ) STRICT;",
        )?;
        Ok(Self { db_path })
    }

    fn open_connection(path: &Path) -> Result<Connection, CoordinationError> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(Duration::from_millis(25))?;
        Ok(conn)
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }
}

impl LeaseCoordinator for SqliteLeaseCoordinator {
    fn acquire(
        &self,
        key: CoordinationKey,
        now_unix_ms: u64,
        policy: &CoordinationPolicy,
    ) -> Result<LeaseAcquisition, CoordinationError> {
        let mut conn = Self::open_connection(&self.db_path)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

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

            if is_completed {
                tx.commit()?;
                return Ok(LeaseAcquisition::AlreadyCompleted);
            }

            if now_unix_ms < expires_at {
                tx.commit()?;
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
            tx.commit()?;

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
        tx.commit()?;

        Ok(LeaseAcquisition::Leading(LeaderContext {
            key,
            owner_token: new_token,
            fencing_generation: init_gen,
            lease_expires_at_unix_ms: new_expires_at,
            attempt_id: new_attempt_id,
        }))
    }

    fn complete(
        &self,
        key: CoordinationKey,
        owner_token: OwnerToken,
        generation: FencingGeneration,
        now_unix_ms: u64,
    ) -> Result<PublishOutcome, CoordinationError> {
        let mut conn = Self::open_connection(&self.db_path)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        let row: Option<(Vec<u8>, i64, i64, i64)> = tx
            .query_row(
                "SELECT owner_token, fencing_generation, expires_at_unix_ms, is_completed
                 FROM sr_coordination_leases WHERE coordination_key = ?1",
                params![key.as_bytes()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;

        let Some((cur_token, cur_gen_i64, exp_i64, _completed)) = row else {
            tx.commit()?;
            return Ok(PublishOutcome::Superseded {
                expected_generation: generation,
                current_generation: None,
            });
        };

        let cur_gen = FencingGeneration(cur_gen_i64 as u64);
        let expires_at = exp_i64 as u64;

        if cur_token.as_slice() != owner_token.as_bytes() || cur_gen != generation {
            tx.commit()?;
            return Ok(PublishOutcome::Superseded {
                expected_generation: generation,
                current_generation: Some(cur_gen),
            });
        }

        if now_unix_ms > expires_at {
            tx.commit()?;
            return Ok(PublishOutcome::Superseded {
                expected_generation: generation,
                current_generation: Some(cur_gen),
            });
        }

        tx.execute(
            "UPDATE sr_coordination_leases SET is_completed = 1 WHERE coordination_key = ?1",
            params![key.as_bytes()],
        )?;
        tx.commit()?;

        Ok(PublishOutcome::Published)
    }

    fn check_lease(&self, key: CoordinationKey) -> Result<Option<LeaseRecord>, CoordinationError> {
        let conn = Self::open_connection(&self.db_path)?;
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

            let lease = self.check_lease(key)?;
            let Some(record) = lease else {
                return Ok(FollowerResolution::LeaderFailed);
            };

            if record.is_completed {
                return Ok(FollowerResolution::LeaderFailed); // caller retrieves from cache
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

    /// Primary entry point: coordinates execution of an exact provider request.
    ///
    /// Shares ONLY the validated response. Decisions, identity, and exposures remain distinct.
    pub fn coordinate_request<F>(
        &self,
        query: &CoordinateRequestQuery<'_>,
        cache: &MemoryResponseCache,
        now_fn: impl Fn() -> u64,
        execute_provider: F,
    ) -> Result<CoordinatedResponse, CoordinationError>
    where
        F: FnOnce(&str) -> Result<(CachedResponseEntry, Usage), String>,
    {
        let coord_key =
            CoordinationKey::compute(query.key, query.namespace, query.request_fingerprint);
        let now = now_fn();

        // 1. Initial cache check
        if self.policy.cache_enabled {
            let lookup = cache.get(&CacheLookupQuery {
                key: query.key,
                namespace: query.namespace,
                stage: super::fingerprint::RequestStage::Wide,
                fingerprint: query.request_fingerprint,
                now_unix_ms: now,
                active_model: query.active_model,
                active_revision: query.active_revision,
            })?;
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
        let acq = coordinator.acquire(coord_key, now, &self.policy)?;
        match acq {
            LeaseAcquisition::AlreadyCompleted => {
                if self.policy.cache_enabled {
                    let lookup = cache.get(&CacheLookupQuery {
                        key: query.key,
                        namespace: query.namespace,
                        stage: super::fingerprint::RequestStage::Wide,
                        fingerprint: query.request_fingerprint,
                        now_unix_ms: now_fn(),
                        active_model: query.active_model,
                        active_revision: query.active_revision,
                    })?;
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
                }
                // If cache disabled, cannot read shared body
                Err(CoordinationError::StorageError(
                    "cache disabled; cannot share response body".to_string(),
                ))
            }
            LeaseAcquisition::Leading(leader) => {
                // We are the leader! Execute the provider call
                let attempt_id = leader.attempt_id.clone();
                let (response_entry, usage) = execute_provider(&attempt_id).map_err(|e| {
                    CoordinationError::StorageError(format!("provider execution failed: {e}"))
                })?;

                let finish_now = now_fn();
                let outcome = coordinator.complete(
                    coord_key,
                    leader.owner_token,
                    leader.fencing_generation,
                    finish_now,
                )?;

                match outcome {
                    PublishOutcome::Published => {
                        // Put in cache if enabled
                        if self.policy.cache_enabled {
                            cache.put(query.key, query.namespace, response_entry.clone())?;
                        }
                        Ok(CoordinatedResponse {
                            entry: response_entry,
                            served_from_cache: false,
                            new_requests: 1,
                            new_tokens: usage.total_tokens(),
                            attempt_id: Some(attempt_id),
                            is_follower: false,
                        })
                    }
                    PublishOutcome::Superseded {
                        expected_generation,
                        current_generation,
                    } => {
                        // Late completion rejected; quiet fallback
                        Err(CoordinationError::StorageError(format!(
                            "leader superseded (gen {:?}, current {:?}); quiet fallback",
                            expected_generation, current_generation
                        )))
                    }
                }
            }
            LeaseAcquisition::Following(_follower) => {
                if !self.policy.cache_enabled {
                    // With cache disabled, cross-process response sharing is forbidden
                    return Err(CoordinationError::StorageError(
                        "--no-cache disables response sharing; coordination stores no bodies"
                            .to_string(),
                    ));
                }

                // Follower wait loop
                let resolution = if let Some(sql) = &self.sqlite {
                    sql.wait_for_completion(
                        coord_key,
                        &now_fn,
                        query.deadline_unix_ms,
                        Duration::from_millis(15),
                    )?
                } else {
                    self.memory
                        .wait_for_completion(coord_key, &now_fn, query.deadline_unix_ms)
                };

                match resolution {
                    FollowerResolution::DeadlineExceeded => Err(CoordinationError::StorageError(
                        "follower deadline exceeded; quiet fallback".to_string(),
                    )),
                    FollowerResolution::LeaseExpired => Err(CoordinationError::StorageError(
                        "leader lease expired without completion".to_string(),
                    )),
                    FollowerResolution::CacheDisabled => Err(CoordinationError::StorageError(
                        "cache disabled; cannot read response body".to_string(),
                    )),
                    FollowerResolution::Reused(entry) => Ok(CoordinatedResponse {
                        entry,
                        served_from_cache: true,
                        new_requests: 0,
                        new_tokens: 0,
                        attempt_id: None,
                        is_follower: true,
                    }),
                    FollowerResolution::LeaderFailed => {
                        // Check if response was published to cache
                        let lookup = cache.get(&CacheLookupQuery {
                            key: query.key,
                            namespace: query.namespace,
                            stage: super::fingerprint::RequestStage::Wide,
                            fingerprint: query.request_fingerprint,
                            now_unix_ms: now_fn(),
                            active_model: query.active_model,
                            active_revision: query.active_revision,
                        })?;
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
                }
            }
        }
    }
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
