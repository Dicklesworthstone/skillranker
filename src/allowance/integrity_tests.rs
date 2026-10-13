//! Enforcement state is validated, never normalized into a fresh allowance.
use super::*;

const LOCK: Duration = Duration::from_millis(200);

fn database() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(ACCOUNTING_DDL).unwrap();
    db.execute("INSERT INTO allowance_meta VALUES (1, 1, 1, 0)", [])
        .unwrap();
    db
}

fn paths() -> AllowancePaths {
    let root = std::fs::canonicalize("/tmp")
        .expect("resolve trusted system temporary directory")
        .join(format!(
            "sr-allowance-integrity-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
    AllowancePaths::new(&root.join("config"), &root.join("cache"))
}

fn debit(paths: &AllowancePaths) -> Result<Admission, AllowanceError> {
    admit(
        paths,
        &CanonicalOrigin::production(),
        RankingStage::Wide,
        true,
        LOCK,
    )
}

fn guard(phase: GuardPhase, generation: u64) -> Guard {
    Guard {
        phase,
        generation,
        max_attempts: 3,
    }
}

#[test]
fn empty_database_is_initializable_but_empty_metadata_is_not() {
    let empty = Connection::open_in_memory().unwrap();
    assert_eq!(read_meta(&empty).unwrap(), None);
    empty.execute_batch(ACCOUNTING_DDL).unwrap();
    assert!(read_meta(&empty).is_err());
    empty
        .execute("INSERT INTO allowance_meta VALUES (1, 1, 1, 0)", [])
        .unwrap();
    assert_eq!(read_meta(&empty).unwrap(), Some((1, 1, 0)));
}

#[test]
fn metadata_requires_the_supported_version_and_valid_signed_values() {
    for (schema, generation, last) in [
        (0, 1, 0),
        (2, 1, 0),
        (-1, 1, 0),
        (1, 0, 0),
        (1, -1, 0),
        (1, 1, -1),
    ] {
        let db = database();
        db.pragma_update(None, "ignore_check_constraints", true)
            .unwrap();
        db.execute(
            "UPDATE allowance_meta SET schema_version=?1, ready_generation=?2, last_charge_unix_ms=?3",
            params![schema, generation, last],
        ).unwrap();
        db.pragma_update(None, "ignore_check_constraints", false)
            .unwrap();
        let before = db.total_changes();
        assert!(
            read_meta(&db).is_err(),
            "invalid metadata {schema}/{generation}/{last}"
        );
        assert_eq!(
            db.total_changes(),
            before,
            "validation must not repair state"
        );
    }
    let db = database();
    db.execute(
        "UPDATE allowance_meta SET ready_generation=?1, last_charge_unix_ms=?1",
        [i64::MAX],
    )
    .unwrap();
    assert_eq!(
        read_meta(&db).unwrap(),
        Some((1, i64::MAX as u64, i64::MAX as u64))
    );
}

#[test]
fn deleted_or_extra_metadata_rows_are_not_fresh_setup() {
    let db = database();
    db.execute("INSERT INTO allowance_charges VALUES ('o', 0, 3)", [])
        .unwrap();
    db.execute("DELETE FROM allowance_meta", []).unwrap();
    assert!(read_meta(&db).is_err());
    assert_eq!(charged(&db, "o", 0).unwrap(), 3);
    db.execute("INSERT INTO allowance_meta VALUES (1, 1, 1, 0)", [])
        .unwrap();
    assert!(read_meta(&db).is_ok());
    db.pragma_update(None, "ignore_check_constraints", true)
        .unwrap();
    db.execute("INSERT INTO allowance_meta VALUES (2, 1, 1, 0)", [])
        .unwrap();
    db.pragma_update(None, "ignore_check_constraints", false)
        .unwrap();
    assert!(read_meta(&db).is_err());
}

#[test]
fn partial_foreign_and_extended_schemas_are_refused_without_repair() {
    for mutation in [
        "DROP TABLE allowance_meta",
        "DROP TABLE allowance_charges",
        "DROP TABLE allowance_attempts",
        "CREATE TABLE unrelated (id INTEGER)",
        "CREATE VIEW extra AS SELECT * FROM allowance_meta",
        "CREATE INDEX extra ON allowance_charges (attempts)",
        "ALTER TABLE allowance_charges ADD COLUMN extra INTEGER",
        "PRAGMA user_version=2",
        "PRAGMA application_id=1234",
    ] {
        let db = database();
        db.execute_batch(mutation).unwrap();
        let before = db.total_changes();
        assert!(read_meta(&db).is_err(), "accepted {mutation}");
        assert_eq!(db.total_changes(), before);
    }
    let foreign = Connection::open_in_memory().unwrap();
    foreign
        .execute_batch("CREATE TABLE unrelated (id INTEGER)")
        .unwrap();
    assert!(read_meta(&foreign).is_err());
    assert!(read_meta(&database()).is_ok());
}

#[test]
fn invalid_counts_are_not_clamped_and_valid_counts_are_not_changed() {
    let db = database();
    assert_eq!(
        charged(&db, "o", 0).unwrap(),
        0,
        "absent window is still empty"
    );
    for attempts in [0, 1, 10_000, i64::from(u32::MAX)] {
        db.execute(
            "INSERT OR REPLACE INTO allowance_charges VALUES ('o', 0, ?1)",
            [attempts],
        )
        .unwrap();
        assert_eq!(charged(&db, "o", 0).unwrap(), attempts as u32);
    }
    for attempts in [-1, i64::from(u32::MAX) + 1, i64::MAX] {
        db.pragma_update(None, "ignore_check_constraints", true)
            .unwrap();
        db.execute("UPDATE allowance_charges SET attempts=?1", [attempts])
            .unwrap();
        db.pragma_update(None, "ignore_check_constraints", false)
            .unwrap();
        let before = db.total_changes();
        assert!(charged(&db, "o", 0).is_err());
        assert_eq!(db.total_changes(), before);
    }
    assert!(charged(&db, "o", u64::MAX).is_err());
}

#[test]
fn guard_generations_must_fit_the_accounting_integer_domain() {
    for generation in [1, i64::MAX as u64] {
        let value = guard(GuardPhase::Ready, generation);
        assert_eq!(Guard::decode(&value.encode()).unwrap(), value);
    }
    for generation in [0, i64::MAX as u64 + 1, u64::MAX] {
        assert!(Guard::decode(&guard(GuardPhase::Ready, generation).encode()).is_err());
    }
}

#[test]
fn setup_does_not_overflow_and_only_allocates_a_generation_when_needed() {
    let last = i64::MAX as u64;
    let ready = guard(GuardPhase::Ready, last);
    assert!(plan_setup(Some(ready), last, 3).unwrap().unchanged);
    let intent = guard(GuardPhase::Intent, last);
    let resumed = plan_setup(Some(intent), last - 1, 3).unwrap();
    assert!(resumed.resumes_intent);
    assert_eq!(resumed.target.generation, last);
    assert!(plan_setup(Some(ready), last, 4).is_err());
    assert!(plan_setup(None, last, 3).is_err());
    assert!(plan_setup(None, u64::MAX, 3).is_err());
    let next = plan_setup(Some(guard(GuardPhase::Ready, 2)), 2, 4).unwrap();
    assert_eq!(next.target.generation, 3);
}

#[test]
fn only_the_exact_first_intent_can_resume_without_accounting_history() {
    assert_eq!(plan_setup(None, 0, 3).unwrap().target.generation, 1);
    let initial = guard(GuardPhase::Intent, 1);
    let resumed = plan_setup(Some(initial), 0, 3).unwrap();
    assert!(resumed.resumes_intent);
    assert_eq!(resumed.target.generation, 1);
    assert!(plan_setup(Some(initial), 0, 4).is_err());
    for phase in [GuardPhase::Ready, GuardPhase::Intent] {
        for generation in [2, 3, i64::MAX as u64] {
            assert!(plan_setup(Some(guard(phase, generation)), 0, 3).is_err());
        }
    }
    assert!(plan_setup(Some(guard(GuardPhase::Ready, 1)), 0, 3).is_err());
}

#[test]
fn lost_accounting_cannot_reset_a_ready_allowance_and_restoration_keeps_the_charge() {
    let paths = paths();
    apply_setup(&paths, 1, LOCK, None).unwrap();
    assert!(matches!(debit(&paths).unwrap(), Admission::Debited(_)));
    let original_guard = fs::read(&paths.guard).unwrap();
    // Preserve the database instead of deleting evidence to simulate loss.
    let saved = paths.accounting.with_extension("saved");
    fs::rename(&paths.accounting, &saved).unwrap();
    for limit in [1, 2] {
        assert!(preview_setup(&paths, limit).is_err());
        assert!(apply_setup(&paths, limit, LOCK, None).is_err());
        assert!(debit(&paths).is_err());
        assert!(!paths.accounting.exists(), "lost history was recreated");
        assert_eq!(fs::read(&paths.guard).unwrap(), original_guard);
    }
    fs::rename(saved, &paths.accounting).unwrap();
    assert!(apply_setup(&paths, 1, LOCK, None).unwrap().unchanged);
    assert!(matches!(
        debit(&paths),
        Err(AllowanceError::Exhausted { .. })
    ));
}

#[test]
fn first_activation_resumes_at_both_crash_points_with_a_working_budget() {
    for step in [SetupStep::IntentPublished, SetupStep::AccountingReady] {
        let paths = paths();
        let started = apply_setup(&paths, 1, LOCK, Some(step)).unwrap();
        assert_eq!(started.target.generation, 1);
        assert!(debit(&paths).is_err());
        let resumed = apply_setup(&paths, 1, LOCK, None).unwrap();
        assert!(resumed.resumes_intent);
        assert_eq!(resumed.target.generation, 1);
        assert!(matches!(debit(&paths).unwrap(), Admission::Debited(_)));
        assert!(matches!(
            debit(&paths),
            Err(AllowanceError::Exhausted { .. })
        ));
    }
}

#[test]
fn later_intent_cannot_recreate_missing_history() {
    let paths = paths();
    apply_setup(&paths, 1, LOCK, None).unwrap();
    debit(&paths).unwrap();
    apply_setup(&paths, 3, LOCK, Some(SetupStep::IntentPublished)).unwrap();
    let original_guard = fs::read(&paths.guard).unwrap();
    let saved = paths.accounting.with_extension("saved");
    fs::rename(&paths.accounting, &saved).unwrap();
    assert!(apply_setup(&paths, 3, LOCK, None).is_err());
    assert!(!paths.accounting.exists());
    assert_eq!(fs::read(&paths.guard).unwrap(), original_guard);
    fs::rename(saved, &paths.accounting).unwrap();
    assert!(apply_setup(&paths, 3, LOCK, None).unwrap().resumes_intent);
    let report = inspect(
        &paths,
        &CanonicalOrigin::production(),
        wall_clock_ms().unwrap(),
    );
    assert_eq!(report.charged_attempts, Some(1));
    assert_eq!(report.remaining(), Some(2));
}

#[test]
fn unknown_schema_blocks_admission_inspection_and_setup_without_new_charges() {
    let paths = paths();
    apply_setup(&paths, 3, LOCK, None).unwrap();
    debit(&paths).unwrap();
    let original_guard = fs::read(&paths.guard).unwrap();
    let db = Connection::open(&paths.accounting).unwrap();
    db.execute("UPDATE allowance_meta SET schema_version=2", [])
        .unwrap();
    assert!(matches!(debit(&paths), Err(AllowanceError::State(_))));
    assert!(preview_setup(&paths, 4).is_err());
    assert!(apply_setup(&paths, 4, LOCK, None).is_err());
    let report = inspect(
        &paths,
        &CanonicalOrigin::production(),
        wall_clock_ms().unwrap(),
    );
    assert_eq!(report.health, "invalid");
    assert_eq!(report.remaining(), None);
    assert_eq!(fs::read(&paths.guard).unwrap(), original_guard);
    let count: i64 = db
        .query_row("SELECT count(*) FROM allowance_attempts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
    let version: i64 = db
        .query_row("SELECT schema_version FROM allowance_meta", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(version, 2, "unsupported version was repaired");
}

#[test]
fn missing_metadata_does_not_reinitialize_an_existing_budget() {
    let paths = paths();
    apply_setup(&paths, 3, LOCK, None).unwrap();
    debit(&paths).unwrap();
    let original_guard = fs::read(&paths.guard).unwrap();
    let db = Connection::open(&paths.accounting).unwrap();
    db.execute("DELETE FROM allowance_meta", []).unwrap();
    assert!(debit(&paths).is_err());
    assert!(preview_setup(&paths, 4).is_err());
    assert!(apply_setup(&paths, 4, LOCK, None).is_err());
    assert_eq!(fs::read(&paths.guard).unwrap(), original_guard);
    let count: i64 = db
        .query_row("SELECT count(*) FROM allowance_meta", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
    let attempts: i64 = db
        .query_row("SELECT count(*) FROM allowance_attempts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(attempts, 1);
}

#[test]
fn corrupt_count_blocks_the_real_debit_without_repairing_the_counter() {
    let paths = paths();
    apply_setup(&paths, 3, LOCK, None).unwrap();
    debit(&paths).unwrap();
    let db = Connection::open(&paths.accounting).unwrap();
    db.pragma_update(None, "ignore_check_constraints", true)
        .unwrap();
    db.execute("UPDATE allowance_charges SET attempts=-1", [])
        .unwrap();
    db.pragma_update(None, "ignore_check_constraints", false)
        .unwrap();
    assert!(matches!(debit(&paths), Err(AllowanceError::State(_))));
    let report = inspect(
        &paths,
        &CanonicalOrigin::production(),
        wall_clock_ms().unwrap(),
    );
    assert_eq!(report.health, "invalid");
    assert_eq!(report.remaining(), None);
    let used: i64 = db
        .query_row("SELECT attempts FROM allowance_charges", [], |r| r.get(0))
        .unwrap();
    assert_eq!(used, -1);
    let attempts: i64 = db
        .query_row("SELECT count(*) FROM allowance_attempts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(attempts, 1);
    // A deliberate restoration of correct evidence, not production auto-repair.
    db.execute("UPDATE allowance_charges SET attempts=1", [])
        .unwrap();
    let Admission::Debited(next) = debit(&paths).unwrap() else {
        panic!("expected debit")
    };
    assert_eq!(next.charged_attempts, 2);
}

#[test]
fn invalid_setup_limits_do_not_create_any_state() {
    for limit in [0, MAX_ATTEMPTS_LIMIT + 1, u32::MAX] {
        let paths = paths();
        let root = paths
            .guard
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        assert!(preview_setup(&paths, limit).is_err());
        assert!(apply_setup(&paths, limit, LOCK, None).is_err());
        assert!(!root.exists());
    }
}
