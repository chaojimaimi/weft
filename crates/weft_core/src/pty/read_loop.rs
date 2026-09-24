//! The PTY reader task: the async read loop plus the pure flood-phase read
//! aggregation it drives (T8, PLAN_v11217_PERF_TRAIN §3.4).
//!
//! Moved out of `pty.rs` via the standard `pty.rs` + `pty/` directory layout
//! while rewiring (same file split as `pty/tests.rs`), so the production
//! file stays under its architecture budget. T8's two changes live here:
//!
//! - Backlog-state UI wake interval tightened 16 ms → 2 ms (the decision
//!   itself stays in the parent as `pty_wake_due`);
//! - Consecutive non-blocking reads are aggregated into one
//!   `PtyEvent::Output` batch by the pure [`read_batch`] until the soft
//!   target, a WouldBlock, the hard [`EVENT_CAP`], or EOF ends the batch —
//!   no timers, no data hold: a dry read flushes what was accumulated
//!   immediately, so interactive echo latency is unchanged.

use std::io;
use std::os::unix::io::{AsRawFd, OwnedFd};

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use tokio::sync::mpsc;

use super::{monotonic_millis, pty_wake_due, PtyEvent, WakeThrottle, PTY_CHANNEL_CAP};
use crate::input::{MouseDisableScanner, MouseSuppressFlag};

/// Hard upper bound on one aggregated read batch — i.e. one `PtyEvent::Output`
/// message (T8, PLAN_v11217 §3.4, fourth-round review P1). Enforced
/// STRUCTURALLY by [`read_batch`]: the first read is capped at the caller's
/// scratch buffer (production: the 256 KiB PTY read buffer) and every
/// continuation read requests `min(scratch_len, EVENT_CAP − batch.len())`,
/// so no sequence of kernel reads can push a batch past `EVENT_CAP`. Single
/// source of truth: the app crate's oversize-split threshold
/// (`pane_pump.rs::MAX_BYTES_PER_MESSAGE`) re-exports this constant, so a
/// production message can never trigger the split path and T1's
/// `has_pending_tail`-is-production-false invariant stays true.
pub const EVENT_CAP: usize = 256 * 1024;

/// Soft flush target (T8): a batch is flushed once it holds at least this
/// many bytes. Soft — [`EVENT_CAP`] remains the hard bound, and WouldBlock /
/// EOF flush whatever was accumulated regardless of this target.
pub(crate) const READ_BATCH_TARGET: usize = 64 * 1024;

/// Why an aggregated read batch ended. `Eof` / `Error` are terminal: the
/// caller must flush the returned batch (if non-empty) into the channel
/// before taking the shared exit tail.
#[derive(Debug)]
pub(crate) enum BatchStop {
    /// Batch reached the soft target — normal flood-path flush; keep reading.
    SoftTarget,
    /// A read returned WouldBlock — no more data right now; keep reading.
    WouldBlock,
    /// A read returned `Ok(0)`: the child closed the PTY (EOF).
    Eof,
    /// A read failed with something other than WouldBlock (EIO/EBADF/…).
    Error(io::Error),
}

