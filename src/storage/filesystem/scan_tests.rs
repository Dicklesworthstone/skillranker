//! Real-directory controls for the production scanner, independent of SQLite
//! engine and filesystem-type qualification. These do not open a database.
use super::*;
use std::fs::{self, OpenOptions, Permissions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt, symlink};
use std::sync::atomic::{AtomicU64, Ordering};

fn directory() -> PrivateDirectory {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "sr-cache-scan-{}-{nonce}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
    let handle = File::open(&path).unwrap();
    let stat = fstat(&handle).unwrap();
    PrivateDirectory {
        path,
        handle,
        identity: (stat.st_dev, stat.st_ino),
    }
}

fn regular(path: &Path, size: u64) {
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.set_permissions(Permissions::from_mode(0o600)).unwrap();
    file.set_len(size).unwrap();
}

#[test]
fn counts_database_sidecars_and_other_files_once_on_every_scan() {
    let dir = directory();
    for (name, bytes) in [
        (CACHE_FILE, 11),
        ("cache.sqlite3-wal", 13),
        ("cache.sqlite3-shm", 17),
        ("cache.sqlite3-journal", 19),
        ("private-leftover", 23),
    ] {
        regular(&dir.path.join(name), bytes);
    }
    for _ in 0..3 {
        assert_eq!(dir.inspect_files(), Ok(83));
    }
    // A failed/aborted scan must not advance another scan's directory offset.
    assert_eq!(
        inspect_directory(&dir.handle, 1, CACHE_QUOTA_BYTES, || Ok(())),
        Err(StoreError::Quota)
    );
    assert_eq!(dir.inspect_files(), Ok(83));
}

#[test]
fn byte_limit_is_inclusive_and_refusal_does_not_mutate_files() {
    let dir = directory();
    let path = dir.path.join("large-private-leftover");
    regular(&path, CACHE_QUOTA_BYTES);
    assert_eq!(dir.inspect_files(), Ok(CACHE_QUOTA_BYTES));
    OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(CACHE_QUOTA_BYTES + 1)
        .unwrap();
    assert_eq!(dir.inspect_files(), Err(StoreError::Quota));
    assert_eq!(fs::metadata(path).unwrap().len(), CACHE_QUOTA_BYTES + 1);
}

// Linux permits arbitrary non-NUL filename bytes. Darwin filesystems need not
// accept these names, so creating them is not part of native macOS evidence.
#[cfg(target_os = "linux")]
#[test]
fn raw_filename_bytes_cannot_evade_quota_or_collide_with_replacement_text() {
    use std::os::unix::ffi::OsStrExt;
    let dir = directory();
    let raw = dir.path.join(std::ffi::OsStr::from_bytes(b"leftover-\xff"));
    let text = dir.path.join("leftover-\u{fffd}");
    regular(&raw, 13);
    regular(&text, 3);
    assert_eq!(dir.inspect_files(), Ok(16));
    OpenOptions::new()
        .write(true)
        .open(&raw)
        .unwrap()
        .set_len(CACHE_QUOTA_BYTES + 1)
        .unwrap();
    assert_eq!(dir.inspect_files(), Err(StoreError::Quota));
    assert_eq!(fs::metadata(&text).unwrap().len(), 3);
}

#[cfg(target_os = "linux")]
#[test]
fn raw_filename_symlink_is_not_skipped() {
    use std::os::unix::ffi::OsStrExt;
    let dir = directory();
    let name = std::ffi::OsStr::from_bytes(b"link-\xff");
    symlink("absent-private-target", dir.path.join(name)).unwrap();
    assert_eq!(dir.inspect_files(), Err(StoreError::UnsafePath));
    assert!(fs::symlink_metadata(dir.path.join(name)).unwrap().is_symlink());
}

#[test]
fn renaming_the_path_cannot_replace_the_directory_being_inspected() {
    let dir = directory();
    regular(&dir.path.join("real-private-file"), 29);
    let moved = dir.path.with_extension("moved");
    fs::rename(&dir.path, &moved).unwrap();
    fs::DirBuilder::new().mode(0o700).create(&dir.path).unwrap();
    regular(&dir.path.join("decoy"), 2);
    assert_eq!(dir.inspect_files(), Ok(29));
    assert_eq!(
        fs::metadata(moved.join("real-private-file")).unwrap().len(),
        29
    );
}

#[test]
fn a_missing_path_does_not_silently_skip_extra_files_on_the_held_directory() {
    let dir = directory();
    regular(&dir.path.join("large-private-file"), CACHE_QUOTA_BYTES + 1);
    let moved = dir.path.with_extension("moved");
    fs::rename(&dir.path, &moved).unwrap();
    assert!(!dir.path.exists());
    assert_eq!(dir.inspect_files(), Err(StoreError::Quota));
}

