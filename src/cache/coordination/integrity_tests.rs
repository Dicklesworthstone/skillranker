//! Production lease primitives exercised with real SQLite transactions.
//! Response rollback/body preservation are covered by cache_atomic_publication.
//! The permissive lease fixture intentionally permits malformed rows: admission
//! must fail closed even when an older or damaged store lacks SQL CHECK guards.

use super::*;

fn key() -> CoordinationKey {
    CoordinationKey::from_bytes([3; 32])
}

fn policy() -> CoordinationPolicy {
    CoordinationPolicy { lease_ttl_ms: 100 }
}

fn leading(outcome: LeaseAcquisition) -> LeaderContext {
    match outcome {
        LeaseAcquisition::Leading(leader) => leader,
        other => panic!("expected leadership, got {other:?}"),
    }
}

fn initialize(connection: &Connection) {
    connection
        .execute_batch(
            "CREATE TABLE sr_coordination_leases (
                coordination_key BLOB PRIMARY KEY,
                owner_token BLOB NOT NULL,
                fencing_generation INTEGER NOT NULL,
                acquired_at_unix_ms INTEGER NOT NULL,
                expires_at_unix_ms INTEGER NOT NULL,
                attempt_id TEXT NOT NULL,
                is_completed INTEGER NOT NULL
             ) STRICT;",
        )
        .unwrap();
}

fn database() -> Connection {
    let connection = Connection::open_in_memory().unwrap();
    initialize(&connection);
    connection
}

fn transact<T>(
    connection: &mut Connection,
    operation: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T, CoordinationError>,
) -> Result<T, CoordinationError> {
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let result = operation(&transaction)?;
    transaction.commit()?;
    Ok(result)
}

fn acquire(connection: &mut Connection, now: u64) -> LeaderContext {
    leading(
        transact(connection, |transaction| {
            SqliteLeaseCoordinator::acquire_in_transaction(transaction, key(), now, &policy())
        })
        .unwrap(),
    )
}

fn complete(
    connection: &mut Connection,
    leader: &LeaderContext,
    now: u64,
) -> Result<PublishOutcome, CoordinationError> {
    transact(connection, |transaction| {
        SqliteLeaseCoordinator::complete_in_transaction(
            transaction,
            leader.key,
            leader.owner_token,
            leader.fencing_generation,
            now,
        )
    })
}

