#![cfg(any(target_os = "linux", target_os = "macos"))]
use skillranker::limits::DurationMillis;
use skillranker::runtime::{EntryClock, ProcessInvocation};
use skillranker::subprocess::{ChildRequest, SubprocessError, TrustedExecutable, run};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn request(program: &str, args: &[&str]) -> ChildRequest {
    let canonical = Path::new(program)
        .canonicalize()
        .expect("installed fixture executable");
    let root = canonical
        .parent()
        .expect("absolute executable parent")
        .to_path_buf();
    ChildRequest {
        executable: TrustedExecutable::resolve(&canonical, &[root]).unwrap(),
        args: args.iter().map(Into::into).collect(),
        directory: PathBuf::from("/"),
        environment: Vec::new(),
        stdin: Vec::new(),
        stdout_limit: 1024,
        stderr_limit: 1024,
    }
}

#[test]
fn symlinked_executable_resolves_inside_trusted_root_only() {
    let temp = std::env::temp_dir().join(format!("sr-trust-{}", std::process::id()));
    std::fs::create_dir_all(&temp).unwrap();
    let link = temp.join("sleep-link");
    std::os::unix::fs::symlink("/bin/sleep", &link).unwrap();
    // The symlink's own parent is not a trust grant for the target.
    assert_eq!(
        TrustedExecutable::resolve(&link, std::slice::from_ref(&temp)).unwrap_err(),
        SubprocessError::InvalidRequest
    );
    // The canonical target resolves only under a root containing it.
    let canonical = link.canonicalize().unwrap();
    let root = canonical.parent().unwrap().to_path_buf();
    assert!(TrustedExecutable::resolve(&canonical, &[root]).is_ok());
    std::fs::remove_dir_all(&temp).unwrap();
}
fn invocation() -> ProcessInvocation {
    ProcessInvocation::from_clock(
        EntryClock::capture_with(
            DurationMillis::new("total", 400, 3000).unwrap(),
            DurationMillis::new("cleanup", 200, 3000).unwrap(),
        )
        .unwrap(),
    )
    .unwrap()
}
#[test]
fn sleeping_child_is_killed_and_reaped_before_two_seconds() {
    let invocation = invocation();
    let cx = invocation.request_cx().unwrap();
    let start = Instant::now();
    let result = invocation.runtime().block_on(run(
        &cx,
        &invocation.clock(),
        request("/bin/sleep", &["10"]),
    ));
    assert_eq!(result.unwrap_err(), SubprocessError::DeadlineExceeded);
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(invocation.shutdown());
}
#[test]
fn successful_child_output_is_preserved() {
    let invocation = invocation();
    let cx = invocation.request_cx().unwrap();
    let output = invocation
        .runtime()
        .block_on(run(
            &cx,
            &invocation.clock(),
            request("/bin/echo", &["hello"]),
        ))
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"hello\n");
    assert!(output.stderr.is_empty());
    assert!(invocation.shutdown());
}
#[test]
fn simultaneous_pipe_limits_and_environment_refusal() {
    let invocation = invocation();
    let cx = invocation.request_cx().unwrap();
    let mut child = request("/bin/echo", &["more than cap"]);
    child.stdout_limit = 2;
    assert_eq!(
        invocation
            .runtime()
            .block_on(run(&cx, &invocation.clock(), child))
            .unwrap_err(),
        SubprocessError::OutputLimit
    );
    let mut child = request("/bin/echo", &["never"]);
    child
        .environment
        .push(("TYPESAFE_API_KEY".into(), "synthetic-secret".into()));
    assert_eq!(
        invocation
            .runtime()
            .block_on(run(&cx, &invocation.clock(), child))
            .unwrap_err(),
        SubprocessError::InvalidRequest
    );
    assert!(invocation.shutdown());
}

