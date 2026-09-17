//! Bounded blocking leaves for hook-path work.
//!
//! Filesystem, regex, and SQLite work can stall. Putting that work in a task
//! or under a timeout does **not** make the closure cancellable. Cancellation
//! can wait for cleanup; publication still uses the completion timestamp.
//!
//! POSIX uninterruptible I/O (D-state page-in, some NFS/FUSE waits) can
//! outlive SIGINT/SIGTERM and pool shutdown. Those operations are not admitted
//! on the 3s hook path; isolate them to maintenance/batch. Supported hook-path
//! leaves are bounded regular-file reads, bounded regex over already-copied
//! bytes, and short SQLite statements whose busy timeout is the remaining
//! work window (default 25 ms, never past cleanup reserve).

use crate::runtime::{EntryClock, ProcessInvocation, RuntimeError, admit_publication};
use asupersync::Cx;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockingLeafKind {
    Filesystem,
    Regex,
    Database,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockingAdmission {
    /// Bounded, syscall-interruptible work may run on the hook-path pool.
    HookPath,
    /// Must not run inside the ranking/hook deadline.
    MaintenanceOnly,
}

#[derive(Debug, Eq, PartialEq)]
pub struct BlockingOutcome<T> {
    pub completed_at: crate::limits::MonotonicMillis,
    pub value: T,
}

/// Hook-path admission. `uninterruptible` is a declared property of the
/// operation (not inferred from runtime wait state).
pub fn hook_path_admission(_kind: BlockingLeafKind, uninterruptible: bool) -> BlockingAdmission {
    if uninterruptible {
        BlockingAdmission::MaintenanceOnly
    } else {
        BlockingAdmission::HookPath
    }
}

pub fn admit_blocking_leaf(
    clock: &EntryClock,
    kind: BlockingLeafKind,
    uninterruptible: bool,
) -> Result<(), RuntimeError> {
    if hook_path_admission(kind, uninterruptible) == BlockingAdmission::MaintenanceOnly {
        return Err(RuntimeError::UnboundedLeaf);
    }
    clock.admit_new_work()?;
    Ok(())
}

/// Refuse to publish a completed leaf after caller cancellation or a late
/// completion timestamp.
///
/// `TaskHandle::join` waits uninterruptibly for the blocking closure. A
/// cancelled caller or a completion in the cleanup reserve still cannot be
/// emitted as a timely hook result.
pub fn admit_blocking_publication(
    clock: &EntryClock,
    completed_at: crate::limits::MonotonicMillis,
    cx: &Cx,
) -> Result<(), RuntimeError> {
    if cx.is_cancel_requested() {
        return Err(RuntimeError::Cancelled);
    }
    admit_publication(clock.deadline(), completed_at, clock.now())
}

/// Run `f` on the process blocking pool.
///
/// The closure itself is not cancelled. A cancelled caller, a cancelled join,
/// or a completion in the cleanup reserve cannot be published as a timely
/// hook result.
pub fn run_blocking_leaf<F, T>(
    invocation: &ProcessInvocation,
    cx: &Cx,
    kind: BlockingLeafKind,
    uninterruptible: bool,
    f: F,
) -> Result<BlockingOutcome<T>, RuntimeError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let clock = invocation.clock();
    admit_blocking_leaf(&clock, kind, uninterruptible)?;
    invocation.runtime().block_on(async {
        let mut handle = cx
            .spawn_blocking(move |_child| f())
            .map_err(|_| RuntimeError::BlockingPoolUnavailable)?;
        let joined = handle.join(cx).await;
        let completed_at = clock.now();
        match joined {
            Ok(value) => {
                admit_blocking_publication(&clock, completed_at, cx)?;
                Ok(BlockingOutcome {
                    completed_at,
                    value,
                })
            }
            Err(_) => Err(RuntimeError::Cancelled),
        }
    })
}

/// Remaining work window, for SQLite busy waits and similar bounded leaves.
pub fn remaining_busy_wait(clock: &EntryClock, cap: Duration) -> Result<Duration, RuntimeError> {
    clock.admit_new_work()?;
    let remaining = Duration::from_millis(clock.remaining_before_cleanup().as_millis());
    Ok(remaining.min(cap))
}
