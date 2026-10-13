//! Real SQLite cancellation/transactions and actual guarded admission controls.
//! File-backed cases require SR_TEST_PERSISTENT_ROOT (default /tmp), just like
//! the existing filesystem contract; wrong mounts fail rather than skip tests.
use super::*;
use rusqlite::{ErrorCode, TransactionBehavior};

#[test]
fn copies_keep_one_start_and_exact_expiry_is_not_renewed() {
    let budget = AdmissionBudget::start(Duration::from_secs(5));
    let copy = budget;
    for (elapsed, remaining) in [(0, 5), (3, 2), (5, 0), (6, 0)] {
        let now = budget.started + Duration::from_secs(elapsed);
        assert_eq!(copy.remaining_at(now), Duration::from_secs(remaining));
        assert_eq!(budget.remaining_at(now), copy.remaining_at(now));
    }
}

#[test]
fn zero_time_refuses_but_setup_and_inspection_have_no_admission_clock() {
    let expired = AdmissionBudget::start(Duration::ZERO);
    assert!(expired.remaining().is_err());
    assert!(checkpoint(Some(expired)).is_err());
    assert!(checkpoint(None).is_ok());
    // No checked_add(huge duration) overflow or manufactured expiry.
    assert!(AdmissionBudget::start(Duration::MAX).remaining().is_ok());
}

#[test]
fn sqlite_wait_is_capped_and_expiry_cannot_reset_the_wait() {
    let db = Connection::open_in_memory().unwrap();
    let live = AdmissionBudget::start(Duration::from_secs(30));
    live.refresh_busy_wait(&db).unwrap();
    let wait: u64 = db.pragma_query_value(None, "busy_timeout", |r| r.get(0)).unwrap();
    assert_eq!(wait, 25);
    db.busy_timeout(Duration::from_millis(3)).unwrap();
    assert!(AdmissionBudget::start(Duration::ZERO).refresh_busy_wait(&db).is_err());
    let after: u64 = db.pragma_query_value(None, "busy_timeout", |r| r.get(0)).unwrap();
    assert_eq!(after, 3, "expiry must not replenish a connection's wait");
}

#[test]
fn progress_handler_interrupts_an_expired_scan_and_allows_live_work() {
    let db = Connection::open_in_memory().unwrap();
    let sql = "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000) \
               SELECT sum(x) FROM n";
    AdmissionBudget::start(Duration::from_secs(30)).install_progress_handler(&db).unwrap();
    assert_eq!(db.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap(), 500_500);
    // Install an already-expired callback deterministically; no timing assertion
    // or sleep determines whether the statement must be interrupted.
    AdmissionBudget::start(Duration::ZERO).install_progress_handler(&db).unwrap();
    let error = db.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap_err();
    assert_eq!(error.sqlite_error_code(), Some(ErrorCode::OperationInterrupted));
    db.progress_handler(0, None::<fn() -> bool>).unwrap();
    assert_eq!(db.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap(), 500_500);
}

