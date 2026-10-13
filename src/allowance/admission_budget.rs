//! One monotonic budget for a configured allowance's complete admission.
//!
//! Copies retain the original start; opening another connection never grants
//! more time. SQLite progress cancellation and bounded busy waits cooperate with
//! checkpoints around filesystem work. Neither interrupts a kernel wait or sync.
use super::{AllowanceError, state};
use rusqlite::Connection;
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
pub(super) struct AdmissionBudget {
    started: Instant,
    limit: Duration,
}

impl AdmissionBudget {
    pub(super) fn start(limit: Duration) -> Self {
        Self {
            started: Instant::now(),
            limit,
        }
    }

    fn remaining_at(self, now: Instant) -> Duration {
        self.limit
            .saturating_sub(now.saturating_duration_since(self.started))
    }

    pub(super) fn remaining(self) -> Result<Duration, AllowanceError> {
        let remaining = self.remaining_at(Instant::now());
        if remaining.is_zero() {
            Err(state(
                "the shared allowance admission deadline expired; not sent",
            ))
        } else {
            Ok(remaining)
        }
    }

    pub(super) fn check(self) -> Result<(), AllowanceError> {
        self.remaining().map(|_| ())
    }

    pub(super) fn refresh_busy_wait(self, connection: &Connection) -> Result<(), AllowanceError> {
        connection
            .busy_timeout(self.remaining()?.min(Duration::from_millis(25)))
            .map_err(|_| state("the accounting busy wait cannot be bounded"))
    }

    pub(super) fn install_progress_handler(
        self,
        connection: &Connection,
    ) -> Result<(), AllowanceError> {
        // No database access or error allocation inside SQLite's callback.
        connection
            .progress_handler(
                100,
                Some(move || self.remaining_at(Instant::now()).is_zero()),
            )
            .map_err(|_| state("the accounting progress check cannot be installed"))
    }
}

pub(super) fn checkpoint(budget: Option<AdmissionBudget>) -> Result<(), AllowanceError> {
    match budget {
        Some(budget) => budget.check(),
        None => Ok(()), // Setup and inspection keep their existing policies.
    }
}

#[cfg(test)]
#[path = "admission_budget_tests.rs"]
mod tests;