#[test]
fn concurrent_saturated_pipes_are_drained_without_deadlock() {
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    // Fixed synthetic fixture, not interpolated user or repository text.
    let mut child = request(
        "/bin/sh",
        &[
            "-c",
            "i=0; while [ $i -lt 2000 ]; do printf 'abcdefgh01234567'; printf 'stderr0123456789' >&2; i=$((i+1)); done",
        ],
    );
    child.stdout_limit = 40_000;
    child.stderr_limit = 40_000;
    let output = invocation
        .runtime()
        .block_on(run(&cx, &invocation.clock(), child))
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"abcdefgh01234567".repeat(2000));
    assert_eq!(output.stderr, b"stderr0123456789".repeat(2000));
    assert!(invocation.shutdown());
}

#[test]
fn stderr_overflow_and_early_stdin_close_are_observable() {
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let mut child = request("/bin/sh", &["-c", "printf 'too much error' >&2"]);
    child.stderr_limit = 2;
    assert_eq!(
        invocation
            .runtime()
            .block_on(run(&cx, &invocation.clock(), child))
            .unwrap_err(),
        SubprocessError::OutputLimit
    );
    let mut child = request(
        "/bin/sh",
        &["-c", "exec 0<&-; /bin/sleep 0.05; printf done"],
    );
    child.stdin = vec![b'x'; 256 * 1024];
    let output = invocation
        .runtime()
        .block_on(run(&cx, &invocation.clock(), child))
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"done");
    assert!(output.stdin_closed_early);
    assert!(invocation.shutdown());
}

#[test]
fn child_environment_has_no_inherited_variables() {
    let invocation = invocation();
    let cx = invocation.request_cx().unwrap();
    let output = invocation
        .runtime()
        .block_on(run(&cx, &invocation.clock(), request("/usr/bin/env", &[])))
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"");
    assert!(invocation.shutdown());
}

#[test]
fn ignored_term_cannot_extend_deadline() {
    let invocation = invocation();
    let cx = invocation.request_cx().unwrap();
    let start = Instant::now();
    let child = request("/bin/sh", &["-c", "trap '' TERM; /bin/sleep 10"]);
    assert_eq!(
        invocation
            .runtime()
            .block_on(run(&cx, &invocation.clock(), child))
            .unwrap_err(),
        SubprocessError::DeadlineExceeded
    );
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(invocation.shutdown());
}

#[test]
fn descendant_holding_pipes_is_terminated_after_parent_exits() {
    let invocation = invocation();
    let cx = invocation.request_cx().unwrap();
    let start = Instant::now();
    let child = request("/bin/sh", &["-c", "/bin/sleep 10 & printf '%s' $!; exit 0"]);
    let output = invocation
        .runtime()
        .block_on(run(&cx, &invocation.clock(), child))
        .unwrap();
    assert!(output.status.success());
    assert!(start.elapsed() < Duration::from_secs(2));
    let pid: i32 = std::str::from_utf8(&output.stdout)
        .unwrap()
        .parse()
        .unwrap();
    // Linux do_exit closes files before exit_notify publishes EXIT_ZOMBIE
    // (kernel/exit.c). EOF can therefore precede the final /proc state. Observe
    // actual termination within the ORIGINAL total bound, not a fresh timeout.
    #[cfg(target_os = "linux")]
    assert!(
        terminated_by(pid, start + Duration::from_secs(2)).unwrap(),
        "descendant survived the process-group kill"
    );
    assert!(start.elapsed() < Duration::from_secs(2));
    #[cfg(target_os = "macos")]
    let _ = pid;
    assert!(invocation.shutdown());
}

#[test]
fn explicit_cancellation_terminates_live_child() {
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let canceller = cx.clone();
    let cancel = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        canceller.set_cancel_requested(true);
    });
    let result = invocation.runtime().block_on(run(
        &cx,
        &invocation.clock(),
        request("/bin/sleep", &["10"]),
    ));
    cancel.join().unwrap();
    assert_eq!(result.unwrap_err(), SubprocessError::Cancelled);
    assert!(invocation.shutdown());
}

