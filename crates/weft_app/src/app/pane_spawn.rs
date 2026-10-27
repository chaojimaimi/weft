//! v1.14.2 (B3, PLAN_v1142 §3): asynchronous PTY spawn — request assembly,
//! fork worker, and main-thread adoption.
//!
//! The pre-B3 chain forked/exec'd synchronously on the main thread
//! (`Pane::spawn` → `Pty::spawn_with_args`), so 18×Cmd+T froze the UI per
//! fork (tab 风暴债②). The two-segment tokio contract:
//!
//! 1. **fork** — `Pty::fork_pty` is pure fork + fd assembly, NO tokio, and
//!    runs on a throwaway `weft-fork-<id>` thread; the env/argv CString
//!    pre-build discipline happens there, before the fork, exactly as
//!    before (the worker thread never holds anything across the fork).
//! 2. **read loop** — `ForkedPty::start_read_loop` contains the
//!    `tokio::spawn`; the runtime is only `enter`ed on the MAIN thread
//!    (main.rs), so it is called exclusively from the adoption step below.
//!    A worker-thread call would panic AFTER the fork succeeded (orphan
//!    child + uncleaned master fd) — the whole reason this module exists.
//!
//! Adoption idempotency / exit-period semantics: the forked PTY rides the
//! `AppEvent::SpawnCompleted` payload. If the event is never adopted (pane
//! already closed, or the app exiting so the event is dropped with the
//! queue), the `ForkedPty` is dropped — closing the PTY master SIGHUPs the
//! child and the read loop hits EOF. Dropping IS the cleanup.

use std::sync::Arc;

use weft_core::input::MouseSuppressFlag;
use weft_core::pty::{ForkedPty, Pty};
use winit::event_loop::EventLoopProxy;

use crate::pane::Pane;
use crate::tab::Tab;
use crate::{App, AppEvent};

/// v1.11.2 X2 precedent: bounded PTY input buffering while the PTY is
/// pending (B3-4). 64 KiB ≈ a worst-case paste storm during the (few-ms)
/// fork window; beyond it the incoming bytes are dropped with a trace —
/// an unbounded Vec would let a held-down key OOM the fork window.
const PENDING_STDIN_CAP: usize = 64 * 1024;

/// One pending async PTY spawn. Built on the main thread at pane-creation
/// time, consumed by the fork worker (thread side) and the adoption step
/// (main-thread side: `mouse_suppress` at read-loop start, `cwd_fallback`
/// for the B2 backfill).
#[derive(Clone)]
pub(crate) struct SpawnRequest {
    pub(crate) pane_session_id: u64,
    /// The shell ($SHELL or /bin/zsh) — absolute, per the `execve` contract.
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
    /// Requested winsize — `fork_pty` uses it directly (R2 review P3), so
    /// the child's first prompt lands at the placeholder's grid size.
    pub(crate) rows: usize,
    pub(crate) cols: usize,
    /// `KEY=VALUE` shell-integration env (built by `shell_integration_env`).
    pub(crate) env: Vec<(String, String)>,
    /// `Some(path)` → the child chdirs there before exec. `None` → inherit
    /// the weft process cwd.
    pub(crate) cwd: Option<String>,
    /// B2 (plan §3): when `cwd` is `None` the tab title has nothing to show
    /// until the first OSC 7 — this pre-captured process cwd backfills
    /// `launch_cwd` at adoption (set-once; never clobbers a restore or
    /// workspace fallback).
    pub(crate) cwd_fallback: Option<String>,
    /// The pane's reader-side mouse-suppression flag — the read loop and
    /// the Terminal parser must share ONE flag per pane (v1.11.15 FIX A);
    /// the placeholder Terminal already holds a clone (pane.rs:216/247
    /// semantics preserved through the async seam).
    pub(crate) mouse_suppress: MouseSuppressFlag,
}

