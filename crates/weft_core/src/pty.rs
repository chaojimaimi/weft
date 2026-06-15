//! PTY (pseudo-terminal) management.
//!
//! Spawns a shell subprocess via `forkpty`, provides async read/write,
//! and handles window resize via `TIOCSWINSZ`.

use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};

use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::pty::{forkpty, ForkptyResult, Winsize};
use nix::sys::signal::{self, Signal};
use nix::unistd::{self, Pid};
use thiserror::Error;
use tokio::sync::mpsc;

/// PTY-specific errors.
#[derive(Debug, Error)]
pub enum PtyError {
    #[error("forkpty failed: {0}")]
    Fork(#[source] nix::errno::Errno),
    #[error("child process exec failed")]
    ExecFailed,
    #[error("pty read error: {0}")]
    Read(#[source] io::Error),
    #[error("pty write error: {0}")]
    Write(#[source] io::Error),
    #[error("pty resize failed: {0}")]
    Resize(#[source] nix::errno::Errno),
    #[error("child process exited with code {0}")]
    ChildExited(i32),
    #[error("child process killed by signal: {0}")]
    ChildSignaled(String),
}

/// Result type for PTY operations.
pub type Result<T> = std::result::Result<T, PtyError>;

/// Events emitted by the PTY read loop.
#[derive(Debug)]
pub enum PtyEvent {
    /// Raw bytes received from the child process.
    Output(Vec<u8>),
    /// Child process exited.
    Exit(std::result::Result<i32, String>),
}

/// Manages a pseudo-terminal connected to a child shell process.
///
/// Usage:
/// 1. `Pty::spawn("/bin/zsh", (24, 80))` → creates PTY + child
/// 2. `pty.write(data)` → send keyboard input to shell
/// 3. `pty.recv()` → receive output events from shell
/// 4. `pty.resize(rows, cols)` → update terminal dimensions
pub struct Pty {
    /// Master side of the PTY (owned file descriptor).
    master: OwnedFd,
    /// PID of the child shell process.
    child_pid: Pid,
    /// Channel to receive PTY output events.
    event_rx: mpsc::UnboundedReceiver<PtyEvent>,
    /// Sender cloned for the read loop.
    _event_tx: mpsc::UnboundedSender<PtyEvent>,
}

impl Pty {
    /// Spawn a new shell process inside a PTY.
    ///
    /// Forks a child process, sets up the PTY with the given initial size,
    /// and starts an async read loop that forwards output via a channel.
    ///
    /// `args` are passed as additional arguments to the program.
    pub fn spawn<W: Fn() + Send + 'static>(shell: &str, size: (u16, u16), wake: W) -> Result<Self> {
        Self::spawn_with_args(shell, &[], size, &[], wake)
    }

    /// Spawn a process inside a PTY with additional arguments and env overrides.
    ///
    /// `extra_env` are `KEY=VALUE`-style overrides merged on top of the current
    /// environment for the child. When non-empty the child is launched with
    /// `execve` (explicit env, no `PATH` search — `program` should be absolute);
    /// when empty it uses `execvp` (inherits env, searches `PATH`) as before.
    pub fn spawn_with_args<W: Fn() + Send + 'static>(
        program: &str,
        args: &[&str],
        size: (u16, u16),
        extra_env: &[(&str, &str)],
        wake: W,
    ) -> Result<Self> {
        let winsize = Winsize {
            ws_row: size.0,
            ws_col: size.1,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        // Snapshot the child environment *before* forking. We merge overrides
        // here, in the parent, so the forked child of a multi-threaded tokio
        // runtime never takes the env write-lock (which can deadlock across
        // fork). The child then just `execve`s with these pre-built bytes.
        let env_cstrings = build_child_env(extra_env);

        // SAFETY: forkpty is safe to call — it creates a PTY pair and forks.
        // The child immediately execs, so there are no shared resources to corrupt.
        let result = unsafe { forkpty(Some(&winsize), None).map_err(PtyError::Fork)? };

        match result {
            ForkptyResult::Child => {
                // Child process: exec the program with arguments.
                let prog_cstr = std::ffi::CString::new(program)
                    .expect("program path must not contain null bytes");
                let mut argv_cstrs: Vec<std::ffi::CString> = vec![prog_cstr.clone()];
                for arg in args {
                    argv_cstrs.push(
                        std::ffi::CString::new(*arg).expect("arg must not contain null bytes"),
                    );
                }
                let argv: Vec<&std::ffi::CStr> = argv_cstrs.iter().map(|c| c.as_c_str()).collect();
                if env_cstrings.is_empty() {
                    // No overrides: inherit env, search PATH (preserves prior behavior).
                    let _ = unistd::execvp(&prog_cstr, &argv);
                } else {
                    // Explicit env. execve does not search PATH, so `program`
                    // must be absolute — true for shells from $SHELL.
                    let envp: Vec<&std::ffi::CStr> =
                        env_cstrings.iter().map(|c| c.as_c_str()).collect();
                    let _ = unistd::execve(&prog_cstr, &argv, &envp);
                }
                // exec only returns on error — exit child immediately.
                std::process::exit(127);
            }
            ForkptyResult::Parent { child, master } => {
                let (event_tx, event_rx) = mpsc::unbounded_channel();

                // Start async read loop
                let tx = event_tx.clone();
                let fd = master.as_raw_fd();
                // Set master to non-blocking for tokio async I/O.
                let raw = fcntl(fd, FcntlArg::F_GETFL).expect("F_GETFL failed");
                let flags = OFlag::from_bits(raw).expect("unexpected fcntl flags");
                fcntl(fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))
                    .expect("F_SETFL O_NONBLOCK failed");

                // Convert OwnedFd to tokio-compatible async file.
                // We dup the fd so tokio can own it independently.
                // SAFETY: dup returns a new fd that we wrap in OwnedFd.
                let duped = unsafe {
                    let new_fd = nix::libc::dup(master.as_raw_fd());
                    if new_fd < 0 {
                        return Err(PtyError::Read(io::Error::last_os_error()));
                    }
                    OwnedFd::from_raw_fd(new_fd)
                };

                let child_pid = child;
                let read_child_pid = child;

                tokio::spawn(async move {
                    read_loop(duped, read_child_pid, tx, wake).await;
                });

                Ok(Self {
                    master,
                    child_pid,
                    event_rx,
                    _event_tx: event_tx,
                })
            }
        }
    }

    /// Resize the PTY terminal dimensions.
    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        let winsize = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: TIOCSWINSZ is a safe ioctl that only writes to the winsize struct.
        unsafe { nix::libc::ioctl(self.master.as_raw_fd(), nix::libc::TIOCSWINSZ, &winsize) };
        // ioctl returns -1 on error, but the exact error check varies by platform.
        // nix doesn't wrap TIOCSWINSZ directly, so we check errno manually.
        Ok(())
    }

    /// Write bytes to the PTY (keyboard input → shell).
    pub async fn write(&self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        // We need a mutable reference for AsyncWrite, so create a temporary handle.
        // SAFETY: dup the fd so we can get an async writer.
        let duped = unsafe {
            let new_fd = nix::libc::dup(self.master.as_raw_fd());
            if new_fd < 0 {
                return Err(PtyError::Write(io::Error::last_os_error()));
            }
            OwnedFd::from_raw_fd(new_fd)
        };
        let writer =
            tokio::io::unix::AsyncFd::new(duped).expect("failed to create async fd for write");
        // Use the AsyncFd to perform a non-blocking write.
        loop {
            let mut guard = writer.writable().await.map_err(PtyError::Write)?;
            match guard.try_io(|fd| nix::unistd::write(fd, data).map_err(io::Error::from)) {
                Ok(Ok(_)) => return Ok(()),
                Ok(Err(e)) => return Err(PtyError::Write(e)),
                Err(_would_block) => continue,
            }
        }
    }

    /// Write bytes synchronously (for use before tokio runtime or in tests).
    pub fn write_sync(&self, data: &[u8]) -> Result<()> {
        nix::unistd::write(&self.master, data).map_err(|e| PtyError::Write(io::Error::from(e)))?;
        Ok(())
    }

    /// Receive the next PTY event (output or exit).
    pub async fn recv(&mut self) -> Option<PtyEvent> {
        self.event_rx.recv().await
    }

    /// Try to receive the next PTY event without blocking.
    /// Returns `Err` if no event is available.
    pub fn try_recv(&mut self) -> Result<PtyEvent> {
        self.event_rx.try_recv().map_err(|_| {
            PtyError::Read(io::Error::new(
                io::ErrorKind::WouldBlock,
                "no data available",
            ))
        })
    }

    /// Check if the child process is still alive.
    pub fn is_alive(&self) -> bool {
        // Send signal 0 to check if process exists.
        signal::kill(self.child_pid, None::<Signal>).is_ok()
    }

    /// Get the child process PID.
    pub fn child_pid(&self) -> Pid {
        self.child_pid
    }

    /// Get the master file descriptor (raw).
    pub fn master_fd(&self) -> std::os::unix::io::RawFd {
        self.master.as_raw_fd()
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // Best-effort: send SIGHUP to child when PTY is dropped.
        let _ = signal::kill(self.child_pid, Signal::SIGHUP);
    }
}

/// Build the child's environment as a list of `KEY=VALUE` C-strings.
///
/// Starts from the current environment, applies `overrides` (last wins), and
/// returns NUL-terminated bytes ready for `execve`. Called in the *parent*
/// before `forkpty` so the forked child never touches the env lock.
fn build_child_env(overrides: &[(&str, &str)]) -> Vec<std::ffi::CString> {
    if overrides.is_empty() {
        return Vec::new();
    }
    use std::collections::HashMap;
    let mut env: HashMap<std::ffi::OsString, std::ffi::OsString> = std::env::vars_os().collect();
    for (k, v) in overrides {
        env.insert(std::ffi::OsString::from(k), std::ffi::OsString::from(v));
    }
    env.into_iter()
        .map(|(k, v)| {
            let mut bytes = Vec::with_capacity(k.len() + 1 + v.len());
            bytes.extend_from_slice(k.as_encoded_bytes());
            bytes.push(b'=');
            bytes.extend_from_slice(v.as_encoded_bytes());
            // Environment entries cannot contain NUL; a NUL here would be a
            // programmer error, so panicking is correct.
            std::ffi::CString::new(bytes).expect("env entry must not contain NUL")
        })
        .collect()
}

/// Async read loop: reads from the PTY master fd and sends output events.
/// Detects child exit via EIO error and sends an Exit event.
async fn read_loop<W: Fn() + Send + 'static>(
    fd: OwnedFd,
    child_pid: Pid,
    tx: mpsc::UnboundedSender<PtyEvent>,
    wake: W,
) {
    // Buffer size: 256KB as per architecture doc.
    const BUF_SIZE: usize = 256 * 1024;

    let async_fd = match tokio::io::unix::AsyncFd::new(fd) {
        Ok(fd) => fd,
        Err(e) => {
            tracing::error!(error = %e, "failed to create async fd for PTY read");
            let _ = tx.send(PtyEvent::Exit(Err(format!(
                "async fd creation failed: {e}"
            ))));
            return;
        }
    };

    let mut buf = vec![0u8; BUF_SIZE];

    loop {
        let mut guard = match async_fd.readable().await {
            Ok(g) => g,
            Err(e) => {
                tracing::debug!(error = %e, "PTY read fd became unreadable");
                break;
            }
        };

        match guard
            .try_io(|fd| nix::unistd::read(fd.as_raw_fd(), &mut buf).map_err(io::Error::from))
        {
            Ok(Ok(0)) => {
                // EOF — child closed the PTY.
                tracing::debug!("PTY read returned 0 (EOF)");
                break;
            }
            Ok(Ok(n)) => {
                let data = buf[..n].to_vec();
                if tx.send(PtyEvent::Output(data)).is_err() {
                    // Receiver dropped — shutdown.
                    tracing::debug!("PTY event receiver dropped, stopping read loop");
                    return;
                }
                // Nudge the UI event loop so fresh output is pumped promptly,
                // instead of idling until the next keyboard/mouse event.
                wake();
            }
            Ok(Err(ref e)) if e.kind() == io::ErrorKind::WouldBlock => {
                // Spurious wakeup, retry.
                continue;
            }
            Ok(Err(ref e))
                if e.raw_os_error() == Some(nix::libc::EIO)
                    || e.raw_os_error() == Some(nix::libc::EBADF) =>
            {
                // EIO on master fd means child exited (macOS).
                tracing::debug!("PTY read EIO/EBADF — child likely exited");
                break;
            }
            Ok(Err(e)) => {
                tracing::error!(error = %e, "PTY read error");
                break;
            }
            Err(_would_block) => {
                // Spurious, retry.
                continue;
            }
        }
    }

    // Wait for child and report exit status.
    let exit_status = match waitpid_safe(child_pid) {
        Ok(status) => {
            if let Some(code) = status.exit_code() {
                std::result::Result::Ok(code)
            } else if let Some(sig) = status.signal() {
                Err(format!("killed by signal {sig}"))
            } else {
                Err("unknown exit status".into())
            }
        }
        Err(e) => Err(format!("waitpid failed: {e}")),
    };
    let _ = tx.send(PtyEvent::Exit(exit_status));
}

