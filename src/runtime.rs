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
        Self::from_clock_with_blocking_pool(clock, 1, 4)
    }

    pub fn from_clock_with_blocking_pool(
        clock: EntryClock,
        min_threads: usize,
        max_threads: usize,
    ) -> Result<Self, RuntimeError> {
        let runtime = RuntimeBuilder::current_thread()
            .blocking_threads(min_threads.max(1), max_threads.max(1))
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

    pub fn shutdown(self) -> bool {
        let remaining = self.clock.remaining_until_expiry();
        let bound = Duration::from_millis(remaining.as_millis().max(1));
        self.runtime.shutdown_timeout(bound)
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
    clock
        .admit_new_work()
        .map_err(|_| RuntimeError::StdinTimeout)?;
    buf.clear();
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        clock
            .admit_new_work()
            .map_err(|_| RuntimeError::StdinTimeout)?;
        if buf.len() >= max_bytes {
            return Err(LimitError::AboveLimit {
                name: "hook_stdin",
                observed: buf.len() as u128 + 1,
                limit: max_bytes as u128,
                unit: crate::limits::LimitUnit::Bytes,
            }
            .into());
        }
        let want = chunk.len().min(max_bytes.saturating_sub(buf.len()));
        match reader.read(&mut chunk[..want]) {
            Ok(0) => return Ok(()),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(RuntimeError::StdinTimeout),
        }
    }
}

pub const fn default_deadline_ms() -> u64 {
    DEFAULT_INVOCATION_DEADLINE_MS
}

pub const fn default_cleanup_reserve_ms() -> u64 {
    DEFAULT_OUTPUT_CLEANUP_RESERVE_MS
}
