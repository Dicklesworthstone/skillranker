//! Totals and per-attempt evidence must agree before an enforced admission.
use super::*;

const LOCK: Duration = Duration::from_millis(200);

fn database() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(ACCOUNTING_DDL).unwrap();
    db.execute("INSERT INTO allowance_meta VALUES (1, 1, 1, 0)", [])
        .unwrap();
    db
}

fn seed(db: &Connection, origin: &str, window: u64, count: u32) {
    db.execute(
        "INSERT INTO allowance_charges VALUES (?1, ?2, ?3)",
        params![origin, window as i64, count],
    )
    .unwrap();
    for n in 0..count {
        db.execute(
            "INSERT INTO allowance_attempts VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                format!("{origin}-{window}-{n}"),
                origin,
                window as i64,
                i64::from(n) + 1,
                ["wide", "rerank", "probe", "evaluation"][n as usize % 4],
                window as i64 + i64::from(n),
            ],
        )
        .unwrap();
    }
}

fn verified(db: &mut Connection, origin: &str, window: u64) -> Result<u32, AllowanceError> {
    let tx = db.transaction().unwrap();
    verified_charged(&tx, origin, window)
}

fn paths() -> AllowancePaths {
    let root = std::env::temp_dir().join(format!(
        "sr-charge-consistency-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    AllowancePaths::new(&root.join("config"), &root.join("cache"))
}

fn debit(paths: &AllowancePaths, stage: RankingStage) -> Result<Admission, AllowanceError> {
    admit(paths, &CanonicalOrigin::production(), stage, true, LOCK)
}

fn total_receipts(db: &Connection) -> i64 {
    db.query_row("SELECT count(*) FROM allowance_attempts", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn verification_requires_a_snapshot_and_keeps_empty_windows_usable() {
    let mut db = database();
    assert!(verified_charged(&db, "o", 0).is_err());
    assert_eq!(verified(&mut db, "o", 0).unwrap(), 0);
    assert_eq!(db.total_changes(), 1, "verification must not create counters");
}

#[test]
fn a_missing_or_reduced_total_cannot_erase_retained_attempts() {
    for replacement in [None, Some(0), Some(1), Some(2)] {
        let mut db = database();
        seed(&db, "o", 0, 3);
        match replacement {
            None => db.execute("DELETE FROM allowance_charges", []).unwrap(),
            Some(n) => db
                .execute("UPDATE allowance_charges SET attempts=?1", [n])
                .unwrap(),
        };
        let before = db.total_changes();
        assert!(matches!(verified(&mut db, "o", 0), Err(AllowanceError::State(_))));
        assert_eq!(total_receipts(&db), 3);
        assert_eq!(db.total_changes(), before, "never reconstruct the total implicitly");
    }
}

#[test]
fn a_total_without_matching_receipts_is_not_verified_evidence() {
    for keep in [0, 1, 2] {
        let mut db = database();
        seed(&db, "o", 0, 3);
        db.execute(
            "DELETE FROM allowance_attempts WHERE guard_generation>?1",
            [keep],
        )
        .unwrap();
        let before = db.total_changes();
        assert!(verified(&mut db, "o", 0).is_err());
        assert_eq!(charged(&db, "o", 0).unwrap(), 3, "do not refund lost receipts");
        assert_eq!(db.total_changes(), before);
    }
}

#[test]
fn exact_origin_and_window_match_but_all_stages_and_generations_count() {
    let mut db = database();
    seed(&db, "o", 0, 4); // All four stages, four generations, one budget.
    seed(&db, "o", WINDOW_MS, 2);
    seed(&db, "other", 0, 3);
    assert_eq!(verified(&mut db, "o", 0).unwrap(), 4);
    assert_eq!(verified(&mut db, "o", WINDOW_MS).unwrap(), 2);
    assert_eq!(verified(&mut db, "other", 0).unwrap(), 3);
    assert_eq!(verified(&mut db, "new", 0).unwrap(), 0);
    assert_eq!(verified(&mut db, "o", 2 * WINDOW_MS).unwrap(), 0);
    db.execute("DELETE FROM allowance_charges WHERE origin='o' AND window_start_unix_ms=0", [])
        .unwrap();
    assert!(verified(&mut db, "o", 0).is_err());
    assert_eq!(verified(&mut db, "o", WINDOW_MS).unwrap(), 2);
    assert_eq!(verified(&mut db, "other", 0).unwrap(), 3);
}

#[test]
fn failed_partial_mutation_rolls_back_without_losing_verified_charges() {
    let mut db = database();
    seed(&db, "o", 0, 2);
    {
        let tx = db.transaction().unwrap();
        tx.execute("DELETE FROM allowance_charges", []).unwrap();
        assert!(verified_charged(&tx, "o", 0).is_err());
        // As in a failed debit, dropping the transaction restores both sides.
    }
    assert_eq!(verified(&mut db, "o", 0).unwrap(), 2);
    assert_eq!(total_receipts(&db), 2);
}

#[test]
fn invalid_numeric_totals_still_fail_before_receipt_verification() {
    let mut db = database();
    seed(&db, "o", 0, 1);
    db.pragma_update(None, "ignore_check_constraints", true).unwrap();
    for total in [-1, i64::from(u32::MAX) + 1, i64::MAX] {
        db.execute("UPDATE allowance_charges SET attempts=?1", [total])
            .unwrap();
        assert!(verified(&mut db, "o", 0).is_err());
    }
    db.execute("UPDATE allowance_charges SET attempts=1", []).unwrap();
    assert!(verified(&mut db, "o", u64::MAX).is_err());
    assert_eq!(verified(&mut db, "o", 0).unwrap(), 1);
}

#[test]
fn missing_total_blocks_real_admission_and_restoration_preserves_exhaustion() {
    let paths = paths();
    apply_setup(&paths, 1, LOCK, None).unwrap();
    let Admission::Debited(first) = debit(&paths, RankingStage::Wide).unwrap() else {
        panic!("expected a durable debit")
    };
    let guard_before = fs::read(&paths.guard).unwrap();
    let db = Connection::open(&paths.accounting).unwrap();
    db.execute("DELETE FROM allowance_charges", []).unwrap();
    let before = db.total_changes();
    assert!(matches!(debit(&paths, RankingStage::Rerank), Err(AllowanceError::State(_))));
    let report = inspect(&paths, &CanonicalOrigin::production(), wall_clock_ms().unwrap());
    assert_eq!(report.health, "invalid");
    assert_eq!(report.charged_attempts, None);
    assert_eq!(report.remaining(), None);
    assert_eq!(total_receipts(&db), 1);
    assert_eq!(charged(&db, CanonicalOrigin::production().as_str(), first.window_start_unix_ms).unwrap(), 0);
    assert_eq!(db.total_changes(), before);
    assert_eq!(fs::read(&paths.guard).unwrap(), guard_before);
    // Explicitly restore the fixture's lost total. Production never repairs it.
    db.execute(
        "INSERT INTO allowance_charges VALUES (?1, ?2, 1)",
        params![CanonicalOrigin::production().as_str(), first.window_start_unix_ms as i64],
    )
    .unwrap();
    assert!(matches!(debit(&paths, RankingStage::Rerank), Err(AllowanceError::Exhausted { .. })));
    apply_setup(&paths, 2, LOCK, None).unwrap();
    let Admission::Debited(second) = debit(&paths, RankingStage::Rerank).unwrap() else {
        panic!("the remaining allowance must still work")
    };
    assert_eq!(second.charged_attempts, 2);
    assert_eq!(total_receipts(&db), 2);
}

#[test]
fn snapshot_verification_ignores_a_peer_commit_until_the_next_read() {
    let paths = paths();
    apply_setup(&paths, 3, LOCK, None).unwrap();
    let Admission::Debited(first) = debit(&paths, RankingStage::Wide).unwrap() else {
        panic!("expected debit")
    };
    let origin = CanonicalOrigin::production();
    read_accounting(&paths.accounting, |db| {
        read_meta(db)?.unwrap(); // Establish the WAL snapshot before the peer writes.
        let peer = Connection::open(&paths.accounting).unwrap();
        peer.execute("DELETE FROM allowance_charges", []).unwrap();
        assert_eq!(verified_charged(db, origin.as_str(), first.window_start_unix_ms)?, 1);
        Ok(())
    })
    .unwrap()
    .unwrap();
    let reopened = read_accounting(&paths.accounting, |db| {
        verified_charged(db, origin.as_str(), first.window_start_unix_ms)
    });
    assert!(matches!(reopened, Err(AllowanceError::State(_))));
}

#[test]
fn normal_debits_and_limit_changes_keep_one_count_across_every_stage() {
    let paths = paths();
    apply_setup(&paths, 2, LOCK, None).unwrap();
    for (index, stage) in [RankingStage::Wide, RankingStage::Rerank, RankingStage::Probe, RankingStage::Evaluation]
        .into_iter()
        .enumerate()
    {
        if index == 2 {
            apply_setup(&paths, 4, LOCK, None).unwrap();
        }
        let Admission::Debited(receipt) = debit(&paths, stage).unwrap() else {
            panic!("expected debit")
        };
        assert_eq!(receipt.charged_attempts, index as u32 + 1);
        let report = inspect(&paths, &CanonicalOrigin::production(), wall_clock_ms().unwrap());
        assert_eq!(report.health, "ready");
        assert_eq!(report.charged_attempts, Some(index as u32 + 1));
    }
    assert!(matches!(debit(&paths, RankingStage::Wide), Err(AllowanceError::Exhausted { .. })));
    let another_origin = CanonicalOrigin::parse("https://other.example").unwrap();
    assert!(matches!(
        admit(&paths, &another_origin, RankingStage::Wide, true, LOCK).unwrap(),
        Admission::Debited(Debit { charged_attempts: 1, .. })
    ));
}
