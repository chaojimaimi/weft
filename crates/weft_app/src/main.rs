// App orchestration; runtime callbacks and macOS integration live in modules.
//! Weft v1.0 "Weave" — Metal GPU-rendered terminal emulator
//!
//! Full pipeline: PTY → VT parser → Grid → Metal renderer
//! Features: scrollback, selection, clipboard, CJK, mouse, IME, shell integration
#[cfg(test)]
mod acceptance_snapshots;
mod accessibility;
mod accessibility_actions;
mod accessibility_model;
// v1.8 AI integration — local Ollama only. Wired into the app shell in
// v1.8.0 (App holds an `AiState`, drained via `poll_ai_results`).
mod ai;
// v1.8.2: Block diagnose controller — bridges block view ↔ AiState::spawn_diagnose.
mod ai_block_controller;
mod alloc_probe;
// v1.10.21: alt-screen wheel routing + peek entry gate (pure logic).
mod alt_peek;
// v1.10.21: alt-screen wheel dispatch (route() wiring; controller stays lean).
mod alt_wheel_controller;
mod app;
mod app_runtime;
mod app_state;
mod block_actions;
mod block_component;
mod close_confirmation;
mod completion_component;
mod completion_worker;
mod config_controller;
mod config_state;
mod context_menu_component;
// v1.11.13 (PLAN_v11113 §M1): graphic Dock progress bar (OSC 9;4) —
// DockVisual pipeline + bar synthesis drawing.
mod dock_progress;
mod editor_controller;
mod effect;
mod event_replay;
mod find_component;
mod find_controller;
mod find_worker;
mod frame_trace;
mod geometry_controller;
mod glyph;
mod ime;
mod ime_event_controller;
mod input_router;
mod layout;
mod lifecycle_controller;
mod macos_alert;
mod macos_file_dialog;
mod recovery_restore;
// v1.11.12 (PLAN_v11112 M-D): pointer-parameterized pure decision core for
// the system-appearance query (macos_system.rs sits at the 800-line ceiling).
mod macos_appearance;
// v1.11.5 (PLAN_v1115 §M4): native notification sink (UNUserNotificationCenter
// + bundle-identity hard gate).
mod macos_notifications;
mod macos_system;
mod macos_window;
mod macos_zoom;
mod menu;
mod mouse_controller;
mod mouse_press_controller;
mod mouse_protocol_controller;
// v1.11.5 (PLAN_v1115 §M5): pure notify / OSC52-deny decision logic.
mod notify_policy;
mod overlay;
mod paint;
mod palette_activation;
mod palette_component;
mod palette_controller;
mod palette_form;
mod palette_search_worker;
mod palette_state;
mod pane;
mod panel_component;
mod panel_controller;
mod panel_scrollbar;
mod performance_probe;
mod profiles_controller;
mod recovery_controller;
mod redraw_controller;
mod redraw_gates;
mod renderer;
mod runbook_controller;
mod scene;
mod scroll_input;
mod scrollbar_component;
mod selection;
mod settings_component;
mod settings_controller;
mod settings_validation;
mod smart_select_controller;
mod snapshot_persistence;
mod tab;
mod tab_bar_component;
mod tab_drag_controller;
mod terminal_geometry;
mod transfer_controller;
mod ui_tokens;
mod window_event_controller;
mod workspace_controller;
// Allocation forensics gate — see `alloc_probe` for env vars / threshold.
#[global_allocator]
static GLOBAL_ALLOC: alloc_probe::ProbeAllocator = alloc_probe::ProbeAllocator;
use app_state::{
    ConfigState, ContextMenu, DragState, DragTarget, FindState, InteractionState, NoteEditorState,
    PanelState, SessionManager, SettingsState, TabBarState, WindowRuntimeState,
};
use block_component::block_content_metrics_with_cache;
use effect::Effect;
use macos_system::{
    clipboard_copy, clipboard_paste, load_window_icon, open_url, reveal_path_in_finder,
    scan_path_bins, set_dock_icon, system_appearance_is_dark, system_increase_contrast,
    system_reduce_motion,
};
use macos_window::configure_titlebar;
use paint::overlays::FindDrawState;
use paint::tab_bar::TabBarDrawState;
use paint::ui_helpers::{block_matches_query, panel_filtered_count, visible_panel_rows};
use palette_state::{
    BuiltinCmd, CreateStep, PaletteEntry, PaletteState, PaletteSubMode, WorkflowForm,
};
use renderer::MetalRenderer;
use std::sync::atomic::Ordering;
use tab::{Tab, TuiScrollResolution};
use terminal_geometry::{dimensions_for_renderer, terminal_layout_for_renderer, TerminalLayout};
use weft_core::blocks::{BlockId, ShellPhase};
use weft_core::complete::CompletePosition;
use weft_core::config::{Action, Config};
use weft_core::input::{KeyCode, Modifiers, MouseAction, MouseButton, MouseProtocol};
use weft_core::persistence::BlockStore;
use weft_core::selection::{BlockSelAnchor, BlockViewRowKind, GridPos, SelectionMode};
use weft_core::vt::Terminal;