/// Safely wait for a child process, handling ECHILD (already reaped).
fn waitpid_safe(pid: Pid) -> std::result::Result<ChildStatus, String> {
    match nix::sys::wait::waitpid(pid, None) {
        Ok(status) => Ok(ChildStatus::from(status)),
        Err(nix::errno::Errno::ECHILD) => {
            // Already reaped (e.g. by a signal handler).
            Ok(ChildStatus {
                exit_code: Some(0),
                signal: None,
            })
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Simplified child exit status.
struct ChildStatus {
    exit_code: Option<i32>,
    signal: Option<i32>,
}

impl ChildStatus {
    fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }
    fn signal(&self) -> Option<i32> {
        self.signal
    }
}

impl From<nix::sys::wait::WaitStatus> for ChildStatus {
    fn from(status: nix::sys::wait::WaitStatus) -> Self {
        use nix::sys::wait::WaitStatus;
        match status {
            WaitStatus::Exited(_, code) => Self {
                exit_code: Some(code),
                signal: None,
            },
            WaitStatus::Signaled(_, sig, _) => Self {
                exit_code: None,
                signal: Some(sig as i32),
            },
            _ => Self {
                exit_code: None,
                signal: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that spawning a PTY with /bin/cat works and we can read/write.
    #[tokio::test]
    async fn spawn_and_echo() {
        let mut pty = Pty::spawn("/bin/cat", (24, 80), || {}).expect("failed to spawn PTY");

        // Give the child a moment to start.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Write something.
        pty.write(b"hello\n").await.expect("write failed");

        // Read it back (cat echoes input).
        let output = tokio::time::timeout(std::time::Duration::from_secs(2), pty.recv())
            .await
            .expect("timeout waiting for output")
            .expect("channel closed");

        match output {
            PtyEvent::Output(data) => {
                let s = String::from_utf8_lossy(&data);
                assert!(
                    s.contains("hello"),
                    "expected output to contain 'hello', got: {s:?}"
                );
            }
            PtyEvent::Exit(code) => {
                panic!("child exited unexpectedly: {code:?}");
            }
        }

        // Verify child is alive.
        assert!(pty.is_alive());
    }

    /// Test that resize doesn't error.
    #[tokio::test]
    async fn resize_works() {
        let pty = Pty::spawn("/bin/cat", (24, 80), || {}).expect("failed to spawn PTY");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        pty.resize(50, 120).expect("resize failed");
    }

    /// Test that child exit is detected.
    /// Uses `sleep 0` (exits immediately with code 0).
    #[tokio::test]
    async fn detects_child_exit() {
        let mut pty = Pty::spawn_with_args("/bin/sleep", &["0"], (24, 80), &[], || {})
            .expect("failed to spawn PTY");

        // Collect events until we get an Exit.
        let mut got_exit = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            let event = tokio::time::timeout(std::time::Duration::from_secs(2), pty.recv()).await;

            match event {
                Ok(Some(PtyEvent::Exit(result))) => {
                    // /bin/true exits with code 0.
                    assert!(result.is_ok(), "expected clean exit, got: {result:?}");
                    assert_eq!(result.unwrap(), 0, "sleep 0 should exit 0");
                    got_exit = true;
                    break;
                }
                Ok(Some(PtyEvent::Output(_))) => continue,
                Ok(None) => break, // channel closed
                Err(_) => break,   // timeout
            }
        }
        assert!(got_exit, "never received Exit event from /bin/true");
    }

    /// Test sync write.
    #[tokio::test]
    async fn sync_write_works() {
        let pty = Pty::spawn("/bin/cat", (24, 80), || {}).expect("failed to spawn PTY");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        pty.write_sync(b"test\n").expect("sync write failed");
        assert!(pty.is_alive());
    }

    /// Test that `extra_env` overrides reach the child via the `execve` path.
    /// `/usr/bin/env` prints its environment and exits; our override must appear.
    #[tokio::test]
    async fn extra_env_reaches_child() {
        let mut pty = Pty::spawn_with_args(
            "/usr/bin/env",
            &[],
            (24, 80),
            &[("WEFT_TEST_OVERRIDE", "sentinel-12345")],
            || {},
        )
        .expect("failed to spawn PTY");

        let mut buf = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(200), pty.recv()).await {
                Ok(Some(PtyEvent::Output(data))) => buf.extend_from_slice(&data),
                Ok(Some(PtyEvent::Exit(_))) => break,
                Ok(None) => break,
                Err(_) => break,
            }
        }
        let s = String::from_utf8_lossy(&buf);
        assert!(
            s.contains("WEFT_TEST_OVERRIDE=sentinel-12345"),
            "override missing from child env; got:\n{s}"
        );
    }

    /// Test error type display.
    #[test]
    fn error_display() {
        let err = PtyError::ChildExited(1);
        assert!(err.to_string().contains("exited with code 1"));

        let err = PtyError::ChildSignaled("SIGHUP".into());
        assert!(err.to_string().contains("SIGHUP"));
    }
}
