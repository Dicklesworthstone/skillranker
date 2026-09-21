#![cfg(unix)]
//! An owner-only storage refusal has to say which path is wrong (sr-488b).
//!
//! The refusal used to read, in full: "Failed to initialize ledger: cache path does not satisfy
//! owner-only permissions", with the hint "Use sr --help; inspect trusted-user and project
//! configuration." Neither names a path, and the hint points at configuration when the fix is a
//! chmod on one directory. Two separate reviews lost several attempts each to it, both times on a
//! directory the harness itself had just created, which is the ordinary case rather than an exotic
//! one.
//!
//! These cases drive the built binary, because the wording is what a person reads.
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

fn temp_root(label: &str) -> PathBuf {
    // Linux's root-owned sticky /tmp is the one ancestor the store accepts, so a test that wants a
    // REJECTED ancestor has to create it here itself.
    let root = PathBuf::from("/tmp").join(format!(
        "sr-owner-refusal-{}-{}-{}",
        label,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&root)
        .unwrap();
    root
}

fn run(root: &Path, dir: &Path) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_sr"))
        .env_clear()
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_DATA_HOME", root.join("data"))
        .args(["ledger", "init", "--dir", dir.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
    )
}

#[test]
fn a_group_writable_ancestor_is_named_with_its_mode_and_the_chmod_that_fixes_it() {
    let root = temp_root("ancestor");
    let shared = root.join("shared");
    std::fs::DirBuilder::new()
        .mode(0o775)
        .recursive(true)
        .create(&shared)
        .unwrap();
    // Some umasks clear the group bit on create, so set it explicitly rather than trusting mode().
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o775)).unwrap();
    let store = shared.join("ledger");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&store)
        .unwrap();

    let (ok, stdout) = run(&root, &store);
    assert!(!ok, "a group-writable ancestor must still be refused");
    assert!(
        stdout.contains(shared.to_str().unwrap()),
        "the refusal must name the offending directory; got: {stdout}"
    );
    assert!(
        stdout.contains("0775"),
        "the refusal must state the mode it found; got: {stdout}"
    );
    assert!(
        stdout.contains("chmod go-w"),
        "the refusal must give the command that fixes it, not point at configuration; got: {stdout}"
    );
}

#[test]
fn a_store_directory_with_the_wrong_mode_is_named_with_the_mode_it_must_have() {
    let root = temp_root("leaf");
    let store = root.join("ledger");
    std::fs::DirBuilder::new()
        .mode(0o755)
        .recursive(true)
        .create(&store)
        .unwrap();
    std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o755)).unwrap();

    let (ok, stdout) = run(&root, &store);
    assert!(!ok, "a 0755 store directory must be refused");
    assert!(
        stdout.contains(store.to_str().unwrap()),
        "the refusal must name the store directory; got: {stdout}"
    );
    assert!(
        stdout.contains("0700"),
        "the refusal must state the required mode; got: {stdout}"
    );
    assert!(
        stdout.contains("chmod 700"),
        "the refusal must give the fix; got: {stdout}"
    );
}

#[test]
fn a_healthy_tree_still_initializes_and_says_nothing_extra() {
    // The honest counterpart: the diagnosis must not attach itself to a success, and must not
    // appear for refusals that are not about permissions.
    let root = temp_root("healthy");
    let store = root.join("ledger");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&store)
        .unwrap();
    let (ok, stdout) = run(&root, &store);
    assert!(ok, "an owner-only tree must initialize: {stdout}");
    assert!(
        !stdout.contains("chmod"),
        "a success must not carry a remedy; got: {stdout}"
    );
}
