//! The PTY reader task: the async read loop plus the pure flood-phase read
//! aggregation it drives (T8, PLAN_v11217_PERF_TRAIN §3.4) and the
//! flood-phase batch hold the loop layers on top of it (T5', §3.7).
//!
//! Moved out of `pty.rs` via the standard `pty.rs` + `pty/` directory layout
//! while rewiring (same file split as `pty/tests.rs`), so the production
//! file stays under its architecture budget. Changes living here:
//!
//! - Backlog-state UI wake interval tightened 16 ms → 2 ms (the decision
//!   itself stays in the parent as `pty_wake_due`, T8);
//! - Consecutive non-blocking reads are aggregated into one
//!   `PtyEvent::Output` batch by the pure [`read_batch`] until the soft
//!   target, a WouldBlock, the hard [`EVENT_CAP`], or EOF ends the batch —
//!   `read_batch` itself stays timer-free and hold-free (T8);
//! - T5' (§3.7) seq short-line fix: on an ACTIVE stream a
//!   WouldBlock-truncated batch is CARRIED OVER in `pending` across the
//!   WouldBlock boundary (with a [`HOLD_MAX_MS`] timer racing readiness in
//!   a select-armed loop top, so a trickle can never strand the bytes), and
//!   the batch count collapses from ~50k/s to hundreds/s. An idle stream's
//!   first batch is never held — interactive echo is unchanged.

use std::io;
use std::os::unix::io::{AsRawFd, OwnedFd};
use std::time::Duration;

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

/// T5' (PLAN_v11217 §3.7): data seen within this window means the producing
/// stream is ACTIVE and a WouldBlock-truncated batch is worth carrying over
/// into [`HOLD_MAX_MS`]-bounded hold state instead of paying one
/// send+wake round-trip per few-line batch.
pub(crate) const STREAM_ACTIVE_WINDOW_MS: u64 = 50;

/// T5': how long a carried batch may wait for the next trickle before the
/// hold timer flushes it — the echo-latency ceiling for an active stream
/// (≤4 ms is invisible under an 8 ms vsync; an idle stream's first batch is
/// never held at all).
pub(crate) const HOLD_MAX_MS: u64 = 4;

/// T16a (PLAN_v11217 §3.11): the three audited constants above become
/// DEFAULTS that a `WEFT_READ_*` env var can override, for the user's
/// three-step tuning sweep (4/8/16 ms hold) before new defaults are
/// pinned. Tuning-only by design — no behavioral branch reads these.
///
/// Resolution is PROCESS-LEVEL, once, via `OnceLock` (review P2: a
/// read_loop is spawned per tab, so per-thread `env::var` would pay the
/// process-global lock once per tab for a value that never changes).
///
/// Test isolation (review P2): under `cfg(test)` the env is NEVER read —
/// `wake_and_batch_tests` pins exact batch shapes against the defaults,
/// and a user shell's leftover `WEFT_READ_*` must not pollute `cargo test`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReadLoopTuning {
    /// Overrides [`STREAM_ACTIVE_WINDOW_MS`] (`WEFT_READ_ACTIVE_MS`).
    pub(crate) stream_active_window_ms: u64,
    /// Overrides [`HOLD_MAX_MS`] (`WEFT_READ_HOLD_MS`).
    pub(crate) hold_max_ms: u64,
    /// Overrides [`READ_BATCH_TARGET`] (`WEFT_READ_BATCH_BYTES`).
    pub(crate) read_batch_target: usize,
}

/// Single source for the defaults: the three constants above verbatim, so
/// a constant change cannot drift from the resolver's fallback.
const DEFAULT_TUNING: ReadLoopTuning = ReadLoopTuning {
    stream_active_window_ms: STREAM_ACTIVE_WINDOW_MS,
    hold_max_ms: HOLD_MAX_MS,
    read_batch_target: READ_BATCH_TARGET,
};

/// Ceiling for the millisecond overrides — a garbage value (or typo like
/// `16000`) must not turn the hold into a multi-second echo freeze.
const TUNING_MS_CAP: u64 = 1_000;

