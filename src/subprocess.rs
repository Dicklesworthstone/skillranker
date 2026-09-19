//! Owned Unix subprocesses with bounded nonblocking pipes and explicit authority.
//!
//! The caller must trust the executable, its arguments, and its local filesystem.
//! Process groups contain cooperative descendants, not children that deliberately
//! escape with setsid. Synchronous OS spawn and uninterruptible kernel I/O cannot
//! be given a hard wall-time guarantee. Cancellation kills the group and reaps the
//! direct child before return; no reader or reaper tasks are detached.
use crate::runtime::EntryClock;
use asupersync::Cx;
use asupersync::io::{AsyncRead, AsyncWrite, ReadBuf};
use asupersync::process::{Command, ExitStatus, ProcessGroupMode, ProcessSignalTarget, Stdio};
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

pub const MAX_PIPE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_ARGUMENT_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubprocessError {
    InvalidRequest,
    UnsupportedPlatform,
    Spawn,
    Io,
    OutputLimit,
    DeadlineExceeded,
    Cancelled,
    Cleanup,
}
impl fmt::Display for SubprocessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidRequest => "invalid trusted subprocess request",
            Self::UnsupportedPlatform => "subprocess containment unsupported on this platform",
            Self::Spawn => "trusted subprocess could not start",
            Self::Io => "subprocess pipe operation failed",
            Self::OutputLimit => "subprocess output exceeded its byte limit",
            Self::DeadlineExceeded => "subprocess exceeded its work deadline",
            Self::Cancelled => "subprocess cancelled",
            Self::Cleanup => "subprocess cleanup failed",
        })
    }
}
impl std::error::Error for SubprocessError {}

/// Construction is an explicit caller authority grant, never discovery from input.
/// The canonical executable must lie inside one of the caller's trusted roots.
#[derive(Clone)]
pub struct TrustedExecutable(PathBuf);
impl fmt::Debug for TrustedExecutable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TrustedExecutable(<private>)")
    }
}
impl TrustedExecutable {
    pub fn resolve(path: &Path, trusted_roots: &[PathBuf]) -> Result<Self, SubprocessError> {
        if !path.is_absolute() || trusted_roots.is_empty() || trusted_roots.len() > 32 {
            return Err(SubprocessError::InvalidRequest);
        }
        let canonical = path
            .canonicalize()
            .map_err(|_| SubprocessError::InvalidRequest)?;
        if !canonical.is_file() {
            return Err(SubprocessError::InvalidRequest);
        }
        let mut allowed = false;
        for root in trusted_roots {
            if !root.is_absolute() {
                return Err(SubprocessError::InvalidRequest);
            }
            let root = root
                .canonicalize()
                .map_err(|_| SubprocessError::InvalidRequest)?;
            allowed |= canonical.starts_with(root);
        }
        if !allowed {
            return Err(SubprocessError::InvalidRequest);
        }
        Ok(Self(canonical))
    }
}

