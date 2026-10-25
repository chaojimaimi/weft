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
                        let _ = worker_proxy.send_event(AppEvent::SpawnFailed {
                            pane_session_id,
                            error: error.to_string(),
                        });
                    }
                }
            }));
            if let Err(payload) = worker_outcome {
                let error = fork_panic_message(payload);
                tracing::error!(pane_session_id, %error, "fork worker panicked");
                let _ = worker_proxy.send_event(AppEvent::SpawnFailed {
                    pane_session_id,
                    error,
                });
            }
        });
    if let Err(error) = spawned {
        tracing::error!(pane_session_id, %error, "failed to spawn fork worker thread");
        let _ = proxy.send_event(AppEvent::SpawnFailed {
            pane_session_id,
            error: error.to_string(),
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
    pub(crate) fn handle_spawn_failed(&mut self, pane_session_id: u64, error: &str) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::SessionManager;

    /// A placeholder pane with its spawn request armed (bypasses `spawn` —
    /// that would fire a real fork thread; these tests are wiring-only).
    fn placeholder_with_pending_spawn() -> Pane {
        let mut pane = Pane::placeholder(24, 80, 1000);
        pane.pending_spawn = Some(SpawnRequest::for_new_pane(
            pane.pane_session_id,
            24,
            80,
            None,
            pane.mouse_suppress.clone(),
        ));
        pane
    }

    fn tab_holding(pane: Pane) -> Tab {
        Tab::with_single_pane(pane)
    }

    // ── location / adopt-target kernels ─────────────────────────────────

    #[test]
    fn find_pane_location_truth_table() {
        let pane_a = Pane::placeholder(24, 80, 1000);
        let pane_b = Pane::placeholder(24, 80, 1000);
        let id_a = pane_a.pane_session_id;
        let id_b = pane_b.pane_session_id;
        let mut tabs = vec![tab_holding(pane_a), tab_holding(pane_b)];
        assert_eq!(
            find_pane_location(&tabs, id_a),
            Some((0, weft_core::pane_layout::PaneId(id_a)))
        );
        assert_eq!(
            find_pane_location(&tabs, id_b),
            Some((1, weft_core::pane_layout::PaneId(id_b)))
        );
        assert_eq!(
            find_pane_location(&tabs, 999_999),
            None,
            "unknown id → None"
        );
        tabs.clear();
        assert_eq!(find_pane_location(&tabs, id_a), None);
    }

    #[test]
    fn spawn_adopt_target_rejects_gone_non_placeholder_and_closing_panes() {
        let mut sessions = SessionManager::new();
        // Adoptable placeholder → Some.
        let adoptable = placeholder_with_pending_spawn();
        let id_ok = adoptable.pane_session_id;
        sessions.push_tab(tab_holding(adoptable));
        assert!(spawn_adopt_target(&sessions, id_ok).is_some());

        // Not a placeholder (no pending spawn) → None (idempotent drop).
        let plain = Pane::with_terminal_only(1000);
        let id_plain = plain.pane_session_id;
        sessions.push_tab(tab_holding(plain));
        assert_eq!(
            spawn_adopt_target(&sessions, id_plain),
            None,
            "a non-placeholder pane must not adopt"
        );

        // Mid-close (watchdog stamped) → None.
        let closing = placeholder_with_pending_spawn();
        let id_closing = closing.pane_session_id;
        let mut closing_tab = tab_holding(closing);
        closing_tab.active_mut().begin_close_teardown();
        sessions.push_tab(closing_tab);
        assert_eq!(
            spawn_adopt_target(&sessions, id_closing),
            None,
            "a closing pane must not resurrect via adoption"
        );

        // Unknown id → None.
        assert_eq!(spawn_adopt_target(&sessions, 424_242), None);
    }

    // ── failure-path kernel (失败关 tab) ─────────────────────────────────

    #[test]
    fn spawn_failure_closes_the_owning_tab_and_reports_last() {
        let mut sessions = SessionManager::new();
        let first = Pane::placeholder(24, 80, 1000);
        let first_id = first.pane_session_id;
        sessions.push_tab(tab_holding(first));
        let victim = Pane::placeholder(24, 80, 1000);
        let victim_id = victim.pane_session_id;
        sessions.push_tab(tab_holding(victim));

        // Failing the BACKGROUND tab's pane removes tab 1 only.
        assert_eq!(
            spawn_failure_tab_close(&mut sessions, victim_id),
            Some((1, false)),
            "background failure removes that tab, not the last"
        );
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions.active_idx(), 0);

        // Unknown id → no-op (idempotent).
        assert_eq!(spawn_failure_tab_close(&mut sessions, 777), None);
        assert_eq!(sessions.len(), 1);

        // Failing the LAST remaining tab's pane reports was_last (the
        // caller turns it into the Exit effect).
        assert_eq!(
            spawn_failure_tab_close(&mut sessions, first_id),
            Some((0, true)),
            "last-tab failure must request Exit like a manual last-tab close"
        );
        assert!(sessions.is_empty());
    }

    // ── B3-4: pending stdin buffer ──────────────────────────────────────

    /// B2 (plan §3): the backfill value is captured at OPEN time and only
    /// for the inherited-None case — an inherited cwd needs no fallback
    /// (the child chdirs there and OSC 7 reports it).
    #[test]
    fn spawn_request_captures_cwd_fallback_only_for_inherited_none() {
        let flag = weft_core::input::new_flag();
        let inherited = SpawnRequest::for_new_pane(1, 24, 80, Some("/inherited/dir"), flag.clone());
        assert_eq!(inherited.cwd.as_deref(), Some("/inherited/dir"));
        assert_eq!(
            inherited.cwd_fallback, None,
            "B2 scope: an inherited cwd needs no fallback"
        );
        let none = SpawnRequest::for_new_pane(2, 24, 80, None, flag);
        assert_eq!(none.cwd, None);
        assert_eq!(
            none.cwd_fallback,
            std::env::current_dir()
                .ok()
                .map(|path| path.to_string_lossy().into_owned()),
            "the fallback is the process cwd the child will actually inherit"
        );
    }

    #[test]
    fn pending_stdin_buffers_then_drops_over_the_cap() {
        let mut pane = Pane::placeholder(24, 80, 1000);
        assert_eq!(pane.buffer_pending_stdin(b"hello"), 5);
        assert_eq!(pane.pending_stdin, b"hello");

        // Overflow: the cap is respected, the overflow is dropped, and the
        // call still reports the full length (buffered bytes WILL be
        // delivered at adoption).
        let chunk = vec![b'x'; 64 * 1024];
        assert_eq!(pane.buffer_pending_stdin(&chunk), chunk.len());
        assert_eq!(pane.pending_stdin.len(), 64 * 1024, "cap enforced");
        assert_eq!(pane.buffer_pending_stdin(b"overflow!"), 9);
        assert_eq!(
            pane.pending_stdin.len(),
            64 * 1024,
            "bytes past the cap are dropped, not queued"
        );
    }

    // ── adoption wiring with an injected fake ForkedPty ─────────────────

    /// Independent dup of /dev/null for the fake's master/read/writer fds.
    fn devnull_fd() -> std::os::unix::io::OwnedFd {
        use std::os::unix::io::{AsRawFd, FromRawFd};
        let file = std::fs::File::open("/dev/null").expect("open /dev/null");
        let duped = unsafe { nix::libc::dup(file.as_raw_fd()) };
        assert!(duped >= 0, "dup failed");
        drop(file); // the dup is independent of the original fd
        unsafe { std::os::unix::io::OwnedFd::from_raw_fd(duped) }
    }

    /// Fake `ForkedPty`: /dev/null master (EOFs instantly), a real
    /// already-exiting child (`/usr/bin/true`) so the read loop's waitpid
    /// and the Pty Drop SIGHUP both behave. NO fork happens here.
    // The Child handle is deliberately never `wait()`ed — the read loop's
    // `waitpid` reaps it (zombie only for the µs before that).
    #[allow(clippy::zombie_processes)]
    fn fake_forked() -> ForkedPty {
        let child = std::process::Command::new("/usr/bin/true")
            .spawn()
            .expect("spawn /usr/bin/true");
        ForkedPty::from_parts(
            devnull_fd(),
            nix::unistd::Pid::from_raw(child.id() as nix::libc::pid_t),
            devnull_fd(),
            devnull_fd(),
        )
    }

    #[test]
    fn adopt_installs_pty_starts_read_loop_and_flushes_pending_stdin() {
        // start_read_loop contains the ONLY tokio::spawn; the test enters a
        // runtime exactly like main.rs does (the adoption contract).
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let _guard = rt.enter();

        let mut pane = placeholder_with_pending_spawn();
        // A resize queued during the placeholder window must survive the
        // adoption — the RESIZE_COMMIT queue semantics apply it later.
        pane.pending_pty_resize = Some((30, 100));
        assert_eq!(pane.pending_stdin.len(), 0);
        pane.buffer_pending_stdin(b"early keystrokes");

        let installed = pane.install_spawned_pty(fake_forked(), Box::new(|| {}));
        assert!(installed, "adoptable placeholder must install");
        assert!(pane.pty.is_some(), "pty installed");
        assert!(pane.writer.is_some(), "writer installed");
        assert!(pane.pending_spawn.is_none(), "request consumed");
        assert!(
            pane.pending_stdin.is_empty(),
            "buffered stdin flushed to the writer at adoption"
        );
        assert_eq!(
            pane.pending_pty_resize,
            Some((30, 100)),
            "pending resize stays queued (existing RESIZE_COMMIT semantics)"
        );

        // Second adopt attempt is rejected (already adopted) — the
        // defensive arm of the idempotency contract.
        assert!(!pane.install_spawned_pty(fake_forked(), Box::new(|| {})));

        // Read loop started: /dev/null EOFs immediately → Exit flows through
        // the parse worker as PtyExited. Drive the current-thread runtime
        // (tasks only run under block_on) and poll the control channel.
        let exited = (0..100).any(|_| {
            rt.block_on(async { tokio::time::sleep(std::time::Duration::from_millis(10)).await });
            matches!(pane.msg_rx.try_recv(), Ok(crate::AppMsg::PtyExited(_)))
        });
        assert!(exited, "read loop must run and report the child exit");
    }
}
