use skillranker::blocking::{
    BlockingAdmission, BlockingLeafKind, admit_blocking_leaf, admit_blocking_publication,
    hook_path_admission, remaining_busy_wait, run_blocking_leaf,
};
use skillranker::runtime::{EntryClock, ProcessInvocation, RuntimeError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

#[test]
fn uninterruptible_leaves_are_maintenance_only() {
    assert_eq!(
        hook_path_admission(BlockingLeafKind::Filesystem, false),
        BlockingAdmission::HookPath
    );
    assert_eq!(
        hook_path_admission(BlockingLeafKind::Database, true),
        BlockingAdmission::MaintenanceOnly
    );
    let clock = EntryClock::capture().unwrap();
    assert_eq!(
        admit_blocking_leaf(&clock, BlockingLeafKind::Regex, true),
        Err(RuntimeError::UnboundedLeaf)
    );
    admit_blocking_leaf(&clock, BlockingLeafKind::Regex, false).unwrap();
}

#[test]
fn timely_filesystem_leaf_publishes() {
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let outcome = run_blocking_leaf(
        &invocation,
        &cx,
        BlockingLeafKind::Filesystem,
        false,
        || 9_u8,
    )
    .unwrap();
    assert_eq!(outcome.value, 9);
    assert!(invocation.shutdown());
}

#[test]
fn stalled_blocking_leaf_cannot_publish_after_cleanup() {
    let clock = EntryClock::capture().unwrap();
    let invocation = ProcessInvocation::from_clock(clock).unwrap();
    let cx = invocation.request_cx().unwrap();
    let entered = Arc::new(AtomicBool::new(false));
    let leaf_entered = Arc::clone(&entered);
    let err = run_blocking_leaf(
        &invocation,
        &cx,
        BlockingLeafKind::Database,
        false,
        move || {
            leaf_entered.store(true, Ordering::SeqCst);
            // Cross the actual work boundary, independent of runtime startup time.
            thread::sleep(Duration::from_millis(
                clock.remaining_before_cleanup().as_millis() + 1,
            ));
            "stalled"
        },
    )
    .unwrap_err();
    assert!(entered.load(Ordering::SeqCst), "the stalled leaf must run");
    assert!(
        matches!(
            err,
            RuntimeError::LateResultSuppressed
                | RuntimeError::Cancelled
                | RuntimeError::Deadline(_)
        ),
        "{err:?}"
    );
    let _ = invocation.shutdown();
}

#[test]
fn cancelling_a_blocking_join_does_not_publish_the_leaf() {
    let clock = EntryClock::capture().unwrap();
    let invocation = ProcessInvocation::from_clock_with_blocking_pool(clock, 1, 1).unwrap();
    let cx = invocation.request_cx().unwrap();
    let published = invocation.runtime().block_on(async {
        let (started_tx, mut started_rx) = asupersync::channel::oneshot::channel();
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let mut running = cx
            .spawn_blocking(move |_child| {
                started_tx.send_blocking(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                1_u8
            })
            .expect("first blocking worker");
        started_rx.recv(&cx).await.unwrap();
        let mut queued = cx
            .spawn_blocking(|_child| 2_u8)
            .expect("second blocking worker");
        cx.cancel_with(asupersync::CancelKind::User, Some("queued cancel"));
        release_tx.send(()).unwrap();
        let queued_joined = queued.join(&cx).await;
        let _ = running.join(&cx).await;
        match queued_joined {
            Ok(value) => {
                admit_blocking_publication(&invocation.clock(), invocation.clock().now(), &cx)?;
                Ok(value)
            }
            Err(_) => Err(RuntimeError::Cancelled),
        }
    });
    assert!(
        matches!(
            published,
            Err(RuntimeError::Cancelled) | Err(RuntimeError::LateResultSuppressed)
        ),
        "cancelled queued leaf must not publish, got {published:?}"
    );
    let _ = invocation.shutdown();
}

#[test]
fn cancelling_a_running_blocking_leaf_does_not_publish() {
    let clock = EntryClock::capture().unwrap();
    let invocation = ProcessInvocation::from_clock_with_blocking_pool(clock, 1, 1).unwrap();
    let cx = invocation.request_cx().unwrap();
    let published = invocation.runtime().block_on(async {
        let (started_tx, mut started_rx) = asupersync::channel::oneshot::channel();
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let mut running = cx
            .spawn_blocking(move |_child| {
                started_tx.send_blocking(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                "late"
            })
            .expect("running blocking worker");
        started_rx.recv(&cx).await.unwrap();
        cx.cancel_with(asupersync::CancelKind::User, Some("running cancel"));
        release_tx.send(()).unwrap();
        match running.join(&cx).await {
            Ok(value) => {
                admit_blocking_publication(&invocation.clock(), invocation.clock().now(), &cx)?;
                Ok(value)
            }
            Err(_) => Err(RuntimeError::Cancelled),
        }
    });
    assert!(
        matches!(
            published,
            Err(RuntimeError::Cancelled) | Err(RuntimeError::LateResultSuppressed)
        ),
        "cancelled running leaf must not publish, got {published:?}"
    );
    let _ = invocation.shutdown();
}

#[test]
fn busy_wait_cap_stays_inside_the_work_window() {
    let clock = EntryClock::capture().unwrap();
    let wait = remaining_busy_wait(&clock, Duration::from_millis(25)).unwrap();
    assert!(wait <= Duration::from_millis(25));
    assert!(wait > Duration::from_millis(0));
}
