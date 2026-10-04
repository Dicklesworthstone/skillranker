//! Fenced lease metadata for the production `CacheStore`.
//!
//! The caller supplies an already qualified SQLite connection/transaction.
//! This module never opens a database, stores response bodies, waits for a
//! follower, or runs provider work. The pipeline owns those effects; response
//! publication and lease completion share `CacheStore::publish_evaluation`'s
//! transaction in the sole production `cache.sqlite3`.

use super::fingerprint::{CacheKey, CacheNamespace, RequestFingerprint};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::Read;

/// Default lease TTL in milliseconds (5,000 ms).
pub const DEFAULT_LEASE_TTL_MS: u64 = 5_000;

/// Error kinds during single-flight coordination.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CoordinationError {
    StorageBusy,
    StorageError(String),
    InvalidTimestamp,
}

impl fmt::Display for CoordinationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StorageBusy => f.write_str("coordination storage is busy"),
            Self::StorageError(e) => write!(f, "coordination storage error: {e}"),
            Self::InvalidTimestamp => {
                f.write_str("invalid or rolling-back timestamp during coordination")
            }
        }
    }
}

impl std::error::Error for CoordinationError {}

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

// SQLite stores signed integers. Refuse values outside its admitted domain
// before changing timestamps or fencing generations.
fn lease_expiry(now: u64, ttl: u64) -> Result<u64, CoordinationError> {
    now.checked_add(ttl)
        .filter(|expiry| ttl != 0 && *expiry <= i64::MAX as u64)
        .ok_or(CoordinationError::InvalidTimestamp)
}

fn next_lease_generation(
    current: FencingGeneration,
) -> Result<FencingGeneration, CoordinationError> {
    current
        .as_u64()
        .checked_add(1)
        .filter(|next| current.as_u64() != 0 && *next <= i64::MAX as u64)
        .map(FencingGeneration)
        .ok_or_else(|| {
            CoordinationError::StorageError("coordination fence generation exhausted".into())
        })
}

fn invalid_lease_record() -> CoordinationError {
    CoordinationError::StorageError("coordination lease contains invalid metadata".into())
}

impl LeaseRecord {
    fn check_time(&self, now: u64) -> Result<(), CoordinationError> {
        if now < self.acquired_at_unix_ms || now > i64::MAX as u64 {
            return Err(CoordinationError::InvalidTimestamp);
        }
        Ok(())
    }
}

/// Policy governing single-flight lease coordination.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CoordinationPolicy {
    /// Lease duration in milliseconds.
    pub lease_ttl_ms: u64,
}

impl Default for CoordinationPolicy {
    fn default() -> Self {
        Self {
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
    /// Lease completion was accepted in the caller-owned transaction.
    Published,
    /// The lease expired or was superseded by a successor with a higher fencing generation.
    /// The caller must withhold publication to preserve the current owner.
    Superseded {
        expected_generation: FencingGeneration,
        current_generation: Option<FencingGeneration>,
    },
}

/// Transaction primitives used only through the qualified storage boundary.
pub(crate) struct SqliteLeaseCoordinator;

impl SqliteLeaseCoordinator {
    pub(crate) fn acquire_in_transaction(
        tx: &rusqlite::Transaction<'_>,
        key: CoordinationKey,
        now_unix_ms: u64,
        policy: &CoordinationPolicy,
    ) -> Result<LeaseAcquisition, CoordinationError> {
        let admitted_expiry = lease_expiry(now_unix_ms, policy.lease_ttl_ms)?;
        let row = Self::check_lease_on_connection(tx, key)?;
        if let Some(existing) = row {
            existing.check_time(now_unix_ms)?;
            let cur_fence_gen = existing.fencing_generation;
            let expires_at = existing.expires_at_unix_ms;
            let is_completed = existing.is_completed;

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
            let new_gen = next_lease_generation(cur_fence_gen)?;
            let new_token = OwnerToken::generate()
                .map_err(|e| CoordinationError::StorageError(e.to_string()))?;
            let new_expires_at = admitted_expiry;
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
        let new_expires_at = admitted_expiry;
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
        let admitted_expiry = lease_expiry(now_unix_ms, policy.lease_ttl_ms)?;
        let row = Self::check_lease_on_connection(tx, key)?;
        if let Some(existing) = row {
            existing.check_time(now_unix_ms)?;
            let cur_fence_gen = existing.fencing_generation;
            let expires_at = existing.expires_at_unix_ms;
            let is_completed = existing.is_completed;

            // If another leader already reacquired to refresh and is currently unexpired, follow them
            if !is_completed && now_unix_ms < expires_at {
                return Ok(LeaseAcquisition::Following(FollowerContext {
                    key,
                    leader_generation: cur_fence_gen,
                    lease_expires_at_unix_ms: expires_at,
                }));
            }

            let new_gen = next_lease_generation(cur_fence_gen)?;
            let new_token = OwnerToken::generate()
                .map_err(|e| CoordinationError::StorageError(e.to_string()))?;
            let new_expires_at = admitted_expiry;
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
        let new_expires_at = admitted_expiry;
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
        let row = Self::check_lease_on_connection(tx, leader.key)?;
        Ok(row.is_some_and(|record| {
            !record.is_completed
                && record.owner_token == leader.owner_token
                && record.fencing_generation == leader.fencing_generation
                && record.expires_at_unix_ms == leader.lease_expires_at_unix_ms
                && now >= record.acquired_at_unix_ms
                && now < record.expires_at_unix_ms
        }))
    }
}

impl SqliteLeaseCoordinator {
    pub(crate) fn complete_in_transaction(
        tx: &rusqlite::Transaction<'_>,
        key: CoordinationKey,
        owner_token: OwnerToken,
        generation: FencingGeneration,
        now_unix_ms: u64,
    ) -> Result<PublishOutcome, CoordinationError> {
        let Some(record) = Self::check_lease_on_connection(tx, key)? else {
            return Ok(PublishOutcome::Superseded {
                expected_generation: generation,
                current_generation: None,
            });
        };
        record.check_time(now_unix_ms)?;
        // Completion is single-use. Replaying a successful publication must
        // never overwrite its response, even with the same token and fence.
        if record.is_completed
            || record.owner_token != owner_token
            || record.fencing_generation != generation
            || now_unix_ms >= record.expires_at_unix_ms
        {
            return Ok(PublishOutcome::Superseded {
                expected_generation: generation,
                current_generation: Some(record.fencing_generation),
            });
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

        let token_arr: [u8; 16] = token_bytes.try_into().map_err(|_| invalid_lease_record())?;
        let generation = u64::try_from(gen_i64).map_err(|_| invalid_lease_record())?;
        let acquired = u64::try_from(acq).map_err(|_| invalid_lease_record())?;
        let expires = u64::try_from(exp).map_err(|_| invalid_lease_record())?;
        if generation == 0 || expires < acquired || !matches!(comp, 0 | 1) || att.len() > 128 {
            return Err(invalid_lease_record());
        }
        Ok(Some(LeaseRecord {
            owner_token: OwnerToken::from_bytes(token_arr),
            fencing_generation: FencingGeneration(generation),
            acquired_at_unix_ms: acquired,
            expires_at_unix_ms: expires,
            attempt_id: att,
            is_completed: comp == 1,
        }))
    }
}

#[cfg(test)]
#[path = "coordination/integrity_tests.rs"]
mod integrity_tests;
