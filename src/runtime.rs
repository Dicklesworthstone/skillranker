//! Process-owned Asupersync runtime, entry deadline, and publication gate.
//!
//! Capture the monotonic entry clock before stdin, configuration, or discovery.
//! Owned work uses an explicit `&Cx` and remaining-time budget. Results that
//! complete in the cleanup reserve or after expiry cannot be published.

use crate::limits::{
    DEFAULT_INVOCATION_DEADLINE_MS, DEFAULT_OUTPUT_CLEANUP_RESERVE_MS, DurationMillis,
    InvocationDeadline, LimitError, MonotonicMillis,
};
use asupersync::runtime::{Runtime, RuntimeBuilder};
use asupersync::{Budget, CancelKind, Cx};
use std::io::{self, Read};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeError {
    RuntimeUnavailable,
    Deadline(LimitError),
    LateResultSuppressed,
    Cancelled,
    StdinTimeout,
    StdinIo,
    UnboundedLeaf,
    BlockingPoolUnavailable,
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RuntimeUnavailable => f.write_str("asupersync runtime could not be constructed"),
            Self::Deadline(err) => write!(f, "{err}"),
            Self::LateResultSuppressed => {
                f.write_str("late result suppressed after cleanup reserve or expiry")
            }
            Self::Cancelled => f.write_str("invocation cancelled"),
            Self::StdinTimeout => f.write_str("stdin read reached the cleanup reserve"),
            Self::StdinIo => f.write_str("stdin read I/O error"),
            Self::UnboundedLeaf => {
                f.write_str("uninterruptible blocking leaf is not admitted on the hook path")
            }
            Self::BlockingPoolUnavailable => f.write_str("blocking pool could not admit the leaf"),
        }
    }
}

impl std::error::Error for RuntimeError {}

impl From<LimitError> for RuntimeError {
    fn from(err: LimitError) -> Self {
        Self::Deadline(err)
    }
}

/// Clock captured at process entry, before argument parsing or I/O.
#[derive(Clone, Copy, Debug)]
pub struct EntryClock {
    started: Instant,
    deadline: InvocationDeadline,
}

impl EntryClock {
    pub fn capture() -> Result<Self, RuntimeError> {
        let started = Instant::now();
        let deadline = InvocationDeadline::default_from_start(MonotonicMillis::from_millis(0))?;
        Ok(Self { started, deadline })
    }

    pub fn capture_with(
        total: DurationMillis,
        cleanup_reserve: DurationMillis,
    ) -> Result<Self, RuntimeError> {
        let started = Instant::now();
        let deadline =
            InvocationDeadline::new(MonotonicMillis::from_millis(0), total, cleanup_reserve)?;
        Ok(Self { started, deadline })
    }

    /// The same entry instant with another total deadline, for a deadline
    /// chosen by configuration read after entry. Expiry is still measured from
    /// process entry, and the cleanup reserve is unchanged.
    pub fn with_total(self, total: DurationMillis) -> Result<Self, RuntimeError> {
        let deadline = InvocationDeadline::new(
            MonotonicMillis::from_millis(0),
            total,
            self.deadline.cleanup_reserve(),
        )?;
        Ok(Self {
            started: self.started,
            deadline,
        })
    }

    /// The same deadline with half of the cleanup reserve returned to work, for recording
    /// that this invocation failed.
    ///
    /// A run that fails at its work deadline has nothing left to record the failure with, so
    /// every timeout used to vanish from the ledger, and timeouts are exactly the operational
    /// failures an availability measure has to count (sr-73b6). Finalizing one typed
    /// `unavailable` row is cleanup, not new work: it publishes no result, and the other half
    /// of the reserve stays for output and runtime shutdown. Nothing else may use this clock.
    pub fn for_failure_finalization(self) -> Self {
        let reserve = self.deadline.cleanup_reserve().as_millis();
        let deadline = DurationMillis::new(
            "failure_finalization_reserve",
            (reserve / 2).max(1),
            reserve,
        )
        .and_then(|reserve| {
            InvocationDeadline::new(self.deadline.start(), self.deadline.total(), reserve)
        })
        .expect("half of a valid cleanup reserve is a valid cleanup reserve");
        Self {
            started: self.started,
            deadline,
        }
    }