use tracing::{info, warn};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::PhysicalKey;
use winit::window::{Window, WindowAttributes};
// v1.5.2: MainThreadMarker is required by the macOS file panel wrappers
// (NSOpenPanel/NSSavePanel must run on the main thread). Re-exported here
// so `transfer_controller` can request it via `use super::*`.
use objc2_foundation::MainThreadMarker;

// Re-export helpers used by other modules so they can call `crate::foo()`.
pub(crate) use app::helpers::{
    chord_label, first_run_welcome, is_command_position, resolve_text_char, seed_workflows,
    shell_integration_env, strip_prompt_prefix, weft_cache_dir, word_at,
};
// v1.11.13 (PLAN_v11113 §M5): the labels moved to the menu scene component;
// the re-export keeps every `crate::CONTEXT_MENU_ITEMS` consumer unchanged.
pub(crate) use context_menu_component::CONTEXT_MENU_ITEMS;

// ── Messages between threads ─────────────────────────────────────────

pub(crate) enum AppMsg {
    PtyOutput(Vec<u8>),
    PtyExit(Result<i32, String>),
}

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
}

// ── Application ──────────────────────────────────────────────────────

struct App {
    window: Option<Window>,
    renderer: Option<MetalRenderer>,
    /// v0.9 H1: per-tab session state. The active tab is `tabs[active_tab]`.
    /// Currently always has exactly one tab; Stage 2 adds Cmd+T/W multi-tab.
    sessions: SessionManager,
    /// v0.9 W1+: index of the tab currently hovered by the mouse, or `None`
    /// when the cursor is outside the tab bar. Drives the Warp-style
    /// hover-to-show close "×" button. Reset on tab close/switch and on
    /// `CursorLeft` (mouse leaves the window).
    tab_bar: TabBarState,
    /// v1.2: horizontal scroll offset of the tab bar in physical pixels.
    /// When tabs overflow the window width, this lets the user scroll
    /// left/right via arrows, wheel, or trackpad. Clamped to
    /// [0, total_tab_width - visible_width] each frame.
    window_runtime: WindowRuntimeState,
    interaction: InteractionState,
    /// Proxy used by background threads (PTY reader, blink timer) to wake the
    /// event loop without a vsync busy-loop.
    proxy: EventLoopProxy<AppEvent>,
    screen_exit_watchdog_pending: std::sync::Arc<std::sync::atomic::AtomicBool>,
    config_state: ConfigState,
    /// Whether the command-history sidebar panel is shown.
    panel: PanelState,
    /// v0.9 W2: block currently highlighted in the terminal because the user
    /// clicked its row in the history panel. The renderer draws an accent
    /// border around this block. Cleared after 1.5s.
    // ── Command Palette (v0.7) ────────────────────────────────────────
    palette: PaletteState,

    find: FindState,

