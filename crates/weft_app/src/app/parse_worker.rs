//! The per-pane parse worker (v1.13.6 T10 P2, PLAN_v1136 §1 D2/D3/D4/D5/D6).
//!
//! One `weft-parse-<session_id>` thread per PTY-backed pane. It owns the
//! receive half of the pane's `Pty` event channel (D2 split) and feeds the
//! pane's `Arc<FairMutex<Terminal>>` directly — the Warp-verified model
//! where no frame data ever crosses a thread, only the terminal lock plus
//! merged wake-ups.
//!
//! Loop shape (D2 — the order is a HARD rule, rust-reviewer R1-4):
//! 1. `blocking_recv` one batch (channel backpressure bounds the batch at
//!    `EVENT_CAP` = 256 KiB, so the locked slice below has a natural upper
//!    bound of one batch's parse cost);
//! 2. **inside the terminal lock**: parse → take_response → alt-flip diff
//!    (`Instant::now()` stamped here, carried in the `AltFlipped` payload
//!    to keep the storm/debounce windows drift-free) — NOTHING else. No
//!    channel ops, no fd writes inside the guard (D9 rule 4; the
//!    non-blocking `try_*` forms are the only exemption);
//! 3. **after unlocking**: write the VT query reply through the shared
//!    [`PtyWriter`] (D4/D5), send the control event on the pane's crossbeam
//!    channel, and wake the main thread through the throttle.
//!
//! Wake ownership (D3): the reader no longer wakes anyone. The worker
//! reuses the weft_core throttle (`pty_wake_due` + `WakeThrottle`,
//! deliberately still defined and tested there): a caught-up worker wakes
//! immediately, a flood wakes at most once per 2 ms, and `Exit` always
//! forces a wake.
//!
//! Panic containment (D9 rule 6): parking_lot has no poison, so a panicking
//! `Terminal::process` cannot be observed via a poisoned guard. The locked
//! segment therefore runs under `catch_unwind`; on panic the worker emits
//! an exit-grade `PtyExited` control event and STOPS — a half-alive session
//! that swallows bytes silently is exactly the failure mode this forbids.
//!
//! Lifecycle: the thread ends when the event channel closes (pane dropped
//! its `Pty`), when `PtyEvent::Exit` arrives (after the FIFO-guaranteed
//! backlog drain), or on a parse panic. No join handle is kept — the pane
//! close path relies on the channel close, which makes the worker
//! self-terminating (D2: no leaks).

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use parking_lot::FairMutex;
use tokio::sync::mpsc;
use weft_core::pty::{monotonic_millis, pty_wake_due, PtyEvent, PtyWriter, WakeThrottle};
use weft_core::vt::Terminal;

use crate::app::parse_worker_stats::ParseWorkerStats;
use crate::AppMsg;

/// The UI wake seam. `EventLoopProxy<AppEvent>` in production (sending
/// `AppEvent::Wake`); a counting closure in tests — the proxy cannot be
/// constructed off an event loop (see close_confirmation.rs's note).
pub(crate) type WakeFn = Box<dyn Fn() + Send + Sync>;

/// One locked parse step's extraction (D2 step 2). Everything the worker
/// needs AFTER unlocking is pulled out here so no guard can leak.
#[derive(Debug, Default)]
pub(crate) struct ParsedBatch {
    /// VT query reply bytes (DA/DSR/CPR/DECRQSS) to write to the PTY.
    pub(crate) response: Vec<u8>,
    /// `Terminal::alt_flip_count` diff across the batch.
    pub(crate) alt_flips: u64,
    /// `is_alt_screen_history_peek` true → false across the batch.
    pub(crate) peek_exited: bool,
}

/// The locked parse segment shared by the production worker and the app
/// test seams — the alt-flip/peek diff logic exists in exactly ONE place.
pub(crate) fn parse_batch_locked(terminal: &mut Terminal, data: &[u8]) -> ParsedBatch {
    let before = terminal.alt_flip_count();
    let was_peeking = terminal.is_alt_screen_history_peek();
    terminal.process(data);
    let alt_flips = terminal.alt_flip_count().saturating_sub(before);
    let peek_exited = was_peeking && !terminal.is_alt_screen_history_peek();
    ParsedBatch {
        response: terminal.take_response(),
        alt_flips,
        peek_exited,
    }
}

