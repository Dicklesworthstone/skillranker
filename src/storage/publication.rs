//! Publish one complete evaluation, never independently visible response stages.
//!
//! Only bounded database work runs inside the transaction. Provider calls and
//! response validation finish before this boundary. The disposable cache keeps
//! its existing NORMAL durability policy; atomicity is not a power-loss promise.

use super::{
    CacheStore, MAX_RESPONSE_TTL_SECONDS, StoreError, cache_wall_clock_ms, check_stamp, check_work,
    configure, coordination_error, refresh_busy_limit, sql_integer, storage_path,
};
use crate::blocking::{BlockingLeafKind, run_blocking_leaf};
use crate::cache::{
    CachedResponseEntry, LeaderContext, PublishOutcome, RequestStage, SqliteLeaseCoordinator,
};
use crate::jev::codec::MAX_RESPONSE_BYTES;
use crate::runtime::ProcessInvocation;
use asupersync::Cx;
use rusqlite::{Transaction, TransactionBehavior, params};
use std::path::PathBuf;

impl CacheStore {
    /// Replace this namespace's cached evaluation and complete its optional
    /// lease in one transaction. `rerank=None` is only for a validated Wide
    /// outcome that needs no Rerank. Callers must not publish a partial failure.
    ///
    /// A namespace retains at most one complete evaluation. Otherwise two Wide
    /// fingerprints sharing a Rerank fingerprint could form a mixed pair even
    /// when each writer commits atomically. Other namespaces are not replaced.
    /// The combined response-byte cap equals the existing single-record cap,
    /// keeping transaction size within the cache's existing mutation allowance.
    ///
    /// On success the lease is already complete: do not complete it again.
    /// On error no partial evaluation is published, but an error after commit
    /// can mean the complete evaluation and lease were committed together.
    pub fn publish_evaluation(
        mut self,
        invocation: &ProcessInvocation,
        cx: &Cx,
        namespace: [u8; 32],
        wide: CachedResponseEntry,
        rerank: Option<CachedResponseEntry>,
        fence: Option<(PathBuf, LeaderContext)>,
    ) -> Result<Self, StoreError> {
        validate_evaluation(&wide, rerank.as_ref())?;
        let clock = invocation.clock();
        let child = cx.clone();
        run_blocking_leaf(
            invocation,
            cx,
            BlockingLeafKind::Database,
            false,
            move || {
                if fence.as_ref().is_some_and(|(path, _)| {
                    storage_path(path.clone()) != self.directory.database_path()
                }) {
                    return Err(StoreError::LeaseUnavailable);
                }
                configure(&self.connection, clock, &child)?;
                self.directory
                    .verify_database_file(&self.file, clock, &child)?;
                refresh_busy_limit(&self.connection, clock, &child)?;
                let tx = self
                    .connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)?;
                check_stamp(&tx, self.stamp)?;
                if let Some((_, leader)) = &fence
                    && !SqliteLeaseCoordinator::active_lease_in_transaction(
                        &tx,
                        leader,
                        cache_wall_clock_ms(),
                    )
                    .map_err(coordination_error)?
                {
                    return Err(StoreError::LeaseSuperseded);
                }
                self.directory.admit_space()?;
                replace_responses(
                    &tx,
                    sql_integer(self.stamp.generation)?,
                    namespace,
                    &wide,
                    rerank.as_ref(),
                    sql_integer(cache_wall_clock_ms())?,
                    || check_work(clock, &child),
                )?;
                if let Some((_, leader)) = &fence {
                    // None means no helper-table body: both production response
                    // rows have already been written in this same transaction.
                    let outcome = SqliteLeaseCoordinator::complete_in_transaction(
                        &tx,
                        leader.key,
                        leader.owner_token,
                        leader.fencing_generation,
                        cache_wall_clock_ms(),
                        None,
                    )
                    .map_err(coordination_error)?;
                    if outcome != PublishOutcome::Published {
                        return Err(StoreError::LeaseSuperseded);
                    }
                }
                refresh_busy_limit(&tx, clock, &child)?;
                if fence.as_ref().is_some_and(|(_, leader)| {
                    cache_wall_clock_ms() >= leader.lease_expires_at_unix_ms
                }) {
                    return Err(StoreError::LeaseSuperseded);
                }
                tx.commit()?;
                self.directory
                    .verify_database_file(&self.file, clock, &child)?;
                check_work(clock, &child)?;
                Ok(self)
            },
        )
        .map_err(StoreError::Runtime)?
        .value
    }
}

pub(super) fn validate_response(entry: &CachedResponseEntry) -> Result<(), StoreError> {
    if entry.response_bytes.len() > MAX_RESPONSE_BYTES
        || !(1..=MAX_RESPONSE_TTL_SECONDS).contains(&entry.ttl_seconds)
        || entry.model.is_empty()
        || entry.model.len() > 256
        || entry.model_revision.as_ref().is_some_and(|r| r.len() > 256)
    {
        return Err(StoreError::Quota);
    }
    Ok(())
}

fn validate_evaluation(
    wide: &CachedResponseEntry,
    rerank: Option<&CachedResponseEntry>,
) -> Result<(), StoreError> {
    validate_response(wide)?;
    if wide.stage != RequestStage::Wide {
        return Err(StoreError::InvalidRecord);
    }
    if let Some(rerank) = rerank {
        validate_response(rerank)?;
        if rerank.stage != RequestStage::Rerank
            || rerank.model != wide.model
            || rerank.model_revision != wide.model_revision
            || rerank.received_at_unix_ms < wide.received_at_unix_ms
        {
            return Err(StoreError::InvalidRecord);
        }
        if wide
            .response_bytes
            .len()
            .saturating_add(rerank.response_bytes.len())
            > MAX_RESPONSE_BYTES
        {
            return Err(StoreError::Quota);
        }
    }
    Ok(())
}

