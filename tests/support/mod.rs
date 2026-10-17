//! Shared test support.
//!
//! Per-invocation private stores so integration tests never touch the
//! operator's platform-default state (sr-ksjn): a `None` cache or ledger
//! directory resolves the real user store, and several pipeline tests were
//! recording events into the maintainer's actual ledger.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static STORE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(target_os = "linux")]
fn base_store_root() -> &'static std::path::Path {
    use nix::sys::statfs::{TMPFS_MAGIC, statfs};
    let tmp = std::path::Path::new("/tmp");
    let var_tmp = std::path::Path::new("/var/tmp");
    if matches!(statfs(tmp), Ok(stat) if stat.filesystem_type() == TMPFS_MAGIC) && var_tmp.is_dir()
    {
        var_tmp
    } else {
        tmp
    }
}

#[cfg(not(target_os = "linux"))]
fn base_store_root() -> &'static std::path::Path {
    std::path::Path::new("/tmp")
}

/// A fresh owner-only directory for one store consumer. Each call returns a
/// unique path under root-owned sticky /tmp (or /var/tmp when /tmp is tmpfs on
/// Linux), so parallel tests cannot share, lock, or contaminate one another's
/// stores, and nothing reaches the operator's platform-default cache or ledger.
pub fn private_store_dir(label: &str) -> PathBuf {
    let dir = base_store_root().join(format!(
        "sr-test-store-{}-{}-{}",
        std::process::id(),
        STORE_COUNTER.fetch_add(1, Ordering::Relaxed),
        label
    ));
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .expect("create private test store");
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(&dir).expect("create private test store");
    dir
}