    // ── Settings panel (v1.0 S1, Cmd+,) ───────────────────────────────
    /// Whether the Settings overlay is open.
    settings: SettingsState,
    accessibility: accessibility::AccessibilityBridge,
    performance_probe: performance_probe::PerformanceProbe,
    /// Frame-trace state (WARP_REFERENCE R3 task 6). Monotonic frame id
    /// stamped on each render; the GPU-completion receiver drains
    /// `add_completed_handler` messages posted from Metal internal threads.
    frame_id: u64,
    frame_trace_enabled: bool,
    gpu_completion_rx: std::sync::mpsc::Receiver<frame_trace::FrameGpuComplete>,
    /// v1.0 H4: set to true when the user closes the last tab — the main
    /// event loop checks this and calls `event_loop.exit()`.
    should_exit: bool,
    /// v1.6.3: Crash recovery controller. Manages debounced snapshot
    /// writes, clean-shutdown markers, and startup detection.
    recovery: recovery_controller::RecoveryController,
    // ── v1.11.5 (PLAN_v1115 §M2): notification / OSC 52 plumbing ────────
    /// Window focus state, maintained by the `WindowEvent::Focused` arm
    /// (window_event_controller.rs). Initial `true` — conservative: a
    /// not-yet-focused launch window posts fewer notifications, never more.
    window_focused: bool,
    /// Notify throttle shared by block-completion and remote (OSC 9/777)
    /// notifications: at most one per `NOTIFY_MIN_GAP` (1s).
    notify_limiter: crate::notify_policy::RateLimiter,
    /// OSC 9;4 Dock badge debounce (200 ms latest-wins; Clear immediate).
    dock_badge_debounce: crate::app::ui_events::DockBadgeDebounce,
    /// v1.11.5 (PLAN_v1115 §M4): the process's notification sink (built in
    /// `resumed` after the bundle-identity gate; NoopSink outside .app).
    notification_sink: Box<dyn crate::macos_notifications::NotificationSink>,
    /// v1.11.5 (PLAN_v1115 §M3): parked OSC 52 read request — single slot
    /// (H-i). `Park` carries the seq handed to the modal and the PaneId the
    /// answer must reach (tab indices shift; pane ids don't, D-g).
    pending_osc52_read: Option<crate::app::ui_events::Osc52ReadPark>,
    /// v1.11.5 (PLAN_v1115 D-c): after a user deny, read requests inside
    /// the 30 s window are treated as deny without prompting again.
    osc52_deny_cooldown: crate::notify_policy::DenyCooldown,
    /// v1.10.23: The recovery snapshot detected at startup, parked while
    /// the deferred recovery prompt is on screen. Consumed exactly once by
    /// `App::apply_recovery_choice` when `AppEvent::RecoveryChosen`
    /// arrives; `None` means no prompt is in flight (a stray choice event
    /// is ignored). See docs/FIX_RECOVERY_MODAL_SPIN.md.
    pending_recovery: Option<weft_core::recovery::RecoverySnapshot>,
    /// v1.11.1 (PLAN_v1111 §4.4): a large/dangerous paste parked while its
    /// deferred confirmation dialog is on screen. Consumed exactly once by
    /// `App::apply_paste_decision` when `AppEvent::PasteDecided` arrives;
    /// `None` means no prompt is in flight (a stray decision event is a
    /// logged no-op).
    pending_paste_confirm: Option<crate::app::effect_dispatch::PendingPaste>,
    /// v1.11.1 (PLAN_v1111 §4.4): session-wide "always allow" granted from
    /// the paste dialog. Exempts BOTH risk classes and resets only when the
    /// process exits — deliberately never persisted.
    paste_allow_for_session: bool,
    /// v1.7.1: Main-thread search index for upsert/delete (index maintenance).
    /// The background PaletteSearchWorker owns its own SearchIndex for queries.
    search_index: Option<weft_core::search::SearchIndex>,
    /// v1.7.3-C: Inline note editor for block annotations. When open,
    /// keyboard input is captured before overlay routing.
    note_editor: NoteEditorState,
    completion_worker: completion_worker::CompletionWorker,
    runbook_worker: runbook_controller::RunbookWorker,
    /// v1.7.3-C / v1.11 audit (PLAN_audit_fix_batch3 C3): Arc-shared with the
    /// renderer so the per-frame handoff is a refcount bump, not a set clone.
    bookmarked_blocks: std::sync::Arc<std::collections::HashSet<BlockId>>,
    /// v1.8: Local AI state (Ollama-only). Holds the config snapshot,
    /// optional backend, and result channel. Background tokio tasks
    /// communicate via crossbeam-channel; drained in `poll_ai_results`.
    ai_state: ai::AiState,
    /// v1.8.2: Per-block AI diagnose state. Keyed by BlockId. An entry
    /// exists when a diagnose request is in flight or a result is being
    /// shown. Removed when the user closes the panel or the block is
    /// deleted.
    block_diagnose_state: std::collections::HashMap<BlockId, crate::app_state::BlockDiagnoseState>,
    /// v1.8.3: Cached Ollama model list from the last `/api/tags` refresh.
    /// Empty until the user clicks "Test Connection" in Settings. Drives
    /// the LocalAi tab's model dropdown.
    ai_models: Vec<ai::client::TagModel>,
    /// v1.8.3: Connection status for the Settings LocalAi tab. Updated by
    /// `poll_ai_results` when an `AiResultEvent::ModelsRefreshed` event arrives.
    ai_connection_status: crate::app_state::AiConnectionStatus,
    /// v1.8.3: In-flight `/api/tags` request id, if any. Used to correlate
    /// `AiResultEvent::ModelsRefreshed` with the Settings "Test Connection"
    /// button. `None` when no refresh is pending.
    ai_models_request_id: Option<u64>,
    /// T14 (PLAN_v11217 §3.9): when the first block-prune may run — the App
    /// construction instant; the gate requires 10s of elapsed time to stay
    /// out of the cold-start measurement window.
    block_prune_arm: std::time::Instant,
    /// T14: last prune trigger time (set at spawn time on the main thread);
    /// `None` until the first prune — the 24h gate in
    /// `run_tabs_autosave_tick` re-arms from it.
    last_block_prune: Option<std::time::Instant>,
    /// T14: re-entrancy guard for the background prune thread — swap
    /// false→true on the main thread before spawning, cleared by the thread
    /// when the pass finishes (success, failure, or busy skip).
    block_prune_in_flight: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl App {
    /// Immutable borrow of the active tab.
    fn tab(&self) -> &Tab {
        self.sessions.active()
    }

    /// Mutable borrow of the active tab.
    fn tab_mut(&mut self) -> &mut Tab {
        self.sessions.active_mut()
    }

    fn new(proxy: EventLoopProxy<AppEvent>) -> Self {
        // v1.5.0: Use `load_resolved` so the source/effective split is
        // preserved (profile overrides are not flattened into the base on
        // save). On any error (missing file, parse error, profile error)
        // we fall back to defaults — matching the legacy `Config::load()`
        // startup behavior so a broken config never blocks app launch.
        let path_bins = scan_path_bins();
        // v1.11.12 (PLAN_v11112 M-B): PATH-scan phase boundary — the scan
        // spawns the login shell (1500ms deadline) and is the top "+63ms"
        // cold-start suspect (architect P1-3); without this boundary its
        // cost blurs into the config phase.
        performance_probe::report_phase(performance_probe::StartupPhase::PathScan);
        let config_state = match weft_core::config::load_resolved() {
            Ok(loaded) => {
                info!(
                    theme = %loaded.effective.theme.name,
                    font = %loaded.effective.font.family,
                    size = loaded.effective.font.size,
                    active_profile = ?loaded.effective.active_profile,
                    fingerprint = loaded.fingerprint,
                    "config loaded"
                );
                ConfigState::from_loaded(loaded, path_bins)
            }
            Err(e) => {
                // Missing file is the common case (fresh install) — log at
                // debug. Parse / profile errors are louder so the user
                // knows why their config didn't take effect.
                if matches!(e, weft_core::config::ConfigLoadError::NoPath) {
                    tracing::debug!("no config path; using defaults");
                } else {
                    tracing::warn!(error = %e, "config load failed; using defaults");
                }
                ConfigState::new(Config::default(), path_bins)
            }
        };
        // v1.11.12 (PLAN_v11112 M-B): config phase boundary — `ConfigState`
        // is fully built here (the PATH scan above is NOT part of it).
        performance_probe::report_phase(performance_probe::StartupPhase::Config);
        let probe = performance_probe::PerformanceProbe::from_env();
        // M6-d (PLAN_M6 §三): the frame-trace line's output gate widens from
        // probe-only to probe OR the M4.1 `WEFT_TRACE_CHANNELS=1` channel.
        // The probe's warmup/sample/auto-exit logic still runs only under
        // `WEFT_GUI_PERF_PROBE=1` — a channels-only run emits frame lines
        // forever and never exits, which is what makes an instrumented
        // interactive drag possible.
        let frame_trace_enabled = frame_trace::trace_enabled(probe.enabled());
        let completion_proxy = proxy.clone();
        let runbook_proxy = proxy.clone();
        // v1.8.7: AI waker — wakes the event loop when background AI tasks
        // send a result, so poll_ai_results runs promptly. Without this,
        // results sit in the channel until the next unrelated event.
        let ai_proxy = proxy.clone();
        // v1.8: Snapshot the AI config before `config_state` is moved into
        // the struct initializer below. `AiState` owns its own copy so it can
        // keep driving requests even while the user edits other settings.
        let ai_config_snapshot = config_state.config.ai.clone();
        Self {
            window: None,
            renderer: None,
            sessions: SessionManager::new(),
            tab_bar: TabBarState::default(),
            window_runtime: WindowRuntimeState::new(),
            interaction: InteractionState::new(),
            find: FindState::new(proxy.clone()),
            proxy,
            screen_exit_watchdog_pending: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
                false,
            )),
            config_state,
            panel: PanelState::default(),
            palette: PaletteState::new(),
            settings: SettingsState::new(),
            accessibility: accessibility::AccessibilityBridge::default(),
            performance_probe: probe,
            frame_id: 0,
            // Frame trace follows the M6-d output gate (probe OR
            // WEFT_TRACE_CHANNELS): only pay the instrumentation cost when a
            // capture switch is active. Idle production runs keep the
            // recorder in its disabled no-op mode.
            frame_trace_enabled,
            gpu_completion_rx: frame_trace::gpu_completion_rx(),
            should_exit: false,
            recovery: recovery_controller::RecoveryController::new(weft_cache_dir().as_deref()),
            window_focused: true,
            notify_limiter: crate::notify_policy::RateLimiter::new(
                crate::notify_policy::NOTIFY_MIN_GAP,
            ),
            dock_badge_debounce: crate::app::ui_events::DockBadgeDebounce::default(),
            // v1.11.5: the sink is replaced by `build_sink` in `resumed`
            // (needs a MainThreadMarker there); the placeholder is safe —
            // it drops everything with a debug trail.
            notification_sink: Box::new(crate::macos_notifications::NoopSink),
            pending_osc52_read: None,
            osc52_deny_cooldown: crate::notify_policy::DenyCooldown::new(
                crate::notify_policy::OSC52_DENY_COOLDOWN,
            ),
            pending_recovery: None,
            pending_paste_confirm: None,
            paste_allow_for_session: false,
            search_index: None,
            note_editor: NoteEditorState::default(),
            completion_worker: completion_worker::CompletionWorker::new(move |generation| {
                // v1.11.11 (M-C): the waker now carries the completed
                // request's generation — kept in the debug log only;
                // AppEvent::Wake stays payload-less (runbook/AI workers keep
                // the Fn() form, asymmetry documented in PROGRESS).
                tracing::debug!(generation, "completion worker wake");
                let _ = completion_proxy.send_event(AppEvent::Wake);
            }),
            runbook_worker: runbook_controller::RunbookWorker::new(move || {
                let _ = runbook_proxy.send_event(AppEvent::Wake);
            }),
            bookmarked_blocks: std::sync::Arc::new(std::collections::HashSet::new()),
            ai_state: ai::AiState::new_with_waker(
                ai_config_snapshot,
                Some(std::sync::Arc::new(move || {
                    let _ = ai_proxy.send_event(AppEvent::Wake);
                })),
            ),
            block_diagnose_state: std::collections::HashMap::new(),
            ai_models: Vec::new(),
            ai_connection_status: crate::app_state::AiConnectionStatus::Idle,
            ai_models_request_id: None,
            block_prune_arm: std::time::Instant::now(),
            last_block_prune: None,
            block_prune_in_flight: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    fn spawn_pty(&mut self, rows: usize, cols: usize) {
        let mut tab = Tab::new(
            rows,
            cols,
            self.config_state.config.scrollback.lines,
            &self.proxy,
            None,
        );
        // v1.11.2 X4: propagate the block retention cap to the fresh pane.
        crate::config_controller::apply_blocks_retained_limit(
            &mut tab,
            self.config_state.config.blocks.retained_limit,
        );
        // PLAN_v11217 §3.5 (T4): propagate the configured output cap too.
        crate::config_controller::apply_blocks_output_cap(
            &mut tab,
            self.config_state.config.blocks.output_cap_mib,
        );
        // v1.11.7 (P2-3): inject the user's TUI render tier — the core default
        // is Classic; the factory default here is `noninteractive`.
        crate::config_controller::apply_tui_render_mode(
            &mut tab,
            self.config_state.config.experimental.tui_render_mode,
        );
        // v1.0 V13: On first launch, inject a welcome banner via PTY.
        // The printf is prefixed with a space (HIST_IGNORE_SPACE keeps it
        // out of zsh history). The marker file is created in
        // first_run_welcome() so this only fires once ever.
        if let Some(cmd) = first_run_welcome() {
            if let Some(p) = tab.pty.as_ref() {
                let _ = p.write_sync(cmd.as_bytes());
            }
        }
        self.sessions.push_tab(tab);
    }

    /// Non-blocking drain of PTY events into channel. Drains ALL tabs per
    /// frame (v0.9 H1 decision: background tabs keep their PTY buffers
    /// flushed so switching to them is instant; only the active tab is
    /// rendered).
    fn pump_pty(&mut self) {
        for tab in self.sessions.tabs_mut() {
            tab.pump_pty();
        }
    }

    fn process_messages(&mut self) -> bool {
        let mut any_redraw = false;
        let mut had_pty_output = false;
        let mut deferred_local_scroll = 0_i32;
        let mut drained_blocks: Vec<weft_core::blocks::Block> = Vec::new();
        // FIX_background_pane_pump §2.4: events are (source pane, event)
        // pairs — every pane is drained now, so the OSC 52 read path can
        // route its reply back to the pane that asked.
        let mut drained_ui_events: Vec<(weft_core::pane_layout::PaneId, weft_core::vt::UiEvent)> =
            Vec::new();
        let mut exit_requested = false;
        for i in 0..self.sessions.len() {
            let (alive, drained, need_redraw, ui_events) =
                self.sessions.tabs_mut()[i].process_messages();
            // Collect final blocks before handling a shell exit.
            drained_blocks.extend(drained);
            // v1.11.5 (PLAN_v1115 §M2): app-facing ui events (OSC 52 / 9 /
            // 777), source-pane tagged — dispatch after the loop, once tab
            // borrows are released.
            drained_ui_events.extend(ui_events);
            if !alive {
                // Exit the app only when the last shell exits; Cmd+W is separate.
                let dead_session_id = self.sessions.tab(i).map(|tab| tab.session_id);
                let menu_belongs_to_dead_session = dead_session_id.is_some_and(|session_id| {
                    self.interaction
                        .context_menu
                        .as_ref()
                        .is_some_and(|menu| menu.belongs_to_session(session_id))
                });
                if menu_belongs_to_dead_session {
                    self.take_context_menu("context menu owner shell exited");
                }
                // v1.12.24 (N-2): the dying tab's final snapshot must land in the tabs
                // DB BEFORE removal — after remove_dead no save ever runs on the
                // should_exit tail (saving empty tabs would wipe the table), so the
                // 1 Hz autosave's ≤1s lag was the last command's only loss window.
                self.persist_tabs_snapshot_now();
                let is_last = self.sessions.remove_dead(i);
                if is_last {
                    exit_requested = true;
                    break;
                }
                info!(
                    closed = i,
                    active = self.sessions.active_idx(),
                    "tab shell exited"
                );
                break;
            }
            if let Some(resolution) = self.sessions.tabs_mut()[i].resolve_pending_tui_scroll() {
                match resolution {
                    TuiScrollResolution::PtyBytes(bytes) => {
                        if let Some(tab) = self.sessions.tab_mut(i) {
                            if let Err(e) = tab.write_user_input(&bytes) {
                                warn!(error = %e, tab = i, "failed to replay queued TUI scroll");
                            }
                        }
                    }
                    TuiScrollResolution::LocalRows(rows) if i == self.sessions.active_idx() => {
                        deferred_local_scroll =
                            deferred_local_scroll.saturating_add(rows).clamp(-100, 100);
                    }
                    TuiScrollResolution::LocalRows(_) => {}
                }
                any_redraw = true;
            }
            if need_redraw {
                any_redraw = true;
                had_pty_output = true;
            }
        }
        // v1.11.15 (FIX B, PLAN_v11115_EXIT_RACE_MOUSE_LEAK §2): when the
        // dead tab was the LAST one, `remove_dead` has already emptied
        // `tabs` by this point — the UiEvents collected above came from a
        // session that no longer exists, and the dispatch arms deref the
        // active tab. Drop them at the source (#4 panic entry).
        // Review P3: this guard also precedes `scroll_local_view` (active
        // tab deref) as pure defense against future drain restructuring.
        if self.sessions.tabs().is_empty() {
            tracing::debug!(
                dropped = drained_ui_events.len(),
                "dropping stale ui events — last session exited"
            );
            drained_ui_events.clear();
        } else if deferred_local_scroll != 0 {
            self.scroll_local_view(deferred_local_scroll);
        }
        // v1.11.5 (PLAN_v1115 §M2): app-facing ui events (OSC 52 clipboard,
        // OSC 9/777 notify, OSC 9;4 Dock progress) — dispatched after all
        // tab borrows are released. Each arm gates against config + state;
        // the sinks land in later modules (M3 read prompt, M4 notify sink,
        // M7 Dock badge). FIX_background_pane_pump §2.4: the events carry
        // their source pane, so the OSC 52 read reply routes to the asking
        // pane instead of assuming the active one.
        if !drained_ui_events.is_empty() {
            self.dispatch_ui_events(drained_ui_events);
        }
        // v1.11.5 (PLAN_v1115 §M5): finished command blocks → completion
        // notifications (threshold + focus gate + rate limiter inside).
        if !drained_blocks.is_empty() {
            self.dispatch_block_completion_notifications(&drained_blocks);
        }
        // v1.11.5: land a debounced Dock badge whose window elapsed (a
        // stalled OSC 9;4 stream still reaches its final value ~200ms late).
        self.flush_dock_badge();
        let effects = effect::process_message_effects(exit_requested, drained_blocks, any_redraw);
        self.drain_effects(effects);
        had_pty_output
    }

    fn request_redraw(&self) {
        if let (Some(window), Some(_renderer)) = (&self.window, &self.renderer) {
            window.request_redraw();
        }
    }
}

fn main() {
    performance_probe::start_startup_clock();
    app_runtime::install_runtime_diagnostics();
    info!("Starting Weft v1.0 \"Weave\"");

    // Create a tokio runtime for PTY async operations.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .thread_name("weft-tokio")
        .enable_all()
        .build()
        .expect("Failed to create tokio runtime");
    let _guard = rt.enter();

    let event_loop = EventLoop::<AppEvent>::with_user_event().build().unwrap();
    let proxy = event_loop.create_proxy();
    crate::macos_zoom::stash_resize_wake_proxy(proxy.clone());
    // v1.11.13 (PLAN_v11113 §M2): install the UN notification delegate
    // BEFORE winit's didFinishLaunching (runs inside run_app) so a
    // cold-start notification click reaches the app (bundle-identity gate
    // + exception::catch inside; no-op for dev binaries).
    crate::macos_notifications::install_early_delegate(&proxy);
    let mut app = App::new(proxy);
    // A run-loop error falls through to the hard exit below as well -- the
    // old destructor-order stall is worse than a lost panic message here.
    // v1.12.23 audit batch 1: an error exit must not masquerade as success —
    // exit 1 (still skipping the destructor long tail, see below).
    if let Err(e) = event_loop.run_app(&mut app) {
        tracing::error!(?e, "event loop terminated with error");
        std::process::exit(1);
    }
    // FIX (field run, v1.12.6): everything that matters (block persistence,
    // tab snapshots, the recovery snapshot, the clean-exit marker) was
    // already written by the teardown before the loop stopped, and every
    // PTY was signalled. Skip the long tail of destructors: the tokio
    // blocking pool would otherwise wait for a PTY waitpid thread whose
    // child can outlive us (a full-screen TUI), which is the sampled
    // "still running after close" state. Orphaned children are reaped by
    // launchd.
    std::process::exit(0);
}