    pub fn now(&self) -> MonotonicMillis {
        let elapsed = self.started.elapsed();
        let ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        MonotonicMillis::from_millis(ms)
    }

    pub const fn deadline(self) -> InvocationDeadline {
        self.deadline
    }

    pub fn admit_new_work(&self) -> Result<MonotonicMillis, RuntimeError> {
        let now = self.now();
        self.deadline.ensure_can_start_work(now, "runtime_work")?;
        Ok(now)
    }

    pub fn remaining_before_cleanup(&self) -> DurationMillis {
        self.deadline.remaining_before_cleanup(self.now())
    }

    pub fn remaining_until_expiry(&self) -> DurationMillis {
        self.deadline.remaining_until_expiry(self.now())
    }

    pub fn work_budget(&self) -> Result<Budget, RuntimeError> {
        let now = self.admit_new_work()?;
        let remaining = self.deadline.remaining_before_cleanup(now);
        Ok(budget_until(remaining))
    }

    pub fn cleanup_budget(&self) -> Budget {
        let now = self.now();
        let remaining = self.deadline.remaining_until_expiry(now);
        if remaining.as_millis() == 0 {
            Budget::MINIMAL
        } else {
            budget_until(remaining)
        }
    }
}

fn budget_until(remaining: DurationMillis) -> Budget {
    // Asupersync's native timers share a process epoch, not EntryClock's
    // invocation epoch. Convert the remaining duration using the timer clock.
    Budget::new().with_timeout(
        asupersync::time::wall_now(),
        Duration::from_millis(remaining.as_millis()),
    )
}

/// Decide whether a completed result may be emitted.
///
/// New work stops at the cleanup reserve. A result whose completion timestamp
/// is already in that reserve, or after expiry, is suppressed even if emission
/// is still in progress.
pub fn admit_publication(
    deadline: InvocationDeadline,
    completed_at: MonotonicMillis,
    now: MonotonicMillis,
) -> Result<(), RuntimeError> {
    if now >= deadline.expires_at() || completed_at >= deadline.latest_work_time() {
        return Err(RuntimeError::LateResultSuppressed);
    }
    Ok(())
}

/// One process invocation: entry clock plus an owned current-thread runtime.
pub struct ProcessInvocation {
    clock: EntryClock,
    runtime: Runtime,
}

impl ProcessInvocation {
    /// Build the runtime after the entry clock is already running.
    pub fn enter() -> Result<Self, RuntimeError> {
        let clock = EntryClock::capture()?;
        Self::from_clock(clock)
    }

    pub fn from_clock(clock: EntryClock) -> Result<Self, RuntimeError> {
        // An invocation need not submit a blocking leaf. Keep the pool
        // available, but let Asupersync start its first worker on submission
        // instead of spending the entry budget starting an idle thread.
        Self::from_clock_with_blocking_pool(clock, 0, 4)
    }

    pub fn from_clock_with_blocking_pool(
        clock: EntryClock,
        min_threads: usize,
        max_threads: usize,
    ) -> Result<Self, RuntimeError> {
        // Runtime allocation and eager worker startup are work too. In
        // particular, never spend the cleanup reserve constructing a runtime
        // that cannot admit even its first request.
        clock.admit_new_work()?;
        let runtime = RuntimeBuilder::current_thread()
            .blocking_threads(min_threads, max_threads.max(1))
            .build()
            .map_err(|_| RuntimeError::RuntimeUnavailable)?;
        Ok(Self { clock, runtime })
    }