pub(super) fn write_response(
    tx: &Transaction<'_>,
    generation: i64,
    namespace: [u8; 32],
    entry: &CachedResponseEntry,
    now_unix_ms: u64,
) -> Result<(), StoreError> {
    // The receipt on this boot's clock, back-dated by the wall-clock time
    // since receipt, so the delay before this write does not extend the
    // entry's life (sr-4t02). Absent where the platform has no boot clock.
    let (boot_id, boot_ms) = match super::boot_clock() {
        Some((id, now_ms)) => {
            let since_receipt = now_unix_ms.saturating_sub(entry.received_at_unix_ms);
            (
                Some(id),
                Some(sql_integer(now_ms.saturating_sub(since_receipt))?),
            )
        }
        None => (None, None),
    };
    tx.execute(
        "INSERT OR REPLACE INTO sr_cache_response VALUES \
         (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            generation,
            &namespace[..],
            entry.stage.as_str(),
            &entry.request_fingerprint.as_bytes()[..],
            entry.response_bytes,
            sql_integer(entry.received_at_unix_ms)?,
            entry.ttl_seconds,
            entry.model,
            entry.model_revision,
            sql_integer(entry.original_usage.input_tokens)?,
            sql_integer(entry.original_usage.output_tokens)?,
            boot_id,
            boot_ms,
        ],
    )?;
    Ok(())
}

fn replace_responses(
    tx: &Transaction<'_>,
    generation: i64,
    namespace: [u8; 32],
    wide: &CachedResponseEntry,
    rerank: Option<&CachedResponseEntry>,
    now: i64,
    mut checkpoint: impl FnMut() -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    checkpoint()?;
    // Preserve ordinary stale-row pruning while replacing this namespace's
    // *whole* evaluation, including any old Rerank after a Wide-only result.
    tx.execute(
        "DELETE FROM sr_cache_response WHERE generation<>?1 OR namespace=?2 \
         OR received_at_unix_ms>?3 OR received_at_unix_ms+ttl_seconds*1000<=?3",
        params![generation, &namespace[..], now],
    )?;
    for entry in std::iter::once(wide).chain(rerank) {
        checkpoint()?;
        write_response(
            tx,
            generation,
            namespace,
            entry,
            u64::try_from(now).unwrap_or(0),
        )?;
    }
    checkpoint()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::RequestFingerprint;
    use crate::jev::codec::Usage;

    fn entry(stage: RequestStage, body: &[u8]) -> CachedResponseEntry {
        CachedResponseEntry {
            stage,
            request_fingerprint: RequestFingerprint::from_bytes([1; 32]),
            response_bytes: body.to_vec(),
            received_at_unix_ms: 1_000,
            ttl_seconds: 600,
            model: "test-model".to_owned(),
            model_revision: None,
            original_usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
            },
            attempt_id: None,
        }
    }

    #[test]
    fn cancellation_between_stages_rolls_back_replacement() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(super::super::RESPONSE_DDL).unwrap();
        {
            let tx = db.transaction().unwrap();
            replace_responses(
                &tx,
                0,
                [3; 32],
                &entry(RequestStage::Wide, b"old"),
                Some(&entry(RequestStage::Rerank, b"old")),
                1_000,
                || Ok(()),
            )
            .unwrap();
            tx.commit().unwrap();
        }
        {
            let tx = db.transaction().unwrap();
            let mut checks = 0;
            let result = replace_responses(
                &tx,
                0,
                [3; 32],
                &entry(RequestStage::Wide, b"new"),
                Some(&entry(RequestStage::Rerank, b"new")),
                1_000,
                || {
                    checks += 1;
                    if checks == 3 {
                        Err(StoreError::Cancelled)
                    } else {
                        Ok(())
                    }
                },
            );
            assert_eq!(result, Err(StoreError::Cancelled));
            assert_eq!(checks, 3, "cancel after the Wide insert, before Rerank");
            // Returning from the production closure drops exactly this transaction.
        }
        let old: i64 = db
            .query_row(
                "SELECT count(*) FROM sr_cache_response WHERE response=?1",
                [&b"old"[..]],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old, 2);
    }

    #[test]
    fn failed_completion_rolls_back_both_response_rows() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(super::super::RESPONSE_DDL).unwrap();
        db.execute_batch(super::super::LEASE_DDL).unwrap();
        let tx = db.transaction().unwrap();
        replace_responses(
            &tx,
            0,
            [3; 32],
            &entry(RequestStage::Wide, b"new"),
            Some(&entry(RequestStage::Rerank, b"new")),
            1_000,
            || Ok(()),
        )
        .unwrap();
        let outcome = SqliteLeaseCoordinator::complete_in_transaction(
            &tx,
            crate::cache::CoordinationKey::from_bytes([1; 32]),
            crate::cache::OwnerToken::from_bytes([1; 16]),
            crate::cache::FencingGeneration::initial(),
            1_000,
            None,
        )
        .unwrap();
        assert!(matches!(outcome, PublishOutcome::Superseded { .. }));
        drop(tx);
        let count: i64 = db
            .query_row("SELECT count(*) FROM sr_cache_response", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}