/// Aggregate consecutive non-blocking reads into one batch (T8,
/// PLAN_v11217 §3.4; closure injection, same testability shape as
/// `write_all_nonblocking`).
///
/// The first read fills up to `scratch.len()` bytes; every continuation read
/// requests `min(scratch.len(), EVENT_CAP − batch.len())` — that per-request
/// cap is what makes [`EVENT_CAP`] a hard bound (fourth-round review P1:
/// "check after" aggregation could overshoot, capped requests cannot). The
/// loop stops on the first of: batch ≥ [`READ_BATCH_TARGET`] (soft target), a
/// `WouldBlock` read, a terminal read error, or `Ok(0)` (EOF). No timers, no
/// data hold.
///
/// `read_fn` receives the slice to fill and answers with real read
/// semantics: byte count, `Ok(0)` = EOF, `Err(WouldBlock)` = drained.
pub(crate) fn read_batch<R>(read_fn: &mut R, scratch: &mut [u8]) -> (Vec<u8>, BatchStop)
where
    R: FnMut(&mut [u8]) -> io::Result<usize>,
{
    let mut batch: Vec<u8> = Vec::with_capacity(READ_BATCH_TARGET);
    loop {
        // Hard-cap structure: never ask a read for more than the batch's
        // remaining room, so `batch.len() ≤ EVENT_CAP` holds by induction —
        // every read appends at most `EVENT_CAP − batch.len()` bytes.
        let request = scratch.len().min(EVENT_CAP - batch.len());
        if request == 0 {
            // Unreachable in practice: a batch at EVENT_CAP also satisfies
            // the soft-target check below and returns first. Kept as a
            // structural guard so `read_fn` is never invoked with a
            // zero-length slice.
            return (batch, BatchStop::SoftTarget);
        }
        match read_fn(&mut scratch[..request]) {
            Ok(0) => return (batch, BatchStop::Eof),
            Ok(n) => {
                batch.extend_from_slice(&scratch[..n]);
                if batch.len() >= READ_BATCH_TARGET {
                    return (batch, BatchStop::SoftTarget);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                return (batch, BatchStop::WouldBlock);
            }
            Err(e) => return (batch, BatchStop::Error(e)),
        }
    }
}

/// Async read loop: reads from the PTY master fd and sends output events.
/// Detects child exit via EIO error and sends an Exit event.
///
/// v1.11.2 X2 (PLAN_v1112 §2): `tx` is bounded; a full channel suspends this
/// task on `send().await`, which backpressures into the kernel PTY buffer and
/// ultimately blocks the child's writes — bytes are never dropped. UI wakes
/// are throttled during floods (`WakeThrottle` + `pty_wake_due`); Exit
/// always wakes.
///
/// v1.11.15 (FIX A, PLAN_v11115_EXIT_RACE_MOUSE_LEAK §1): every chunk is fed
/// through a persistent [`MouseDisableScanner`] BEFORE it is queued; the
/// first mouse-disable DECRST flips `suppress` right here on the reader
/// thread, closing the parse-latency window in which the UI used to keep
/// writing hover bytes into a shell that had already left the TUI. The exit
/// paths (EOF / EIO / EBADF / read error) force-set the flag too, covering a
/// TUI killed before it could emit its disable sequences.
///
/// T8 (PLAN_v11217 §3.4): reads are aggregated into batches by [`read_batch`].
/// The scanner and the `WEFT_PTY_CAPTURE` tee stay PER-READ (inside the
/// `read_one` closure) so the suppression flag still flips as early as the
/// bytes exist and the tee records exact kernel read boundaries. EOF / EIO /
/// EBADF flush the already-aggregated batch into the channel BEFORE the exit
/// tail runs — no byte loss on child exit. The vte FSM state spans the
/// boundaries unchanged (pty.rs note: sequences persist across reads with no
/// carry buffer; a batch edge is just another arbitrary split point, no
/// different from today's kernel read edges).
pub(super) async fn read_loop<W: Fn() + Send + 'static>(
    fd: OwnedFd,
    child_pid: Pid,
    tx: mpsc::Sender<PtyEvent>,
    suppress: MouseSuppressFlag,
    wake: W,
) {
    // Buffer size: 256KB as per architecture doc (== EVENT_CAP; read_batch
    // additionally caps every request to the batch's remaining room).
    const BUF_SIZE: usize = 256 * 1024;

    let mut throttle = WakeThrottle::default();
    // Persistent across chunks: a sequence split across reads stays inside
    // the FSM (no carry buffer needed).
    let mut mouse_scanner = MouseDisableScanner::new();

    let async_fd = match tokio::io::unix::AsyncFd::new(fd) {
        Ok(fd) => fd,
        Err(e) => {
            tracing::error!(error = %e, "failed to create async fd for PTY read");
            let _ = tx
                .send(PtyEvent::Exit(Err(format!(
                    "async fd creation failed: {e}"
                ))))
                .await;
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

        // One non-blocking read, carrying the two per-read side effects T8
        // must preserve: (FIX A) the mouse scanner sees each read's bytes
        // BEFORE they enter the batch, and (R1-4) the capture tee writes per
        // read. Tokio's readiness-level WouldBlock (TryIoError) is
        // normalized to the io::Error kind read_batch stops on.
        let mut read_one = |dst: &mut [u8]| -> io::Result<usize> {
            let n = match guard
                .try_io(|fd| nix::unistd::read(fd.as_raw_fd(), dst).map_err(io::Error::from))
            {
                Ok(result) => result?,
                Err(_would_block) => return Err(io::Error::from(io::ErrorKind::WouldBlock)),
            };
            let data = &dst[..n];
            // v1.11.15 (FIX A): scan before queueing so the flag flips as
            // early as the bytes exist. A hit here is always followed by
            // the main-thread parser clearing it (h and l arms alike).
            if mouse_scanner.feed(data) {
                crate::input::set_suppressed(&suppress);
            }
            // R1-4: env-gated capture tee for recording real PTY byte
            // streams. Disabled by default (one env::var lookup per read);
            // set WEFT_PTY_CAPTURE=/path/to/capture.bin to record. Fixtures
            // committed to the repo use inline byte literals (see
            // tests/replay_fixtures.rs), but this tee is the tool for
            // discovering the exact byte shapes of new TUI apps.
            if let Ok(path) = std::env::var("WEFT_PTY_CAPTURE") {
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(&path)
                {
                    let _ = std::io::Write::write_all(&mut f, data);
                }
            }
            Ok(n)
        };

        // T8: aggregate continuation reads into one batch (soft target 64KB,
        // hard cap EVENT_CAP, WouldBlock / EOF / error stop — see
        // `read_batch`).
        let (data, stop) = read_batch(&mut read_one, &mut buf);

        // Classify the stop. `terminal` also means: flush the batch below
        // BEFORE breaking into the shared exit tail (T8 hard requirement).
        let terminal = match &stop {
            BatchStop::SoftTarget | BatchStop::WouldBlock => None,
            BatchStop::Eof => {
                tracing::debug!("PTY read returned 0 (EOF)");
                Some(())
            }
            BatchStop::Error(e)
                if e.raw_os_error() == Some(nix::libc::EIO)
                    || e.raw_os_error() == Some(nix::libc::EBADF) =>
            {
                // EIO on master fd means child exited (macOS).
                tracing::debug!("PTY read EIO/EBADF — child likely exited");
                Some(())
            }
            BatchStop::Error(e) => {
                tracing::error!(error = %e, "PTY read error");
                Some(())
            }
        };

        if data.is_empty() {
            // Spurious wakeup with no bytes (would-block on the first read):
            // nothing to pump, nothing to flush — retry, exactly as the
            // pre-T8 loop's WouldBlock arms did.
            if terminal.is_some() {
                break;
            }
            continue;
        }

        // v1.11.2 X2: sample emptiness BEFORE the send — an empty
        // queue means the consumer is caught up and must be woken so
        // fresh output is pumped promptly. Suspending here on a full
        // channel is the intended backpressure path.
        // (tokio 1.53's Sender has no len(); full remaining capacity
        // is exactly "queue is empty" for this single-producer task.)
        // T8: the sampled unit is the whole batch.
        let consumer_caught_up = tx.capacity() >= PTY_CHANNEL_CAP;
        if tx.send(PtyEvent::Output(data)).await.is_err() {
            // v1.11.16 (Fix B1): receiver dropped ⟹ Pty dropped (no take/move
            // path for event_rx — verified). Drop::drop runs before field drops,
            // so SIGHUP is already sent; re-kill is a harmless ESRCH no-op.
            // Break into the shared exit tail (set_suppressed + waitpid_safe).
            let _ = signal::kill(child_pid, Signal::SIGHUP);
            break;
        }
        // Nudge the UI event loop so fresh output is pumped promptly,
        // instead of idling until the next keyboard/mouse event — but at
        // most once per `FLOOD_WAKE_INTERVAL_MS` while backlogged (T8
        // tightened the v1.11.2 X2 interval from 16 ms ~60 Hz).
        let now_ms = monotonic_millis();
        if pty_wake_due(false, consumer_caught_up, throttle.last(), now_ms) {
            wake();
            throttle.stamp(now_ms);
        }
        if terminal.is_some() {
            break;
        }
    }

    // v1.11.15 (FIX A): every exit path lands here (EOF, EIO/EBADF, read
    // error, unreadable fd). Force-set the flag so a TUI killed before it
    // could emit its mouse-disable sequences cannot leave the UI writing
    // hover bytes into a dead or legacy-mode PTY. The pending tab teardown (or
    // the parser's next mouse-mode DECSET, for a surviving shell) is the
    // authoritative follow-up.
    crate::input::set_suppressed(&suppress);
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
    // v1.11.2 X2: Exit bypasses the throttle entirely (pty_wake_due's
    // is_exit arm) — the UI must learn of the dead child immediately.
    if tx.send(PtyEvent::Exit(exit_status)).await.is_ok() {
        wake();
    }
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
