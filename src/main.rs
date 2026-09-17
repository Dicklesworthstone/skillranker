//! Standalone command boundary; the deadline starts before parsing or I/O.
#![forbid(unsafe_code)]

use skillranker::runtime::EntryClock;
use std::process::ExitCode;

fn main() -> ExitCode {
    match EntryClock::capture() {
        Ok(clock) => ExitCode::from(skillranker::cli::run(clock)),
        Err(_) => ExitCode::from(6),
    }
}