/// Pure resolver: field-by-field fallback, so a single malformed key
/// cannot discard the other two overrides. Unit-testable with an injected
/// getter; production feeds `std::env::var`. `pub(crate)` for the test
/// module only (wake_and_batch_tests drives it with a fake env map).
pub(crate) fn resolve_tuning(get: impl Fn(&str) -> Option<String>) -> ReadLoopTuning {
    let mut tuning = DEFAULT_TUNING;
    if let Some(v) = get("WEFT_READ_ACTIVE_MS").and_then(|s| s.parse::<u64>().ok()) {
        tuning.stream_active_window_ms = v.min(TUNING_MS_CAP);
    }
    if let Some(v) = get("WEFT_READ_HOLD_MS").and_then(|s| s.parse::<u64>().ok()) {
        tuning.hold_max_ms = v.min(TUNING_MS_CAP);
    }
    if let Some(v) = get("WEFT_READ_BATCH_BYTES").and_then(|s| s.parse::<usize>().ok()) {
        // The soft target must stay under the structural EVENT_CAP hard
        // bound (read_batch's induction depends on it); 0 would flush per
        // read, degenerating the aggregation entirely.
        tuning.read_batch_target = v.clamp(1, EVENT_CAP);
    }
    tuning
}

/// Process-level tuning snapshot. Test builds always get the defaults
/// (see the isolation note above); production parses the env exactly once.
#[cfg(test)]
pub(crate) fn read_tuning() -> &'static ReadLoopTuning {
    &DEFAULT_TUNING
}

#[cfg(not(test))]
pub(crate) fn read_tuning() -> &'static ReadLoopTuning {
    static TUNING: std::sync::OnceLock<ReadLoopTuning> = std::sync::OnceLock::new();
    TUNING.get_or_init(|| {
        let tuning = resolve_tuning(|key| std::env::var(key).ok());
        if tuning != DEFAULT_TUNING {
            tracing::info!(?tuning, "WEFT_READ_* override applied (tuning sweep)");
        }
        tuning
    })
}

/// T5' pure hold decision (truth-tabled in `wake_and_batch_tests.rs`):
/// `None` — no data has EVER arrived in this session — is deliberately NOT
/// active (review P3a): a naive `0` default would misjudge the session's
/// first 50 ms as active and hold the very first prompt for 4 ms.
pub(crate) fn stream_active(last_data_ms: Option<u64>, now_ms: u64) -> bool {
    match last_data_ms {
        None => false,
        Some(last) => now_ms.saturating_sub(last) < read_tuning().stream_active_window_ms,
    }
}

