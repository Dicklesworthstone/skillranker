//! Bootstrap command boundary. Product commands are added only with their gates.
#![forbid(unsafe_code)]

use std::io::{self, Write};
use std::process::ExitCode;

use skillranker::runtime::EntryClock;

const HELP: &str = "SkillRanker (sr) — implementation in progress\n\n\
Usage: sr --help | --version\n\n\
Only help and version are available in this foundation build.\n\
Planned ranking uses TypeSafe.ai Jev and requires your own API key\n\
plus explicit trusted network consent.\n";

fn main() -> ExitCode {
    // Deadline starts at process entry, before argument parsing or I/O.
    let _clock = match EntryClock::capture() {
        Ok(clock) => clock,
        Err(_) => return ExitCode::from(6),
    };
    let mut args = std::env::args_os().skip(1);
    let first = args.next();
    if args.next().is_none() {
        let output = match first.as_deref().and_then(std::ffi::OsStr::to_str) {
            Some("--help" | "-h") => Some(HELP),
            Some("--version" | "-V") => Some(concat!("sr ", env!("CARGO_PKG_VERSION"), "\n")),
            _ => None,
        };
        if let Some(output) = output {
            return match io::stdout().lock().write_all(output.as_bytes()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(_) => ExitCode::from(1),
            };
        }
    }
    // Never echo unknown arguments: they can contain private paths or secrets.
    let _ = writeln!(io::stderr().lock(), "sr: unsupported arguments; use --help");
    ExitCode::from(2)
}