#[test]
fn unsafe_entry_types_and_permissions_remain_refusals() {
    let dir = directory();
    symlink("nonexistent", dir.path.join("symlink")).unwrap();
    assert_eq!(dir.inspect_files(), Err(StoreError::UnsafePath));

    let dir = directory();
    fs::create_dir(dir.path.join("subdirectory")).unwrap();
    assert_eq!(dir.inspect_files(), Err(StoreError::UnsafePath));

    let dir = directory();
    nix::unistd::mkfifo(&dir.path.join("fifo"), Mode::from_bits_truncate(0o600)).unwrap();
    assert_eq!(dir.inspect_files(), Err(StoreError::UnsafePath));

    let dir = directory();
    regular(&dir.path.join("first"), 4);
    fs::hard_link(dir.path.join("first"), dir.path.join("alias")).unwrap();
    assert_eq!(dir.inspect_files(), Err(StoreError::UnsafePath));

    let dir = directory();
    regular(&dir.path.join("shared"), 4);
    fs::set_permissions(dir.path.join("shared"), Permissions::from_mode(0o640)).unwrap();
    assert_eq!(dir.inspect_files(), Err(StoreError::Permissions));
}

#[test]
fn entry_limit_is_inclusive_even_for_empty_files() {
    let dir = directory();
    assert_eq!(inspect_directory(&dir.handle, 0, 0, || Ok(())), Ok(0));
    regular(&dir.path.join("first"), 0);
    regular(&dir.path.join("second"), 0);
    assert_eq!(inspect_directory(&dir.handle, 2, 0, || Ok(())), Ok(0));
    regular(&dir.path.join("third"), 0);
    assert_eq!(
        inspect_directory(&dir.handle, 2, 0, || Ok(())),
        Err(StoreError::Quota)
    );
    assert_eq!(dir.inspect_files(), Ok(0));
    assert_eq!(fs::read_dir(&dir.path).unwrap().count(), 3);
}

#[test]
fn over_quota_scan_stops_before_reading_the_rest() {
    let dir = directory();
    for index in 0..32 {
        regular(&dir.path.join(format!("file-{index}")), 1);
    }
    let mut checkpoints = 0;
    let result = inspect_directory(&dir.handle, 32, 0, || {
        checkpoints += 1;
        Ok(())
    });
    assert_eq!(result, Err(StoreError::Quota));
    // One initial check, at most two dot entries, then readdir + stat of the
    // first real file. Directory ordering does not change this upper bound.
    assert!(checkpoints <= 5, "scan continued after exceeding quota");
    assert_eq!(dir.inspect_files(), Ok(32));
}

#[test]
fn cancellation_is_checked_before_io_and_during_enumeration() {
    let dir = directory();
    regular(&dir.path.join("regular-not-directory"), 1);
    let file = File::open(dir.path.join("regular-not-directory")).unwrap();
    assert_eq!(
        inspect_directory(&file, 8, 8, || Err(StoreError::Cancelled)),
        Err(StoreError::Cancelled),
        "cancellation must precede even the directory-open failure"
    );
    for index in 0..16 {
        regular(&dir.path.join(format!("file-{index}")), 0);
    }
    let mut checks = 0;
    let result = inspect_directory(&dir.handle, 32, 8, || {
        checks += 1;
        if checks == 12 {
            Err(StoreError::Cancelled)
        } else {
            Ok(())
        }
    });
    assert_eq!(result, Err(StoreError::Cancelled));
    assert_eq!(checks, 12);
    assert_eq!(dir.inspect_files(), Ok(1));
}

#[test]
fn directory_stream_open_failure_is_never_a_successful_partial_count() {
    let dir = directory();
    regular(&dir.path.join("not-a-directory"), 0);
    let file = File::open(dir.path.join("not-a-directory")).unwrap();
    assert_eq!(
        inspect_directory(&file, 8, 8, || Ok(())),
        Err(StoreError::UnsafePath)
    );
    assert_eq!(dir.inspect_files(), Ok(0));
}

#[test]
fn a_budget_expiring_during_the_last_read_cannot_publish_a_count() {
    let dir = directory();
    let mut total_checks = 0;
    assert_eq!(
        inspect_directory(&dir.handle, 0, 0, || {
            total_checks += 1;
            Ok(())
        }),
        Ok(0)
    );
    let mut checks = 0;
    assert_eq!(
        inspect_directory(&dir.handle, 0, 0, || {
            checks += 1;
            if checks == total_checks {
                Err(StoreError::Cancelled)
            } else {
                Ok(())
            }
        }),
        Err(StoreError::Cancelled)
    );
    assert_eq!(checks, total_checks);
}
