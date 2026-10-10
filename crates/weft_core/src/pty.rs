//! PTY (pseudo-terminal) management.
//!
//! Spawns a shell subprocess via `forkpty`, provides async read/write,
//! and handles window resize via `TIOCSWINSZ`.
//!
//! v1.13.6 T10 P2 (PLAN_v1136 §1 D2–D5): `Pty` no longer owns the receive
//! half of its event channel. `take_event_rx` hands it to the per-pane
//! parse worker (weft_app); `Pty` keeps the main-thread concerns — master
//! fd (resize ioctl / tcflush / drop-SIGHUP), child pid, the write half
//! ([`PtyWriter`], shared with the worker via `Arc` clones), and the
//! `event_tx` clone that implements the [`PtyEvent::Flush`] marker of the
//! flush protocol (D2).

use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Instant;

use nix::fcntl::{fcntl, FcntlArg, OFlag};
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

/// Events emitted by the PTY read loop (and, for [`PtyEvent::Flush`], by the
/// main-thread flush protocol).
#[derive(Debug)]
pub enum PtyEvent {
    /// Raw bytes received from the child process.
    Output(Vec<u8>),
    /// v1.13.6 T10 P2 (D2): flush marker injected by `Pty::flush_input`.
    /// Precise semantics (review P2-1 — do NOT gloss this as equivalent to
    /// the retired synchronous drain):
    /// - BACKLOG BEFORE the marker (FIFO): already parsed into the terminal
    ///   by the time the worker consumes the marker — NOT dropped. Today's
    ///   Ctrl+C dropped all of it instantly; keeping it is a deliberate
    ///   change (the interrupt-capture/rollback on the main thread handles
    ///   the stale tail).
    /// - What IS dropped: every `Output` still queued in the sweep executed
    ///   at marker-consumption time (i.e. queued BEHIND the marker).
    /// - Channel full (flood): the `try_send` marker is SKIPPED — degraded
    ///   to tcflush only, and up to the channel's ~8 MiB backlog keeps
    ///   parsing (the retired main-thread drain dropped it instantly, so
    ///   this arm is a WEAKENING; worst-case Ctrl+C truncation delay is
    ///   bounded by channel capacity ÷ parse rate — tracked in the plan's
    ///   P3 probe list).
    /// - The v1.11.15 guarantee that output around Ctrl+C is retained for
    ///   resume tails is correspondingly WEAKENED in the full-channel arm.
    Flush,
    /// Child process exited.
    Exit(std::result::Result<i32, String>),
}

/// Manages a pseudo-terminal connected to a child shell process.
///
/// Usage:
/// 1. `Pty::spawn("/bin/zsh", (24, 80))` → creates PTY + child
/// 2. `pty.write_sync(data)` / `pty.writer()` → send keyboard input
/// 3. `pty.take_event_rx()` → hand the receive half to the parse worker
/// 4. `pty.resize(rows, cols)` → update terminal dimensions
pub struct Pty {
    /// Master side of the PTY (owned file descriptor). Kept on the main
    /// thread for the resize ioctl, `tcflush`, and the Drop SIGHUP.
    master: OwnedFd,
    /// PID of the child shell process.
    child_pid: Pid,
    /// Sender cloned for the read loop — retained on the main thread ONLY
    /// for the `flush_input` Flush-marker injection (D2).
    event_tx: mpsc::Sender<PtyEvent>,
    /// Receive half — `Some` until [`Pty::take_event_rx`] hands it to the
    /// parse worker (D2).
    event_rx: Option<mpsc::Receiver<PtyEvent>>,
    /// The write half (D4): shared with the worker via `Arc` clones; the
    /// write mutex lives inside it.
    writer: std::sync::Arc<crate::pty::writer::PtyWriter>,
}