/// Spawn the worker thread. Fire-and-forget: the handle is dropped (the
/// thread is detached), termination being driven by the event channel
/// close / Exit / panic — never by a join.
pub(crate) fn spawn(
    pane_session_id: u64,
    event_rx: mpsc::Receiver<PtyEvent>,
    terminal: Arc<FairMutex<Terminal>>,
    writer: Arc<PtyWriter>,
    ctrl_tx: crossbeam_channel::Sender<AppMsg>,
    had_output: Arc<AtomicBool>,
    wake: WakeFn,
) {
    let thread_name = format!("weft-parse-{pane_session_id}");
    let spawned = std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || run(event_rx, &terminal, &writer, &ctrl_tx, &had_output, &wake));
    if let Err(error) = spawned {
        // Only reachable under resource exhaustion; the pane degrades to
        // the pre-P2 symptom (no parsing) with a loud trace instead of a
        // silent one.
        tracing::error!(pane_session_id, %error, "failed to spawn parse worker thread");
    }
}

/// Outcome of the stale-backlog sweep behind a Flush marker.
enum DrainOutcome {
    /// Channel momentarily empty — resume normal processing.
    Drained,
    /// A queued `Exit` was reached (never dropped): run the exit tail.
    Exited(std::result::Result<i32, String>),
}

/// Discard every event still queued when a Flush marker is consumed (D2):
/// the marker orders "output older than this instant is stale". A queued
/// `Exit` is preserved for the exit tail. The sweep is try_recv-only (D9
/// rule 4 exemption) and terminates on the first empty poll, so a producer
/// racing the sweep simply continues past it — the residual window both
/// this protocol and the retired synchronous drain share.
fn drain_stale_backlog(rx: &mut mpsc::Receiver<PtyEvent>) -> DrainOutcome {
    loop {
        match rx.try_recv() {
            Ok(PtyEvent::Output(_)) | Ok(PtyEvent::Flush) => continue,
            Ok(PtyEvent::Exit(status)) => return DrainOutcome::Exited(status),
            Err(_) => return DrainOutcome::Drained,
        }
    }
}

/// Process ONE output batch: the locked parse segment (D2 step 2) followed
/// by the unlocked reply/control side effects (D2 step 3). Generic over
/// the parse step so the D9-rule-6 panic containment is testable without
/// bending the real parser (R5: core untouched).
///
/// Returns `Err(panic_reason)` when the parse panicked — the caller must
/// emit an exit-grade control event and stop the thread.
fn process_batch<P>(
    terminal: &FairMutex<Terminal>,
    writer: &PtyWriter,
    ctrl_tx: &crossbeam_channel::Sender<AppMsg>,
    had_output: &AtomicBool,
    stats: &mut ParseWorkerStats,
    data: &[u8],
    parse: P,
) -> Result<(), String>
where
    P: FnOnce(&mut Terminal, &[u8]) -> ParsedBatch,
{
    had_output.store(true, Ordering::Relaxed);
    // D2 step 2 — LOCKED: parse + extraction only. The FairMutex lock here
    // is the pane's own; nothing below the extraction touches the guard.
    // D9 rule 6: catch_unwind around the parse (parking_lot has no poison).
    let t_lock = Instant::now();
    let parsed = catch_unwind(AssertUnwindSafe(|| {
        let mut guard = terminal.lock();
        let lock_wait = t_lock.elapsed();
        let t_parse = Instant::now();
        let parsed = parse(&mut guard, data);
        let parse_busy = t_parse.elapsed();
        // Worker-side stamp at the diff point — consumed for the
        // AltFlipped payload so the main thread's storm/debounce windows
        // don't drift by one frame (P2 review).
        let at = (parsed.alt_flips > 0).then(Instant::now);
        (parsed, at, lock_wait, parse_busy)
    }));
    let (parsed, at, lock_wait, parse_busy) = match parsed {
        Ok((parsed, at, lock_wait, parse_busy)) => (parsed, at, lock_wait, parse_busy),
        Err(payload) => {
            let reason = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "parse worker panicked".to_string());
            return Err(reason);
        }
    };
    // Drain telemetry (§5.1 lock probe): lock_wait is the starvation signal
    // (main-thread render/pump holding the pane lock), parse_busy the
    // worker's own cost. Emission is gated by WEFT_TRACE_CHANNELS=1.
    stats.record(lock_wait, parse_busy, data.len(), Instant::now());
    stats.emit_if_due(Instant::now(), crate::frame_trace::trace_enabled(false));
    // D2 step 3 — UNLOCKED: reply write (D5: freshest cursor/mode state,
    // serialized with main-thread input by the PtyWriter's D4 lock) →
    // control event → wake handled by the caller.
    if !parsed.response.is_empty() {
        if let Err(error) = writer.write_sync(&parsed.response) {
            tracing::warn!(%error, "failed to write terminal response");
        }
    }
    if parsed.alt_flips > 0 || parsed.peek_exited {
        let _ = ctrl_tx.send(AppMsg::AltFlipped {
            at: at.unwrap_or_else(Instant::now),
            flips: parsed.alt_flips,
            peek_exited: parsed.peek_exited,
        });
    }
    Ok(())
}