/// T5' pure hold decision: true once the hold window (default
/// [`HOLD_MAX_MS`]) has elapsed since the held batch's FIRST byte arrived —
/// the batch must flush regardless of stream activity (the hold ceiling is
/// absolute, not per-append).
pub(crate) fn hold_expired(first_byte_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(first_byte_ms) >= read_tuning().hold_max_ms
}

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
    // T16a: the soft target is env-overridable (process-level, resolved
    // once); the EVENT_CAP hard bound is NOT — read_batch's structural
    // induction (`request ≤ EVENT_CAP − batch.len()`) must never bend.
    let soft_target = read_tuning().read_batch_target;
    let mut batch: Vec<u8> = Vec::with_capacity(soft_target);
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
                if batch.len() >= soft_target {
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
///
/// T5' (PLAN_v11217 §3.7): while a stream is active, a WouldBlock-truncated
/// batch is carried in `pending` across the WouldBlock boundary and only
/// flushed on soft-target close, hold expiry (a `HOLD_MAX_MS` timer racing
/// readability in a select-armed loop top), a terminal stop, or the EVENT_CAP
/// pre-append check. Invariants untouched: the scanner/`WEFT_PTY_CAPTURE`
/// tee stay per-read (so the suppression flag still flips the moment the
/// bytes EXIST — on an active stream their forwarding to the UI may now lag
/// by ≤4 ms, which is the declared echo-latency ceiling), caught-up is
/// sampled before every send (inside [`flush_output`]), Exit always wakes,
/// and the wake throttle is unchanged. An idle stream's first batch is never
/// held: the activity decision uses the pre-batch stamp.
pub(super) async fn read_loop<W: Fn() + Send + 'static>(
    fd: OwnedFd,
    child_pid: Pid,
    tx: mpsc::Sender<PtyEvent>,
    suppress: MouseSuppressFlag,
    mut wake: W,
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

    // R1-4 (v1.12.23 audit batch 1): the WEFT_PTY_CAPTURE tee is resolved
    // ONCE here, before the loop — it used to cost one env::var global-lock
    // lookup plus one file open PER KERNEL READ. The file is opened 0600
    // (and an existing file re-chmod'ed: the old create() inherited the
    // umask), a failed open is no longer silently swallowed, and enabling
    // the tee is announced at warn level because it records plaintext
    // secrets. Disabled by default: with the env unset this is dead code.
    let mut capture: Option<std::fs::File> = std::env::var("WEFT_PTY_CAPTURE")
        .ok()
        .and_then(|path| {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            match std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(f) => {
                    let _ =
                        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
                    tracing::warn!(
                        path = %path,
                        "PTY capture enabled: ALL terminal output (including secrets) is recorded in plaintext"
                    );
                    Some(f)
                }
                Err(e) => {
                    tracing::warn!(%e, path = %path, "WEFT_PTY_CAPTURE set but unopenable");
                    None
                }
            }
        });

    // T5' (§3.7) hold state: a WouldBlock-truncated batch on an active
    // stream is carried here across the WouldBlock boundary — bytes plus
    // the arrival timestamp of the batch's FIRST byte (the hold anchor);
    // `last_data_ms` feeds the stream-activity window. Any path that sends
    // or exits flushes `pending` first, so no byte can strand.
    let mut pending: Option<(Vec<u8>, u64)> = None;
    let mut last_data_ms: Option<u64> = None;

    loop {
        // T5' change point 3: while a batch is held, race readiness against
        // a `HOLD_MAX_MS` timer anchored at the held batch's FIRST byte, so
        // a trickle (seq's line-by-line writes) can never strand bytes past
        // the hold ceiling. `biased` prefers the readable arm when both are
        // ready — continuous data flushes via the soft target / cap anyway;
        // the timer only wins when the stream actually pauses.
        //
        // Epoch conversion (review P2a): the state timestamps come from
        // `monotonic_millis()` (std Instant, process start) while tokio's
        // clock starts at runtime start — the two are NEVER mixed raw. The
        // deadline is rebuilt each iteration from the SAME first-byte
        // anchor: `first_byte_ms` only changes when the pending batch is
        // replaced, so as `now` advances the remaining time shrinks and the
        // absolute deadline stays put — a trickle append does NOT reset the
        // timer. T16a: the hold ceiling is env-overridable (resolved once
        // per process).
        let hold_max_ms = read_tuning().hold_max_ms;
        let mut guard = if let Some((_, first_byte_ms)) = &pending {
            let held_for = monotonic_millis()
                .saturating_sub(*first_byte_ms)
                .min(hold_max_ms);
            let deadline =
                tokio::time::Instant::now() + Duration::from_millis(hold_max_ms - held_for);
            tokio::select! {
                biased;
                readable = async_fd.readable() => match readable {
                    Ok(guard) => guard,
                    Err(e) => {
                        tracing::debug!(error = %e, "PTY read fd became unreadable");
                        // Same rule as the terminal stops: bytes already
                        // read are flushed before the exit tail.
                        if let Some((held, _)) = pending.take() {
                            let _ = flush_output(&tx, child_pid, &mut throttle, &mut wake, held).await;
                        }
                        break;
                    }
                },
                _ = tokio::time::sleep_until(deadline) => {
                    // Hold expired: flush the held batch (send + wake) and
                    // drop back to the plain loop top — held bytes never
                    // wait on data that may never come.
                    let (held, _) = pending.take().expect("pending held across the timer arm");
                    if !flush_output(&tx, child_pid, &mut throttle, &mut wake, held).await {
                        break;
                    }
                    continue;
                }
            }
        } else {
            match async_fd.readable().await {
                Ok(g) => g,
                Err(e) => {
                    tracing::debug!(error = %e, "PTY read fd became unreadable");
                    break;
                }
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
            // R1-4: capture tee write stays PER-READ (exact kernel read
            // boundaries); set WEFT_PTY_CAPTURE=/path/to/capture.bin to
            // record — the file is resolved once before the loop (see the
            // startup block there). Fixtures committed to the repo use
            // inline byte literals (see tests/replay_fixtures.rs), but this
            // tee is the tool for discovering the exact byte shapes of new
            // TUI apps.
            if let Some(f) = capture.as_mut() {
                if let Err(e) = std::io::Write::write_all(f, data) {
                    tracing::debug!(?e, "capture tee write failed");
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
            if terminal.is_some() {
                // T5' combination table — terminal + empty batch + held
                // bytes (review P1b): a command's tail can sit in `pending`
                // (child exited right after its last writes reached the
                // hold) — flush it before the exit tail (R2: no byte loss
                // on child exit). Receiver gone or not, the tail runs
                // either way; the helper already SIGHUP'd on Err.
                if let Some((held, _)) = pending.take() {
                    let _ = flush_output(&tx, child_pid, &mut throttle, &mut wake, held).await;
                }
                break;
            }
            // Spurious wakeup with no bytes (would-block on the first
            // read): nothing to pump. With a held batch this `continue`
            // lands on the SELECT-ARMED loop top above (combination table
            // row 3, review P1b) — a plain `readable().await` top would
            // leave pending stranded without its hold timer.
            continue;
        }

        // T5' change point 2: every read that returned data marks the
        // stream active. The activity DECISION below uses the pre-batch
        // value on purpose — the first small batch after an idle period
        // must flush immediately ("an idle stream's first batch is never
        // held"), so `was_active` is sampled before this batch refreshes
        // the stamp.
        let now_ms = monotonic_millis();
        let was_active = stream_active(last_data_ms, now_ms);
        last_data_ms = Some(now_ms);

        // Hold gate: ONLY a WouldBlock-truncated batch on an active stream
        // with the hold window unexpired is carried over. A SoftTarget stop
        // closes the batch; a terminal stop or an expired hold flushes.
        let hold_first_ms = pending.as_ref().map_or(now_ms, |(_, first)| *first);
        let carry_over = matches!(stop, BatchStop::WouldBlock)
            && was_active
            && !hold_expired(hold_first_ms, now_ms);

        if carry_over {
            // Review P1a: keep "any Output ≤ EVENT_CAP" a STRUCTURAL
            // guarantee — if the append would reach the cap, flush the
            // held batch FIRST (byte order preserved) and start a fresh
            // pending from this batch; the oversize-split path stays
            // test-injection-only.
            let cap_hit = pending
                .as_ref()
                .is_some_and(|(held, _)| held.len() + data.len() >= EVENT_CAP);
            if cap_hit {
                let (held, _) = pending.take().expect("cap_hit implies pending");
                if !flush_output(&tx, child_pid, &mut throttle, &mut wake, held).await {
                    break;
                }
            }
            match &mut pending {
                // min: the combined batch's anchor is its EARLIEST byte — a
                // trickle append must not extend the hold deadline.
                Some((held, first)) => {
                    *first = (*first).min(now_ms);
                    held.extend_from_slice(&data);
                }
                None => pending = Some((data, now_ms)),
            }
            // No send, no wake — deferring both is the entire point.
            continue;
        }

        // Immediate flush: soft-target close, idle-stream batch, or expired
        // hold. Pending and the fresh batch are NEVER merged (T5' change
        // point 4) — pending goes first so byte order holds and no single
        // Output can exceed EVENT_CAP; a failed first send short-circuits
        // the second (helper returns false → break).
        if let Some((held, _)) = pending.take() {
            if !flush_output(&tx, child_pid, &mut throttle, &mut wake, held).await {
                break;
            }
        }
        if !flush_output(&tx, child_pid, &mut throttle, &mut wake, data).await {
            break;
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

/// Shared Output flush (T5' second-round review P2): every send point in
/// [`read_loop`] funnels through here, so the send-error semantics exist in
/// exactly one place — sample `caught-up` BEFORE the send (v1.11.2 X2: an
/// empty queue means the consumer must be woken so fresh output is pumped
/// promptly; a full channel suspends here, the intended backpressure path),
/// then send, then the wake check.
///
/// On `Err` (receiver dropped ⟹ Pty dropped — v1.11.16 Fix B1 semantics,
/// explicitly preserved) the child is SIGHUP'd and `false` tells the caller
/// to break into the shared exit tail; for a pending+data pair a failed
/// first send short-circuits the second. `PtyEvent::Exit` does NOT go
/// through here (it has its own always-wake arm).
///
/// (tokio 1.53's Sender has no len(); full remaining capacity is exactly
/// "queue is empty" for this single-producer task.)
/// (`W` reaches the helper as `&mut W`: a mutable reference is `Send`
/// whenever `W: Send`, so the spawned `read_loop` future stays `Send`
/// without widening `Pty::spawn`'s public `W` bound to `Sync`.)
async fn flush_output<W: Fn()>(
    tx: &mpsc::Sender<PtyEvent>,
    child_pid: Pid,
    throttle: &mut WakeThrottle,
    wake: &mut W,
    data: Vec<u8>,
) -> bool {
    let consumer_caught_up = tx.capacity() >= PTY_CHANNEL_CAP;
    if tx.send(PtyEvent::Output(data)).await.is_err() {
        // Drop::drop runs before field drops, so SIGHUP is already sent on
        // the Pty drop path; re-kill is a harmless ESRCH no-op.
        let _ = signal::kill(child_pid, Signal::SIGHUP);
        return false;
    }
    // Nudge the UI event loop so fresh output is pumped promptly, instead
    // of idling until the next keyboard/mouse event — but at most once per
    // `FLOOD_WAKE_INTERVAL_MS` while backlogged (T8 tightened the v1.11.2
    // X2 interval from 16 ms ~60 Hz).
    let now_ms = monotonic_millis();
    if pty_wake_due(false, consumer_caught_up, throttle.last(), now_ms) {
        wake();
        throttle.stamp(now_ms);
    }
    true
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