fn lease_image(connection: &Connection) -> String {
    connection
        .query_row(
            "SELECT quote(owner_token) || ':' || quote(fencing_generation) || ':' ||
                    quote(acquired_at_unix_ms) || ':' || quote(expires_at_unix_ms) || ':' ||
                    quote(attempt_id) || ':' || quote(is_completed)
             FROM sr_coordination_leases",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn completion_is_single_use_and_refresh_requires_a_new_fence() {
    let mut connection = database();
    let first = acquire(&mut connection, 100);
    assert_eq!(
        complete(&mut connection, &first, 110),
        Ok(PublishOutcome::Published)
    );
    let before = lease_image(&connection);
    assert!(matches!(
        complete(&mut connection, &first, 111),
        Ok(PublishOutcome::Superseded { .. })
    ));
    assert_eq!(lease_image(&connection), before);
    assert!(matches!(
        transact(&mut connection, |tx| {
            SqliteLeaseCoordinator::acquire_in_transaction(tx, key(), 112, &policy())
        })
        .unwrap(),
        LeaseAcquisition::AlreadyCompleted
    ));
    let second = leading(
        transact(&mut connection, |tx| {
            SqliteLeaseCoordinator::force_reacquire_in_transaction(tx, key(), 113, &policy())
        })
        .unwrap(),
    );
    assert!(second.fencing_generation > first.fencing_generation);
    assert!(matches!(
        complete(&mut connection, &first, 114),
        Ok(PublishOutcome::Superseded { .. })
    ));
    assert_eq!(
        complete(&mut connection, &second, 115),
        Ok(PublishOutcome::Published)
    );
}

#[test]
fn rolled_back_completion_keeps_the_active_owner_retryable() {
    let mut connection = database();
    let leader = acquire(&mut connection, 100);
    let before = lease_image(&connection);
    let tx = connection.transaction().unwrap();
    assert_eq!(
        SqliteLeaseCoordinator::complete_in_transaction(
            &tx,
            leader.key,
            leader.owner_token,
            leader.fencing_generation,
            110,
        ),
        Ok(PublishOutcome::Published)
    );
    tx.rollback().unwrap();
    assert_eq!(lease_image(&connection), before);
    assert_eq!(
        complete(&mut connection, &leader, 111),
        Ok(PublishOutcome::Published)
    );
}

#[test]
fn backwards_time_never_acquires_refreshes_or_publishes() {
    let mut connection = database();
    let leader = acquire(&mut connection, 100);
    let before = lease_image(&connection);
    assert!(matches!(
        transact(&mut connection, |tx| {
            SqliteLeaseCoordinator::acquire_in_transaction(tx, key(), 99, &policy())
        }),
        Err(CoordinationError::InvalidTimestamp)
    ));
    assert!(matches!(
        transact(&mut connection, |tx| {
            SqliteLeaseCoordinator::force_reacquire_in_transaction(tx, key(), 99, &policy())
        }),
        Err(CoordinationError::InvalidTimestamp)
    ));
    assert_eq!(
        complete(&mut connection, &leader, 99),
        Err(CoordinationError::InvalidTimestamp)
    );
    assert_eq!(lease_image(&connection), before);
}

#[test]
fn expiry_is_exclusive_for_completion_and_reacquisition() {
    for now in [199, 200, 201] {
        let mut connection = database();
        let leader = acquire(&mut connection, 100);
        let outcome = complete(&mut connection, &leader, now).unwrap();
        assert_eq!(outcome == PublishOutcome::Published, now < 200);
        let acquired = transact(&mut connection, |tx| {
            SqliteLeaseCoordinator::acquire_in_transaction(tx, key(), now, &policy())
        })
        .unwrap();
        if now < 200 {
            assert!(matches!(acquired, LeaseAcquisition::AlreadyCompleted));
        } else {
            assert_eq!(leading(acquired).fencing_generation, FencingGeneration(2));
        }
    }
}

#[test]
fn invalid_expiry_is_refused_before_creating_state() {
    for (now, ttl) in [(100, 0), (i64::MAX as u64, 1), (u64::MAX, 1), (1, u64::MAX)] {
        let invalid = CoordinationPolicy { lease_ttl_ms: ttl };
        let mut connection = database();
        assert!(matches!(
            transact(&mut connection, |tx| {
                SqliteLeaseCoordinator::acquire_in_transaction(tx, key(), now, &invalid)
            }),
            Err(CoordinationError::InvalidTimestamp)
        ));
        assert!(matches!(
            transact(&mut connection, |tx| {
                SqliteLeaseCoordinator::force_reacquire_in_transaction(tx, key(), now, &invalid)
            }),
            Err(CoordinationError::InvalidTimestamp)
        ));
        assert!(
            SqliteLeaseCoordinator::check_lease_on_connection(&connection, key())
                .unwrap()
                .is_none()
        );
    }
    assert_eq!(lease_expiry(i64::MAX as u64 - 1, 1), Ok(i64::MAX as u64));
}

#[test]
fn exhausted_generation_never_wraps_or_replaces_the_owner() {
    let mut connection = database();
    acquire(&mut connection, 100);
    connection
        .execute(
            "UPDATE sr_coordination_leases SET fencing_generation=?1",
            [i64::MAX],
        )
        .unwrap();
    let before = lease_image(&connection);
    assert!(
        transact(&mut connection, |tx| {
            SqliteLeaseCoordinator::acquire_in_transaction(tx, key(), 201, &policy())
        })
        .is_err()
    );
    assert!(
        transact(&mut connection, |tx| {
            SqliteLeaseCoordinator::force_reacquire_in_transaction(tx, key(), 201, &policy())
        })
        .is_err()
    );
    assert_eq!(lease_image(&connection), before);
}

#[test]
fn all_sqlite_lease_paths_reject_corruption_without_repair() {
    for mutation in [
        "owner_token=zeroblob(15)",
        "owner_token=zeroblob(17)",
        "fencing_generation=0",
        "fencing_generation=-1",
        "acquired_at_unix_ms=-1",
        "expires_at_unix_ms=-1",
        "expires_at_unix_ms=99",
        "is_completed=2",
        "attempt_id=printf('%0130d', 0)",
    ] {
        let mut connection = database();
        let leader = acquire(&mut connection, 100);
        connection
            .execute(&format!("UPDATE sr_coordination_leases SET {mutation}"), [])
            .unwrap();
        let before = lease_image(&connection);
        assert!(
            SqliteLeaseCoordinator::check_lease_on_connection(&connection, key()).is_err(),
            "{mutation}"
        );
        assert!(
            transact(&mut connection, |tx| {
                SqliteLeaseCoordinator::acquire_in_transaction(tx, key(), 201, &policy())
            })
            .is_err(),
            "{mutation}"
        );
        assert!(
            transact(&mut connection, |tx| {
                SqliteLeaseCoordinator::force_reacquire_in_transaction(tx, key(), 201, &policy())
            })
            .is_err(),
            "{mutation}"
        );
        assert!(
            transact(&mut connection, |tx| {
                SqliteLeaseCoordinator::active_lease_in_transaction(tx, &leader, 110)
            })
            .is_err(),
            "{mutation}"
        );
        assert!(
            complete(&mut connection, &leader, 110).is_err(),
            "{mutation}"
        );
        assert_eq!(lease_image(&connection), before, "{mutation}");
    }
}

#[test]
fn active_fence_validation_remains_strict_after_shared_row_admission() {
    let mut connection = database();
    let leader = acquire(&mut connection, 100);
    for (now, expected) in [(99, false), (100, true), (199, true), (200, false)] {
        assert_eq!(
            transact(&mut connection, |tx| {
                SqliteLeaseCoordinator::active_lease_in_transaction(tx, &leader, now)
            })
            .unwrap(),
            expected
        );
    }
    let mut altered = leader.clone();
    altered.lease_expires_at_unix_ms += 1;
    assert!(
        !transact(&mut connection, |tx| {
            SqliteLeaseCoordinator::active_lease_in_transaction(tx, &altered, 110)
        })
        .unwrap()
    );
    complete(&mut connection, &leader, 110).unwrap();
    assert!(
        !transact(&mut connection, |tx| {
            SqliteLeaseCoordinator::active_lease_in_transaction(tx, &leader, 111)
        })
        .unwrap()
    );
}

#[cfg(unix)]
#[test]
fn fresh_reopen_cannot_replay_a_completed_lease() {
    use std::os::unix::fs::DirBuilderExt;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root =
        std::env::temp_dir().join(format!("sr-lease-integrity-{}-{nonce}", std::process::id()));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&root)
        .unwrap();
    let path = root.join("lease.sqlite3");
    let mut connection = Connection::open(&path).unwrap();
    initialize(&connection);
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .unwrap();
    let leader = acquire(&mut connection, 100);
    assert_eq!(
        complete(&mut connection, &leader, 110),
        Ok(PublishOutcome::Published)
    );
    drop(connection);
    let mut reopened = Connection::open(&path).unwrap();
    assert!(
        SqliteLeaseCoordinator::check_lease_on_connection(&reopened, key())
            .unwrap()
            .unwrap()
            .is_completed
    );
    assert!(matches!(
        complete(&mut reopened, &leader, 111),
        Ok(PublishOutcome::Superseded { .. })
    ));
    drop(reopened);
    // Retain the uniquely owned fixture under the repository's no-deletion policy.
}