impl Pty {
    /// Spawn a new shell process inside a PTY.
    ///
    /// Forks a child process, sets up the PTY with the given initial size,
    /// and starts an async read loop that forwards output via a channel.
    ///
    /// `args` are passed as additional arguments to the program.
    ///
    /// v1.13.6 T10 P2 (D3): the `wake` closure parameter is GONE — UI wake
    /// ownership moved from the reader to the parse worker (see
    /// `weft_app::app::parse_worker`, which reuses the exported
    /// [`pty_wake_due`] throttle).
    pub fn spawn(shell: &str, size: (u16, u16)) -> Result<Self> {
        Self::spawn_with_args(shell, &[], size, &[], None, new_flag())
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
    pub fn spawn_with_args(
        program: &str,
        args: &[&str],
        size: (u16, u16),
        extra_env: &[(&str, &str)],
        cwd: Option<&str>,
        mouse_suppress: MouseSuppressFlag,
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

                // The writer's fd is ANOTHER independent dup: dropping `Pty`
                // (pane close) closes the master while the parse worker's
                // writer clone can still drain its in-flight reply.
                // SAFETY: dup returns a new fd that we wrap in OwnedFd.
                let writer_fd = unsafe {
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
                    ));
                    if let Err(join_err) = handle.await {
                        let msg = if join_err.is_panic() {
                            panic_exit_reason(join_err.into_panic())
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
                    event_tx,
                    event_rx: Some(event_rx),
                    writer: std::sync::Arc::new(writer::PtyWriter::new(writer_fd)),
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

    /// v1.0 fix: flush the PTY's kernel-side read buffer; v1.13.6 T10 P2
    /// (D2): the app-side half became the Flush MARKER protocol.
    ///
    /// Full contract on the [`PtyEvent::Flush`] doc — the short form here:
    /// `tcflush` + a best-effort non-blocking marker to the parse worker.
    /// BACKLOG QUEUED AHEAD of the marker was already parsed into the
    /// terminal (NOT dropped — today's Ctrl+C dropped it instantly); the
    /// sweep at marker-consumption time drops only what is still queued
    /// BEHIND it. A full channel SKIPS the marker (tcflush-only degradation,
    /// review P2-1): up to the ~8 MiB backlog keeps parsing, so the
    /// v1.11.15 "output around Ctrl+C is retained" guarantee weakens in
    /// that arm. Called from the Ctrl+C path (`Pane::interrupt_pty`) —
    /// retaining output around the ETX is intentional there (v1.11.15
    /// resume tails); the old "flood-recovery only" warning described the
    /// retired channel-draining implementation.
    pub fn flush_input(&self) {
        // P3 (review): the tcflush lives in `inject_flush_marker` — the
        // public semantics have exactly one implementation point.
        inject_flush_marker(&self.master, &self.event_tx);
    }

    /// Hand the receive half to this pane's parse worker (T10 P2 D2, the
    /// `into_parts()`-style split). After this call `Pty` keeps only the
    /// write half (`event_tx` for the Flush marker + the [`PtyWriter`] Arc).
    /// Returns `None` if the receiver was already handed off (a fresh `Pty`
    /// always holds it — callers may `expect`).
    pub fn take_event_rx(&mut self) -> Option<mpsc::Receiver<PtyEvent>> {
        self.event_rx.take()
    }

    /// Clone of the shared write half (D4/D5): the main thread and the parse
    /// worker each hold one; the write mutex lives inside [`PtyWriter`].
    pub fn writer(&self) -> std::sync::Arc<crate::pty::writer::PtyWriter> {
        self.writer.clone()
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

/// The flush marker injection, split from [`Pty::flush_input`] so both arms
/// of the `try_send` contract are unit-testable without a real PTY (D2):
/// with channel room the marker is queued; with a full bounded channel it is
/// skipped (degrade to tcflush-only) instead of blocking the UI thread.
/// `tcflush` on a non-tty fd (the tests pass /dev/null) fails silently — the
/// result is deliberately ignored there too.
fn inject_flush_marker(master: &OwnedFd, event_tx: &mpsc::Sender<PtyEvent>) {
    // Flush kernel PTY read buffer (slave→master direction).
    // SAFETY: tcflush is a safe ioctl that discards pending data.
    unsafe {
        nix::libc::tcflush(master.as_raw_fd(), nix::libc::TCIFLUSH);
    }
    let _ = event_tx.try_send(PtyEvent::Flush);
}

/// Monotonic milliseconds since process start (v1.11.2 X2 wake throttle).
/// rust-reviewer Minor-3: deliberately NOT wall-clock `SystemTime` — NTP
/// steps or a manual clock change can move it backwards, which would
/// suppress wakes until real time caught up with the stale stamp while the
/// queue is non-empty. `Instant` only ever moves forward.
///
/// v1.13.6 T10 P2 (D3): `pub` — the parse worker stamps its wake throttle
/// with it (the reader no longer wakes the UI).
pub fn monotonic_millis() -> u64 {
    use std::sync::OnceLock;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Tracks when the last UI wake was stamped (v1.11.2 X2). v1.13.6 T10 P2
/// (D3): `pub` — definition deliberately STAYS in weft_core (the decision
/// table tests pin it here) while ownership of the wake moved to the
/// weft_app parse worker.
#[derive(Debug, Default)]
pub struct WakeThrottle {
    last_ms: u64,
}

impl WakeThrottle {
    pub fn last(&self) -> u64 {
        self.last_ms
    }

    pub fn stamp(&mut self, now_ms: u64) {
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
///
/// v1.13.6 T10 P2 (D3): `pub` — consumed by the weft_app parse worker
/// (the reader no longer wakes). Definition and decision-table tests stay
/// in weft_core, untouched.
pub fn pty_wake_due(is_exit: bool, consumer_caught_up: bool, last_ms: u64, now_ms: u64) -> bool {
    if is_exit {
        return true;
    }
    consumer_caught_up || now_ms.saturating_sub(last_ms) >= FLOOD_WAKE_INTERVAL_MS
}

/// v1.13.2 (WP-A): the reader-task panic wrapper's payload downcast, extracted
/// verbatim from the inline code so all three arms are unit testable.
/// Behavior is unchanged: `&str` and `String` payloads surface their message;
/// anything else falls back to the generic wording that the synthetic
/// `Exit(Err(..))` carries into the tab teardown path.
fn panic_exit_reason(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "reader task panicked".to_string())
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
    strip_inherited_debug_env(&mut env);
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

/// v1.13.2 (WP-C, audit L-1): do not leak Weft's own PTY-output capture knob
/// into child sessions — a `WEFT_PTY_CAPTURE=path` used to start Weft used to
/// stay visible to every command it spawns (an inherited debug surface).
///
/// Premise (pinned per plan): this strip only guards the `build_child_env`
/// path — when `overrides` is empty, `build_child_env` returns early and the
/// child inherits the full parent environment via `execvp`, a path this line
/// does not cover (production always passes `shell_integration_env`, so the
/// overrides list is non-empty there).
///
/// Deliberately NOT merged with `strip_launcher_presentation_env`: launcher
/// presentation policy vs. Weft's own debug surface are semantically
/// independent and must stay independently traceable to their audits.
fn strip_inherited_debug_env(
    env: &mut std::collections::HashMap<std::ffi::OsString, std::ffi::OsString>,
) {
    env.remove(std::ffi::OsStr::new("WEFT_PTY_CAPTURE"));
}

/// v1.11.2 X2 (PLAN_v1112 §2): capacity of the bounded PTY event channel.
/// 32 × 256 KiB chunks = an 8 MiB ceiling on in-flight output between the
/// read task and the parse worker. When full, `send().await` suspends the
/// read task, which backpressures into the kernel PTY buffer and ultimately
/// blocks the child's writes — bytes are never dropped by Weft.
const PTY_CHANNEL_CAP: usize = 32;

// Reader task moved to `pty/read_loop.rs` (T8, PLAN_v11217 §3.4; standard
// `pty.rs` + `pty/` directory layout) so this file stays within its
// architecture budget. EVENT_CAP is the single source of truth for the
// per-message size ceiling — the app crate's oversize-split threshold
// re-exports it.
mod read_loop;
mod read_loop_stats;
pub use read_loop::EVENT_CAP;

// The write half (v1.13.6 T10 P2, D4/D5): `pty/pty.rs` + `pty/writer.rs`
// layout. The bounded-retry machinery is re-exported pub(crate) so the
// test module's `use super::*` keeps reaching it.
mod writer;
pub use writer::PtyWriter;
#[allow(unused_imports)] // consumed by the test module via `use super::*`
pub(crate) use writer::{map_write_outcome, write_all_nonblocking, WriteOutcome};

fn resize_ioctl_result(result: nix::libc::c_int) -> Result<()> {
    if result == -1 {
        Err(PtyError::Resize(nix::errno::Errno::last()))
    } else {
        Ok(())
    }
}

// Tests extracted to `pty/tests.rs` (repo convention) to keep the
// production file within the architecture gate.
#[cfg(test)]
#[path = "pty/tests.rs"]
mod tests;
