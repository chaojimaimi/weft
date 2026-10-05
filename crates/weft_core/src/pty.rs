//! PTY (pseudo-terminal) management.
//!
//! Spawns a shell subprocess via `forkpty`, provides async read/write,
//! and handles window resize via `TIOCSWINSZ`.

use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::pty::{forkpty, ForkptyResult, Winsize};
use nix::sys::signal::{self, Signal};
use nix::unistd::{self, Pid};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::input::{new_flag, MouseSuppressFlag};

/// PTY-specific errors.
#[derive(Debug, Error)]
pub enum PtyError {
    #[error("forkpty failed: {0}")]
    Fork(#[source] nix::errno::Errno),
    #[error("child process exec failed")]
    ExecFailed,
    #[error("program or argument contains NUL byte: {0}")]
    InvalidProgram(String),
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
    /// v1.11.2 X2: bounded (`PTY_CHANNEL_CAP`) instead of unbounded — see
    /// the const's doc comment for the backpressure contract.
    event_rx: mpsc::Receiver<PtyEvent>,
    /// Sender cloned for the read loop.
    _event_tx: mpsc::Sender<PtyEvent>,
}

impl Pty {
    /// Spawn a new shell process inside a PTY.
    ///
    /// Forks a child process, sets up the PTY with the given initial size,
    /// and starts an async read loop that forwards output via a channel.
    ///
    /// `args` are passed as additional arguments to the program.
    pub fn spawn<W: Fn() + Send + 'static>(shell: &str, size: (u16, u16), wake: W) -> Result<Self> {
        Self::spawn_with_args(shell, &[], size, &[], None, new_flag(), wake)
    }

    /// Spawn a process inside a PTY with additional arguments and env overrides.
    ///
    /// `extra_env` are `KEY=VALUE`-style overrides merged on top of the current
    /// environment for the child. When non-empty the child is launched with
    /// `execve` (explicit env, no `PATH` search — `program` should be absolute);
    /// when empty it uses `execvp` (inherits env, searches `PATH`) as before.
    ///
    /// `cwd` — if `Some(path)`, the child `chdir`s to `path` before exec, so the
    /// shell starts in that directory without needing to send a `cd` command
    /// (which would pollute shell history and the block tracker). `PWD` env var
    /// is also set so shell integration OSC 7 reports the correct cwd.
    ///
    /// `mouse_suppress` — v1.11.15 (FIX A, PLAN_v11115_EXIT_RACE_MOUSE_LEAK
    /// §1): the per-pane reader-side mouse-suppression flag. The read loop
    /// flips it when the byte stream carries a mouse-disable DECRST and on
    /// every exit path, so the UI stops sending hover/wheel bytes into a PTY
    /// whose TUI is (or may be) gone before the main thread parses those
    /// bytes. The flag's authoritative undo is the main-thread vte parser
    /// (`Terminal::handle_dec_private_mode`).
    pub fn spawn_with_args<W: Fn() + Send + 'static>(
        program: &str,
        args: &[&str],
        size: (u16, u16),
        extra_env: &[(&str, &str)],
        cwd: Option<&str>,
        mouse_suppress: MouseSuppressFlag,
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
        // v1.0: if cwd is set, inject PWD so shell integration's OSC 7
        // reports the correct cwd and `~` expansion works.
        let mut env_with_cwd: Vec<(&str, String)> =
            extra_env.iter().map(|(k, v)| (*k, v.to_string())).collect();
        if let Some(c) = cwd {
            env_with_cwd.push(("PWD", c.to_string()));
        }
        let env_refs_cwd: Vec<(&str, &str)> =
            env_with_cwd.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let env_cstrings = build_child_env(&env_refs_cwd);

        // SAFETY sibling of the env snapshot above: argv CStrings are built
        // in the parent too — a CString allocation or panic inside the
        // post-fork child can take the malloc lock another runtime thread
        // holds across fork. A pre-fork failure returns Err normally, so no
        // fork happens and no orphan process can be left behind.
        let prog_cstr = std::ffi::CString::new(program)
            .map_err(|_| PtyError::InvalidProgram(program.to_string()))?;
        let mut argv_cstrs: Vec<std::ffi::CString> = vec![prog_cstr];
        for arg in args {
            argv_cstrs.push(
                std::ffi::CString::new(*arg)
                    .map_err(|_| PtyError::InvalidProgram((*arg).to_string()))?,
            );
        }

        // SAFETY: forkpty is safe to call — it creates a PTY pair and forks.
        // The child immediately execs, so there are no shared resources to corrupt.
        let result = unsafe { forkpty(Some(&winsize), None).map_err(PtyError::Fork)? };