impl SpawnRequest {
    /// Assemble the request for a freshly created placeholder pane.
    /// Request assembly lives HERE (not pane.rs) per the plan's file
    /// budget: pane.rs only wires fields.
    pub(crate) fn for_new_pane(
        pane_session_id: u64,
        rows: usize,
        cols: usize,
        cwd: Option<&str>,
        mouse_suppress: MouseSuppressFlag,
    ) -> Self {
        let program = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        let env = crate::shell_integration_env(&program);
        // B2: capture the process cwd NOW (open time) — the value the child
        // will actually inherit — so adoption does not depend on the
        // process cwd being unchanged or even readable later.
        let cwd_fallback = if cwd.is_none() {
            std::env::current_dir()
                .ok()
                .map(|path| path.to_string_lossy().into_owned())
        } else {
            None
        };
        Self {
            pane_session_id,
            program,
            args: Vec::new(),
            rows,
            cols,
            env,
            cwd: cwd.map(str::to_owned),
            cwd_fallback,
            mouse_suppress,
        }
    }
}

/// Best-effort panic payload → message for the fork worker's `SpawnFailed`
/// report: `&str`/`String` payloads surface their message, anything else
/// falls back to generic wording. Mirrors weft_core's `panic_exit_reason`.
fn fork_panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| format!("fork worker panicked: {s}"))
        .or_else(|| {
            payload
                .downcast_ref::<String>()
                .map(|s| format!("fork worker panicked: {s}"))
        })
        .unwrap_or_else(|| "fork worker panicked".to_string())
}

/// v1.14.3 (T3, PLAN_v1143 §2): the grid text written into a
/// resource-exhausted diagnostic pane (`Terminal::process`, \r\n line
/// discipline — plain text, no OSC). The pane stays open; the user closes
/// idle tabs or reboots, then Cmd+T retries — or presses R (v1.15.1, app/retry_spawn.rs).
const RESOURCE_EXHAUSTED_DIAGNOSTIC: &str =
    "系统终端资源(PTY)已耗尽，无法启动 shell。请关闭空闲的终端标签页或重启系统后，再用 Cmd+T 新建。\r\n按 R 重试启动 shell。";

/// The diagnostic bytes as fed to `Terminal::process` — a leading blank line
/// plus a trailing newline so the message renders as its own paragraph on
/// the placeholder's empty grid.
pub(crate) fn resource_exhausted_diagnostic_bytes() -> Vec<u8> {
    format!("\r\n{RESOURCE_EXHAUSTED_DIAGNOSTIC}\r\n").into_bytes()
}

/// v1.14.3 (T3): failure classification, fork arm — a `forkpty` errno from
/// the kernel PTY/process pool means the system is out of terminal
/// resources (the user machine's PTY pool exhaustion manifests as ENXIO).
/// No string re-parsing: the errno is still typed (`PtyError::Fork(Errno)`)
/// at this point. Pure — truth-table tested.
pub(crate) fn fork_error_is_resource_exhausted(error: &weft_core::pty::PtyError) -> bool {
    use weft_core::pty::PtyError;
    matches!(
        error,
        PtyError::Fork(
            nix::errno::Errno::ENXIO
                | nix::errno::Errno::EMFILE
                | nix::errno::Errno::ENFILE
                | nix::errno::Errno::EAGAIN
        )
    )
}

/// v1.14.3 (T3): failure classification, worker-thread arm — thread creation
/// failing with EAGAIN (mapped to `ErrorKind::WouldBlock`) is the same
/// resource-exhaustion signal; every other io::Error is not. Pure.
pub(crate) fn worker_spawn_error_is_resource_exhausted(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
        || error.raw_os_error() == Some(nix::libc::EAGAIN)
}

