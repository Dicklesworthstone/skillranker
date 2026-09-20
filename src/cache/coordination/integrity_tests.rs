//! Lease-state regressions exercise real SQLite transactions and the memory backend.
//! The permissive lease fixture intentionally permits malformed rows: admission
//! must fail closed even when an older or damaged store lacks SQL CHECK guards.

use super::*;
use crate::identity::HarnessId;
use std::cell::Cell;

fn key() -> CoordinationKey {
    CoordinationKey::from_bytes([3; 32])
}

fn policy() -> CoordinationPolicy {
    CoordinationPolicy {
        lease_ttl_ms: 100,
        ..CoordinationPolicy::default()
    }
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
             ) STRICT;
             CREATE TABLE sr_response_cache (
                namespace_hash BLOB NOT NULL,
                stage TEXT NOT NULL,
                request_fingerprint BLOB NOT NULL,
                response_bytes BLOB NOT NULL CHECK(length(response_bytes) <= 16),
                received_at_unix_ms INTEGER NOT NULL,
                ttl_seconds INTEGER NOT NULL,
                model TEXT NOT NULL,
                model_revision TEXT,
                input_tokens INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL,
                attempt_id TEXT,
                PRIMARY KEY(namespace_hash, stage, request_fingerprint)
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

fn entry(body: &[u8]) -> CachedResponseEntry {
    CachedResponseEntry {
        stage: RequestStage::Wide,
        request_fingerprint: RequestFingerprint::from_bytes([4; 32]),
        response_bytes: body.to_vec(),
        received_at_unix_ms: 110,
        ttl_seconds: 10,
        model: "test-model".into(),
        model_revision: None,
        original_usage: Usage {
            input_tokens: 2,
            output_tokens: 3,
        },
        attempt_id: None,
    }
}

fn publish(
    connection: &mut Connection,
    leader: &LeaderContext,
    now: u64,
    body: &[u8],
) -> Result<PublishOutcome, CoordinationError> {
    let secret = CacheKey::from_bytes([5; 32]);
    let namespace = CacheNamespace::new(HarnessId::new("claude").unwrap(), 1);
    let response = entry(body);
    transact(connection, |transaction| {
        SqliteLeaseCoordinator::complete_in_transaction(
            transaction,
            leader.key,
            leader.owner_token,
            leader.fencing_generation,
            now,
            Some((&secret, &namespace, &response)),
        )
    })
}

fn response(connection: &Connection) -> Option<Vec<u8>> {
    connection
        .query_row("SELECT response_bytes FROM sr_response_cache", [], |row| {
            row.get(0)
        })
        .optional()
        .unwrap()
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
fn memory_completion_is_single_use_and_refresh_requires_a_new_fence() {
    let coordinator = MemoryCoordinator::new();
    let first = leading(coordinator.acquire(key(), 100, &policy()).unwrap());
    let writes = Cell::new(0);
    let publish = || {
        writes.set(writes.get() + 1);
        Ok(())
    };
    assert_eq!(
        coordinator.complete_and_publish(
            key(),
            first.owner_token,
            first.fencing_generation,
            110,
            publish
        ),
        Ok(PublishOutcome::Published)
    );
    assert!(matches!(
        coordinator.complete_and_publish(
            key(),
            first.owner_token,
            first.fencing_generation,
            111,
            publish
        ),
        Ok(PublishOutcome::Superseded { .. })
    ));
    assert_eq!(writes.get(), 1);
    assert!(matches!(
        coordinator.acquire(key(), 112, &policy()).unwrap(),
        LeaseAcquisition::AlreadyCompleted
    ));
    let second = leading(coordinator.force_reacquire(key(), 113, &policy()).unwrap());
    assert!(second.fencing_generation > first.fencing_generation);
    assert!(matches!(
        coordinator.complete_and_publish(
            key(),
            first.owner_token,
            first.fencing_generation,
            114,
            publish
        ),
        Ok(PublishOutcome::Superseded { .. })
    ));
    assert_eq!(
        coordinator.complete_and_publish(
            key(),
            second.owner_token,
            second.fencing_generation,
            115,
            publish
        ),
        Ok(PublishOutcome::Published)
    );
    assert_eq!(writes.get(), 2);
}

#[test]
fn sqlite_completion_preserves_the_first_response_until_a_new_fence_wins() {
    let mut connection = database();
    let first = acquire(&mut connection, 100);
    assert_eq!(
        publish(&mut connection, &first, 110, b"first"),
        Ok(PublishOutcome::Published)
    );
    let completed = lease_image(&connection);
    assert!(matches!(
        publish(&mut connection, &first, 111, b"overwrite"),
        Ok(PublishOutcome::Superseded { .. })
    ));
    assert_eq!(response(&connection), Some(b"first".to_vec()));
    assert_eq!(lease_image(&connection), completed);
    let second = leading(
        transact(&mut connection, |tx| {
            SqliteLeaseCoordinator::force_reacquire_in_transaction(tx, key(), 112, &policy())
        })
        .unwrap(),
    );
    assert!(second.fencing_generation > first.fencing_generation);
    assert!(matches!(
        publish(&mut connection, &first, 113, b"stale"),
        Ok(PublishOutcome::Superseded { .. })
    ));
    assert_eq!(
        publish(&mut connection, &second, 114, b"second"),
        Ok(PublishOutcome::Published)
    );
    assert_eq!(response(&connection), Some(b"second".to_vec()));
}

#[test]
fn failed_sqlite_response_write_does_not_complete_the_lease() {
    let mut connection = database();
    let leader = acquire(&mut connection, 100);
    let before = lease_image(&connection);
    assert!(
        publish(
            &mut connection,
            &leader,
            110,
            b"larger than the fixture response limit"
        )
        .is_err()
    );
    assert_eq!(lease_image(&connection), before);
    assert_eq!(response(&connection), None);
    assert_eq!(
        publish(&mut connection, &leader, 111, b"retry"),
        Ok(PublishOutcome::Published)
    );
}

#[test]
fn failed_memory_callback_does_not_complete_the_lease() {
    let coordinator = MemoryCoordinator::new();
    let leader = leading(coordinator.acquire(key(), 100, &policy()).unwrap());
    let before = coordinator.check_lease(key()).unwrap();
    assert!(
        coordinator
            .complete_and_publish(
                key(),
                leader.owner_token,
                leader.fencing_generation,
                110,
                || { Err(CoordinationError::StorageBusy) }
            )
            .is_err()
    );
    assert_eq!(coordinator.check_lease(key()).unwrap(), before);
    assert_eq!(
        coordinator.complete(key(), leader.owner_token, leader.fencing_generation, 111),
        Ok(PublishOutcome::Published)
    );
}

#[test]
fn backwards_time_never_acquires_refreshes_or_publishes() {
    let coordinator = MemoryCoordinator::new();
    let leader = leading(coordinator.acquire(key(), 100, &policy()).unwrap());
    let before = coordinator.check_lease(key()).unwrap();
    assert!(matches!(
        coordinator.acquire(key(), 99, &policy()),
        Err(CoordinationError::InvalidTimestamp)
    ));
    assert!(matches!(
        coordinator.force_reacquire(key(), 99, &policy()),
        Err(CoordinationError::InvalidTimestamp)
    ));
    assert_eq!(
        coordinator.complete_and_publish(
            key(),
            leader.owner_token,
            leader.fencing_generation,
            99,
            || { panic!("backwards time invoked publisher") }
        ),
        Err(CoordinationError::InvalidTimestamp)
    );
    assert_eq!(coordinator.check_lease(key()).unwrap(), before);

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
        publish(&mut connection, &leader, 99, b"future"),
        Err(CoordinationError::InvalidTimestamp)
    );
    assert_eq!(lease_image(&connection), before);
    assert_eq!(response(&connection), None);
}

#[test]
fn expiry_is_exclusive_in_both_backends() {
    for now in [199, 200, 201] {
        let coordinator = MemoryCoordinator::new();
        let leader = leading(coordinator.acquire(key(), 100, &policy()).unwrap());
        let mut connection = database();
        let sqlite_leader = acquire(&mut connection, 100);
        let memory = coordinator
            .complete(key(), leader.owner_token, leader.fencing_generation, now)
            .unwrap();
        let sqlite = publish(&mut connection, &sqlite_leader, now, b"response").unwrap();
        assert_eq!(memory, sqlite);
        assert_eq!(memory == PublishOutcome::Published, now < 200);
    }
}

#[test]
fn invalid_expiry_is_refused_before_creating_state() {
    for (now, ttl) in [(100, 0), (i64::MAX as u64, 1), (u64::MAX, 1), (1, u64::MAX)] {
        let invalid = CoordinationPolicy {
            lease_ttl_ms: ttl,
            ..policy()
        };
        let coordinator = MemoryCoordinator::new();
        assert!(matches!(
            coordinator.acquire(key(), now, &invalid),
            Err(CoordinationError::InvalidTimestamp)
        ));
        assert!(matches!(
            coordinator.force_reacquire(key(), now, &invalid),
            Err(CoordinationError::InvalidTimestamp)
        ));
        assert!(coordinator.check_lease(key()).unwrap().is_none());
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
    let coordinator = MemoryCoordinator::new();
    coordinator.acquire(key(), 100, &policy()).unwrap();
    coordinator
        .leases
        .write()
        .unwrap()
        .get_mut(&key())
        .unwrap()
        .fencing_generation = FencingGeneration(i64::MAX as u64);
    let before = coordinator.check_lease(key()).unwrap();
    assert!(coordinator.acquire(key(), 201, &policy()).is_err());
    assert!(coordinator.force_reacquire(key(), 201, &policy()).is_err());
    assert_eq!(coordinator.check_lease(key()).unwrap(), before);
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
            publish(&mut connection, &leader, 110, b"corrupt").is_err(),
            "{mutation}"
        );
        assert_eq!(lease_image(&connection), before, "{mutation}");
        assert_eq!(response(&connection), None);
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
    publish(&mut connection, &leader, 110, b"done").unwrap();
    assert!(
        !transact(&mut connection, |tx| {
            SqliteLeaseCoordinator::active_lease_in_transaction(tx, &leader, 111)
        })
        .unwrap()
    );
}

#[cfg(unix)]
#[test]
fn fresh_reopen_cannot_replay_a_completed_publication() {
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
        publish(&mut connection, &leader, 110, b"durable"),
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
        publish(&mut reopened, &leader, 111, b"replayed"),
        Ok(PublishOutcome::Superseded { .. })
    ));
    assert_eq!(response(&reopened), Some(b"durable".to_vec()));
    drop(reopened);
    // Only this test's uniquely owned files are removed.
    std::fs::remove_file(&path).unwrap();
    for suffix in ["-wal", "-shm"] {
        let sidecar = root.join(format!("lease.sqlite3{suffix}"));
        match std::fs::remove_file(sidecar) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("test cleanup failed: {error}"),
        }
    }
    std::fs::remove_dir(root).unwrap();
}