/// Environment is empty by default. Only these non-routing scalar variables can
/// be supplied: LANG, LC_ALL, TZ. PATH, HOME, loader, provider, proxy and Git
/// configuration variables are not forwarded. Programs must use absolute argv.
pub struct ChildRequest {
    pub executable: TrustedExecutable,
    pub args: Vec<OsString>,
    pub directory: PathBuf,
    pub environment: Vec<(OsString, OsString)>,
    pub stdin: Vec<u8>,
    pub stdout_limit: usize,
    pub stderr_limit: usize,
}
impl fmt::Debug for ChildRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ChildRequest(<private>)")
    }
}
pub struct ChildOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdin_closed_early: bool,
}
impl fmt::Debug for ChildOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChildOutput")
            .field("status", &self.status)
            .field("stdout_bytes", &self.stdout.len())
            .field("stderr_bytes", &self.stderr.len())
            .field("stdin_closed_early", &self.stdin_closed_early)
            .finish()
    }
}
impl ChildRequest {
    fn validate(&self) -> Result<(), SubprocessError> {
        if !self.directory.is_absolute()
            || self.args.len() > 1024
            || self.stdin.len() > MAX_PIPE_BYTES
            || self.stdout_limit > MAX_PIPE_BYTES
            || self.stderr_limit > MAX_PIPE_BYTES
            || self.environment.len() > 3
        {
            return Err(SubprocessError::InvalidRequest);
        }
        let mut bytes = 0usize;
        for arg in &self.args {
            bytes = bytes
                .checked_add(arg.len())
                .ok_or(SubprocessError::InvalidRequest)?;
            if bytes > MAX_ARGUMENT_BYTES || arg.as_encoded_bytes().contains(&0) {
                return Err(SubprocessError::InvalidRequest);
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for (key, value) in &self.environment {
            if !matches!(key.to_str(), Some("LANG" | "LC_ALL" | "TZ"))
                || !seen.insert(key)
                || value.len() > 128
                || value
                    .as_encoded_bytes()
                    .iter()
                    .any(|b| *b < 32 || *b == 127)
            {
                return Err(SubprocessError::InvalidRequest);
            }
        }
        Ok(())
    }
}

fn read_pipe<R: AsyncRead + Unpin>(
    pipe: &mut Option<R>,
    output: &mut Vec<u8>,
    cap: usize,
    cx: &mut Context<'_>,
) -> Result<(), SubprocessError> {
    let Some(reader) = pipe.as_mut() else {
        return Ok(());
    };
    let mut bytes = [0u8; 8192];
    let want = bytes
        .len()
        .min(cap.saturating_sub(output.len()).saturating_add(1));
    let mut buf = ReadBuf::new(&mut bytes[..want]);
    match Pin::new(reader).poll_read(cx, &mut buf) {
        Poll::Ready(Ok(())) => {
            if buf.filled().is_empty() {
                *pipe = None;
            } else if buf.filled().len() > cap.saturating_sub(output.len()) {
                return Err(SubprocessError::OutputLimit);
            } else {
                output.extend_from_slice(buf.filled());
            }
        }
        Poll::Ready(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => {}
        Poll::Ready(Err(_)) => return Err(SubprocessError::Io),
        Poll::Pending => {}
    }
    Ok(())
}

/// Drives one owned child without detached workers. The returned future must be
/// driven to completion after Cx cancellation; dropping it early synchronously
/// kills/reaps via the owner guard, rather than leaving an unowned child.
pub async fn run(
    cx: &Cx,
    clock: &EntryClock,
    request: ChildRequest,
) -> Result<ChildOutput, SubprocessError> {
    request.validate()?;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (cx, clock);
        Err(SubprocessError::UnsupportedPlatform)
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        check_work(cx, clock)?;
        let child = Command::new(&request.executable.0)
            .args(&request.args)
            .env_clear()
            .envs(request.environment.iter().map(|(k, v)| (k, v)))
            .current_dir(&request.directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group_mode(ProcessGroupMode::NewProcessGroup)
            .signal_target(ProcessSignalTarget::ProcessGroup)
            .kill_on_drop(false)
            .spawn()
            .map_err(|_| SubprocessError::Spawn)?;
        let mut owner = Owner {
            group: child.process_group_id().ok_or(SubprocessError::Cleanup)?,
            child,
            reaped: false,
            killed: false,
        };
        let mut input = owner.child.stdin();
        let mut stdout = owner.child.stdout();
        let mut stderr = owner.child.stderr();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut offset = 0;
        let mut early = false;
        let mut status = None;
        let result = loop {
            if let Err(error) = check_work(cx, clock) {
                break Err(error);
            }
            let before = (out.len(), err.len(), offset);
            let step = {
                let mut task = Context::from_waker(Waker::noop());
                read_pipe(&mut stdout, &mut out, request.stdout_limit, &mut task)
                    .and_then(|()| {
                        read_pipe(&mut stderr, &mut err, request.stderr_limit, &mut task)
                    })
                    .and_then(|()| {
                        if offset == request.stdin.len() {
                            input = None;
                        }
                        if let Some(writer) = input.as_mut() {
                            let end = request.stdin.len().min(offset.saturating_add(8192));
                            match Pin::new(writer)
                                .poll_write(&mut task, &request.stdin[offset..end])
                            {
                                Poll::Ready(Ok(0)) => return Err(SubprocessError::Io),
                                Poll::Ready(Ok(n)) => offset += n,
                                Poll::Ready(Err(e))
                                    if e.kind() == std::io::ErrorKind::BrokenPipe =>
                                {
                                    early = true;
                                    input = None;
                                }
                                Poll::Ready(Err(e))
                                    if e.kind() == std::io::ErrorKind::Interrupted => {}
                                Poll::Ready(Err(_)) => return Err(SubprocessError::Io),
                                Poll::Pending => {}
                            }
                        }
                        Ok(())
                    })
            };
            if let Err(error) = step {
                break Err(error);
            }
            if status.is_none() {
                match owner.child.try_wait() {
                    Ok(Some(s)) => {
                        owner.reaped = true;
                        status = Some(s);
                        if let Err(e) = owner.kill_group() {
                            break Err(e);
                        }
                    }
                    Ok(None) => {}
                    Err(_) => break Err(SubprocessError::Io),
                }
            }
            if status.is_some() && stdout.is_none() && stderr.is_none() {
                break check_work(cx, clock).map(|()| ChildOutput {
                    status: status.take().expect("checked child status"),
                    stdout: out,
                    stderr: err,
                    stdin_closed_early: early,
                });
            }
            if (out.len(), err.len(), offset) != before {
                // Each pass services all three pipes and rechecks the deadline.
                // Yield fairly without imposing a timer delay on every 8 KiB:
                // a large, ready export must not spend its budget sleeping.
                asupersync::runtime::yield_now().await;
            } else {
                // Pipe polls use a noop waker, so idle children still require
                // bounded polling. Back off only when no bytes moved.
                asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(1))
                    .await;
            }
        };
        drop((input, stdout, stderr));
        owner.cleanup()?;
        result
    }
}
fn check_work(cx: &Cx, clock: &EntryClock) -> Result<(), SubprocessError> {
    clock
        .admit_new_work()
        .map_err(|_| SubprocessError::DeadlineExceeded)?;
    if cx.checkpoint().is_err() {
        return Err(SubprocessError::Cancelled);
    }
    Ok(())
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct Owner {
    child: asupersync::process::Child,
    group: i32,
    reaped: bool,
    killed: bool,
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Owner {
    fn kill_group(&mut self) -> Result<(), SubprocessError> {
        if self.killed {
            return Ok(());
        }
        match nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(self.group),
            nix::sys::signal::Signal::SIGKILL,
        ) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => {
                self.killed = true;
                Ok(())
            }
            Err(_) => Err(SubprocessError::Cleanup),
        }
    }
    fn cleanup(&mut self) -> Result<(), SubprocessError> {
        self.kill_group()?;
        if !self.reaped {
            self.child.wait().map_err(|_| SubprocessError::Cleanup)?;
            self.reaped = true;
        }
        Ok(())
    }
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}