/// Hand a spawn request to a `weft-fork-<id>` worker thread. Fire-and-forget:
/// the outcome comes back through the event loop as `SpawnCompleted` /
/// `SpawnFailed`. A worker-spawn failure (resource exhaustion) is reported
/// synchronously through the SAME `SpawnFailed` semantics — the pane never
/// hangs as a silent placeholder. A worker PANIC converges to `SpawnFailed`
/// too (P2-1): the catch_unwind below is the last line of that contract.
pub(crate) fn dispatch_spawn_request(request: SpawnRequest, proxy: &EventLoopProxy<AppEvent>) {
    let worker_proxy = proxy.clone();
    let pane_session_id = request.pane_session_id;
    let spawned = std::thread::Builder::new()
        .name(format!("weft-fork-{pane_session_id}"))
        .spawn(move || {
            // P2-1: guard the whole worker body so ANY panic (e.g. the
            // fork_pty Parent-branch F_GETFL/F_SETFL expects firing AFTER
            // the fork, pty.rs:271-274) still surfaces as `SpawnFailed`.
            // An unwitnessed thread death would leave the placeholder tab
            // hanging forever (ghost tab) with no SpawnCompleted AND no
            // SpawnFailed. A post-fork panic also risks an orphaned child —
            // SpawnFailed → tab close drops the master fd → the child gets
            // SIGHUP, which is exactly the desired cleanup. `worker_proxy`
            // is captured OUTSIDE the guarded closure so the failure report
            // itself cannot be unwound away; the default panic hook already
            // prints the panic, so no extra logging of the payload here.
            let worker_outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // CString pre-build happens INSIDE fork_pty, on this worker
                // thread, strictly before the fork (multi-threaded-fork
                // discipline preserved verbatim from the old main-thread path).
                let env_refs: Vec<(&str, &str)> = request
                    .env
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect();
                let arg_refs: Vec<&str> = request.args.iter().map(String::as_str).collect();
                let forked = Pty::fork_pty(
                    &request.program,
                    &arg_refs,
                    (request.rows as u16, request.cols as u16),
                    &env_refs,
                    request.cwd.as_deref(),
                );
                match forked {
                    // The fd rides the event; an unadopted event drops it (see
                    // the module doc — dropping is the cleanup).
                    Ok(forked) => {
                        let _ = worker_proxy.send_event(AppEvent::SpawnCompleted {
                            pane_session_id,
                            forked,
                        });
                    }
                    Err(error) => {
                        tracing::error!(pane_session_id, %error, "async PTY fork failed");
                        // v1.14.3 (T3): classify while the errno is still
                        // typed — ENXIO/EMFILE/ENFILE/EAGAIN → the resource
                        // branch keeps a diagnostic tab instead of closing.
                        let resource_exhausted = fork_error_is_resource_exhausted(&error);
                        let _ = worker_proxy.send_event(AppEvent::SpawnFailed {
                            pane_session_id,
                            error: error.to_string(),
                            resource_exhausted,
                        });
                    }
                }
            }));
            if let Err(payload) = worker_outcome {
                let error = fork_panic_message(payload);
                tracing::error!(pane_session_id, %error, "fork worker panicked");
                // v1.14.3 (T3): a panic is NOT a resource signal — false.
                let _ = worker_proxy.send_event(AppEvent::SpawnFailed {
                    pane_session_id,
                    error,
                    resource_exhausted: false,
                });
            }
        });
    if let Err(error) = spawned {
        tracing::error!(pane_session_id, %error, "failed to spawn fork worker thread");
        // v1.14.3 (T3): EAGAIN-class thread-creation failure = resource
        // exhaustion; anything else = not.
        let resource_exhausted = worker_spawn_error_is_resource_exhausted(&error);
        let _ = proxy.send_event(AppEvent::SpawnFailed {
            pane_session_id,
            error: error.to_string(),
            resource_exhausted,
        });
    }
}

/// Locate `(tab_idx, pane_id)` for a pane session id across all tabs.
/// Pure over the tab slice so the spawn-wiring tests run without an event
/// loop / App.
pub(crate) fn find_pane_location(
    tabs: &[Tab],
    pane_session_id: u64,
) -> Option<(usize, weft_core::pane_layout::PaneId)> {
    tabs.iter().enumerate().find_map(|(tab_idx, tab)| {
        tab.panes()
            .find(|(id, _)| id.0 == pane_session_id)
            .map(|(id, _)| (tab_idx, id))
    })
}

