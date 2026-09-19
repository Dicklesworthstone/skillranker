use asupersync::CancelKind;
use skillranker::limits::{
    DEFAULT_INVOCATION_DEADLINE_MS, DEFAULT_OUTPUT_CLEANUP_RESERVE_MS, DurationMillis,
    InvocationDeadline, LimitError, MonotonicMillis,
};
use skillranker::runtime::{
    EntryClock, ProcessInvocation, RuntimeError, admit_publication, read_stdin_before_cleanup,
};
use std::io::{self, Cursor, Read};
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
    // Make the process epoch older than the entire invocation budget, while
    // leaving the normal budget for constructing the real runtime itself.
    thread::sleep(Duration::from_millis(DEFAULT_INVOCATION_DEADLINE_MS + 100));
    let clock = EntryClock::capture().unwrap();
    let invocation = ProcessInvocation::from_clock(clock).unwrap();
    let cx = invocation.request_cx().unwrap();
    cx.checkpoint()
        .expect("fresh invocation must not inherit elapsed process time");
    thread::sleep(Duration::from_millis(
        clock.remaining_before_cleanup().as_millis() + 1,
    ));
    assert!(cx.checkpoint().is_err());
    assert_eq!(
        cx.cancel_reason().map(|reason| reason.kind),
        Some(CancelKind::Deadline)
    );
}

#[test]
fn stdin_reads_at_cap_minus_one_succeeds() {
    let clock = EntryClock::capture().unwrap();
    let mut input = Cursor::new(b"123456789");
    let mut buf = Vec::new();
    read_stdin_before_cleanup(&clock, &mut input, 10, &mut buf).unwrap();
    assert_eq!(buf, b"123456789");
}

#[test]
fn stdin_reads_at_exact_cap_succeeds() {
    let clock = EntryClock::capture().unwrap();
    let mut input = Cursor::new(b"1234567890");
    let mut buf = Vec::new();
    read_stdin_before_cleanup(&clock, &mut input, 10, &mut buf).unwrap();
    assert_eq!(buf, b"1234567890");
}

#[test]
fn stdin_reads_at_cap_plus_one_fails_without_admitting_oversized() {
    let clock = EntryClock::capture().unwrap();
    let mut input = Cursor::new(b"12345678901");
    let mut buf = Vec::new();
    let err = read_stdin_before_cleanup(&clock, &mut input, 10, &mut buf).unwrap_err();
    assert!(matches!(
        err,
        RuntimeError::Deadline(LimitError::AboveLimit {
            limit: 10,
            observed: 11,
            ..
        })
    ));
    assert_eq!(buf.len(), 10);
}

#[test]
fn stdin_reads_cap_zero_empty_succeeds() {
    let clock = EntryClock::capture().unwrap();
    let mut input = Cursor::new(b"");
    let mut buf = Vec::new();
    read_stdin_before_cleanup(&clock, &mut input, 0, &mut buf).unwrap();
    assert!(buf.is_empty());
}

#[test]
fn stdin_reads_cap_zero_non_empty_fails() {
    let clock = EntryClock::capture().unwrap();
    let mut input = Cursor::new(b"x");
    let mut buf = Vec::new();
    let err = read_stdin_before_cleanup(&clock, &mut input, 0, &mut buf).unwrap_err();
    assert!(matches!(
        err,
        RuntimeError::Deadline(LimitError::AboveLimit {
            limit: 0,
            observed: 1,
            ..
        })
    ));
    assert!(buf.is_empty());
}

#[test]
fn stdin_reads_multibyte_utf8_cap_boundaries() {
    let clock = EntryClock::capture().unwrap();
    // UTF-8 checkmark is 3 bytes: [0xe2, 0x9c, 0x93]
    let checkmark = "\u{2713}".as_bytes();
    assert_eq!(checkmark.len(), 3);

    // Exact cap at 3 bytes
    let mut input = Cursor::new(checkmark);
    let mut buf = Vec::new();
    read_stdin_before_cleanup(&clock, &mut input, 3, &mut buf).unwrap();
    assert_eq!(buf, checkmark);

    // Exact cap at 6 bytes (2 checkmarks)
    let two_checkmarks = "\u{2713}\u{2713}".as_bytes();
    let mut input = Cursor::new(two_checkmarks);
    let mut buf = Vec::new();
    read_stdin_before_cleanup(&clock, &mut input, 6, &mut buf).unwrap();
    assert_eq!(buf, two_checkmarks);

    // Cap - 1 (cap=3, input=2 bytes of multibyte prefix)
    let mut input = Cursor::new(&checkmark[..2]);
    let mut buf = Vec::new();
    read_stdin_before_cleanup(&clock, &mut input, 3, &mut buf).unwrap();
    assert_eq!(buf, &checkmark[..2]);

    // Cap + 1 (cap=3, input=4 bytes)
    let four_bytes = b"\xe2\x9c\x93x";
    let mut input = Cursor::new(four_bytes);
    let mut buf = Vec::new();
    let err = read_stdin_before_cleanup(&clock, &mut input, 3, &mut buf).unwrap_err();
    assert!(matches!(
        err,
        RuntimeError::Deadline(LimitError::AboveLimit {
            limit: 3,
            observed: 4,
            ..
        })
    ));
    assert_eq!(buf.len(), 3);
}

