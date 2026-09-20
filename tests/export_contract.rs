#![cfg(any(target_os = "linux", target_os = "macos"))]

use skillranker::storage::export::{ExportConfig, ExportError, export_private_atomic};
use std::fs::{self, DirBuilder};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

// Retain real fixtures; the repository does not authorize deleting test trees.
fn tree() -> PathBuf {
    let path = Path::new("/tmp").join(format!(
        "sr-export-contract-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    DirBuilder::new().mode(0o700).create(&path).unwrap();
    path
}

#[test]
fn safe_export_keeps_content_permissions_and_existing_targets() {
    let root = tree();
    let target = root.join("snapshot.json");
    export_private_atomic(&target, b"public test content", ExportConfig::default()).unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"public test content");
    assert_eq!(fs::metadata(&target).unwrap().mode() & 0o7777, 0o600);
    assert!(matches!(
        export_private_atomic(&target, b"replacement", ExportConfig::default()),
        Err(ExportError::TargetAlreadyExists(_))
    ));
    assert_eq!(fs::read(&target).unwrap(), b"public test content");
    assert_eq!(fs::read_dir(root).unwrap().count(), 1);
}

#[test]
fn untrusted_ancestor_cannot_redirect_a_private_export() {
    let root = tree();
    let shared = root.join("shared");
    fs::create_dir(&shared).unwrap();
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).unwrap();
    let private = shared.join("private");
    DirBuilder::new().mode(0o700).create(&private).unwrap();
    let target = private.join("snapshot.json");
    assert!(
        export_private_atomic(&target, b"private test content", ExportConfig::default()).is_err(),
        "a safe leaf cannot authorize a replaceable ancestor"
    );
    assert!(!target.exists());
    assert_eq!(fs::read_dir(private).unwrap().count(), 0);
}

#[test]
fn symlinked_ancestor_is_not_an_export_authority() {
    let root = tree();
    let actual = root.join("actual");
    DirBuilder::new().mode(0o700).create(&actual).unwrap();
    let inner = actual.join("inner");
    DirBuilder::new().mode(0o700).create(&inner).unwrap();
    symlink(&actual, root.join("alias")).unwrap();
    let target = root.join("alias/inner/snapshot.json");
    assert!(
        export_private_atomic(&target, b"private test content", ExportConfig::default()).is_err()
    );
    assert_eq!(fs::read_dir(inner).unwrap().count(), 0);
}

#[test]
fn valid_long_target_name_does_not_overflow_the_temporary_name() {
    let root = tree();
    let target = root.join("x".repeat(240));
    export_private_atomic(&target, b"long-name content", ExportConfig::default()).unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"long-name content");
    assert_eq!(fs::read_dir(root).unwrap().count(), 1);
}

#[test]
fn restrictive_umask_and_relative_destination_preserve_private_success() {
    const CHILD: &str = "SKILLRANKER_EXPORT_UMASK_FIXTURE";
    if let Some(root) = std::env::var_os(CHILD) {
        // The child runs only this test; changing umask cannot race sibling tests.
        std::env::set_current_dir(root).unwrap();
        nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o777));
        export_private_atomic(
            Path::new("relative.json"),
            b"relative content",
            ExportConfig::default(),
        )
        .unwrap();
        assert_eq!(
            fs::metadata("relative.json").unwrap().mode() & 0o7777,
            0o600
        );
        return;
    }
    let root = tree();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "restrictive_umask_and_relative_destination_preserve_private_success",
            "--nocapture",
        ])
        .env_clear()
        .env(CHILD, &root)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "isolated umask child failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        fs::read(root.join("relative.json")).unwrap(),
        b"relative content"
    );
    assert_eq!(fs::read_dir(root).unwrap().count(), 1);
}

#[test]
fn parent_components_keep_their_filesystem_meaning() {
    let root = tree();
    DirBuilder::new()
        .mode(0o700)
        .create(root.join("child"))
        .unwrap();
    export_private_atomic(
        &root.join("child/../result"),
        b"parent content",
        ExportConfig::default(),
    )
    .unwrap();
    assert_eq!(fs::read(root.join("result")).unwrap(), b"parent content");
    assert_eq!(fs::read_dir(root.join("child")).unwrap().count(), 0);
}