/// Adoption-target kernel: locate the pane AND verify it can still adopt
/// (gone / mid-close / already-adopted panes all reject). `None` = the
/// `ForkedPty` must be dropped (idempotent drop — dropping the master fd
/// SIGHUPs the child; see the module doc).
pub(crate) fn spawn_adopt_target(
    sessions: &crate::app_state::SessionManager,
    pane_session_id: u64,
) -> Option<(usize, weft_core::pane_layout::PaneId)> {
    let (tab_idx, pane_id) = find_pane_location(sessions.tabs(), pane_session_id)?;
    sessions
        .tab(tab_idx)
        .and_then(|tab| tab.pane(pane_id))
        .filter(|pane| pane.can_adopt_spawn())?;
    Some((tab_idx, pane_id))
}

/// Failure-path kernel (R2 verdict: SpawnFailed closes the owning tab):
/// remove the tab owning `pane_session_id` from the session list. Returns
/// `(removed_idx, was_last)`; `None` (no change) when no tab owns the pane
/// — the same idempotent drop as adoption. Pure over SessionManager so the
/// close wiring is testable without an event loop / App.
pub(crate) fn spawn_failure_tab_close(
    sessions: &mut crate::app_state::SessionManager,
    pane_session_id: u64,
) -> Option<(usize, bool)> {
    let (tab_idx, _) = find_pane_location(sessions.tabs(), pane_session_id)?;
    let was_last = sessions.remove_dead(tab_idx);
    Some((tab_idx, was_last))
}

/// v1.14.3 (T3) resource-exhaustion keep-kernel: locate the failed
/// placeholder's pane, clear its `pending_spawn` (防后续误 adopt — the fork
/// will never complete, and `can_adopt_spawn` must never accept this pane
/// again), and hand back the owning tab index + pane id + the diagnostic
/// text. The tab STAYS — including the last-tab case (应用不退出; the old
/// `close_tab_effects` → Exit tail is gone for this branch). Pure over
/// SessionManager so the keep wiring is testable without an event loop / App;
/// the caller does `pane_mut → lock_terminal → Terminal::process` +
/// `request_redraw`.
pub(crate) fn spawn_failure_resource_keep(
    sessions: &mut crate::app_state::SessionManager,
    pane_session_id: u64,
) -> Option<(usize, weft_core::pane_layout::PaneId, &'static str)> {
    let (tab_idx, pane_id) = find_pane_location(sessions.tabs(), pane_session_id)?;
    let pane = sessions.tab_mut(tab_idx)?.pane_mut(pane_id)?;
    pane.pending_spawn = None;
    Some((tab_idx, pane_id, RESOURCE_EXHAUSTED_DIAGNOSTIC))
}

impl Pane {
    /// B3-4: buffer user input while the PTY is pending. Always returns
    /// `data.len()` — deliberately NOT the buffered byte count: the input
    /// path (paste truncation toast included) counts dropped bytes as
    /// accepted, which is the intended semantics. Over the cap the incoming
    /// bytes are dropped with a trace (the already-buffered prefix is kept
    /// and delivered at adoption).
    pub(crate) fn buffer_pending_stdin(&mut self, data: &[u8]) -> usize {
        let room = PENDING_STDIN_CAP.saturating_sub(self.pending_stdin.len());
        let accepted = room.min(data.len());
        if accepted < data.len() {
            tracing::warn!(
                pane_session_id = self.pane_session_id,
                dropped = data.len() - accepted,
                cap = PENDING_STDIN_CAP,
                "pending stdin cap exceeded; dropping input"
            );
        }
        self.pending_stdin.extend_from_slice(&data[..accepted]);
        data.len()
    }

    /// Adoption eligibility (checked on the main thread BEFORE building the
    /// worker wake): still a placeholder, no PTY installed yet, and not
    /// mid-close — `begin_close` already dropped this pane's PTY, and
    /// installing a fresh one would resurrect a dying pane for one
    /// watchdog cycle. Any `false` drops the `ForkedPty` (cleanup per the
    /// module doc).
    pub(crate) fn can_adopt_spawn(&self) -> bool {
        self.pty.is_none()
            && self.pending_spawn.is_some()
            && self.close_deadline.is_none()
            && !self.close_settled
    }