struct InterruptedReader {
    data: Vec<u8>,
    pos: usize,
    interrupts_before_data: usize,
    interrupts_before_eof: usize,
}

impl Read for InterruptedReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.interrupts_before_data > 0 {
            self.interrupts_before_data -= 1;
            return Err(io::Error::new(io::ErrorKind::Interrupted, "eintr"));
        }
        if self.pos < self.data.len() {
            let n = buf.len().min(self.data.len() - self.pos);
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            return Ok(n);
        }
        if self.interrupts_before_eof > 0 {
            self.interrupts_before_eof -= 1;
            return Err(io::Error::new(io::ErrorKind::Interrupted, "eintr"));
        }
        Ok(0)
    }
}

#[test]
fn stdin_reads_interrupted_reads_succeed() {
    let clock = EntryClock::capture().unwrap();
    let mut reader = InterruptedReader {
        data: b"interrupted_test_data".to_vec(),
        pos: 0,
        interrupts_before_data: 3,
        interrupts_before_eof: 2,
    };
    let mut buf = Vec::new();
    read_stdin_before_cleanup(&clock, &mut reader, 1024, &mut buf).unwrap();
    assert_eq!(buf, b"interrupted_test_data");
}

struct DelayedEofReader {
    data: Vec<u8>,
    pos: usize,
    eof_delay: Duration,
}

impl Read for DelayedEofReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos < self.data.len() {
            let n = buf.len().min(self.data.len() - self.pos);
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            return Ok(n);
        }
        thread::sleep(self.eof_delay);
        Ok(0)
    }
}

#[test]
fn delayed_eof_cannot_consume_cleanup_reserve_or_publish() {
    let clock = EntryClock::capture_with(
        DurationMillis::new("stdin_total", 50, 3_000).unwrap(),
        DurationMillis::new("stdin_cleanup", 20, 3_000).unwrap(),
    )
    .unwrap();
    let mut reader = DelayedEofReader {
        data: b"payload".to_vec(),
        pos: 0,
        eof_delay: Duration::from_millis(40),
    };
    let mut buf = Vec::new();
    let err = read_stdin_before_cleanup(&clock, &mut reader, 1024, &mut buf).unwrap_err();
    assert_eq!(err, RuntimeError::StdinTimeout);
}

struct DelayedDataReader {
    data: Vec<u8>,
    delay: Duration,
}

impl Read for DelayedDataReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        thread::sleep(self.delay);
        let n = buf.len().min(self.data.len());
        buf[..n].copy_from_slice(&self.data[..n]);
        Ok(n)
    }
}

#[test]
fn delayed_data_cannot_consume_cleanup_reserve() {
    let clock = EntryClock::capture_with(
        DurationMillis::new("stdin_total", 50, 3_000).unwrap(),
        DurationMillis::new("stdin_cleanup", 20, 3_000).unwrap(),
    )
    .unwrap();
    let mut reader = DelayedDataReader {
        data: b"payload".to_vec(),
        delay: Duration::from_millis(40),
    };
    let mut buf = Vec::new();
    let err = read_stdin_before_cleanup(&clock, &mut reader, 1024, &mut buf).unwrap_err();
    assert_eq!(err, RuntimeError::StdinTimeout);
}

#[cfg(unix)]
mod pipe_tests {
    use super::*;
    use nix::unistd::pipe;
    use skillranker::runtime::read_fd_before_cleanup;
    use std::io::Write;
    use std::os::fd::{AsFd, AsRawFd};

    #[test]
    fn real_pipe_writer_held_open_terminates_within_deadline() {
        let (r, _w) = pipe().unwrap();
        let clock = EntryClock::capture_with(
            DurationMillis::new("pipe_total", 60, 3_000).unwrap(),
            DurationMillis::new("pipe_cleanup", 25, 3_000).unwrap(),
        )
        .unwrap();

        let start = std::time::Instant::now();
        let mut buf = Vec::new();
        let err = read_fd_before_cleanup(&clock, r.as_fd(), 1024, &mut buf).unwrap_err();
        let elapsed = start.elapsed();

        assert_eq!(err, RuntimeError::StdinTimeout);
        // Must terminate before the 60ms deadline + declared tolerance, not hang
        assert!(
            elapsed >= Duration::from_millis(30),
            "terminated too early: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(90),
            "stalled beyond deadline: {elapsed:?}"
        );
    }

