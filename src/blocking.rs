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

/// Maximum filesystem leaves admitted together by roster discovery.
pub const MAX_BLOCKING_BATCH: usize = 4;

/// Poll two invocation-owned operations together and drain both, even when
/// one returns an error. No spawned future can outlive this join.
pub async fn join_owned<A: std::future::Future, B: std::future::Future>(
    first: A,
    second: B,
) -> (A::Output, B::Output) {
    use std::task::Poll;
    let mut first = std::pin::pin!(first);
    let mut second = std::pin::pin!(second);
    let mut first_result = None;
    let mut second_result = None;
    std::future::poll_fn(|task| {
        if first_result.is_none()
            && let Poll::Ready(value) = first.as_mut().poll(task)
        {
            first_result = Some(value);
        }
        if second_result.is_none()
            && let Poll::Ready(value) = second.as_mut().poll(task)
        {
            second_result = Some(value);
        }
        if first_result.is_some() && second_result.is_some() {
            Poll::Ready((first_result.take().unwrap(), second_result.take().unwrap()))
        } else {
            Poll::Pending
        }
    })
    .await
}

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
/// Queued work rechecks cancellation and the work window before starting.
/// Once started, the closure itself is not cancelled. A cancelled caller,
/// a cancelled join, or a completion in the cleanup reserve cannot be
/// published as a timely hook result.
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
    run_blocking_leaf_with_clock(invocation, invocation.clock(), cx, kind, uninterruptible, f)
}

/// [`run_blocking_leaf`] admitted and published against an explicit clock, for the one
/// caller that runs on a different deadline than the invocation's own:
/// [`EntryClock::for_failure_finalization`].
pub fn run_blocking_leaf_with_clock<F, T>(
    invocation: &ProcessInvocation,
    clock: EntryClock,
    cx: &Cx,
    kind: BlockingLeafKind,
    uninterruptible: bool,
    f: F,
) -> Result<BlockingOutcome<T>, RuntimeError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    invocation
        .runtime()
        .block_on(run_blocking_leaf_async(clock, cx, kind, uninterruptible, f))
}

/// Async counterpart: leave the executor available for owned subprocess I/O.
pub async fn run_blocking_leaf_async<F, T>(
    clock: EntryClock,
    cx: &Cx,
    kind: BlockingLeafKind,
    uninterruptible: bool,
    f: F,
) -> Result<BlockingOutcome<T>, RuntimeError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let mut outcomes = run_blocking_batch(clock, cx, kind, uninterruptible, vec![f]).await?;
    Ok(outcomes.remove(0))
}

/// Admit at most four leaves, retaining input order. Every admitted handle is
/// joined before returning, including admission failure and cancellation. A
/// queued leaf rechecks admission when its worker starts. A running blocking
/// closure is not interruptible; late values remain unpublishable.
pub async fn run_blocking_batch<F, T>(
    clock: EntryClock,
    cx: &Cx,
    kind: BlockingLeafKind,
    uninterruptible: bool,
    leaves: Vec<F>,
) -> Result<Vec<BlockingOutcome<T>>, RuntimeError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    if leaves.len() > MAX_BLOCKING_BATCH {
        return Err(RuntimeError::UnboundedLeaf);
    }
    let mut handles = Vec::with_capacity(leaves.len());
    let mut error = None;
    for leaf in leaves {
        if let Err(e) = admit_blocking_leaf(&clock, kind, uninterruptible) {
            error = Some(e);
            break;
        }
        let parent = cx.clone();
        match cx.spawn_blocking(move |child| {
            // Submission can precede execution by an entire work window.
            // A wrapper's cancellation alone cannot retract a pool callback
            // that has been claimed, so admit immediately before its body.
            if parent.is_cancel_requested() || child.is_cancel_requested() {
                return Err(RuntimeError::Cancelled);
            }
            admit_blocking_leaf(&clock, kind, uninterruptible)?;
            let value = leaf();
            Ok(BlockingOutcome {
                completed_at: clock.now(),
                value,
            })
        }) {
            Ok(handle) => handles.push(handle),
            Err(_) => {
                error = Some(RuntimeError::BlockingPoolUnavailable);
                break;
            }
        }
    }
    let mut outcomes = Vec::with_capacity(handles.len());
    for mut handle in handles {
        match handle.join(cx).await {
            Ok(Ok(outcome)) => {
                if let Err(e) = admit_blocking_publication(&clock, outcome.completed_at, cx) {
                    error.get_or_insert(e);
                }
                outcomes.push(outcome);
            }
            Ok(Err(e)) => {
                error.get_or_insert(e);
            }
            Err(_) => {
                error.get_or_insert(RuntimeError::Cancelled);
            }
        }
    }
    match error {
        Some(error) => Err(error),
        None => Ok(outcomes),
    }
}

/// Remaining work window, for SQLite busy waits and similar bounded leaves.
pub fn remaining_busy_wait(clock: &EntryClock, cap: Duration) -> Result<Duration, RuntimeError> {
    clock.admit_new_work()?;
    let remaining = Duration::from_millis(clock.remaining_before_cleanup().as_millis());
    Ok(remaining.min(cap))
}
