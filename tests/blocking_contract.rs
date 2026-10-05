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

#[test]
fn an_error_from_one_owned_future_still_drains_the_other_leaf() {
    use skillranker::blocking::{join_owned, run_blocking_leaf_async};
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let finished = Arc::new(AtomicBool::new(false));
    let completed = Arc::clone(&finished);
    let (started_tx, mut started_rx) = asupersync::channel::oneshot::channel();
    let (failed, leaf) = invocation.runtime().block_on(join_owned(
        async {
            started_rx.recv(&cx).await.unwrap();
            Err::<(), _>("first operation failed")
        },
        run_blocking_leaf_async(
            invocation.clock(),
            &cx,
            BlockingLeafKind::Filesystem,
            false,
            move || {
                started_tx.send_blocking(()).unwrap();
                thread::sleep(Duration::from_millis(20));
                completed.store(true, Ordering::SeqCst);
                8_u8
            },
        ),
    ));
    assert_eq!(failed, Err("first operation failed"));
    assert_eq!(leaf.unwrap().value, 8);
    assert!(finished.load(Ordering::SeqCst));
    assert!(invocation.shutdown());
}

#[test]
fn blocking_batch_runs_together_and_preserves_input_order() {
    use skillranker::blocking::{MAX_BLOCKING_BATCH, run_blocking_batch};
    let clock = EntryClock::capture().unwrap();
    let invocation = ProcessInvocation::from_clock_with_blocking_pool(
        clock,
        MAX_BLOCKING_BATCH,
        MAX_BLOCKING_BATCH,
    )
    .unwrap();
    let cx = invocation.request_cx().unwrap();
    let (started_tx, started_rx) = mpsc::channel();
    let mut releases = Vec::new();
    let leaves: Vec<Box<dyn FnOnce() -> usize + Send>> = (0..MAX_BLOCKING_BATCH)
        .map(|index| {
            let started = started_tx.clone();
            let (tx, rx) = mpsc::channel();
            releases.push(tx);
            Box::new(move || {
                started.send(index).unwrap();
                rx.recv_timeout(Duration::from_secs(2))
                    .expect("all leaves must run together");
                index
            }) as Box<dyn FnOnce() -> usize + Send>
        })
        .collect();
    let controller = thread::spawn(move || {
        for _ in 0..MAX_BLOCKING_BATCH {
            started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        for release in releases.into_iter().rev() {
            release.send(()).unwrap();
        }
    });
    let outcomes = invocation
        .runtime()
        .block_on(run_blocking_batch(
            clock,
            &cx,
            BlockingLeafKind::Filesystem,
            false,
            leaves,
        ))
        .unwrap();
    controller.join().unwrap();
    assert_eq!(
        outcomes.iter().map(|o| o.value).collect::<Vec<_>>(),
        vec![0, 1, 2, 3]
    );
    assert!(outcomes.iter().all(|o| o.completed_at <= clock.now()));
    assert!(invocation.shutdown());
}

#[test]
fn cancelled_batch_drains_all_running_leaves_before_returning() {
    use skillranker::blocking::{join_owned, run_blocking_batch};
    let clock = EntryClock::capture().unwrap();
    let invocation = ProcessInvocation::from_clock_with_blocking_pool(clock, 2, 2).unwrap();
    let cx = invocation.request_cx().unwrap();
    let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut starts = Vec::new();
    let mut releases = Vec::new();
    let leaves: Vec<Box<dyn FnOnce() + Send>> = (0..2)
        .map(|_| {
            let finished = Arc::clone(&completed);
            let (tx, rx) = asupersync::channel::oneshot::channel();
            starts.push(rx);
            let (release_tx, release_rx) = mpsc::channel();
            releases.push(release_tx);
            Box::new(move || {
                tx.send_blocking(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                finished.fetch_add(1, Ordering::SeqCst);
            }) as Box<dyn FnOnce() + Send>
        })
        .collect();
    let (result, ()) = invocation.runtime().block_on(join_owned(
        run_blocking_batch(clock, &cx, BlockingLeafKind::Filesystem, false, leaves),
        async {
            for mut started in starts {
                started.recv(&cx).await.unwrap();
            }
            cx.cancel_with(
                asupersync::CancelKind::User,
                Some("cancel running roster batch"),
            );
            for release in releases {
                release.send(()).unwrap();
            }
        },
    ));
    assert_eq!(result, Err(RuntimeError::Cancelled));
    assert_eq!(
        completed.load(Ordering::SeqCst),
        2,
        "both admitted leaves drained before return"
    );
    assert!(invocation.shutdown());
}

#[test]
fn late_batch_drains_timely_and_stalled_siblings_without_publication() {
    use skillranker::blocking::run_blocking_batch;
    use skillranker::limits::DurationMillis;
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let clock = EntryClock::capture_with(
        DurationMillis::new("test", 100, 3_000).unwrap(),
        DurationMillis::new("reserve", 20, 3_000).unwrap(),
    )
    .unwrap();
    let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let leaves: Vec<Box<dyn FnOnce() -> usize + Send>> = [0, 120]
        .into_iter()
        .map(|delay| {
            let finished = Arc::clone(&completed);
            Box::new(move || {
                thread::sleep(Duration::from_millis(delay));
                finished.fetch_add(1, Ordering::SeqCst);
                delay as usize
            }) as Box<dyn FnOnce() -> usize + Send>
        })
        .collect();
    let result = invocation.runtime().block_on(run_blocking_batch(
        clock,
        &cx,
        BlockingLeafKind::Filesystem,
        false,
        leaves,
    ));
    assert!(
        matches!(
            result,
            Err(RuntimeError::LateResultSuppressed | RuntimeError::Deadline(_))
        ),
        "{result:?}"
    );
    assert_eq!(completed.load(Ordering::SeqCst), 2);
    assert!(invocation.shutdown());
}

#[test]
fn oversized_batch_is_rejected_without_running_any_leaf() {
    use skillranker::blocking::run_blocking_batch;
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let ran = Arc::new(AtomicBool::new(false));
    let leaves: Vec<_> = (0..5)
        .map(|_| {
            let ran = Arc::clone(&ran);
            move || {
                ran.store(true, Ordering::SeqCst);
            }
        })
        .collect();
    let result = invocation.runtime().block_on(run_blocking_batch(
        invocation.clock(),
        &cx,
        BlockingLeafKind::Filesystem,
        false,
        leaves,
    ));
    assert_eq!(result, Err(RuntimeError::UnboundedLeaf));
    assert!(!ran.load(Ordering::SeqCst));
    assert!(invocation.shutdown());
}
