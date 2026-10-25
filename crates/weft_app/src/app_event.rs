// Cross-thread event payloads for the winit event loop (v1.14.2 B3 备料:
// moved byte-identical out of main.rs — the file sat one variant away from
// its MAIN_RS_MAX ceiling; `crate::AppEvent` paths are unchanged via the
// crate-root re-export).

/// Cross-thread wake-up for the winit event loop.
///
/// Sent by the PTY reader thread (new output) and the cursor-blink timer so
/// the loop redraws only when there is actual work — instead of busy-looping
/// at vsync and pinning a CPU core.
#[derive(Debug)]
pub(crate) enum AppEvent {
    Wake,
    /// Config file changed on disk — reload and re-apply live.
    ConfigReload,
    TabsAutoSave,
    /// v1.1: A native menu item was clicked — dispatch the action on the
    /// main thread (where `App` is borrowed during `user_event`).
    MenuAction(weft_core::config::Action),
    /// Native Quit Weft / Cmd+Q requested a protected application exit.
    QuitRequested,
    /// AppKit accessibility element invoked its default Press action.
    AccessibilityPress {
        generation: u64,
        node_id: u64,
    },
    /// v1.10.23: The crash-recovery prompt (deferred to a main-queue block
    /// so `runModal` never runs inside the winit handler — see
    /// docs/FIX_RECOVERY_MODAL_SPIN.md) closed; the user picked a recovery
    /// path. Handled in `App::apply_recovery_choice`, which consumes
    /// `App.pending_recovery` (a duplicated event is a no-op).
    RecoveryChosen(crate::macos_alert::RecoveryChoice),
    /// v1.11.1 (PLAN_v1111 §3/§4.4): the large-paste confirmation (deferred
    /// to a main-queue block, same FIX_RECOVERY_MODAL_SPIN discipline)
    /// closed. Carries the full three-way choice — Cancel / Once /
    /// AlwaysSession — because a bare bool cannot express "paste once"
    /// versus "discard" (both would be `false`). v1.11.11 (M-B): `seq` pairs
    /// the reply with the exact parked paste that spawned it. Handled in
    /// `App::apply_paste_decision`, which consumes the seq-matched
    /// `App.pending_paste_confirm` exactly once.
    PasteDecided {
        response: crate::macos_alert::PastePromptResponse,
        seq: u64,
    },
    /// v1.11.5 (PLAN_v1115 §M3): the deferred OSC 52 read prompt closed. The
    /// user's answer pairs with the parked request ONLY when `seq` matches
    /// (peek-compare-take — a stale decision never consumes a newer slot).
    Osc52ReadDecided {
        allowed: bool,
        seq: u64,
    },
    /// v1.11.5 (PLAN_v1115 §M6): a posted notification was clicked (default
    /// action). `block_id` routes back to the command block; `None`/stale
    /// ids only front the window.
    NotificationActivated(i64),
    PerformanceProbeStart,
    PerformanceProbeFinish,
    /// v1.13.5 (T16c): background PATH scan done — payload backfills
    /// `ConfigState::path_bins` (async fill semantics in path_scan.rs).
    PathBinsResolved(Vec<String>),
    /// v1.14.2 (B3, PLAN_v1142 §3): a placeholder pane's PTY fork succeeded
    /// on the spawn worker. The payload carries the fully-assembled
    /// [`weft_core::pty::ForkedPty`] (read loop NOT started — `tokio::spawn`
    /// is main-thread-only); the adoption step (`pane_spawn.rs`) starts it
    /// on the main thread. If the pane is gone (closed / app exiting), the
    /// event — and with it the master fd — is simply dropped: closing the
    /// PTY master SIGHUPs the child and the read loop hits EOF, which IS
    /// the expected cleanup (adoption idempotency + exit-period in-flight
    /// semantics are the same drop).
    SpawnCompleted {
        pane_session_id: u64,
        forked: weft_core::pty::ForkedPty,
    },
    /// v1.14.2 (B3): the fork failed on the spawn worker. BEHAVIOR CHANGE
    /// vs the pre-B3 synchronous branch (pane.rs `Self::empty()` kept an
    /// empty pane): the handler CLOSES the tab that owns the placeholder
    /// pane — a permanently PTY-less tab has no way to ever come alive.
    SpawnFailed {
        pane_session_id: u64,
        error: String,
    },
}