        match result {
            ForkptyResult::Child => {
                // v1.0: chdir to the requested cwd before exec so the shell
                // starts in the right directory without a `cd` command.
                if let Some(c) = cwd {
                    let _ = unistd::chdir(c);
                }
                let argv: Vec<&std::ffi::CStr> = argv_cstrs.iter().map(|c| c.as_c_str()).collect();
                // argv_cstrs[0] is both the exec program argument and
                // argv[0]; sharing its &CStr keeps the two immutable borrows
                // on the same Vec (no CString clone needed).
                let prog: &std::ffi::CStr = argv_cstrs[0].as_c_str();
                if env_cstrings.is_empty() {
                    // No overrides: inherit env, search PATH (preserves prior behavior).
                    let _ = unistd::execvp(prog, &argv);
                } else {
                    // Explicit env. execve does not search PATH, so `program`
                    // must be absolute — true for shells from $SHELL.
                    let envp: Vec<&std::ffi::CStr> =
                        env_cstrings.iter().map(|c| c.as_c_str()).collect();
                    let _ = unistd::execve(prog, &argv, &envp);
                }
                // exec only returns on error — exit child immediately.
                std::process::exit(127);
            }
            ForkptyResult::Parent { child, master } => {
                // v1.11.2 X2: bounded channel — the read loop's `send().await`
                // suspends when full and kernel flow control takes over.
                let (event_tx, event_rx) = mpsc::channel(PTY_CHANNEL_CAP);

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
                    // v1.12.25 (audit S-2): a panicking reader task used to vanish silently
                    // (tokio swallows it; the channel stays open because Pty holds another
                    // tx clone) — the tab froze forever with no Exit and no log. Await the
                    // inner task's JoinHandle and convert its panic payload into a
                    // synthetic Exit so the existing teardown path runs.
                    let reader = tx.clone();
                    let handle = tokio::spawn(read_loop::read_loop(
                        duped,
                        read_child_pid,
                        reader,
                        mouse_suppress,
                        wake,
                    ));
                    if let Err(join_err) = handle.await {
                        let msg = if join_err.is_panic() {
                            let panic = join_err.into_panic();
                            panic
                                .downcast_ref::<&str>()
                                .map(|s| s.to_string())
                                .or_else(|| panic.downcast_ref::<String>().cloned())
                                .unwrap_or_else(|| "reader task panicked".to_string())
                        } else {
                            // Defensive: nothing aborts the inner task, so this arm is
                            // unreachable in practice (review P3) — kept distinct from the
                            // panic wording above.
                            "reader task cancelled".to_string()
                        };
                        tracing::error!(
                            error = %msg,
                            "pty reader task failed; emitting synthetic Exit"
                        );
                        let _ = tx.send(PtyEvent::Exit(Err(msg))).await;
                    }
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
        let result =
            unsafe { nix::libc::ioctl(self.master.as_raw_fd(), nix::libc::TIOCSWINSZ, &winsize) };
        resize_ioctl_result(result)
    }

    /// Write bytes to the PTY (keyboard input → shell).
    ///
    /// NOTE: the production input path does not go through here — it uses
    /// the synchronous [`Self::write_sync_reported`] (tab.rs). This async
    /// variant survives as the test-injection seam.
    ///
    /// v1.11.15 (FIX E): the loop advances by the number of bytes actually
    /// accepted instead of treating ANY successful write as completion — a
    /// partial write (kernel input queue full, reader slow) used to drop the
    /// tail silently while reporting success. A zero-progress write fails
    /// fast (std `write_all` contract) instead of spinning.
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
        let mut written = 0usize;
        loop {
            let mut guard = writer.writable().await.map_err(PtyError::Write)?;
            match guard
                .try_io(|fd| nix::unistd::write(fd, &data[written..]).map_err(io::Error::from))
            {
                Ok(Ok(n)) => {
                    written += n;
                    if written >= data.len() {
                        return Ok(());
                    }
                    if n == 0 {
                        // Zero bytes with data remaining can never make
                        // progress — mirror write_all_nonblocking's contract.
                        return Err(PtyError::Write(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "pty write returned 0 with data remaining",
                        )));
                    }
                }
                Ok(Err(e)) => return Err(PtyError::Write(e)),
                Err(_would_block) => continue,
            }
        }
    }

    /// Write bytes synchronously (for use before tokio runtime or in tests).
    ///
    /// FIX_TERMINAL_CAPABILITY_HARDENING: bounded retry on EAGAIN. The PTY
    /// master fd is non-blocking (set in `spawn_with_args`), so a full kernel
    /// write buffer surfaces as EAGAIN instead of blocking. The v1.0 behavior
    /// dropped the data silently at debug level the moment the buffer filled —
    /// a heavy-output command with an undrained read side loses keystrokes
    /// with no trace. Now `write_all_nonblocking` polls `POLLOUT` and retries
    /// within a ~50ms budget: long enough to ride out a transient full buffer,
    /// short enough that the UI thread never freezes (the v1.0 reason for not
    /// retrying was a 1s blocking retry). If the budget is exhausted the
    /// remaining bytes are dropped, but loudly — warn! reports the dropped
    /// count. The caller keeps deciding on fallbacks (e.g. `send_interrupt`
    /// for Ctrl+C).
    ///
    /// v1.11.15 (FIX E): now a thin wrapper over [`Self::write_sync_reported`]
    /// — the drop warn lives there, and this signature stays `Ok(())` so
    /// every existing caller is undisturbed.
    pub fn write_sync(&self, data: &[u8]) -> Result<()> {
        self.write_sync_reported(data).map(|_| ())
    }

    /// v1.11.15 (FIX E, PLAN_v11115_EXIT_RACE_MOUSE_LEAK §5): honest
    /// synchronous write — returns the number of bytes that actually left
    /// when the retry budget expires (`Ok(written)`) instead of masking a
    /// partial write as `Ok(())`. On production macOS the `TimedOut` arm is
    /// unreachable (the n_tty line discipline silently discards input
    /// overflow, so the master always writes the full count — see the
    /// saturated-child anchor test below); the honest mapping exists for
    /// ssh/remote ptys and future platforms. The warn is kept here so both
    /// wrappers surface the drop exactly once.
    pub fn write_sync_reported(&self, data: &[u8]) -> Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        let fd = self.master.as_raw_fd();
        let mut write_one =
            |buf: &[u8]| nix::unistd::write(&self.master, buf).map_err(io::Error::from);
        let outcome = write_all_nonblocking(&mut write_one, fd, data, WRITE_RETRY_BUDGET);
        map_write_outcome(outcome, data.len())
    }

    /// Deliver one Ctrl+C interrupt to the foreground process.
    ///
    /// Write ETX (`0x03`) through the PTY so line discipline, remote sessions
    /// and raw interactive programs observe exactly the same event as a native
    /// terminal. We deliberately do not replace this with a direct SIGINT:
    /// doing so would interrupt a local `ssh` transport instead of forwarding
    /// Ctrl+C to its remote foreground process.
    ///
    /// Returns true if the interrupt was delivered successfully.
    pub fn send_interrupt(&self) -> bool {
        match nix::unistd::write(&self.master, &[0x03]) {
            Ok(1) => {
                tracing::debug!(delivery = "pty-etx", "Ctrl+C delivered once");
                true
            }
            Ok(_) => {
                tracing::warn!("short Ctrl+C PTY write; interrupt not delivered");
                false
            }
            Err(nix::errno::Errno::EAGAIN) => {
                tracing::warn!("Ctrl+C PTY write would block; interrupt not delivered");
                false
            }
            Err(error) => {
                tracing::warn!(%error, "Ctrl+C PTY write failed; interrupt not delivered");
                false
            }
        }
    }

    /// v1.0 fix: Flush the PTY's kernel-side read buffer and drain queued
    /// output events from the internal channel.
    ///
    /// Draining the channel also discards any queued [`PtyEvent::Exit`], so
    /// this must only be used on the flood-recovery path — never during
    /// normal teardown, where losing the exit status matters.
    ///
    /// Reserved for explicit flood-recovery actions. Ctrl+C itself must never
    /// call this after writing ETX: `tcflush(TCIFLUSH)` can discard the ETX
    /// before the slave line discipline consumes it.
    pub fn flush_input(&mut self) {
        // Flush kernel PTY read buffer (slave→master direction).
        // SAFETY: tcflush is a safe ioctl that discards pending data.
        unsafe {
            nix::libc::tcflush(self.master.as_raw_fd(), nix::libc::TCIFLUSH);
        }
        // Drain queued output events from the internal channel.
        while self.event_rx.try_recv().is_ok() {}
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

    /// Number of events queued at this instant. Close-time draining snapshots
    /// this value so a producer cannot keep the UI thread chasing new output.
    pub fn queued_event_count(&self) -> usize {
        self.event_rx.len()
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

/// Total time a non-blocking synchronous write may spend poll-waiting and
/// retrying after EAGAIN before giving up (FIX_TERMINAL_CAPABILITY_HARDENING).
const WRITE_RETRY_BUDGET: Duration = Duration::from_millis(50);

/// Outcome of a bounded non-blocking write loop, observable by callers and
/// unit tests (FIX_TERMINAL_CAPABILITY_HARDENING).
#[derive(Debug)]
#[must_use]
pub(crate) enum WriteOutcome {
    /// All bytes reached the fd.
    WrittenAll,
    /// Budget exhausted before everything was written; `written` bytes made
    /// it out, the rest were dropped (caller should warn with the count).
    TimedOut { written: usize },
    /// A non-EAGAIN write/poll error.
    Error(io::Error),
}

/// Bounded EAGAIN/EWOULDBLOCK retry for a non-blocking writer.
///
/// Calls `write_fn` with the unsent remainder; on EAGAIN/EWOULDBLOCK waits
/// on `poll(fd, POLLOUT)` for the remaining `budget` before retrying, so a
/// transient full kernel buffer is ridden out while a permanently-full
/// buffer can never stall the caller beyond `budget`. EINTR is retried like
/// EWOULDBLOCK (a signal interrupted the syscall — mirrors the poll loop's
/// own EINTR handling). Partial writes advance (the remainder is re-issued
/// in the same loop); `Ok(0)` with data remaining fails fast with `WriteZero`
/// (std `write_all` contract). Any other error is returned as
/// `WriteOutcome::Error`.
///
/// The fd and the writer are injected separately so unit tests can fake a
/// writer that would-block N times (success path) or forever (timeout
/// path) against a real always-writable fd.
pub(crate) fn write_all_nonblocking<W>(
    mut write_fn: W,
    fd: RawFd,
    data: &[u8],
    budget: Duration,
) -> WriteOutcome
where
    W: FnMut(&[u8]) -> io::Result<usize>,
{
    let deadline = Instant::now() + budget;
    let mut written = 0;
    let mut rest = data;
    loop {
        match write_fn(rest) {
            Ok(n) => {
                written += n;
                rest = &rest[n..];
                if rest.is_empty() {
                    return WriteOutcome::WrittenAll;
                }
                if n == 0 {
                    // std `write_all` contract: zero bytes with data
                    // remaining can never make progress — fail fast
                    // instead of spinning the budget away.
                    return WriteOutcome::Error(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "write returned 0 with data remaining",
                    ));
                }
            }
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::Interrupted =>
            {
                // EWOULDBLOCK (buffer full → poll for writability below) and
                // EINTR (signal interrupted the syscall → retry, mirroring
                // the poll loop's own EINTR continue) are both retried.
            }
            Err(e) => return WriteOutcome::Error(e),
        }
        // Wait for writability with whatever budget remains; never wait
        // past `deadline`.
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return WriteOutcome::TimedOut { written };
        }
        // SAFETY: `fd` outlives this call (callers pass their own live fd,
        // held for the duration), so borrowing it for the poll is sound.
        let mut fds = [PollFd::new(
            unsafe { std::os::unix::io::BorrowedFd::borrow_raw(fd) },
            PollFlags::POLLOUT,
        )];
        let timeout = PollTimeout::try_from(remaining).unwrap_or(PollTimeout::ZERO);
        match poll(&mut fds, timeout) {
            Ok(0) => return WriteOutcome::TimedOut { written },
            // Ready (or POLLERR/POLLNVAL) — retry; the write itself
            // surfaces the real error.
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return WriteOutcome::Error(io::Error::from(e)),
        }
    }
}