    pub const fn clock(&self) -> EntryClock {
        self.clock
    }

    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    pub fn request_cx(&self) -> Result<Cx, RuntimeError> {
        Ok(self
            .runtime
            .request_cx_with_budget(self.clock.work_budget()?))
    }

    pub fn request_cleanup_cx(&self) -> Cx {
        self.runtime
            .request_cx_with_budget(self.clock.cleanup_budget())
    }

    pub fn cancel_user(&self, cx: &Cx) {
        cx.cancel_with(CancelKind::User, Some("process cancellation"));
    }

    /// Drive `work` on the invocation's runtime, routing SIGTERM and SIGINT to
    /// user cancellation of `cx`, so the work drains and finalizes on its own
    /// cancellation paths instead of dying mid-flight. Claude's hook timeout
    /// sends SIGTERM and kills only about 1.2 s later; a signalled turn is then
    /// recorded, not left in flight (sr-c4v6). The signal streams are polled
    /// beside the work, never spawned, so nothing outlives the invocation.
    /// Where a signal stream is unavailable, the work simply runs.
    pub fn block_on_cancellable<F: std::future::Future>(&self, cx: &Cx, work: F) -> F::Output {
        use asupersync::signal::{SignalKind, signal};
        use std::future::Future;
        use std::task::Poll;
        self.runtime.block_on(async {
            let mut signals: Vec<_> = [SignalKind::terminate(), SignalKind::interrupt()]
                .into_iter()
                .filter_map(|kind| signal(kind).ok())
                .collect();
            let mut work = std::pin::pin!(work);
            let mut cancelled = false;
            std::future::poll_fn(|task| {
                if let Poll::Ready(output) = work.as_mut().poll(task) {
                    return Poll::Ready(output);
                }
                if !cancelled {
                    // `recv` is cancel-safe and the delivery count lives on
                    // the stream, so a fresh receive per poll misses nothing.
                    let signalled = signals
                        .iter_mut()
                        .any(|stream| std::pin::pin!(stream.recv()).poll(task).is_ready());
                    if signalled {
                        cancelled = true;
                        self.cancel_user(cx);
                        // Let the work observe the cancellation now.
                        return work.as_mut().poll(task);
                    }
                }
                Poll::Pending
            })
            .await
        })
    }

    pub fn shutdown(self) -> bool {
        let remaining = self.clock.remaining_until_expiry();
        let bound = Duration::from_millis(remaining.as_millis().max(1));
        self.runtime.shutdown_timeout(bound)
    }

    /// Shut the owned runtime down within an explicit wall-clock bound rather
    /// than the invocation's remaining budget. Production paths use
    /// `shutdown()`, which keeps the process-entry deadline semantics; this
    /// variant exists for test harnesses that need a fresh, bounded drain
    /// window after a scenario deliberately consumed nearly all of its own
    /// budget — a shared CI worker cannot guarantee scheduler time inside a
    /// remainder measured in tens of milliseconds. A genuinely wedged runtime
    /// still fails: it will not drain within any honest bound.
    pub fn shutdown_within(self, bound: Duration) -> bool {
        self.runtime
            .shutdown_timeout(bound.max(Duration::from_millis(1)))
    }
}

