//! Budget reports and previews must not enter the accounting writer.
use super::*;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt, symlink};

const LOCK: Duration = Duration::from_millis(200);

fn paths() -> AllowancePaths {
    let root = std::fs::canonicalize("/tmp")
        .expect("resolve trusted system temporary directory")
        .join(format!(
            "sr-accounting-inspection-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
    fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
    AllowancePaths::new(&root.join("config"), &root.join("cache"))
}

fn report(paths: &AllowancePaths) -> Inspection {
    inspect(
        paths,
        &CanonicalOrigin::production(),
        wall_clock_ms().unwrap(),
    )
}

fn delete_journal(paths: &AllowancePaths) {
    let db = Connection::open(&paths.accounting).unwrap();
    let mode: String = db
        .query_row("PRAGMA journal_mode=DELETE", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "delete");
}

fn image(path: &Path) -> (Vec<u8>, u32) {
    (
        fs::read(path).unwrap(),
        fs::metadata(path).unwrap().mode() & 0o7777,
    )
}

#[test]
fn ordinary_reports_and_previews_preserve_database_bytes_and_journal_mode() {
    let paths = paths();
    let original = apply_setup(&paths, 3, LOCK, None).unwrap();
    admit(
        &paths,
        &CanonicalOrigin::production(),
        RankingStage::Wide,
        true,
        LOCK,
    )
    .unwrap();
    delete_journal(&paths);
    let before = image(&paths.accounting);
    assert_eq!(report(&paths).charged_attempts, Some(1));
    assert_eq!(report(&paths).health, "ready");
    assert!(preview_setup(&paths, 3).unwrap().unchanged);
    let preview = preview_setup(&paths, 5).unwrap();
    assert_eq!(preview.target.generation, original.target.generation + 1);
    assert_eq!(image(&paths.accounting), before);
    assert_eq!(read_guard(&paths).unwrap(), Some(original.target));
    for suffix in ["-wal", "-shm", "-journal"] {
        assert!(
            !paths
                .accounting
                .with_file_name(format!("{ACCOUNTING_FILE}{suffix}"))
                .exists()
        );
    }
}

#[test]
fn inspection_reads_private_accounting_from_a_nonwritable_directory() {
    let paths = paths();
    apply_setup(&paths, 4, LOCK, None).unwrap();
    delete_journal(&paths);
    let parent = paths.accounting.parent().unwrap();
    // The database remains owner-only; removing directory write permission is
    // not permission to switch journal mode or repair the directory on read.
    fs::set_permissions(parent, fs::Permissions::from_mode(0o500)).unwrap();
    let before = image(&paths.accounting);
    let result = report(&paths);
    assert_eq!(result.health, "ready");
    assert_eq!(result.charged_attempts, Some(0));
    assert!(preview_setup(&paths, 4).unwrap().unchanged);
    assert_eq!(image(&paths.accounting), before);
    assert_eq!(fs::metadata(parent).unwrap().mode() & 0o7777, 0o500);
}

#[test]
fn foreign_schema_and_headers_are_rejected_without_reconfiguring_the_database() {
    for foreign in [
        "CREATE TABLE unrelated(secret TEXT); INSERT INTO unrelated VALUES ('synthetic');",
        "PRAGMA application_id=123; CREATE TABLE unrelated(value INTEGER);",
        "PRAGMA user_version=99; CREATE TABLE unrelated(value INTEGER);",
    ] {
        let paths = paths();
        apply_setup(&paths, 3, LOCK, None).unwrap();
        // Move our synthetic initialized database aside; preserve it, never delete it.
        fs::rename(&paths.accounting, paths.accounting.with_extension("saved")).unwrap();
        {
            let db = Connection::open(&paths.accounting).unwrap();
            db.execute_batch(foreign).unwrap();
        }
        fs::set_permissions(&paths.accounting, fs::Permissions::from_mode(0o600)).unwrap();
        let before = image(&paths.accounting);
        assert_eq!(report(&paths).health, "invalid");
        assert!(preview_setup(&paths, 3).is_err());
        assert_eq!(image(&paths.accounting), before);
        assert!(
            !paths
                .accounting
                .with_file_name("allowance.sqlite3-wal")
                .exists()
        );
    }
}

#[test]
fn absent_accounting_does_not_create_paths_or_lock_files() {
    let paths = paths();
    assert_eq!(report(&paths).health, "disabled");
    let plan = preview_setup(&paths, 3).unwrap();
    assert_eq!(plan.target.generation, 1);
    assert!(!paths.guard.parent().unwrap().exists());
    assert!(!paths.accounting.parent().unwrap().exists());
}

#[test]
fn dangling_accounting_symlink_is_invalid_not_an_empty_budget() {
    let paths = paths();
    apply_setup(&paths, 3, LOCK, None).unwrap();
    fs::rename(&paths.accounting, paths.accounting.with_extension("saved")).unwrap();
    symlink("absent-database", &paths.accounting).unwrap();
    assert_eq!(report(&paths).health, "invalid");
    assert!(preview_setup(&paths, 3).is_err());
    assert!(
        fs::symlink_metadata(&paths.accounting)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(
        !paths
            .accounting
            .parent()
            .unwrap()
            .join("absent-database")
            .exists()
    );
}

#[test]
fn readonly_snapshot_rejects_writes_and_does_not_invent_a_missing_table() {
    let paths = paths();
    apply_setup(&paths, 3, LOCK, None).unwrap();
    let result = read_accounting(&paths.accounting, |db| {
        assert!(read_meta(db)?.is_some());
        let error = db.execute("DELETE FROM allowance_meta", []).unwrap_err();
        assert_eq!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::ReadOnly)
        );
        Ok(())
    })
    .unwrap();
    assert_eq!(result, Some(()));
    assert_eq!(report(&paths).health, "ready");
}

#[test]
fn metadata_and_charges_share_one_wal_snapshot() {
    let paths = paths();
    apply_setup(&paths, 3, LOCK, None).unwrap();
    admit(
        &paths,
        &CanonicalOrigin::production(),
        RankingStage::Wide,
        true,
        LOCK,
    )
    .unwrap();
    let now = wall_clock_ms().unwrap();
    let start = window_bounds(now).0;
    let result = read_accounting(&paths.accounting, |db| {
        let before = read_meta(db)?.unwrap(); // Establish the reader snapshot.
        let writer = Connection::open(&paths.accounting).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE; UPDATE allowance_meta SET ready_generation=2; UPDATE allowance_charges SET attempts=2; COMMIT;").unwrap();
        assert_eq!(read_meta(db)?.unwrap(), before);
        assert_eq!(charged(db, CanonicalOrigin::production().as_str(), start)?, 1);
        Ok(())
    }).unwrap();
    assert_eq!(result, Some(()));
    let after = report(&paths);
    assert_eq!(after.accounting_generation, Some(2));
    assert_eq!(after.charged_attempts, Some(2));
    assert_eq!(after.health, "generation-mismatch");
}

#[test]
fn read_only_wal_inspection_includes_committed_uncheckpointed_charges() {
    let paths = paths();
    apply_setup(&paths, 3, LOCK, None).unwrap();
    let writer = Connection::open(&paths.accounting).unwrap();
    writer.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    let now = wall_clock_ms().unwrap();
    writer
        .execute(
            "INSERT INTO allowance_charges VALUES (?1, ?2, 2)",
            params![
                CanonicalOrigin::production().as_str(),
                window_bounds(now).0 as i64
            ],
        )
        .unwrap();
    assert_eq!(report(&paths).charged_attempts, Some(2));
    assert_eq!(report(&paths).remaining(), Some(1));
    assert!(preview_setup(&paths, 3).unwrap().unchanged);
}