/// v1.11.15 (FIX E): the pure `WriteOutcome → honest report` mapping shared
/// by `write_sync_reported`. Extracted so the TimedOut-reports-written
/// contract is unit-testable without a real PTY (the partial-write arm is
/// unreachable against a real macOS master — see the module's saturated
/// child anchor). A budget timeout warns with the dropped count and reports
/// the bytes that made it out; a full write reports the total.
fn map_write_outcome(outcome: WriteOutcome, total: usize) -> Result<usize> {
    match outcome {
        WriteOutcome::WrittenAll => Ok(total),
        WriteOutcome::TimedOut { written } => {
            tracing::warn!(
                dropped = total - written,
                "pty write budget exhausted — dropping remaining bytes"
            );
            Ok(written)
        }
        WriteOutcome::Error(e) => Err(PtyError::Write(e)),
    }
}

fn resize_ioctl_result(result: nix::libc::c_int) -> Result<()> {
    if result == -1 {
        Err(PtyError::Resize(nix::errno::Errno::last()))
    } else {
        Ok(())
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
    strip_launcher_presentation_env(&mut env);
    for (k, v) in overrides {
        env.insert(std::ffi::OsString::from(k), std::ffi::OsString::from(v));
    }
    env.into_iter()
        .filter_map(|(k, v)| {
            let mut bytes = Vec::with_capacity(k.len() + 1 + v.len());
            bytes.extend_from_slice(k.as_encoded_bytes());
            bytes.push(b'=');
            bytes.extend_from_slice(v.as_encoded_bytes());
            // v1.12.25 (audit core P2-3): a NUL here used to panic the main
            // thread ("a NUL here would be a programmer error"). It can now
            // arrive via user data (hand-edited workspace YAML cwd) — skip
            // the entry and warn instead; workspace validate() rejects it
            // earlier (defense line one, this is line two).
            match std::ffi::CString::new(bytes) {
                Ok(entry) => Some(entry),
                Err(_) => {
                    tracing::warn!("env entry contains NUL; skipping it for child exec");
                    None
                }
            }
        })
        .collect()
}

/// Do not leak a GUI launcher's presentation policy into terminal sessions.
/// Users can still export `NO_COLOR` from their shell startup files when they
/// intentionally want monochrome command output.
fn strip_launcher_presentation_env(
    env: &mut std::collections::HashMap<std::ffi::OsString, std::ffi::OsString>,
) {
    env.remove(std::ffi::OsStr::new("NO_COLOR"));
}

/// v1.11.2 X2 (PLAN_v1112 §2): capacity of the bounded PTY event channel.
/// 32 × 256 KiB chunks = an 8 MiB ceiling on in-flight output between the
/// read task and the UI pump. When full, `send().await` suspends the read
/// task, which backpressures into the kernel PTY buffer and ultimately
/// blocks the child's writes — bytes are never dropped by Weft.
const PTY_CHANNEL_CAP: usize = 32;

/// Monotonic milliseconds since process start (v1.11.2 X2 wake throttle).
/// rust-reviewer Minor-3: deliberately NOT wall-clock `SystemTime` — NTP
/// steps or a manual clock change can move it backwards, which would
/// suppress wakes until real time caught up with the stale stamp while the
/// queue is non-empty. `Instant` only ever moves forward.
fn monotonic_millis() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Tracks when the read loop last woke the UI (v1.11.2 X2).
#[derive(Debug, Default)]
struct WakeThrottle {
    last_ms: u64,
}

impl WakeThrottle {
    fn last(&self) -> u64 {
        self.last_ms
    }

    fn stamp(&mut self, now_ms: u64) {
        self.last_ms = now_ms;
    }
}

/// Backlog-state wake interval (T8, PLAN_v11217 §3.4): 2 ms. The previous
/// 16 ms (~60 Hz, Warp-precedent) capped cat throughput at 60 Hz × ≤32
/// channel events × kernel read granularity ≈ 5.8 MB/s — exactly the
/// measured T0 baseline. A caught-up consumer (empty queue) wakes
/// immediately regardless (unchanged), and Exit always bypasses the
/// throttle.
const FLOOD_WAKE_INTERVAL_MS: u64 = 2;

/// Pure wake decision (v1.11.2 X2, PLAN_v1112 §2; revised by T8
/// PLAN_v11217 §3.4): during an output backlog the UI is nudged at most
/// once per `FLOOD_WAKE_INTERVAL_MS`, but a caught-up consumer (empty
/// queue) always wakes immediately so fresh output is pumped without
/// latency, and Exit always bypasses the throttle so the UI learns of a
/// dead child instantly.
///
/// Safety of ~500 wakes/s (fourth-round review P3): the guarantee is
/// SELF-LIMITATION, not "each wake is cheap" — wake generation rate ≤
/// batch flush rate ≤ kernel data-availability rate, and a full channel
/// suspends the reader in `send().await` before it can produce another
/// wake, so there is no feedback amplification.
fn pty_wake_due(is_exit: bool, consumer_caught_up: bool, last_ms: u64, now_ms: u64) -> bool {
    if is_exit {
        return true;
    }
    consumer_caught_up || now_ms.saturating_sub(last_ms) >= FLOOD_WAKE_INTERVAL_MS
}

// Reader task moved to `pty/read_loop.rs` (T8, PLAN_v11217 §3.4; standard
// `pty.rs` + `pty/` directory layout) so this file stays within its
// architecture budget. EVENT_CAP is the single source of truth for the
// per-message size ceiling — the app crate's oversize-split threshold
// re-exports it.
mod read_loop;
pub use read_loop::EVENT_CAP;

// Tests extracted to `pty/tests.rs` (repo convention) to keep the
// production file within the architecture gate.
#[cfg(test)]
#[path = "pty/tests.rs"]
mod tests;