/// D9 rule 6 tail: report a parse panic as an exit-grade control event and
/// force the UI wake. Split from `run`'s panic arm so the contract is
/// testable without bending the real parser.
fn report_parse_panic(ctrl_tx: &crossbeam_channel::Sender<AppMsg>, wake: &WakeFn, reason: &str) {
    tracing::error!(
        %reason,
        "parse worker panicked; reporting PtyExited and stopping the thread"
    );
    let _ = ctrl_tx.send(AppMsg::PtyExited(Err(reason.to_string())));
    wake();
}

/// The worker's main loop. All three exit paths (channel closed, Exit
/// event, parse panic) leave the thread — none leaves the pane without a
/// control event the main thread can act on.
fn run(
    mut rx: mpsc::Receiver<PtyEvent>,
    terminal: &Arc<FairMutex<Terminal>>,
    writer: &Arc<PtyWriter>,
    ctrl_tx: &crossbeam_channel::Sender<AppMsg>,
    had_output: &Arc<AtomicBool>,
    wake: &WakeFn,
) {
    let mut throttle = WakeThrottle::default();
    let mut stats = ParseWorkerStats::default();
    loop {
        // D2 step 1: block until the reader delivers a batch (or the pane
        // dies → channel closed → thread ends).
        let Some(event) = rx.blocking_recv() else {
            tracing::debug!(
                thread = std::thread::current().name(),
                "parse worker: channel closed"
            );
            return;
        };
        match event {
            PtyEvent::Output(data) => {
                match process_batch(
                    terminal,
                    writer,
                    ctrl_tx,
                    had_output,
                    &mut stats,
                    &data,
                    parse_batch_locked,
                ) {
                    Ok(()) => {}
                    Err(reason) => {
                        // D9 rule 6: a panicking parse must not leave the
                        // session half-alive. Report exit-grade, force the
                        // wake, and stop the thread.
                        report_parse_panic(ctrl_tx, wake, &reason);
                        return;
                    }
                }
                // D3: throttled wake. "Caught up" = the event channel is
                // momentarily empty (the equivalent of the reader's
                // capacity sample). Flood ⇒ at most one wake per 2 ms.
                let now_ms = monotonic_millis();
                if pty_wake_due(false, rx.is_empty(), throttle.last(), now_ms) {
                    wake();
                    throttle.stamp(now_ms);
                }
            }
            PtyEvent::Flush => {
                // D2 flush marker: discard the stale backlog. An Exit found
                // during the sweep takes the exit tail immediately (it is
                // ordered after every queued Output, so the backlog is
                // already parsed — dropping it would violate the close
                // contract).
                match drain_stale_backlog(&mut rx) {
                    DrainOutcome::Drained => {}
                    DrainOutcome::Exited(status) => {
                        let _ = ctrl_tx.send(AppMsg::PtyExited(status));
                        wake();
                        return;
                    }
                }
                // No wake: the flush was initiated on the main thread,
                // which is awake by construction.
            }
            PtyEvent::Exit(status) => {
                // FIFO contract (D2): Exit is enqueued strictly after every
                // queued Output, so the backlog is fully parsed by now —
                // the last bytes before a close are in the terminal BEFORE
                // the main thread learns of the death.
                let _ = ctrl_tx.send(AppMsg::PtyExited(status));
                // Exit bypasses the throttle: the UI must learn of the dead
                // child immediately (the pty_wake_due is_exit arm, applied
                // here by force — the worker IS the waker now).
                wake();
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::{AsRawFd, RawFd};
    use std::sync::Mutex as StdMutex;

    use crate::pane::Pane;

    /// Harness: real Terminal behind a FairMutex + a pipe-backed PtyWriter
    /// (replies are observable on the read end) + a control channel. No
    /// real PTY: the worker's contract is fully observable without one.
    struct Harness {
        rx_tx: mpsc::Sender<PtyEvent>,
        rx: Option<mpsc::Receiver<PtyEvent>>,
        terminal: Arc<FairMutex<Terminal>>,
        writer: Arc<PtyWriter>,
        ctrl_rx: crossbeam_channel::Receiver<AppMsg>,
        ctrl_tx: crossbeam_channel::Sender<AppMsg>,
        had_output: Arc<AtomicBool>,
        /// Owned read end of the reply pipe — kept alive so the RawFd stays
        /// valid for the whole test.
        _reply_file: std::os::unix::io::OwnedFd,
        reply_fd: RawFd,
    }

    impl Harness {
        fn new() -> Self {
            let (rx_tx, rx) = mpsc::channel(32);
            let (ctrl_tx, ctrl_rx) = crossbeam_channel::unbounded();
            let had_output = Arc::new(AtomicBool::new(false));
            // A pipe stands in for the PTY master write side: the worker's
            // reply path is byte-identical (one bounded non-blocking write).
            let (read_end, write_end) = nix::unistd::pipe().expect("pipe");
            let writer = Arc::new(PtyWriter::new(write_end));
            Self {
                rx_tx,
                rx: Some(rx),
                terminal: Arc::new(FairMutex::new(Terminal::new(24, 80))),
                writer,
                ctrl_rx,
                ctrl_tx,
                had_output,
                reply_fd: read_end.as_raw_fd(),
                _reply_file: read_end,
            }
        }

        /// A fresh wake closure plus its shared counter — the Box moves into
        /// the worker thread, so the pair is built per test instead of being
        /// stored on the harness.
        fn make_wake() -> (WakeFn, Arc<StdMutex<usize>>) {
            let counter = Arc::new(StdMutex::new(0usize));
            let handle = Arc::clone(&counter);
            (
                Box::new(move || {
                    *handle.lock().unwrap() += 1;
                }),
                counter,
            )
        }

        fn grid_text(&self, row: usize) -> String {
            self.terminal.lock().grid().row_text(row)
        }
    }

    /// Worker lifecycle: spawn → parse → Exit 收尾. The output must be in
    /// the terminal BEFORE the PtyExited control event is consumable (D2
    /// FIFO close contract).
    #[test]
    fn worker_lifecycle_spawn_process_then_exit_tail() {
        let mut h = Harness::new();
        let rx = h.rx.take().unwrap();
        let (wake, wakes) = Harness::make_wake();
        spawn(
            1,
            rx,
            Arc::clone(&h.terminal),
            Arc::clone(&h.writer),
            h.ctrl_tx.clone(),
            Arc::clone(&h.had_output),
            wake,
        );

        h.rx_tx
            .try_send(PtyEvent::Output(b"hello worker".to_vec()))
            .unwrap();
        h.rx_tx.try_send(PtyEvent::Exit(Ok(7))).unwrap();

        // The exit control event arrives (bounded wait — event-driven, no
        // timing assertion).
        let code = loop {
            match h.ctrl_rx.recv_timeout(std::time::Duration::from_secs(5)) {
                Ok(AppMsg::PtyExited(code)) => break code,
                Ok(AppMsg::AltFlipped { .. }) => continue,
                Err(_) => panic!("no PtyExited within the bounded wait"),
            }
        };
        assert_eq!(code, Ok(7), "the Exit payload rides PtyExited verbatim");
        assert_eq!(
            h.grid_text(0),
            "hello worker",
            "backlog parsed BEFORE the exit event"
        );
        assert!(
            h.had_output.load(Ordering::Relaxed),
            "worker raises had_output"
        );
        assert!(
            *wakes.lock().unwrap() >= 1,
            "Exit forces a wake even mid-throttle"
        );
    }

    /// D9 rule 6: a panicking parse surfaces as Err with the panic payload,
    /// and the report tail emits an exit-grade control event + wake. The
    /// parse step is injected so the real parser stays untouched (R5).
    #[test]
    fn worker_panic_reports_pty_exited_and_stops() {
        let h = Harness::new();
        let had_output = Arc::clone(&h.had_output);
        let mut stats = ParseWorkerStats::default();
        let err = process_batch(
            &h.terminal,
            &h.writer,
            &h.ctrl_tx,
            &had_output,
            &mut stats,
            b"whatever",
            |_terminal, _data| panic!("synthetic parse panic"),
        )
        .expect_err("a panicking parse must surface as Err");
        assert_eq!(err, "synthetic parse panic");
        assert!(
            h.had_output.load(Ordering::Relaxed),
            "flag raised for the batch"
        );

        let (wake, wakes) = Harness::make_wake();
        report_parse_panic(&h.ctrl_tx, &wake, &err);
        let event = h
            .ctrl_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("no control event after panic");
        match event {
            AppMsg::PtyExited(Err(reason)) => assert_eq!(reason, "synthetic parse panic"),
            other => panic!("expected exit-grade PtyExited, got {other:?}"),
        }
        assert_eq!(*wakes.lock().unwrap(), 1, "the panic tail forces a wake");
    }

    /// Flush marker: the backlog queued BEHIND the marker is discarded; a
    /// queued Exit survives the sweep and takes the exit tail.
    #[test]
    fn flush_sweep_discards_backlog_but_preserves_exit() {
        let (tx, mut rx) = mpsc::channel(8);
        tx.try_send(PtyEvent::Output(b"stale".to_vec())).unwrap();
        tx.try_send(PtyEvent::Flush).unwrap();
        tx.try_send(PtyEvent::Exit(Ok(3))).unwrap();
        match drain_stale_backlog(&mut rx) {
            DrainOutcome::Exited(status) => assert_eq!(status, Ok(3)),
            DrainOutcome::Drained => panic!("the queued Exit must survive the sweep"),
        }
        assert!(
            rx.try_recv().is_err(),
            "the sweep drains the channel (nothing stale survives)"
        );

        // No Exit queued: plain drain.
        tx.try_send(PtyEvent::Output(b"more".to_vec())).unwrap();
        tx.try_send(PtyEvent::Flush).unwrap();
        assert!(matches!(
            drain_stale_backlog(&mut rx),
            DrainOutcome::Drained
        ));
        assert!(rx.try_recv().is_err());
    }

    /// D5: a VT query reply (CPR class) reaches the PTY write half. A pipe
    /// stands in for the master fd — the write path is byte-identical.
    #[test]
    fn worker_writes_cpr_reply_to_the_pty() {
        let mut h = Harness::new();
        // Prime the cursor so the DSR reply has a definite position, then
        // feed the query as an Output batch through the real worker path.
        h.terminal.lock().process(b"hi");
        let rx = h.rx.take().unwrap();
        let (wake, _wakes) = Harness::make_wake();
        spawn(
            2,
            rx,
            Arc::clone(&h.terminal),
            Arc::clone(&h.writer),
            h.ctrl_tx.clone(),
            Arc::clone(&h.had_output),
            wake,
        );
        h.rx_tx
            .try_send(PtyEvent::Output(b"\x1b[6n".to_vec()))
            .unwrap();

        // The reply must appear on the pipe (bounded wait; event-driven).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut buf = Vec::new();
        let fd = h.reply_fd;
        while std::time::Instant::now() < deadline && !buf.contains(&b'R') {
            // SAFETY: fd owned by this harness for the whole call.
            let mut fds = [nix::poll::PollFd::new(
                unsafe { std::os::unix::io::BorrowedFd::borrow_raw(fd) },
                nix::poll::PollFlags::POLLIN,
            )];
            let timeout = nix::poll::PollTimeout::try_from(std::time::Duration::from_millis(200))
                .unwrap_or(nix::poll::PollTimeout::ZERO);
            match nix::poll::poll(&mut fds, timeout) {
                Ok(0) => continue,
                Ok(_) => {
                    let mut chunk = [0u8; 64];
                    match nix::unistd::read(fd, &mut chunk) {
                        Ok(0) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        Err(nix::errno::Errno::EAGAIN) => continue,
                        Err(e) => panic!("pipe read failed: {e}"),
                    }
                }
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => panic!("poll failed: {e}"),
            }
        }
        let reply = String::from_utf8_lossy(&buf);
        assert!(
            reply.starts_with('\u{1b}') && reply.contains('R'),
            "a CPR reply must reach the PTY write half, got {reply:?}"
        );
    }

    /// Lifecycle tail: dropping the event sender ends the worker (pane
    /// dropped its Pty) — no leaks, no hangs.
    #[test]
    fn worker_stops_when_the_event_channel_closes() {
        let mut h = Harness::new();
        let rx = h.rx.take().unwrap();
        let terminal = Arc::clone(&h.terminal);
        let writer = Arc::clone(&h.writer);
        let ctrl_tx = h.ctrl_tx.clone();
        let had_output = Arc::clone(&h.had_output);
        let wake: WakeFn = Box::new(|| {});
        let handle = std::thread::Builder::new()
            .name("weft-parse-test-close".to_string())
            .spawn(move || run(rx, &terminal, &writer, &ctrl_tx, &had_output, &wake))
            .unwrap();
        drop(h.rx_tx);
        handle
            .join()
            .expect("the worker must exit cleanly on channel close");
    }

    /// The shared parse extraction: flip diff counts a batch-internal
    /// h→l pair twice (D-2 semantics the resize storm logic depends on).
    #[test]
    fn parse_batch_locked_counts_net_zero_flip_pairs() {
        let mut terminal = Terminal::new(24, 80);
        let parsed = parse_batch_locked(&mut terminal, b"\x1b[?1049h");
        assert_eq!(parsed.alt_flips, 1);
        assert!(!parsed.peek_exited);
        let parsed = parse_batch_locked(&mut terminal, b"\x1b[?1049l\x1b[?1049h");
        assert_eq!(
            parsed.alt_flips, 2,
            "h→l pair nets zero but counts two flips"
        );
    }

    /// P2-3 joint e2e: close-style teardown → REAL worker thread parses the
    /// backlog and reports PtyExited on the pane's own control channel →
    /// the pump's exit arm force-settles → the tail bytes are IN the block.
    /// The two halves (worker FIFO contract in `worker_lifecycle_*`, pump
    /// exit arm in pane_pump) each had coverage; this pins the JOIN with the
    /// production wiring (spawn + pane.msg_tx / pane.msg_rx), no real PTY.
    #[test]
    fn teardown_worker_drain_and_pty_exited_land_tail_in_block() {
        use crate::tab::Tab;

        let pane = Pane::with_terminal_only(100);
        // In-flight OSC 133 block: A(head) B(command) C(output capture) —
        // everything parsed after this accumulates in the running block.
        pane.lock_terminal()
            .unwrap()
            .process(b"\x1b]133;A\x07\x1b]133;B\x07worker-e2e\x1b]133;C\x07");
        let terminal = pane
            .terminal_arc_for_test()
            .expect("test pane holds a terminal");
        let (wake, wakes) = Harness::make_wake();
        // Pipe-backed writer: the worker's reply leg has a target.
        let (_reply_keepalive, write_end) = nix::unistd::pipe().expect("pipe");
        let writer = Arc::new(PtyWriter::new(write_end));

        // Production wiring: the worker talks into the pane's own channel.
        let (event_tx, event_rx) = mpsc::channel(8);
        spawn(
            pane.pane_session_id,
            event_rx,
            terminal,
            writer,
            pane.msg_tx.clone(),
            Arc::clone(&pane.had_output),
            wake,
        );
        // The tail carries the shell's final 133;D (its exit hook) — the
        // marker the settle path finalizes the block on.
        event_tx
            .try_send(PtyEvent::Output(
                b"worker e2e tail\x1b]133;D;0\x07".to_vec(),
            ))
            .unwrap();
        event_tx.try_send(PtyEvent::Exit(Ok(0))).unwrap();

        // Deterministic rendezvous: the worker's Exit tail forces the wake
        // STRICTLY AFTER the PtyExited send, so once the counter moves the
        // control event is already queued on the pane — no timing assertion.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while *wakes.lock().unwrap() == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(
            *wakes.lock().unwrap() >= 1,
            "worker never reached its exit tail"
        );

        // The pump consumes the real PtyExited: last pane → alive=false,
        // force-settle finalizes the in-flight block WITH the tail.
        let mut tab = Tab::with_single_pane(pane);
        let (alive, drained, _, _) = tab.process_messages();
        assert!(!alive, "the last pane's PtyExited kills the tab");
        assert_eq!(drained.len(), 1, "the in-flight block must be finalized");
        assert!(
            drained[0].output.contains("worker e2e tail"),
            "the backlog parsed before PtyExited must land in the block: {:?}",
            drained[0].output
        );
    }

    /// The Pane flag the main pump drains (worker sets / pump clears) has
    /// take semantics.
    #[test]
    fn pane_had_output_flag_is_take_semantics() {
        let pane = Pane::with_terminal_only(100);
        assert!(!pane.take_had_output(), "fresh flag is false");
        pane.had_output.store(true, Ordering::Relaxed);
        assert!(pane.take_had_output());
        assert!(!pane.take_had_output(), "take clears the flag");
    }
}

// P3 wall-clock probes live in parse_worker_probes.rs (cfg(test) sibling,
// line-count gate) — run with:
//   cargo test -p weft_app --release -- --ignored --nocapture probe_real_pty
#[cfg(test)]
#[path = "parse_worker_probes.rs"]
mod probes;
