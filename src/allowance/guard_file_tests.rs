//! Real control-file, lock, and accounting operations. No provider calls.
use super::*;
use crate::allowance::{self, Admission, GuardPhase, SetupStep};
use crate::jev::{CanonicalOrigin, RankingStage};
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt, symlink};

fn paths() -> AllowancePaths {
    let root = std::fs::canonicalize("/tmp")
        .expect("resolve trusted system temporary directory")
        .join(format!(
            "sr-allowance-control-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
    AllowancePaths::new(&root.join("config"), &root.join("cache"))
}

fn prepared() -> AllowancePaths {
    let paths = paths();
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(paths.guard.parent().unwrap())
        .unwrap();
    paths
}

fn guard(max_attempts: u32) -> Guard {
    Guard {
        phase: GuardPhase::Ready,
        generation: 1,
        max_attempts,
    }
}

fn file(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
}

fn refused<T>(value: Result<T, AllowanceError>) {
    assert!(matches!(value, Err(AllowanceError::State(_))));
}

#[test]
fn absent_guard_does_not_create_any_control_or_accounting_state() {
    let paths = paths();
    assert_eq!(allowance::read_guard(&paths).unwrap(), None);
    for persistent in [false, true] {
        assert_eq!(
            allowance::admit(
                &paths,
                &CanonicalOrigin::production(),
                RankingStage::Wide,
                persistent,
                Duration::ZERO
            )
            .unwrap(),
            Admission::Unguarded
        );
    }
    assert!(!paths.guard.parent().unwrap().parent().unwrap().exists());
    assert!(!paths.accounting.exists());
}

#[test]
fn private_guard_under_nonwritable_searchable_directories_is_usable() {
    let paths = prepared();
    fs::set_permissions(
        paths.guard.parent().unwrap(),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    publish(&paths, &guard(7)).unwrap();
    assert_eq!(allowance::read_guard(&paths).unwrap(), Some(guard(7)));
    assert_eq!(fs::metadata(&paths.guard).unwrap().mode() & 0o7777, 0o600);
    assert_eq!(
        fs::metadata(paths.guard.parent().unwrap()).unwrap().mode() & 0o7777,
        0o755
    );
}

#[test]
fn exact_read_cap_works_and_oversized_or_non_utf8_guards_are_refused() {
    let paths = prepared();
    let mut text = guard(3).encode();
    text.push('#');
    text.push_str(&"x".repeat(GUARD_FILE_MAX_BYTES as usize - text.len()));
    file(&paths.guard, text.as_bytes());
    assert_eq!(allowance::read_guard(&paths).unwrap(), Some(guard(3)));
    OpenOptions::new()
        .append(true)
        .open(&paths.guard)
        .unwrap()
        .write_all(b"x")
        .unwrap();
    refused(allowance::read_guard(&paths));
    fs::write(&paths.guard, [0xff]).unwrap();
    refused(allowance::read_guard(&paths));
}

#[test]
fn growth_after_open_cannot_escape_the_stream_byte_limit() {
    let paths = prepared();
    publish(&paths, &guard(3)).unwrap();
    let reader = Reader::open(&paths.guard).unwrap().unwrap();
    OpenOptions::new()
        .append(true)
        .open(&paths.guard)
        .unwrap()
        .write_all(&vec![b' '; GUARD_FILE_MAX_BYTES as usize * 16])
        .unwrap();
    refused(reader.read());
    assert!(fs::metadata(&paths.guard).unwrap().len() > GUARD_FILE_MAX_BYTES);
}

#[test]
fn a_reader_keeps_its_opened_file_and_rejects_replacement_by_a_fifo() {
    let paths = prepared();
    publish(&paths, &guard(3)).unwrap();
    let reader = Reader::open(&paths.guard).unwrap().unwrap();
    let saved = paths.guard.with_extension("saved");
    fs::rename(&paths.guard, &saved).unwrap();
    nix::unistd::mkfifo(&paths.guard, Mode::from_bits_truncate(0o600)).unwrap();
    // No writer exists. Opening the replacement for a blocking read would hang.
    refused(reader.read());
    assert_eq!(fs::read_to_string(saved).unwrap(), guard(3).encode());
}

#[test]
fn a_dangling_guard_or_ancestor_symlink_is_never_an_unguarded_admission() {
    for ancestor in [false, true] {
        let paths = prepared();
        if ancestor {
            let parent = paths.guard.parent().unwrap();
            fs::rename(parent, parent.with_extension("saved")).unwrap();
            symlink(parent.with_extension("absent"), parent).unwrap();
        } else {
            symlink(paths.guard.with_extension("absent"), &paths.guard).unwrap();
        }
        for persistent in [false, true] {
            refused(allowance::admit(
                &paths,
                &CanonicalOrigin::production(),
                RankingStage::Wide,
                persistent,
                Duration::ZERO,
            ));
        }
        assert!(!paths.accounting.exists());
    }
}

#[test]
fn writable_ancestors_modes_and_hardlinks_do_not_supply_trusted_guard_state() {
    for mode in [0o644, 0o620, 0o4600] {
        let paths = prepared();
        publish(&paths, &guard(3)).unwrap();
        fs::set_permissions(&paths.guard, fs::Permissions::from_mode(mode)).unwrap();
        refused(read(&paths.guard));
        assert_eq!(fs::metadata(&paths.guard).unwrap().mode() & 0o7777, mode);
    }
    let paths = prepared();
    publish(&paths, &guard(3)).unwrap();
    fs::hard_link(&paths.guard, paths.guard.with_extension("alias")).unwrap();
    refused(read(&paths.guard));
    let paths = prepared();
    fs::set_permissions(
        paths.guard.parent().unwrap(),
        fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    refused(read(&paths.guard));
    refused(GuardLock::acquire(&paths, Duration::ZERO));
    assert!(!paths.lock_path().exists());
}

#[test]
fn unsafe_or_unbounded_path_spelling_is_rejected_without_creation() {
    for path in [
        PathBuf::from("relative/allowance.toml"),
        PathBuf::from("/tmp/../allowance.toml"),
        PathBuf::from(format!("/tmp/{}/guard", "x".repeat(4096))),
        PathBuf::from(format!("/tmp/{}guard", "a/".repeat(128))),
    ] {
        refused(read(&path));
    }
}

#[test]
fn lock_open_refuses_fifo_symlink_hardlink_directory_and_shared_write() {
    for kind in 0..5 {
        let paths = prepared();
        let lock = paths.lock_path();
        match kind {
            0 => nix::unistd::mkfifo(&lock, Mode::from_bits_truncate(0o600)).unwrap(),
            1 => symlink(paths.guard.with_extension("absent"), &lock).unwrap(),
            2 => {
                file(&lock, b"untouched");
                fs::hard_link(&lock, lock.with_extension("alias")).unwrap();
            }
            3 => fs::create_dir(&lock).unwrap(),
            _ => {
                file(&lock, b"untouched");
                fs::set_permissions(&lock, fs::Permissions::from_mode(0o666)).unwrap();
            }
        }
        refused(GuardLock::acquire(&paths, Duration::ZERO));
        assert!(!paths.guard.exists());
        assert!(!paths.accounting.exists());
        if kind == 2 || kind == 4 {
            assert_eq!(fs::read(&lock).unwrap(), b"untouched");
        }
    }
}

#[test]
fn legacy_readable_locks_are_not_chmodded_truncated_or_replaced() {
    for mode in [0o600, 0o640, 0o644] {
        let paths = prepared();
        let lock = paths.lock_path();
        file(&lock, b"do-not-truncate");
        fs::set_permissions(&lock, fs::Permissions::from_mode(mode)).unwrap();
        let before = fs::metadata(&lock).unwrap();
        let held = GuardLock::acquire(&paths, Duration::ZERO).unwrap();
        let after = fs::metadata(&lock).unwrap();
        assert_eq!(
            (after.dev(), after.ino(), after.mode()),
            (before.dev(), before.ino(), before.mode())
        );
        assert_eq!(fs::read(&lock).unwrap(), b"do-not-truncate");
        drop(held);
    }
}

#[test]
fn guard_lock_excludes_competing_opens_and_is_reusable_after_drop() {
    let paths = prepared();
    let first = GuardLock::acquire(&paths, Duration::ZERO).unwrap();
    refused(GuardLock::acquire(&paths, Duration::ZERO));
    refused(GuardLock::acquire(&paths, Duration::from_millis(5)));
    drop(first);
    let _next = GuardLock::acquire(&paths, Duration::ZERO).unwrap();
    assert_eq!(
        fs::metadata(paths.lock_path()).unwrap().mode() & 0o7777,
        0o600
    );
}

#[test]
fn publication_replaces_the_guard_atomically_and_old_readers_fail_closed() {
    let paths = prepared();
    let _held = GuardLock::acquire(&paths, Duration::ZERO).unwrap();
    publish(&paths, &guard(2)).unwrap();
    let old = Reader::open(&paths.guard).unwrap().unwrap();
    publish(&paths, &guard(4)).unwrap();
    assert_eq!(read(&paths.guard).unwrap(), Some(guard(4)));
    refused(old.read());
    let files: Vec<_> = fs::read_dir(paths.guard.parent().unwrap())
        .unwrap()
        .collect();
    assert_eq!(files.len(), 2, "only the guard and persistent lock remain");
}

#[test]
fn publication_preserves_an_unsafe_existing_target_instead_of_repairing_it() {
    let paths = prepared();
    file(&paths.guard, b"do-not-repair");
    fs::set_permissions(&paths.guard, fs::Permissions::from_mode(0o644)).unwrap();
    refused(publish(&paths, &guard(3)));
    assert_eq!(fs::read(&paths.guard).unwrap(), b"do-not-repair");
    assert_eq!(fs::metadata(&paths.guard).unwrap().mode() & 0o7777, 0o644);
    let paths = prepared();
    let target = paths.guard.with_extension("target");
    file(&target, b"do-not-replace");
    symlink(&target, &paths.guard).unwrap();
    refused(publish(&paths, &guard(3)));
    assert_eq!(fs::read(&target).unwrap(), b"do-not-replace");
    assert!(
        fs::symlink_metadata(&paths.guard)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn setup_recovery_and_actual_debit_counts_survive_the_new_control_boundary() {
    let paths = paths();
    let lock_budget = Duration::from_secs(2);
    let origin = CanonicalOrigin::production();
    allowance::apply_setup(&paths, 2, lock_budget, None).unwrap();
    assert!(matches!(
        allowance::admit(&paths, &origin, RankingStage::Wide, true, lock_budget),
        Ok(Admission::Debited(_))
    ));
    let interrupted =
        allowance::apply_setup(&paths, 3, lock_budget, Some(SetupStep::AccountingReady)).unwrap();
    assert_eq!(
        allowance::read_guard(&paths).unwrap().unwrap().phase,
        GuardPhase::Intent
    );
    refused(allowance::admit(
        &paths,
        &origin,
        RankingStage::Rerank,
        true,
        lock_budget,
    ));
    let resumed = allowance::apply_setup(&paths, 3, lock_budget, None).unwrap();
    assert!(resumed.resumes_intent);
    assert_eq!(resumed.target.generation, interrupted.target.generation);
    for expected in [2, 3] {
        let Admission::Debited(debit) =
            allowance::admit(&paths, &origin, RankingStage::Rerank, true, lock_budget).unwrap()
        else {
            panic!("expected durable debit")
        };
        assert_eq!(
            debit.charged_attempts, expected,
            "setup did not reset earlier charges"
        );
    }
    assert!(matches!(
        allowance::admit(&paths, &origin, RankingStage::Wide, true, lock_budget),
        Err(AllowanceError::Exhausted { .. })
    ));
}

#[test]
fn unsafe_control_state_never_grants_a_debit_and_recovers_without_resetting_charges() {
    let paths = paths();
    let lock_budget = Duration::from_secs(2);
    let origin = CanonicalOrigin::production();
    allowance::apply_setup(&paths, 3, lock_budget, None).unwrap();
    allowance::admit(&paths, &origin, RankingStage::Wide, true, lock_budget).unwrap();
    fs::set_permissions(&paths.guard, fs::Permissions::from_mode(0o666)).unwrap();
    for persistent in [true, false] {
        refused(allowance::admit(
            &paths,
            &origin,
            RankingStage::Wide,
            persistent,
            lock_budget,
        ));
    }
    // Explicit test repair, never production permission repair.
    fs::set_permissions(&paths.guard, fs::Permissions::from_mode(0o600)).unwrap();
    let old_lock = paths.lock_path().with_extension("saved");
    fs::rename(paths.lock_path(), &old_lock).unwrap();
    nix::unistd::mkfifo(&paths.lock_path(), Mode::from_bits_truncate(0o600)).unwrap();
    refused(allowance::admit(
        &paths,
        &origin,
        RankingStage::Wide,
        true,
        lock_budget,
    ));
    fs::rename(paths.lock_path(), paths.lock_path().with_extension("fifo")).unwrap();
    fs::rename(&old_lock, paths.lock_path()).unwrap();
    let Admission::Debited(debit) =
        allowance::admit(&paths, &origin, RankingStage::Wide, true, lock_budget).unwrap()
    else {
        panic!("expected durable debit")
    };
    assert_eq!(
        debit.charged_attempts, 2,
        "refusals neither charged nor reset the allowance"
    );
}

#[test]
fn a_temporary_name_collision_is_never_removed_by_failure_cleanup() {
    let paths = prepared();
    publish(&paths, &guard(3)).unwrap();
    let (parent, name) = Parent::open(&paths.guard, false).unwrap().unwrap();
    let tmp = "allowance.toml.sr-tmp.collision";
    let collision = paths.guard.parent().unwrap().join(tmp);
    file(&collision, b"not-ours");
    refused(publish_named(
        &parent,
        &name,
        tmp,
        guard(4).encode().as_bytes(),
    ));
    assert_eq!(fs::read(collision).unwrap(), b"not-ours");
    assert_eq!(read(&paths.guard).unwrap(), Some(guard(3)));
}

#[test]
fn directory_replacement_invalidates_an_already_opened_reader() {
    let paths = prepared();
    publish(&paths, &guard(3)).unwrap();
    let reader = Reader::open(&paths.guard).unwrap().unwrap();
    let parent = paths.guard.parent().unwrap();
    fs::rename(parent, parent.with_extension("saved")).unwrap();
    fs::DirBuilder::new().mode(0o700).create(parent).unwrap();
    refused(reader.read());
    assert!(
        !paths.guard.exists(),
        "no replacement guard was manufactured"
    );
}