    /// Adopt a completed fork (MAIN THREAD — `start_read_loop` is the only
    /// legal `tokio::spawn` site). Installs the PTY + writer, spawns this
    /// pane's parse worker (the exact wiring the old `Pane::spawn` ran),
    /// and flushes stdin buffered during the placeholder window. Returns
    /// `false` when the pane turned out non-adoptable — the caller's
    /// `ForkedPty` was already consumed/dropped.
    pub(crate) fn install_spawned_pty(
        &mut self,
        forked: ForkedPty,
        wake: crate::app::parse_worker::WakeFn,
    ) -> bool {
        if self.pty.is_some() {
            tracing::debug!(
                pane_session_id = self.pane_session_id,
                "adopt: pty already installed"
            );
            return false;
        }
        let Some(request) = self.pending_spawn.take() else {
            tracing::debug!(
                pane_session_id = self.pane_session_id,
                "adopt: no pending spawn"
            );
            return false;
        };
        let mut pty = forked.start_read_loop(request.mouse_suppress);
        // D2 split: the receive half belongs to the parse worker.
        let event_rx = pty
            .take_event_rx()
            .expect("a freshly adopted Pty holds its receiver");
        let writer = pty.writer();
        let Some(terminal) = self.terminal_arc_for_adopt() else {
            // Unreachable for a placeholder (it always builds a terminal);
            // dropping the PTY here is still the safe cleanup.
            tracing::error!(
                pane_session_id = self.pane_session_id,
                "adopt: pane has no terminal; dropping pty"
            );
            return false;
        };
        let had_output = Arc::clone(&self.had_output);
        crate::app::parse_worker::spawn(
            self.pane_session_id,
            event_rx,
            terminal,
            Arc::clone(&writer),
            self.msg_tx.clone(),
            had_output,
            wake,
        );
        // B3-4: flush what the user typed while the fork was in flight.
        if !self.pending_stdin.is_empty() {
            let buffered = std::mem::take(&mut self.pending_stdin);
            tracing::debug!(
                pane_session_id = self.pane_session_id,
                bytes = buffered.len(),
                "flushing pending stdin after adopt"
            );
            let _ = writer.write_sync_reported(&buffered);
        }
        self.pty = Some(pty);
        self.writer = Some(writer);
        true
    }
}

impl App {
    /// `AppEvent::SpawnCompleted` — main-thread adoption (plan §3-B3-3):
    /// locate the pane by `pane_session_id` → `start_read_loop` → the
    /// pending RESIZE_COMMIT queue drains on the next frame (a resize
    /// queued while the PTY was pending stays in `pending_pty_resize` —
    /// `apply_winsize_ioctl` kept retrying — so the EXISTING queue
    /// semantics apply it; the redraw below prompts the drain) → flush
    /// buffered stdin (inside install) → B2 launch_cwd backfill.
    pub(crate) fn handle_spawn_completed(&mut self, pane_session_id: u64, forked: ForkedPty) {
        let Some((tab_idx, pane_id)) = spawn_adopt_target(&self.sessions, pane_session_id) else {
            // Pane gone (closed, or the app is exiting), mid-close, or
            // already adopted: drop the fd with the event — PTY master
            // close = SIGHUP/EOF = expected cleanup.
            tracing::debug!(
                pane_session_id,
                "spawn completed for a gone/non-adoptable pane; dropping pty fd"
            );
            return;
        };
        let tab = self
            .sessions
            .tab_mut(tab_idx)
            .expect("location checked above");
        let tab_session_id = tab.session_id;
        let pane = tab.pane_mut(pane_id).expect("location checked above");
        // B2 backfill value rides the request — read before install
        // consumes it.
        let cwd_fallback = pane
            .pending_spawn
            .as_ref()
            .and_then(|request| request.cwd_fallback.clone());
        let wake: crate::app::parse_worker::WakeFn = {
            let proxy = self.proxy.clone();
            Box::new(move || {
                let _ = proxy.send_event(AppEvent::Wake);
            })
        };
        if !pane.install_spawned_pty(forked, wake) {
            return;
        }
        // B2 (plan §3): tabs opened without an inherited cwd show "Tab N"
        // until the first OSC 7 — backfill the title from the spawn
        // request's cwd (set-once: a restore/workspace fallback already on
        // the pane always wins). The write is main-thread-only, invisible
        // to the tracker generation → invalidate that one cached title.
        if let Some(cwd) = cwd_fallback {
            pane.set_restored_cwd_fallback(Some(cwd));
            self.invalidate_tab_title(tab_session_id);
        }
        // Drain the RESIZE_COMMIT queue promptly (comment above).
        self.request_redraw();
    }