    #[test]
    fn real_pipe_delayed_eof_terminates_within_deadline_and_never_publishes() {
        let (r, w) = pipe().unwrap();
        let clock = EntryClock::capture_with(
            DurationMillis::new("pipe_total", 60, 3_000).unwrap(),
            DurationMillis::new("pipe_cleanup", 25, 3_000).unwrap(),
        )
        .unwrap();

        let writer_handle = thread::spawn(move || {
            let mut file = std::fs::File::from(w);
            file.write_all(b"partial").unwrap();
            file.flush().unwrap();
            // Sleep past the reader's cleanup reserve before dropping/closing
            thread::sleep(Duration::from_millis(80));
        });

        let mut buf = Vec::new();
        let err = read_fd_before_cleanup(&clock, r.as_fd(), 1024, &mut buf).unwrap_err();
        assert_eq!(err, RuntimeError::StdinTimeout);

        writer_handle.join().unwrap();
    }

    #[test]
    fn real_pipe_timely_eof_succeeds() {
        let (r, w) = pipe().unwrap();
        let clock = EntryClock::capture_with(
            DurationMillis::new("pipe_total", 1_000, 3_000).unwrap(),
            DurationMillis::new("pipe_cleanup", 200, 3_000).unwrap(),
        )
        .unwrap();

        let writer_handle = thread::spawn(move || {
            let mut file = std::fs::File::from(w);
            file.write_all(b"timely_data").unwrap();
            // Drop immediately closes the pipe
        });

        let mut buf = Vec::new();
        read_fd_before_cleanup(&clock, r.as_fd(), 1024, &mut buf).unwrap();
        assert_eq!(buf, b"timely_data");

        writer_handle.join().unwrap();
    }

    #[test]
    fn real_pipe_exact_cap_and_cap_plus_one() {
        let clock = EntryClock::capture().unwrap();

        // Exact cap: write 10 bytes, close immediately -> succeeds
        {
            let (r, w) = pipe().unwrap();
            let mut file = std::fs::File::from(w);
            file.write_all(b"0123456789").unwrap();
            drop(file);

            let mut buf = Vec::new();
            read_fd_before_cleanup(&clock, r.as_fd(), 10, &mut buf).unwrap();
            assert_eq!(buf, b"0123456789");
        }

        // Cap + 1: write 11 bytes, cap=10 -> fails with AboveLimit
        {
            let (r, w) = pipe().unwrap();
            let mut file = std::fs::File::from(w);
            file.write_all(b"0123456789X").unwrap();
            drop(file);

            let mut buf = Vec::new();
            let err = read_fd_before_cleanup(&clock, r.as_fd(), 10, &mut buf).unwrap_err();
            assert!(matches!(
                err,
                RuntimeError::Deadline(LimitError::AboveLimit {
                    limit: 10,
                    observed: 11,
                    ..
                })
            ));
            assert_eq!(buf.len(), 10);
        }
    }

    #[test]
    fn real_pipe_no_descriptor_leak() {
        let fd_dir = std::path::Path::new("/proc/self/fd");
        if !fd_dir.exists() {
            return;
        }

        for _ in 0..5 {
            let (r, w) = pipe().unwrap();
            let clock = EntryClock::capture_with(
                DurationMillis::new("pipe_total", 50, 3_000).unwrap(),
                DurationMillis::new("pipe_cleanup", 20, 3_000).unwrap(),
            )
            .unwrap();

            let target = std::fs::read_link(format!("/proc/self/fd/{}", r.as_raw_fd())).unwrap();

            let mut buf = Vec::new();
            let _ = read_fd_before_cleanup(&clock, r.as_fd(), 10, &mut buf);
            drop((r, w));

            let open_links: Vec<_> = std::fs::read_dir(fd_dir)
                .unwrap()
                .filter_map(|entry| entry.ok())
                .filter_map(|entry| std::fs::read_link(entry.path()).ok())
                .collect();

            assert!(
                !open_links.contains(&target),
                "pipe descriptor leaked: {target:?} still open in /proc/self/fd"
            );
        }
    }
}

#[test]
#[ignore = "bounded runtime startup diagnostic"]
fn startup_capacity_diagnostic() {
    use asupersync::runtime::RuntimeBuilder;
    use std::sync::{Arc, Barrier};
    use std::time::Instant;
    for compact in [false, true, true, false] {
        let barrier = Arc::new(Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    let start = Instant::now();
                    let mut builder = RuntimeBuilder::current_thread().blocking_threads(1, 4);
                    if compact {
                        builder = builder.capacity_hints(16, 8, 16);
                    }
                    let runtime = builder.build().unwrap();
                    let built_ms = start.elapsed().as_millis();
                    let cx = runtime.request_cx_with_budget(asupersync::Budget::new());
                    runtime.block_on(async {
                        cx.checkpoint().unwrap();
                    });
                    assert!(runtime.shutdown_timeout(Duration::from_secs(1)));
                    built_ms
                })
            })
            .collect();
        let mut times: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        times.sort_unstable();
        eprintln!("case=startup compact={compact} build_ms={times:?}");
    }
}