#[test]
fn interrupted_mutation_rolls_back_and_does_not_poison_a_later_transaction() {
    let mut db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE test_receipts(id INTEGER PRIMARY KEY)").unwrap();
    {
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate).unwrap();
        AdmissionBudget::start(Duration::ZERO).install_progress_handler(&tx).unwrap();
        let error = tx.execute(
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000) \
             INSERT INTO test_receipts SELECT x FROM n", [],
        ).unwrap_err();
        assert_eq!(error.sqlite_error_code(), Some(ErrorCode::OperationInterrupted));
    }
    db.progress_handler(0, None::<fn() -> bool>).unwrap();
    assert_eq!(db.query_row("SELECT count(*) FROM test_receipts", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    let live = AdmissionBudget::start(Duration::from_secs(30));
    live.install_progress_handler(&db).unwrap();
    let tx = db.transaction().unwrap();
    tx.execute("INSERT INTO test_receipts VALUES (1)", []).unwrap();
    live.refresh_busy_wait(&tx).unwrap();
    tx.commit().unwrap();
    live.check().unwrap();
    assert_eq!(db.query_row("SELECT count(*) FROM test_receipts", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
}

#[test]
fn expired_precommit_checkpoint_preserves_existing_rows() {
    let mut db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE test_charges(n INTEGER); INSERT INTO test_charges VALUES(1)").unwrap();
    {
        let tx = db.transaction().unwrap();
        tx.execute("UPDATE test_charges SET n=n+1", []).unwrap();
        assert!(AdmissionBudget::start(Duration::ZERO).refresh_busy_wait(&tx).is_err());
        // The production error return drops the uncommitted transaction too.
    }
    assert_eq!(db.query_row("SELECT n FROM test_charges", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
}

#[test]
fn late_postcommit_checkpoint_does_not_refund_committed_work() {
    let mut db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE test_charges(n INTEGER); INSERT INTO test_charges VALUES(1)").unwrap();
    let tx = db.transaction().unwrap();
    tx.execute("UPDATE test_charges SET n=n+1", []).unwrap();
    tx.commit().unwrap();
    // A failed final admission check changes no persisted row. This tests the
    // post-commit checkpoint, not physical sync latency or a power-loss claim.
    assert!(AdmissionBudget::start(Duration::ZERO).check().is_err());
    assert_eq!(db.query_row("SELECT n FROM test_charges", [], |r| r.get::<_, i64>(0)).unwrap(), 2);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod native {
    use super::*;
    use super::super::super::{
        Admission, AllowancePaths, COUNTER, accounting, admit, apply_setup, inspect, wall_clock_ms,
    };
    use crate::jev::{CanonicalOrigin, RankingStage};
    use std::fs::{self, File};
    use std::os::unix::fs::DirBuilderExt;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;

    const GENEROUS: Duration = Duration::from_secs(30);

    fn unused_paths() -> AllowancePaths {
        let base = std::env::var_os("SR_TEST_PERSISTENT_ROOT")
            .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
        let root = base.join(format!("sr-admission-clock-{}-{}", std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)));
        AllowancePaths::new(&root.join("config"), &root.join("cache"))
    }

    fn ready() -> AllowancePaths {
        let paths = unused_paths();
        let root = paths.accounting.parent().unwrap().parent().unwrap();
        fs::DirBuilder::new().mode(0o700).create(root).unwrap();
        crate::filesystem_policy::require_persistent(&File::open(root).unwrap())
            .expect("SR_TEST_PERSISTENT_ROOT must be a supported persistent filesystem");
        apply_setup(&paths, 2, GENEROUS, None).unwrap();
        paths
    }

    fn debit(paths: &AllowancePaths, budget: Duration) -> Result<Admission, AllowanceError> {
        admit(paths, &CanonicalOrigin::production(), RankingStage::Wide, true, budget)
    }

    fn count(paths: &AllowancePaths) -> u32 {
        inspect(paths, &CanonicalOrigin::production(), wall_clock_ms().unwrap())
            .charged_attempts.unwrap()
    }

    #[test]
    fn zero_budget_does_not_change_the_unconfigured_no_create_path() {
        let paths = unused_paths();
        for persistent in [false, true] {
            assert_eq!(admit(&paths, &CanonicalOrigin::production(), RankingStage::Wide,
                persistent, Duration::ZERO).unwrap(), Admission::Unguarded);
        }
        assert!(!paths.accounting.parent().unwrap().exists());
        assert!(!paths.guard.parent().unwrap().exists());
    }

    #[test]
    fn expired_writer_is_refused_before_path_creation_or_sqlite_open() {
        let paths = unused_paths();
        assert!(accounting::open_writer_with_budget(&paths.accounting, true,
            Some(AdmissionBudget::start(Duration::ZERO))).is_err());
        assert!(!paths.accounting.parent().unwrap().exists());
    }

    #[test]
    fn configured_zero_budget_refuses_without_debit_and_normal_admission_still_works() {
        let paths = ready();
        let guard = fs::read(&paths.guard).unwrap();
        for _ in 0..3 {
            assert!(matches!(debit(&paths, Duration::ZERO), Err(AllowanceError::State(_))));
        }
        assert_eq!(count(&paths), 0);
        assert_eq!(fs::read(&paths.guard).unwrap(), guard);
        assert!(matches!(debit(&paths, GENEROUS).unwrap(), Admission::Debited(_)));
        assert!(matches!(debit(&paths, GENEROUS).unwrap(), Admission::Debited(_)));
        assert!(matches!(debit(&paths, GENEROUS), Err(AllowanceError::Exhausted { .. })));
        assert_eq!(count(&paths), 2);
    }

    #[test]
    fn sqlite_contention_never_debits_and_unlocking_restores_admission() {
        let paths = ready();
        let peer = Connection::open(&paths.accounting).unwrap();
        peer.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert!(matches!(debit(&paths, Duration::from_millis(10)), Err(AllowanceError::State(_))));
        peer.execute_batch("ROLLBACK").unwrap();
        assert_eq!(count(&paths), 0);
        assert!(matches!(debit(&paths, GENEROUS).unwrap(), Admission::Debited(_)));
        assert_eq!(count(&paths), 1);
    }

    #[test]
    fn guard_contention_uses_the_same_budget_and_releases_cleanly() {
        let paths = ready();
        let held = super::super::super::guard_file::GuardLock::acquire(&paths, GENEROUS).unwrap();
        assert!(matches!(debit(&paths, Duration::from_millis(10)), Err(AllowanceError::State(_))));
        assert_eq!(count(&paths), 0);
        drop(held);
        assert!(matches!(debit(&paths, GENEROUS).unwrap(), Admission::Debited(_)));
        assert_eq!(count(&paths), 1);
    }
}
