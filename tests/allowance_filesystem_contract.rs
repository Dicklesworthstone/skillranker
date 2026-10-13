#![cfg(any(target_os = "linux", target_os = "macos"))]
//! Actual filesystem prerequisites for enforcement, without mount or provider work.
//! Persistent fixtures use SR_TEST_PERSISTENT_ROOT (default /tmp); Linux volatile
//! fixtures use SR_TEST_TMPFS_ROOT (default /dev/shm). A wrong fixture filesystem
//! fails explicitly rather than skipping a gate or substituting in-memory SQLite.

use skillranker::allowance::{
    Admission, AllowanceError, AllowancePaths,
    admit, apply_setup, inspect, preview_setup, read_guard, wall_clock_ms,
};
#[cfg(target_os = "linux")]
use skillranker::allowance::{ACCOUNTING_FILE, Guard, GuardPhase};
use skillranker::jev::{CanonicalOrigin, RankingStage};
use std::fs::{self, File};
#[cfg(target_os = "linux")]
use std::fs::OpenOptions;
#[cfg(target_os = "linux")]
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
#[cfg(target_os = "linux")]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static NEXT: AtomicU64 = AtomicU64::new(0);
const LOCK: Duration = Duration::from_millis(200);

fn unique(root: &Path) -> PathBuf {
    let directory = root.join(format!(
        "sr-enforcement-fs-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::DirBuilder::new().mode(0o700).create(&directory).unwrap();
    directory
}

fn persistent_dir() -> PathBuf {
    let root = std::env::var_os("SR_TEST_PERSISTENT_ROOT")
        .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    let file = File::open(&root).unwrap();
    let stat = nix::sys::statfs::fstatfs(&file).unwrap();
    #[cfg(target_os = "linux")]
    let supported = {
        use nix::sys::statfs::{BTRFS_SUPER_MAGIC, EXT4_SUPER_MAGIC, FsType};
        matches!(stat.filesystem_type(), EXT4_SUPER_MAGIC | BTRFS_SUPER_MAGIC)
            || stat.filesystem_type() == FsType(0x5846_5342)
    };
    #[cfg(target_os = "macos")]
    let supported = matches!(stat.filesystem_type_name(), "apfs" | "hfs");
    assert!(supported, "SR_TEST_PERSISTENT_ROOT must name a supported persistent filesystem");
    unique(&root)
}

#[cfg(target_os = "linux")]
fn volatile_dir() -> PathBuf {
    let root = std::env::var_os("SR_TEST_TMPFS_ROOT")
        .map_or_else(|| PathBuf::from("/dev/shm"), PathBuf::from);
    let file = File::open(&root).unwrap();
    assert_eq!(
        nix::sys::statfs::fstatfs(&file).unwrap().filesystem_type(),
        nix::sys::statfs::TMPFS_MAGIC,
        "SR_TEST_TMPFS_ROOT must name a real mounted tmpfs"
    );
    unique(&root)
}

fn paths(root: &Path) -> AllowancePaths {
    AllowancePaths::new(&root.join("config"), &root.join("cache"))
}

fn debit(paths: &AllowancePaths, persistent: bool) -> Result<Admission, AllowanceError> {
    admit(paths, &CanonicalOrigin::production(), RankingStage::Wide, persistent, LOCK)
}

#[cfg(target_os = "linux")]
fn write_private(path: &Path, bytes: &[u8]) {
    fs::DirBuilder::new().recursive(true).mode(0o700)
        .create(path.parent().unwrap()).unwrap();
    let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600)
        .open(path).unwrap();
    file.write_all(bytes).unwrap();
}

#[test]
fn persistent_local_setup_and_enforcement_still_work() {
    let root = persistent_dir();
    let paths = paths(&root);
    let setup = apply_setup(&paths, 2, LOCK, None).unwrap();
    assert_eq!(read_guard(&paths).unwrap(), Some(setup.target));
    assert!(matches!(debit(&paths, true).unwrap(), Admission::Debited(_)));
    assert!(matches!(debit(&paths, true).unwrap(), Admission::Debited(_)));
    assert!(matches!(debit(&paths, true), Err(AllowanceError::Exhausted { .. })));
    assert_eq!(inspect(&paths, &CanonicalOrigin::production(), wall_clock_ms().unwrap()).health, "ready");
    assert!(preview_setup(&paths, 2).unwrap().unchanged);
}

#[cfg(target_os = "linux")]
#[test]
fn volatile_guard_setup_fails_before_creating_control_or_accounting_paths() {
    let root = volatile_dir();
    let paths = paths(&root);
    for _ in 0..2 {
        let error = apply_setup(&paths, 2, LOCK, None).unwrap_err();
        assert!(error.to_string().contains("persistent local filesystem"));
        assert!(!root.join("config").exists());
        assert!(!root.join("cache").exists());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn existing_volatile_guard_is_invalid_not_an_unguarded_admission() {
    let root = volatile_dir();
    let paths = paths(&root);
    let guard = Guard { phase: GuardPhase::Ready, generation: 1, max_attempts: 3 };
    let bytes = guard.encode().into_bytes();
    write_private(&paths.guard, &bytes);
    assert!(read_guard(&paths).is_err());
    for persistent in [false, true] {
        assert!(matches!(debit(&paths, persistent), Err(AllowanceError::State(_))));
    }
    assert_eq!(inspect(&paths, &CanonicalOrigin::production(), wall_clock_ms().unwrap()).health, "invalid");
    assert_eq!(fs::read(&paths.guard).unwrap(), bytes);
    assert!(!paths.guard.with_extension("toml.sr-lock").exists());
    assert!(!paths.accounting.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn refused_setup_preserves_existing_volatile_guard_and_lock() {
    let root = volatile_dir();
    let paths = paths(&root);
    let guard = Guard { phase: GuardPhase::Intent, generation: 1, max_attempts: 3 };
    let bytes = guard.encode().into_bytes();
    write_private(&paths.guard, &bytes);
    let lock = paths.guard.with_extension("toml.sr-lock");
    write_private(&lock, b"synthetic-lock-canary");
    assert!(apply_setup(&paths, 3, LOCK, None).is_err());
    assert_eq!(fs::read(&paths.guard).unwrap(), bytes);
    assert_eq!(fs::read(&lock).unwrap(), b"synthetic-lock-canary");
    assert_eq!(fs::read_dir(paths.guard.parent().unwrap()).unwrap().count(), 2);
    assert!(!paths.accounting.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn persistent_guard_cannot_authorize_a_valid_accounting_copy_on_tmpfs() {
    let root = persistent_dir();
    let original = paths(&root);
    apply_setup(&original, 3, LOCK, None).unwrap();
    assert!(matches!(debit(&original, true).unwrap(), Admission::Debited(_)));
    let volatile = volatile_dir();
    let mut moved = original.clone();
    moved.accounting = volatile.join(ACCOUNTING_FILE);
    // The production writer has closed and checkpointed; preserve the original.
    // This copy is an explicit test fixture, never a recovery or inspection path.
    fs::copy(&original.accounting, &moved.accounting).unwrap();
    let before_db = fs::read(&moved.accounting).unwrap();
    let before_guard = fs::read(&original.guard).unwrap();
    assert!(debit(&moved, true).is_err());
    assert!(preview_setup(&moved, 4).is_err());
    assert!(apply_setup(&moved, 4, LOCK, None).is_err());
    assert_eq!(inspect(&moved, &CanonicalOrigin::production(), wall_clock_ms().unwrap()).health, "invalid");
    assert_eq!(fs::read(&moved.accounting).unwrap(), before_db);
    assert_eq!(fs::read(&original.guard).unwrap(), before_guard);
    assert_eq!(fs::read_dir(&volatile).unwrap().count(), 1, "no SQLite sidecars created");
    let report = inspect(&original, &CanonicalOrigin::production(), wall_clock_ms().unwrap());
    assert_eq!(report.health, "ready");
    assert_eq!(report.charged_attempts, Some(1));
}

#[cfg(target_os = "linux")]
#[test]
fn a_missing_guard_still_admits_without_creating_state_on_tmpfs() {
    let root = volatile_dir();
    let paths = paths(&root);
    for persistent in [false, true] {
        assert_eq!(debit(&paths, persistent).unwrap(), Admission::Unguarded);
    }
    assert!(fs::read_dir(root).unwrap().next().is_none());
}

#[cfg(target_os = "linux")]
#[test]
fn disposable_cache_retains_its_existing_tmpfs_support() {
    use skillranker::runtime::ProcessInvocation;
    use skillranker::storage::{CacheAccess, CacheLocation, CacheOpen, open_cache};
    let root = volatile_dir();
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let opened = open_cache(&invocation, &cx, CacheAccess::Initialize, CacheLocation::Directory(root));
    assert!(matches!(opened.unwrap(), CacheOpen::Ready(_)));
    assert!(invocation.shutdown());
}
