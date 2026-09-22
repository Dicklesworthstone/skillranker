//! Shared test support.
//!
//! Per-invocation private stores so integration tests never touch the
//! operator's platform-default state (sr-ksjn): a `None` cache or ledger
//! directory resolves the real user store, and several pipeline tests were
//! recording events into the maintainer's actual ledger.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static STORE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A fresh owner-only directory for one store consumer. Each call returns a
/// unique path under root-owned sticky /tmp, so parallel tests cannot share,
/// lock, or contaminate one another's stores, and nothing reaches the
/// operator's platform-default cache or ledger. (RCH TMPDIR ancestors can be
/// group-writable, which the store rightly refuses; /tmp is the trusted
/// sticky exception the ancestor walk already accepts.)
pub fn private_store_dir(label: &str) -> PathBuf {
    let dir = std::path::Path::new("/tmp").join(format!(
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