/// Pure bounded decoder from a generic synchronous reader bounded by the clock's
/// work deadline and `max_bytes`.
///
/// Validates admission before and after reading, ensures completion time before
/// returning `Ok(())`, and validates EOF when exactly `max_bytes` have been read.
pub fn decode_bounded<R: Read>(
    clock: &EntryClock,
    reader: &mut R,
    max_bytes: usize,
    buf: &mut Vec<u8>,
) -> Result<(), RuntimeError> {
    clock
        .admit_new_work()
        .map_err(|_| RuntimeError::StdinTimeout)?;
    buf.clear();

    if max_bytes == 0 {
        let mut probe = [0u8; 1];
        loop {
            clock
                .admit_new_work()
                .map_err(|_| RuntimeError::StdinTimeout)?;
            match reader.read(&mut probe) {
                Ok(0) => {
                    clock
                        .admit_new_work()
                        .map_err(|_| RuntimeError::StdinTimeout)?;
                    return Ok(());
                }
                Ok(_) => {
                    return Err(LimitError::AboveLimit {
                        name: "hook_stdin",
                        observed: 1,
                        limit: 0,
                        unit: crate::limits::LimitUnit::Bytes,
                    }
                    .into());
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == io::ErrorKind::TimedOut => {
                    return Err(RuntimeError::StdinTimeout);
                }
                Err(_) => return Err(RuntimeError::StdinIo),
            }
        }
    }

    let mut chunk = [0_u8; 8 * 1024];
    loop {
        clock
            .admit_new_work()
            .map_err(|_| RuntimeError::StdinTimeout)?;

        if buf.len() == max_bytes {
            let mut probe = [0u8; 1];
            loop {
                clock
                    .admit_new_work()
                    .map_err(|_| RuntimeError::StdinTimeout)?;
                match reader.read(&mut probe) {
                    Ok(0) => {
                        clock
                            .admit_new_work()
                            .map_err(|_| RuntimeError::StdinTimeout)?;
                        return Ok(());
                    }
                    Ok(n) => {
                        return Err(LimitError::AboveLimit {
                            name: "hook_stdin",
                            observed: (max_bytes + n) as u128,
                            limit: max_bytes as u128,
                            unit: crate::limits::LimitUnit::Bytes,
                        }
                        .into());
                    }
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                    Err(err) if err.kind() == io::ErrorKind::TimedOut => {
                        return Err(RuntimeError::StdinTimeout);
                    }
                    Err(_) => return Err(RuntimeError::StdinIo),
                }
            }
        }

        let want = chunk.len().min(max_bytes.saturating_sub(buf.len()));
        match reader.read(&mut chunk[..want]) {
            Ok(0) => {
                clock
                    .admit_new_work()
                    .map_err(|_| RuntimeError::StdinTimeout)?;
                return Ok(());
            }
            Ok(n) => {
                clock
                    .admit_new_work()
                    .map_err(|_| RuntimeError::StdinTimeout)?;
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == io::ErrorKind::TimedOut => {
                return Err(RuntimeError::StdinTimeout);
            }
            Err(_) => return Err(RuntimeError::StdinIo),
        }
    }
}

/// Bound a stdin read by the remaining work window. Partial reads that stop
/// because the cleanup reserve arrived are reported as timeout, not as a
/// complete document.
pub fn read_stdin_before_cleanup<R: Read>(
    clock: &EntryClock,
    reader: &mut R,
    max_bytes: usize,
    buf: &mut Vec<u8>,
) -> Result<(), RuntimeError> {
    decode_bounded(clock, reader, max_bytes, buf)
}

/// Read from an owned or borrowed Unix file descriptor (like stdin or a pipe),
/// polling for readability bounded by the remaining work deadline before each read
/// so that waits can be interrupted and never exceed the cleanup reserve.
#[cfg(unix)]
pub fn read_fd_before_cleanup<F: std::os::fd::AsFd>(
    clock: &EntryClock,
    fd: F,
    max_bytes: usize,
    buf: &mut Vec<u8>,
) -> Result<(), RuntimeError> {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    use nix::unistd::read;

    clock
        .admit_new_work()
        .map_err(|_| RuntimeError::StdinTimeout)?;
    buf.clear();

    let raw_fd = fd.as_fd();

    let poll_readable = |clock: &EntryClock| -> Result<(), RuntimeError> {
        loop {
            clock
                .admit_new_work()
                .map_err(|_| RuntimeError::StdinTimeout)?;
            let remaining_ms = clock.remaining_before_cleanup().as_millis();
            if remaining_ms == 0 {
                return Err(RuntimeError::StdinTimeout);
            }
            let timeout_ms = u64::min(remaining_ms, i32::MAX as u64) as i32;
            let timeout =
                PollTimeout::try_from(timeout_ms).map_err(|_| RuntimeError::StdinTimeout)?;
            let mut pfd = PollFd::new(
                raw_fd,
                PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR,
            );
            match poll(std::slice::from_mut(&mut pfd), timeout) {
                Ok(0) => return Err(RuntimeError::StdinTimeout),
                Ok(_) => {
                    clock
                        .admit_new_work()
                        .map_err(|_| RuntimeError::StdinTimeout)?;
                    return Ok(());
                }
                Err(nix::errno::Errno::EINTR) => continue,
                Err(_) => return Err(RuntimeError::StdinIo),
            }
        }
    };

    if max_bytes == 0 {
        poll_readable(clock)?;
        let mut probe = [0u8; 1];
        let n = loop {
            match read(raw_fd, &mut probe) {
                Ok(n) => break n,
                Err(nix::errno::Errno::EINTR) => continue,
                Err(_) => return Err(RuntimeError::StdinIo),
            }
        };
        clock
            .admit_new_work()
            .map_err(|_| RuntimeError::StdinTimeout)?;
        if n == 0 {
            return Ok(());
        } else {
            return Err(LimitError::AboveLimit {
                name: "hook_stdin",
                observed: 1,
                limit: 0,
                unit: crate::limits::LimitUnit::Bytes,
            }
            .into());
        }
    }

    let mut chunk = [0_u8; 8 * 1024];
    loop {
        clock
            .admit_new_work()
            .map_err(|_| RuntimeError::StdinTimeout)?;

        if buf.len() == max_bytes {
            poll_readable(clock)?;
            let mut probe = [0u8; 1];
            let n = loop {
                match read(raw_fd, &mut probe) {
                    Ok(n) => break n,
                    Err(nix::errno::Errno::EINTR) => continue,
                    Err(_) => return Err(RuntimeError::StdinIo),
                }
            };
            clock
                .admit_new_work()
                .map_err(|_| RuntimeError::StdinTimeout)?;
            if n == 0 {
                return Ok(());
            } else {
                return Err(LimitError::AboveLimit {
                    name: "hook_stdin",
                    observed: (max_bytes + n) as u128,
                    limit: max_bytes as u128,
                    unit: crate::limits::LimitUnit::Bytes,
                }
                .into());
            }
        }

        poll_readable(clock)?;
        let want = chunk.len().min(max_bytes.saturating_sub(buf.len()));
        let n = loop {
            match read(raw_fd, &mut chunk[..want]) {
                Ok(n) => break n,
                Err(nix::errno::Errno::EINTR) => continue,
                Err(_) => return Err(RuntimeError::StdinIo),
            }
        };
        clock
            .admit_new_work()
            .map_err(|_| RuntimeError::StdinTimeout)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Read from standard input on supported Unix platforms with deadline-bounded polling.
#[cfg(unix)]
pub fn read_stdin_platform_before_cleanup(
    clock: &EntryClock,
    max_bytes: usize,
    buf: &mut Vec<u8>,
) -> Result<(), RuntimeError> {
    read_fd_before_cleanup(clock, io::stdin(), max_bytes, buf)
}

#[cfg(not(unix))]
pub fn read_stdin_platform_before_cleanup(
    _clock: &EntryClock,
    _max_bytes: usize,
    _buf: &mut Vec<u8>,
) -> Result<(), RuntimeError> {
    Err(RuntimeError::UnboundedLeaf)
}

pub const fn default_deadline_ms() -> u64 {
    DEFAULT_INVOCATION_DEADLINE_MS
}

pub const fn default_cleanup_reserve_ms() -> u64 {
    DEFAULT_OUTPUT_CLEANUP_RESERVE_MS
}