/// A killed descendant is either a zombie awaiting its reaper, dead, or absent.
/// Other states (including stopped/uninterruptible) are not termination proof.
#[cfg(target_os = "linux")]
fn terminated_by(pid: i32, deadline: Instant) -> std::io::Result<bool> {
    loop {
        let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => stat,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(error) => return Err(error),
        };
        let state = stat
            .rsplit_once(") ")
            .and_then(|(_, suffix)| suffix.as_bytes().first())
            .copied()
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidData))?;
        if matches!(state, b'Z' | b'X') {
            return Ok(true);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        std::thread::sleep(remaining.min(Duration::from_millis(1)));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn termination_observer_rejects_a_live_child() {
    let mut child = std::process::Command::new("/bin/sleep")
        .env_clear()
        .arg("10")
        .spawn()
        .unwrap();
    let observation = terminated_by(
        i32::try_from(child.id()).unwrap(),
        Instant::now() + Duration::from_millis(20),
    );
    // Always clean up the deliberate survivor before asserting the observation.
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(
        !observation.unwrap(),
        "live sleeping process was accepted as terminated"
    );
}

// Eight MiB needs at least 1024 8-KiB passes. Sleeping after every
// successful pass cannot fit this work budget, even with zero spawn overhead.
fn bulk_invocation() -> ProcessInvocation {
    ProcessInvocation::from_clock(
        EntryClock::capture_with(
            DurationMillis::new("total", 1200, 3000).unwrap(),
            DurationMillis::new("cleanup", 200, 3000).unwrap(),
        )
        .unwrap(),
    )
    .unwrap()
}

#[test]
fn bulk_output_preserves_exact_cap_and_rejects_one_extra_byte() {
    use skillranker::subprocess::MAX_PIPE_BYTES;
    for extra in [0, 1] {
        let invocation = bulk_invocation();
        let cx = invocation.request_cx().unwrap();
        let bytes = (MAX_PIPE_BYTES + extra).to_string();
        let mut child = request("/usr/bin/head", &["-c", &bytes, "/dev/zero"]);
        child.stdout_limit = MAX_PIPE_BYTES;
        let result = invocation
            .runtime()
            .block_on(run(&cx, &invocation.clock(), child));
        eprintln!(
            "case=bulk-output extra={extra} elapsed_ms={} accepted={}",
            invocation.clock().now().as_millis(),
            result.is_ok()
        );
        if extra == 0 {
            let output = result.unwrap();
            assert!(output.status.success());
            assert_eq!(output.stdout.len(), MAX_PIPE_BYTES);
            assert!(output.stdout.iter().all(|byte| *byte == 0));
            assert!(output.stderr.is_empty());
        } else {
            assert_eq!(result.unwrap_err(), SubprocessError::OutputLimit);
        }
        assert!(invocation.shutdown());
    }
}

#[test]
fn bulk_stdin_and_stdout_make_progress_together() {
    use skillranker::subprocess::MAX_PIPE_BYTES;
    let mut child = request("/bin/cat", &[]);
    child.stdin = (0..MAX_PIPE_BYTES).map(|i| (i % 251) as u8).collect();
    child.stdout_limit = MAX_PIPE_BYTES;
    let expected = child.stdin.clone();
    let invocation = bulk_invocation();
    let cx = invocation.request_cx().unwrap();
    let output = invocation
        .runtime()
        .block_on(run(&cx, &invocation.clock(), child))
        .unwrap();
    eprintln!(
        "case=bulk-duplex bytes={} elapsed_ms={}",
        output.stdout.len(),
        invocation.clock().now().as_millis()
    );
    assert!(output.status.success());
    assert_eq!(output.stdout, expected);
    assert!(output.stderr.is_empty());
    assert!(!output.stdin_closed_early);
    assert!(invocation.shutdown());
}