    /// `AppEvent::SpawnFailed` — R2 review verdict (BEHAVIOR CHANGE vs the
    /// pre-B3 `Self::empty()` empty-pane fallback): CLOSE the tab that owns
    /// the placeholder. A PTY-less tab can never come alive, and a dead
    /// placeholder would sit in the strip as a "Tab N" ghost. Mirrors
    /// `close_tab`'s synchronous tail (tween snap, scroll, find refresh,
    /// close effects — including Exit when it was the last tab).
    ///
    /// v1.14.3 (T3, PLAN_v1143 §2) `resource_exhausted` branch — BEHAVIOR
    /// CHANGE: KEEP the placeholder as a diagnostic tab. `Terminal::process`
    /// writes the diagnostic text into its grid (the placeholder has no parse
    /// worker, so the main-thread lock is uncontended; the grid view renders
    /// it because a placeholder's `bootstrap_ready=false` keeps
    /// `show_block_view()` false), then `request_redraw` (SpawnFailed arrives
    /// through the event loop — without a wake the text would wait for the
    /// next frame), the pending spawn is cleared (防误 adopt), and the last
    /// tab does NOT exit the app. Snapshot persistence excludes the
    /// diagnostic tab per-pane (`Tab::is_persist_excluded`).
    pub(crate) fn handle_spawn_failed(
        &mut self,
        pane_session_id: u64,
        error: &str,
        resource_exhausted: bool,
    ) {
        if resource_exhausted {
            let Some((tab_idx, pane_id, diagnostic)) =
                spawn_failure_resource_keep(&mut self.sessions, pane_session_id)
            else {
                tracing::debug!(
                    pane_session_id,
                    "resource-exhausted spawn failed for a gone pane; nothing to diagnose"
                );
                return;
            };
            tracing::warn!(
                pane_session_id,
                tab = tab_idx,
                error,
                diagnostic,
                "PTY pool exhausted; keeping the diagnostic tab (应用继续运行)"
            );
            let pane = self
                .sessions
                .tab_mut(tab_idx)
                .and_then(|tab| tab.pane_mut(pane_id))
                .expect("keep-kernel located this pane");
            if let Some(mut terminal) = pane.lock_terminal() {
                terminal.process(&resource_exhausted_diagnostic_bytes());
            }
            self.request_redraw();
            return;
        }
        let Some((tab_idx, was_last)) =
            spawn_failure_tab_close(&mut self.sessions, pane_session_id)
        else {
            tracing::debug!(
                pane_session_id,
                "spawn failed for a gone pane; nothing to close"
            );
            return;
        };
        tracing::warn!(
            pane_session_id,
            tab = tab_idx,
            error,
            "PTY spawn failed; closing the owning tab"
        );
        if was_last {
            // Last tab: the effects (Exit + PersistTabs) tear the app down,
            // exactly like closing the last tab manually.
            self.drain_effects(crate::effect::close_tab_effects(tab_idx, 0, true));
            return;
        }
        self.cancel_tab_drag();
        self.reset_ime_context("spawn failed");
        let new_active = self.sessions.active_idx();
        self.tab_bar.hovered_tab = None;
        self.clear_tab_bar_tweens();
        self.clamp_tab_scroll();
        self.scroll_active_tab_into_view();
        self.refresh_find_for_active_tab();
        self.drain_effects(crate::effect::close_tab_effects(tab_idx, new_active, false));
    }
}

// v1.14.3 (T3): classification / resource-keep / capture-timing tests live in
// the gate-exempt sibling module (repo test-module convention,
// block_component/tests.rs precedent) — this file stays within its budget.
#[cfg(test)]
#[path = "pane_spawn/tests.rs"]
mod resource_tests;
