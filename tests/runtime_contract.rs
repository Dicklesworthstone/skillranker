use asupersync::CancelKind;
use skillranker::limits::{
    DEFAULT_INVOCATION_DEADLINE_MS, DEFAULT_OUTPUT_CLEANUP_RESERVE_MS, DurationMillis,
    InvocationDeadline, LimitError, MonotonicMillis,
};
use skillranker::runtime::{
    EntryClock, ProcessInvocation, RuntimeError, admit_publication, read_stdin_before_cleanup,
};
use std::io::Cursor;
use std::thread;
use std::time::Duration;

fn deadline() -> InvocationDeadline {
    InvocationDeadline::default_from_start(MonotonicMillis::from_millis(1_000)).unwrap()
}

#[test]
fn entry_clock_starts_before_subsequent_work() {
    let clock = EntryClock::capture().unwrap();
    thread::sleep(Duration::from_millis(5));
    let now = clock.now();
    assert!(now.as_millis() >= 5);
    assert_eq!(
        clock.deadline().total().as_millis(),
        DEFAULT_INVOCATION_DEADLINE_MS
    );
    assert_eq!(
        clock.deadline().cleanup_reserve().as_millis(),
        DEFAULT_OUTPUT_CLEANUP_RESERVE_MS
    );
    clock.admit_new_work().unwrap();
}

#[test]
fn new_work_stops_in_cleanup_reserve_but_prior_results_may_emit() {
    let deadline = deadline();
    let start = deadline.start();
    let latest = deadline.latest_work_time();
    let in_cleanup = MonotonicMillis::from_millis(latest.as_millis());
    let after_expiry = deadline.expires_at();

    assert!(matches!(
        deadline.ensure_can_start_work(in_cleanup, "runtime_work"),
        Err(LimitError::DeadlineInCleanupReserve { .. })
    ));
    admit_publication(deadline, start, in_cleanup).unwrap();
    assert_eq!(
        admit_publication(deadline, in_cleanup, in_cleanup),
        Err(RuntimeError::LateResultSuppressed)
    );
    assert_eq!(
        admit_publication(deadline, start, after_expiry),
        Err(RuntimeError::LateResultSuppressed)
    );
}

#[test]
fn owned_spawn_joins_and_late_completion_cannot_publish() {
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let value = invocation.runtime().block_on(async {
        let mut handle = cx
            .spawn(|task_cx| async move {
                task_cx.checkpoint().expect("child remains active");
                7_u8
            })
            .expect("spawn owned task");
        handle.join(&cx).await.expect("join owned task")
    });
    assert_eq!(value, 7);
    let completed_at = invocation.clock().deadline().latest_work_time();
    assert_eq!(
        admit_publication(
            invocation.clock().deadline(),
            completed_at,
            invocation.clock().now()
        ),
        Err(RuntimeError::LateResultSuppressed)
    );
    assert!(invocation.shutdown());
}

#[test]
fn timely_stdin_reads_to_eof_inside_the_work_window() {
    let clock = EntryClock::capture().unwrap();
    let mut input = Cursor::new(b"prompt");
    let mut buf = Vec::new();
    read_stdin_before_cleanup(&clock, &mut input, 1_048_576, &mut buf).unwrap();
    assert_eq!(buf, b"prompt");
}

#[test]
fn slow_stdin_cannot_consume_cleanup_reserve() {
    let clock = EntryClock::capture_with(
        DurationMillis::new("stdin_total", 40, 3_000).unwrap(),
        DurationMillis::new("stdin_cleanup", 15, 3_000).unwrap(),
    )
    .unwrap();
    thread::sleep(Duration::from_millis(30));
    let mut input = Cursor::new(vec![b'x'; 64]);
    let mut buf = Vec::new();
    let err = read_stdin_before_cleanup(&clock, &mut input, 1_048_576, &mut buf).unwrap_err();
    assert_eq!(err, RuntimeError::StdinTimeout);
}

#[test]
fn current_thread_runtime_owns_request_cx() {
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    invocation.runtime().block_on(async {
        cx.checkpoint().expect("fresh request context is live");
        assert!(!cx.is_cancel_requested());
    });
    assert!(invocation.shutdown());
}

#[cfg(unix)]
#[test]
fn user_cancel_is_the_signal_path() {
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    invocation.cancel_user(&cx);
    assert!(cx.is_cancel_requested());
    assert_eq!(cx.cancel_reason().map(|r| r.kind), Some(CancelKind::User));
    assert!(invocation.shutdown());
}

#[test]
fn later_invocation_uses_the_runtime_process_epoch() {
    let _ = asupersync::time::wall_now();
    thread::sleep(Duration::from_millis(120));
    let clock = EntryClock::capture_with(
        DurationMillis::new("total", 100, 3000).unwrap(),
        DurationMillis::new("cleanup", 20, 3000).unwrap(),
    )
    .unwrap();
    let invocation = ProcessInvocation::from_clock(clock).unwrap();
    let cx = invocation.request_cx().unwrap();
    cx.checkpoint()
        .expect("fresh invocation must not inherit elapsed process time");
    thread::sleep(Duration::from_millis(110));
    assert!(cx.checkpoint().is_err());
    assert_eq!(
        cx.cancel_reason().map(|reason| reason.kind),
        Some(CancelKind::Deadline)
    );
}
