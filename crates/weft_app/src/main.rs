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
mod app_runtime;
mod app_state;
mod block_component;
mod completion_component;
mod context_menu_component;
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
mod macos_system;
mod macos_window;
mod menu;
mod mouse_controller;
mod mouse_press_controller;
mod mouse_protocol_controller;
mod overlay;
mod paint;
mod palette_component;
mod palette_controller;
mod palette_state;
mod panel_component;
mod panel_controller;
mod panel_scrollbar;
mod performance_probe;
mod redraw_controller;
mod renderer;
mod scene;
mod scroll_input;
mod scrollbar_component;
mod settings_component;
mod settings_controller;
mod settings_validation;
mod snapshot_persistence;
mod tab;
mod tab_bar_component;
mod terminal_geometry;
mod ui_tokens;
mod window_event_controller;
use app_state::{
    ConfigState, ContextMenu, DragState, DragTarget, FindState, InteractionState, PanelState,
    SessionManager, SettingsState, TabBarState, WindowRuntimeState,
};
use block_component::block_content_metrics_with_cache;
use effect::Effect;
use input_router::{OverlayInputContext, OverlayInputOwner};
use macos_system::{
    clipboard_copy, clipboard_paste, load_window_icon, open_url, scan_path_bins, set_dock_icon,
    system_appearance_is_dark, system_increase_contrast, system_reduce_motion,
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
use weft_core::complete::{complete, CompleteCtx, CompletePosition};
use weft_core::config::{Action, Config};
use weft_core::input::{encode_paste, KeyCode, Modifiers, MouseAction, MouseButton, MouseProtocol};
use weft_core::persistence::BlockStore;
use weft_core::selection::{BlockViewPos, BlockViewRowKind, GridPos, SelectionMode};
use weft_core::shell::Integration;
use weft_core::vt::Terminal;

use tracing::{info, warn};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode as WinitKeyCode, PhysicalKey};
use winit::window::{Window, WindowAttributes};

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
    /// AppKit accessibility element invoked its default Press action.
    AccessibilityPress {
        generation: u64,
        node_id: u64,
    },
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
}

/// Context menu item labels.
const CONTEXT_MENU_ITEMS: &[(&str, &str); 4] = &[
    ("Copy Command", "copy_command"),
    ("Copy Output", "copy_output"),
    ("Toggle Fold", "toggle_fold"),
    // v0.9 W4: send the block's command to the input box for re-editing
    // (Warp-style "rerun" — user can tweak parameters before pressing Enter).
    ("Send to Input", "send_to_input"),
];

/// Action triggered by clicking a button in the find popup. Produced by
/// `App::find_button_at` from the renderer's stored hit-test rects.
enum FindButtonAction {
    /// Click the ".*" toggle — flip regex mode (visual only).
    ToggleRegex,
    /// Click the "Aa" toggle — flip case-sensitive search.
    ToggleCase,
    /// Click the "↓" button — jump to next match.
    Next,
    /// Click the "↑" button — jump to previous match.
    Prev,
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
        let config = Config::load();
        info!(
            theme = %config.theme.name,
            font = %config.font.family,
            size = config.font.size,
            "config loaded"
        );
        let config_state = ConfigState::new(config, scan_path_bins());
        let probe = performance_probe::PerformanceProbe::from_env();
        let frame_trace_enabled = probe.enabled();
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
            // Frame trace follows the probe gate: only pay the instrumentation
            // cost when the acceptance probe is active. Idle production runs
            // keep the recorder in its disabled no-op mode.
            frame_trace_enabled,
            gpu_completion_rx: frame_trace::gpu_completion_rx(),
            should_exit: false,
        }
    }

    fn spawn_pty(&mut self, rows: usize, cols: usize) {
        let tab = Tab::new(
            rows,
            cols,
            self.config_state.config.scrollback.lines,
            &self.proxy,
            None,
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
        let mut exit_requested = false;
        for i in 0..self.sessions.len() {
            let (alive, drained, need_redraw) = self.sessions.tabs_mut()[i].process_messages();
            // Collect final blocks before handling a shell exit.
            drained_blocks.extend(drained);
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
        if deferred_local_scroll != 0 {
            self.scroll_local_view(deferred_local_scroll);
        }
        let effects = effect::process_message_effects(exit_requested, drained_blocks, any_redraw);
        self.drain_effects(effects);
        had_pty_output
    }

    fn request_redraw(&self) {
        if let (Some(window), Some(_renderer)) = (&self.window, &self.renderer) {
            window.request_redraw();
        }
    }

    fn drain_effects(&mut self, effects: impl IntoIterator<Item = Effect>) {
        for effect in effects {
            match effect {
                Effect::WritePty { tab, bytes } => {
                    if let Some(session) = self.sessions.tab_mut(tab) {
                        let session_id = session.session_id;
                        let input_seq = session.input_seq();
                        let (screen_owner, settle_state, history_snapshot_due) = session
                            .terminal
                            .as_ref()
                            .map(|t| (t.screen_owner(), t.settle_state(), t.history_snapshot_due()))
                            .unwrap_or((
                                weft_core::vt::ScreenOwner::Shell,
                                weft_core::vt::SettleState::Idle,
                                false,
                            ));
                        tracing::debug!(
                            session_id,
                            input_seq,
                            tab,
                            bytes_len = bytes.len(),
                            %screen_owner,
                            %settle_state,
                            history_snapshot_due,
                            delivery = "pty-write",
                            "effect dispatched",
                        );
                        if let Err(error) = session.write_user_input(&bytes) {
                            warn!(%error, tab, "failed to apply PTY write effect");
                        }
                    }
                }
                Effect::InterruptPty { tab } => {
                    if let Some(session) = self.sessions.tab_mut(tab) {
                        let session_id = session.session_id;
                        let input_seq = session.input_seq();
                        let (screen_owner, settle_state, history_snapshot_due) = session
                            .terminal
                            .as_ref()
                            .map(|t| (t.screen_owner(), t.settle_state(), t.history_snapshot_due()))
                            .unwrap_or((
                                weft_core::vt::ScreenOwner::Shell,
                                weft_core::vt::SettleState::Idle,
                                false,
                            ));
                        tracing::debug!(
                            session_id,
                            input_seq,
                            tab,
                            %screen_owner,
                            %settle_state,
                            history_snapshot_due,
                            delivery = "pty-etx",
                            "interrupt effect dispatched",
                        );
                        let delivered = session.interrupt_pty();
                        if !delivered {
                            warn!(
                                tab,
                                "interrupt delivery failed; preserving PTY output and phase"
                            );
                        }
                    }
                }
                Effect::ResizePty { tab, rows, cols } => {
                    self.apply_pty_resize_effect(tab, rows, cols);
                }
                Effect::CopyClipboard { text } => clipboard_copy(&text),
                Effect::PersistTabs => self.save_all_tabs(),
                Effect::PersistBlocks { blocks } => self.persist_blocks(&blocks),
                Effect::Paste { tab } => self.apply_paste(tab),
                Effect::Exit => self.should_exit = true,
                Effect::TabClosed {
                    removed_idx,
                    new_active,
                    is_last,
                } => {
                    // close_tab already applied the synchronous mutations;
                    // retain this effect as the post-close extension point.
                    info!(removed_idx, new_active, is_last, "tab closed effect");
                }
                Effect::TabSwitched { new_idx, prev_idx } => {
                    // Synchronous mutation (sessions.next/prev, IME reset,
                    // find refresh, tab-bar scroll) already ran in
                    // `next_tab`/`prev_tab`. Extension point for future
                    // post-switch consumers.
                    info!(new_idx, prev_idx, "tab switched effect");
                }
                Effect::RequestRedraw => self.request_redraw(),
            }
        }
    }

    /// Persist a batch of drained command blocks to the BlockStore. Best-effort:
    /// each failure is logged but does not abort the remaining inserts. Extracted
    /// from `process_messages` so the same logic serves the `PersistBlocks` effect.
    fn persist_blocks(&self, blocks: &[weft_core::blocks::Block]) {
        let Some(store) = self.sessions.block_store() else {
            return;
        };
        for block in blocks {
            if let Err(e) = store.insert(block) {
                warn!(error = %e, "failed to persist block");
            }
        }
    }

    /// Read the system clipboard (synchronous — NSPasteboard has AppKit main
    /// thread affinity) and apply the text to `tab`. Editor mode inserts into
    /// the prompt buffer; Passthrough forwards to the PTY with optional
    /// bracketed-paste wrapping. `Effect::Paste` is the system-clipboard
    /// entry point; find-bar Cmd+V can pass already-read text directly.
    fn apply_paste(&mut self, tab: usize) {
        let Some(text) = clipboard_paste() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        self.apply_paste_text(tab, &text);
    }

    /// Apply already-read `text` to `tab` according to its input mode. Split
    /// out so callers that already hold the clipboard text (e.g. the find bar
    /// Cmd+V path) can skip the NSPasteboard round-trip.
    fn apply_paste_text(&mut self, tab: usize, text: &str) {
        let mode = self
            .sessions
            .tab(tab)
            .and_then(|t| t.terminal.as_ref())
            .map(|t| t.effective_input_mode())
            .unwrap_or(weft_core::input::InputMode::Passthrough);

        if mode == weft_core::input::InputMode::Editor {
            // Preserve pasted newlines explicitly because insert_char rejects
            // controls; drop CR to normalize external CRLF text.
            if let Some(t) = self
                .sessions
                .tab_mut(tab)
                .and_then(|tab| tab.terminal.as_mut())
            {
                let buf = &mut t.editor_mut().buffer;
                for c in text.chars() {
                    if c == '\n' {
                        buf.split_newline();
                    } else if c != '\r' {
                        buf.insert_char(c);
                    }
                }
            }
            self.request_redraw();
        } else {
            // Passthrough: forward to the PTY.
            let bracketed = self
                .sessions
                .tab(tab)
                .and_then(|t| t.terminal.as_ref())
                .map(|t| t.bracketed_paste)
                .unwrap_or(false);
            let bytes = encode_paste(text, bracketed);
            if let Some(session) = self.sessions.tab_mut(tab) {
                if let Err(e) = session.write_user_input(&bytes) {
                    warn!(error = %e, tab, "failed to paste to PTY");
                }
            }
        }
    }

    fn overlay_input_owner(&self) -> Option<OverlayInputOwner> {
        OverlayInputOwner::resolve(OverlayInputContext {
            palette_open: self.palette.open,
            settings_open: self.settings.open,
            find_open: self.find.open,
            context_menu_open: self.interaction.context_menu.is_some(),
            panel_search_focused: self.panel.open && self.panel.search_focused,
        })
    }

    fn handle_key_event(
        &mut self,
        key_code: WinitKeyCode,
        mods: winit::event::Modifiers,
        text: Option<&str>,
    ) {
        let Some(key) = event_replay::map_winit_key(key_code) else {
            return;
        };

        let mut m = Modifiers::empty();
        if mods.state().shift_key() {
            m |= Modifiers::SHIFT;
        }
        if mods.state().control_key() {
            m |= Modifiers::CONTROL;
        }
        if mods.state().alt_key() {
            m |= Modifiers::ALT;
        }
        if mods.state().super_key() {
            m |= Modifiers::SUPER;
        }

        let bound_action = self.config_state.keybindings.lookup(key, m);
        let has_terminal = self
            .sessions
            .tab(self.sessions.active_idx())
            .is_some_and(|tab| tab.terminal.is_some());
        match input_router::route_keyboard_entry(
            !self.sessions.is_empty(),
            has_terminal,
            bound_action,
        ) {
            input_router::KeyboardEntryRoute::Action(action) => {
                self.execute_action(action);
                return;
            }
            input_router::KeyboardEntryRoute::Consume => return,
            input_router::KeyboardEntryRoute::Session => {}
        }

        // Configurable keybindings: resolve (key, mods) → action. If it maps to
        // a weft action (copy/paste/scroll/reload), dispatch and consume; else
        // fall through to encoding the key for the PTY.
        //
        // v0.9 fix: when the Find bar is open, intercept Paste (Cmd+V) and
        // SelectAll (Cmd+A) so they target the find query, not the shell
        // editor. Other Cmd chords (Cmd+R regex toggle, Cmd+I case toggle)
        // are handled inside `handle_find_key` below.
        if self.find.open
            && m.contains(Modifiers::SUPER)
            && matches!(key, KeyCode::Char('v') | KeyCode::Char('a'))
        {
            if key == KeyCode::Char('v') {
                if let Some(text) = clipboard_paste() {
                    self.find.query.push_str(&text);
                    self.arm_find_refresh();
                }
                return;
            }
            if key == KeyCode::Char('a') {
                // Select-all in the find bar: clear and re-type from clipboard?
                // For now, just signal "select all" by moving cursor to end —
                // the find bar is single-line with no selection model. No-op.
                return;
            }
        }
        if let Some(action) = bound_action {
            if self.execute_action(action) {
                return;
            }
        }

        let overlay_owner = self.overlay_input_owner();
        let overlay_consumed = match overlay_owner {
            Some(OverlayInputOwner::Palette) => self.handle_palette_key(key, m, text),
            Some(OverlayInputOwner::Settings) => self.handle_settings_key(key, m, text),
            Some(OverlayInputOwner::Find) => self.handle_find_key(key, m, text),
            Some(OverlayInputOwner::ContextMenu) => self.handle_context_menu_key(key, m),
            Some(OverlayInputOwner::PanelSearch) => self.handle_panel_key(key, m),
            None => false,
        };
        if overlay_consumed {
            return;
        }

        // Editor takeover: at the prompt with integration ready, keys drive the
        // input-box editor instead of being forwarded to the PTY. Enter submits
        // (writes the command); Shift+Enter grows the box. Drops back to
        // passthrough automatically in alt-screen / command-running / SSH.
        let input_mode = self
            .tab()
            .terminal
            .as_ref()
            .map(|t| t.effective_input_mode())
            .unwrap_or(weft_core::input::InputMode::Passthrough);
        if input_mode == weft_core::input::InputMode::Editor {
            let prev_lines = self
                .tab()
                .terminal
                .as_ref()
                .map(|t| t.editor().line_count())
                .unwrap_or(1);
            let consumed = self.handle_editor_key(key, m, text);
            let new_lines = self
                .tab()
                .terminal
                .as_ref()
                .map(|t| t.editor().line_count())
                .unwrap_or(1);
            if new_lines != prev_lines {
                self.recompute_layout();
            }
            if consumed {
                self.request_redraw();
                return;
            }
        }

        let app_cursor_keys = self
            .tab()
            .terminal
            .as_ref()
            .map(|t| t.app_cursor_keys())
            .unwrap_or(false);
        self.tab_mut().input_handler.app_cursor_keys = app_cursor_keys;

        let bytes = ime::encode_passthrough_key(&self.tab().input_handler, key, m, text);
        // Diagnostic (set RUST_LOG=weft_app=debug to see): the exact bytes we
        // send for each key, including whether DECCKM/app-cursor mode is on.
        let input_seq = self.tab_mut().next_input_seq();
        tracing::debug!(
            session_id = self.tab().session_id,
            input_seq,
            ?key,
            ?m,
            app_cursor_keys = self.tab().input_handler.app_cursor_keys,
            ?bytes,
            "key → pty"
        );
        let effects = effect::passthrough_key_effects(self.sessions.active_idx(), bytes);
        self.drain_effects(effects);
    }

    /// Dispatch a weft action resolved from a keybinding. Returns true if the
    /// key was consumed (must not be forwarded to the PTY).
    fn execute_action(&mut self, action: Action) -> bool {
        if input_router::route_global_action(self.interaction.context_menu.is_some())
            == input_router::GlobalActionOverlayRoute::DismissContextMenu
        {
            self.take_context_menu("global action dispatched");
        }
        if input_router::route_session_action(!self.sessions.is_empty(), action)
            == input_router::SessionInputRoute::Consume
        {
            return true;
        }
        match action {
            Action::Copy => {
                self.copy_selection();
                true
            }
            Action::Paste => {
                self.drain_effects(vec![Effect::Paste {
                    tab: self.sessions.active_idx(),
                }]);
                true
            }
            Action::ReloadConfig => {
                self.reload_config();
                true
            }
            Action::ScrollPageUp
            | Action::ScrollPageDown
            | Action::ScrollLineUp
            | Action::ScrollLineDown
            | Action::ScrollToTop
            | Action::ScrollToBottom => {
                self.scroll_action(action);
                true
            }
            Action::ToggleBlockPanel => {
                if self.panel.open {
                    self.panel.close();
                } else {
                    self.panel.open = true;
                    // Fresh search/selection each time the panel opens.
                    self.panel.query.clear();
                    self.panel.clear_transient_selection();
                    self.panel.search_focused = false;
                }
                // v0.9 W5: resize grid for sidebar so the terminal content
                // reflows beside the panel instead of being covered by it.
                self.recompute_layout();
                self.request_redraw();
                true
            }
            Action::ToggleCommandPalette => {
                self.reset_ime_context("command palette toggled");
                if self.palette.open {
                    self.palette.close();
                    self.clear_prev_focus_if_no_modal();
                } else {
                    // v0.9 fix: opening the palette closes the find bar (and
                    // vice versa) so only one modal owns keyboard input at a
                    // time. Without this, Cmd+F then Cmd+P leaves both
                    // popups open and keystrokes go to the wrong one.
                    // v1.0 S1: also close the Settings panel.
                    self.close_find();
                    self.close_settings();
                    // F4: save the current focus so it can be restored when
                    // the palette closes.
                    self.save_focus_for_modal(crate::scene::FocusId::PaletteQuery);
                    self.palette.open_search();
                    self.refresh_palette_results();
                }
                self.request_redraw();
                true
            }
            Action::ZoomIn | Action::ZoomOut | Action::ZoomReset => {
                self.zoom_action(action);
                true
            }
            Action::FindInGrid => {
                self.reset_ime_context("find toggled");
                if self.find.open {
                    self.find.close();
                    self.clear_prev_focus_if_no_modal();
                } else {
                    // v0.9 fix: opening find closes the palette (see above).
                    // v1.0 S1: also close the Settings panel.
                    self.close_palette();
                    self.close_settings();
                    // F4: save the current focus so it can be restored when
                    // the find bar closes.
                    self.save_focus_for_modal(crate::scene::FocusId::FindQuery);
                    self.find.reset_query();
                    self.find.open = true;
                }
                self.request_redraw();
                true
            }
            Action::ToggleTheme => {
                self.toggle_theme();
                true
            }
            Action::NewTab => {
                self.new_tab();
                self.drain_effects(vec![Effect::PersistTabs]);
                true
            }
            Action::CloseTab => {
                let effects = self.close_tab();
                self.drain_effects(effects);
                true
            }
            Action::NextTab => {
                let effects = self.next_tab();
                self.drain_effects(effects);
                true
            }
            Action::PrevTab => {
                let effects = self.prev_tab();
                self.drain_effects(effects);
                true
            }
            Action::ToggleSettings => {
                if self.settings.open {
                    self.close_settings();
                } else {
                    self.reset_ime_context("settings opened");
                    // Mutual exclusion: close other modals.
                    self.close_palette();
                    self.close_find();
                    self.save_focus_for_modal(crate::scene::FocusId::Settings);
                    self.open_settings();
                }
                self.request_redraw();
                true
            }
        }
    }

    /// Handle a key while the panel search box is focused. Returns true if
    /// consumed (search typing / arrow nav / expand / unfocus). Modifier
    /// chords fall through (returns false) so keybindings still work.
    fn handle_panel_key(&mut self, key: KeyCode, mods: Modifiers) -> bool {
        // Let cmd/ctrl/alt chords pass through to keybindings / PTY.
        if mods.intersects(Modifiers::SUPER | Modifiers::CONTROL | Modifiers::ALT) {
            return false;
        }
        match key {
            // v0.9 fix: Esc unfocuses the search box instead of closing the
            // panel. The panel itself closes via the Cmd+Shift+B keybinding
            // or by clicking outside the sidebar.
            KeyCode::Escape => {
                self.panel.search_focused = false;
                self.request_redraw();
                true
            }
            KeyCode::Up => {
                self.panel.selection = self.panel.selection.saturating_sub(1);
                self.clamp_panel_selection();
                self.request_redraw();
                true
            }
            KeyCode::Down => {
                self.panel.selection = self.panel.selection.saturating_add(1);
                self.clamp_panel_selection();
                self.request_redraw();
                true
            }
            KeyCode::Backspace => {
                self.panel.query.pop();
                self.clamp_panel_scroll();
                self.clamp_panel_selection();
                self.request_redraw();
                true
            }
            KeyCode::Enter => {
                // v0.9 fix: send the selected command to the prompt input
                // (Warp-style: Enter on a history entry reruns the command).
                self.send_panel_selection_to_input();
                true
            }
            KeyCode::Char(c) if !c.is_control() => {
                self.panel.query.push(c);
                self.clamp_panel_scroll();
                self.clamp_panel_selection();
                self.request_redraw();
                true
            }
            _ => false,
        }
    }

    // ── Command Palette (v0.7) ──────────────────────────────────────────

    /// v0.9: close the find bar and reset its state. Used when another modal
    /// (palette, panel, …) opens so only one owns keyboard input.
    fn close_find(&mut self) {
        if !self.find.open {
            return;
        }
        self.reset_ime_context("find closed");
        self.find.close();
        self.clear_prev_focus_if_no_modal();
    }

    /// v0.9: close the command palette and reset its state. Used when
    /// another modal (find bar, …) opens so only one owns keyboard input.
    fn close_palette(&mut self) {
        if !self.palette.open {
            return;
        }
        self.reset_ime_context("palette closed");
        self.palette.close();
        self.clear_prev_focus_if_no_modal();
    }

    /// v1.0 S1: close the Settings panel, discarding any unsaved draft
    /// changes. Used when another modal opens so only one owns keyboard
    /// input.
    fn close_settings(&mut self) {
        if !self.settings.open {
            return;
        }
        self.reset_ime_context("settings closed");
        self.settings.close();
        self.clear_prev_focus_if_no_modal();
    }

    // ── F4: Focus restore helpers ─────────────────────────────────────
    // These track the keyboard focus before a modal surface opens so it can
    // be restored (visually / for accessibility) when the modal closes. The
    // actual keyboard routing is implicit — when an overlay closes, its key
    // handler stops capturing, so input naturally returns to the editor. The
    // `prev_focus` field is for future semantic/a11y use.

    /// Compute the current logical [`FocusId`] from the overlay state. Mirrors
    /// the priority in `OverlayInputOwner::resolve`.
    fn compute_current_focus(&self) -> Option<crate::scene::FocusId> {
        let active_tab = self.sessions.active_idx();
        crate::paint::command_surface::compute_current_focus(
            self.palette.open,
            self.settings.open,
            self.find.open,
            self.interaction.context_menu.is_some(),
            self.panel.open && self.panel.search_focused,
            /* editor_active */ true,
            self.sessions
                .tab(active_tab)
                .and_then(|tab| tab.terminal.as_ref())
                .map(|t| t.editor().is_completing())
                .unwrap_or(false),
            active_tab,
        )
    }

    /// Save the current focus before opening a modal. Does not overwrite an
    /// already-saved focus (so a second modal opening on top of the first
    /// preserves the *original* focus).
    fn save_focus_for_modal(&mut self, opening: crate::scene::FocusId) {
        let current = self.compute_current_focus();
        let prev = crate::paint::command_surface::save_focus_for_modal(
            current,
            self.interaction.prev_focus,
            opening,
        );
        self.interaction.prev_focus = prev;
    }

    /// Clear the saved focus when all modals are closed. Called from each
    /// modal's close path.
    fn clear_prev_focus_if_no_modal(&mut self) {
        if !self.palette.open
            && !self.find.open
            && !self.settings.open
            && self.interaction.context_menu.is_none()
        {
            self.interaction.prev_focus = None;
        }
    }

    /// Copy selection to system clipboard.
    ///
    /// Dispatches on the active view: block view copies from the captured
    /// `BlockViewSelection` row snapshot (what the user actually sees), grid
    /// view copies from the terminal Grid. This split fixes the "复制错位"
    /// bug where a grid-coordinate copy landed on the wrong line because the
    /// block view's pitch/scroll/layout don't map 1:1 to grid rows.
    fn copy_selection(&mut self) {
        let text = {
            let tab = self.sessions.active();
            let Some(terminal) = tab.terminal.as_ref() else {
                return;
            };
            // Editor drag-selection takes priority over block/grid selection.
            terminal
                .editor()
                .buffer
                .selected_text()
                .filter(|text| !text.is_empty())
                .or_else(|| {
                    if terminal.show_block_view() {
                        tab.selection_handler.block_view_text()
                    } else {
                        tab.selection_handler.selected_text(terminal.grid())
                    }
                })
        };
        self.drain_effects(effect::copy_clipboard_effects(text));
    }

    /// Update cursor blink state.
    ///
    /// Two independent mechanisms share the `cursor_blink_time` anchor:
    /// - **Grid view**: hard on/off toggle every 530ms (unchanged v0.7 logic).
    /// - **Prompt (Editor mode)**: smooth `sin()` breath over a 2400ms period
    ///   (v0.8 §0.3 signature). The phase advances continuously and wraps at
    ///   2π; the renderer maps it to an alpha curve 0.25↔1.0 + amber glow.
    fn update_cursor_blink(&mut self) {
        // F6: When Reduce Motion is on, freeze the cursor visible (no blink
        // toggle, no breath phase). This mirrors the spinner behavior and
        // ensures a steady, non-distracting caret for motion-sensitive users.
        if self.window_runtime.reduce_motion {
            self.window_runtime.cursor_blink_on = true;
            self.window_runtime.cursor_blink_phase = 0.0;
            return;
        }

        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.window_runtime.cursor_blink_time);

        // Grid-view hard blink: toggle every 530ms (anchor reset on toggle).
        if elapsed >= std::time::Duration::from_millis(530) {
            self.window_runtime.cursor_blink_on = !self.window_runtime.cursor_blink_on;
            self.window_runtime.cursor_blink_time = now;
        }

        // Prompt signature breath: advance phase continuously.
        // Period 2400ms → one full sin() cycle; phase stored in radians.
        const PERIOD_MS: f64 = 2400.0;
        let elapsed_ms = elapsed.as_millis() as f64;
        // Each update advances phase by (elapsed_ms / PERIOD_MS) * 2π.
        let delta = (elapsed_ms / PERIOD_MS) * std::f64::consts::TAU;
        self.window_runtime.cursor_blink_phase += delta as f32;
        // Wrap into [0, 2π) to avoid float drift over long sessions.
        if self.window_runtime.cursor_blink_phase >= std::f32::consts::TAU {
            self.window_runtime.cursor_blink_phase -= std::f32::consts::TAU;
        }
    }

    /// F3-2: Advance the running-command spinner phase based on real elapsed
    /// time. The spinner cycles every 800ms (10 braille glyphs × 80ms each).
    /// When `reduce_motion` is on, the phase is frozen at 0 so the renderer
    /// draws a static `●` instead of animating.
    fn update_spinner(&mut self) {
        if self.window_runtime.reduce_motion {
            self.window_runtime.spinner_phase = 0.0;
            return;
        }
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.window_runtime.spinner_time);
        const SPINNER_PERIOD_MS: f64 = 800.0;
        let elapsed_ms = elapsed.as_millis() as f64;
        let delta = (elapsed_ms / SPINNER_PERIOD_MS) as f32;
        self.window_runtime.spinner_phase += delta;
        if self.window_runtime.spinner_phase >= 1.0 {
            self.window_runtime.spinner_phase -= 1.0;
        }
        self.window_runtime.spinner_time = now;
    }
}

/// Resolve the character to insert for a printable editor key. Prefers the
/// keyboard-layout text (`KeyEvent::text`), which already reflects Shift and
/// the active layout — Shift+A -> 'A', Shift+1 -> '!', etc. Falls back to the
/// physical key's base char (uppercased when Shift is held) only when the text
/// is absent or not a single printable char (some IME configurations omit it),
/// so typed commands keep their case even then.
/// The char-column range `(start, end)` of the word ending at the cursor
/// column `col` (the partial token to complete), or `None` when the cursor
/// sits on whitespace / an empty line. `end` == `col`.
/// Strip prompt artifacts from a captured command string. When a command was
/// captured via `snapshot_command_line()` (passthrough mode, or pre-editor
/// sessions), the grid row includes the shell prompt — e.g.
/// `~/projects/foo ❯ ls -la`. This strips everything up to and including the
/// last prompt marker (❯ ❮ › $ % #) so only the command remains.
///
/// A marker is only recognized when followed by a space (so `$HOME` in a
/// command is not mistaken for a `$` prompt).
fn strip_prompt_prefix(s: &str) -> String {
    // Patterns: "❯ ", "❮ ", "› ", "$ ", "% ", "# " — the marker + space.
    let markers = ["❯ ", "❮ ", "› ", "$ ", "% ", "# "];
    // Find the LAST marker occurrence (prompts may contain `$`/`#` in paths).
    let mut best: Option<usize> = None;
    for marker in &markers {
        let mut search_from = 0;
        while let Some(idx) = s[search_from..].find(marker) {
            best = Some(best.map_or(search_from + idx, |b| b.max(search_from + idx)));
            search_from += idx + marker.len();
        }
    }
    if let Some(idx) = best {
        let after = s[idx..].trim_start_matches(['❯', '❮', '›', '$', '%', '#', ' ']);
        if !after.is_empty() {
            return after.to_string();
        }
    }
    s.to_string()
}

fn word_at(line: &str, col: usize) -> Option<(usize, usize)> {
    let chars: Vec<char> = line.chars().collect();
    if chars.is_empty() {
        return None;
    }
    let end = col.min(chars.len());
    let mut start = end;
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    if start == end {
        return None; // cursor on whitespace
    }
    Some((start, end))
}

/// Whether the word at `word_start` is in command position (line start, or
/// after a shell operator `| & ; > <`). Decides whether the `$PATH` command
/// completion source is consulted.
fn is_command_position(line: &str, word_start: usize) -> bool {
    let chars: Vec<char> = line.chars().collect();
    let mut i = word_start;
    while i > 0 && chars[i - 1].is_whitespace() {
        i -= 1;
    }
    if i == 0 {
        return true;
    }
    matches!(chars[i - 1], '|' | '&' | ';' | '>' | '<')
}

/// Insert built-in workflow templates on first launch (empty DB).
/// Insert built-in workflow templates on first launch (empty DB).
#[allow(clippy::type_complexity)]
fn seed_workflows(store: &weft_core::workflow::WorkflowStore) {
    use weft_core::workflow::{Workflow, WorkflowSource, WorkflowStep, WorkflowVar};

    let seeds: &[(&str, &str, &[&str], &[(bool, &str, &str, bool)])] = &[
        // name, description, commands, vars: (is_default, name, default, required)
        (
            "sync",
            "git pull current branch",
            &["git pull origin $(git branch --show-current)"],
            &[],
        ),
        (
            "dev",
            "start dev server",
            &["cd {{project}} && npm run dev"],
            &[(true, "project", ".", true)],
        ),
        (
            "logs",
            "tail service logs",
            &["tail -f {{file}}"],
            &[(true, "file", "/var/log/system.log", true)],
        ),
        (
            "gst",
            "git status + recent log",
            &["git status -sb", "git log --oneline -5"],
            &[],
        ),
        (
            "dclean",
            "prune dangling docker resources",
            &["docker system prune -f"],
            &[],
        ),
    ];

    for (name, desc, cmds, vars) in seeds {
        let workflow = Workflow {
            id: 0,
            name: (*name).into(),
            description: (*desc).into(),
            steps: cmds
                .iter()
                .map(|c| WorkflowStep {
                    command: (*c).into(),
                })
                .collect(),
            variables: vars
                .iter()
                .map(|(has_default, vname, vdefault, vreq)| WorkflowVar {
                    name: (*vname).into(),
                    description: String::new(),
                    default: if *has_default {
                        Some((*vdefault).into())
                    } else {
                        None
                    },
                    required: *vreq,
                })
                .collect(),
            source: WorkflowSource::Manual,
            use_count: 0,
            last_used_ms: 0,
        };
        if let Err(e) = store.insert(&workflow) {
            warn!(error = %e, workflow = name, "failed to seed workflow");
        }
    }
    info!("seeded {} built-in workflows", seeds.len());
}

fn resolve_text_char(text: Option<&str>, fallback: char, shift: bool) -> char {
    if let Some(s) = text {
        let mut it = s.chars();
        if let (Some(c), None) = (it.next(), it.next()) {
            if !c.is_control() {
                return c;
            }
        }
    }
    if shift {
        fallback.to_ascii_uppercase()
    } else {
        fallback
    }
}

/// v1.0 S1: Format a `(KeyCode, Modifiers)` pair as a human-readable chord
/// string (e.g. "cmd+c", "shift+page_up", "cmd+shift+t"). Used by the
/// Settings panel's Keybindings tab.
fn chord_label(key: weft_core::input::KeyCode, mods: weft_core::input::Modifiers) -> String {
    use weft_core::input::{KeyCode, Modifiers};
    let mut parts: Vec<&str> = Vec::new();
    if mods.contains(Modifiers::SUPER) {
        parts.push("cmd");
    }
    if mods.contains(Modifiers::SHIFT) {
        parts.push("shift");
    }
    if mods.contains(Modifiers::ALT) {
        parts.push("alt");
    }
    if mods.contains(Modifiers::CONTROL) {
        parts.push("ctrl");
    }
    let key_str = match key {
        KeyCode::Char(c) => {
            // Lowercase letters for chord display (cmd+c not cmd+C).
            return {
                let mut s = parts.join("+");
                if !s.is_empty() {
                    s.push('+');
                }
                s.push(c.to_ascii_lowercase());
                s
            };
        }
        KeyCode::Enter => "enter",
        KeyCode::Backspace => "backspace",
        KeyCode::Tab => "tab",
        KeyCode::Escape => "esc",
        KeyCode::Up => "up",
        KeyCode::Down => "down",
        KeyCode::Left => "left",
        KeyCode::Right => "right",
        KeyCode::Home => "home",
        KeyCode::End => "end",
        KeyCode::PageUp => "page_up",
        KeyCode::PageDown => "page_down",
        KeyCode::Delete => "delete",
        _ => "other",
    };
    parts.push(key_str);
    parts.join("+")
}

/// Resolve weft's cache dir: `$XDG_CACHE_HOME/weft`, else `~/.cache/weft`.
/// `None` when neither `XDG_CACHE_HOME` nor `HOME` is set.
fn weft_cache_dir() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("weft"));
        }
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache").join("weft"))
}

/// v1.0 V13: Onboarding — first-run welcome message.
///
/// Detects first launch by checking `~/.config/weft/.first_run`. On first
/// launch, returns a `printf` command string that prints a short welcome
/// banner with core shortcuts. The caller writes this to the PTY right
/// after spawn, so it shows up in the user's first shell session. The
/// `.first_run` marker is created here (not by the caller).
///
/// Returns `None` on subsequent launches or if the config dir can't be
/// resolved (we'd rather skip onboarding than spam the user every launch).
fn first_run_welcome() -> Option<String> {
    use std::path::PathBuf;
    // Resolve config dir: $XDG_CONFIG_HOME/weft or ~/.config/weft
    let dir = if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            PathBuf::from(xdg).join("weft")
        } else {
            return None;
        }
    } else {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config").join("weft"))?
    };
    let marker = dir.join(".first_run");
    if marker.exists() {
        return None;
    }
    // Create marker immediately (best-effort). Even if the printf write
    // fails later, we don't want to re-show the welcome on every launch.
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(&marker, b"1");
    // Leading space + HIST_IGNORE_SPACE (default in zsh) keeps this out of
    // shell history. The printf is one-shot; it doesn't persist anywhere.
    let banner = "\x1b[2m# Welcome to Weft v1.0\x1b[0m\n\
\x1b[2m# Core shortcuts:\x1b[0m\n\
\x1b[2m#   Cmd+T        New tab      Cmd+W  Close tab\x1b[0m\n\
\x1b[2m#   Cmd+Shift+[  Prev tab     Cmd+Shift+]  Next tab\x1b[0m\n\
\x1b[2m#   Cmd+P        Command palette (fuzzy)\x1b[0m\n\
\x1b[2m#   Cmd+F        Find         Cmd+Shift+B  Toggle sidebar\x1b[0m\n\
\x1b[2m#   Cmd+,        Settings     Cmd+Shift+T  Cycle theme\x1b[0m\n\
\x1b[2m# Block view groups commands and output. Type a command and press Enter.\x1b[0m\n";
    // Leading space keeps this out of zsh history (HIST_IGNORE_SPACE default).
    Some(format!(" printf {:?}\n", banner))
}

/// Configure shell integration for the child shell and return env overrides.
///
/// - **zsh:** writes a generated `.zshenv` to `<cache>/zsh/` and points
///   `ZDOTDIR` there. The shell sources our OSC 133 hooks itself — no stdin
///   injection, no echo. The user's real `~/.zshrc` still loads (the generated
///   `.zshenv` restores `ZDOTDIR` first).
/// - **bash:** ships a snippet at `<cache>/bash-integration.sh`; the user opts
///   in with one `source` line in `~/.bashrc` (no clean interactive redirect).
///
/// Returns `KEY=VALUE` overrides to pass to the PTY. Terminal capabilities and
/// locale survive shell-integration setup failures.
pub(crate) fn shell_integration_env(shell: &str) -> Vec<(String, String)> {
    let mut env = app_runtime::terminal_capability_env();

    // v1.0 fix: ensure UTF-8 locale for the child shell. When weft is launched
    // from Finder (.app bundle), the GUI environment typically lacks LANG /
    // LC_CTYPE, so the shell falls back to the `C` locale and tools like `ls`
    // render non-ASCII filenames (中文, etc.) as `?`. Force a UTF-8 locale
    // unless the user already has one set.
    let lang_ok = std::env::var("LANG").is_ok_and(|l| l.contains("UTF-8") || l.contains("utf8"));
    let lc_ctype_ok =
        std::env::var("LC_CTYPE").is_ok_and(|l| l.contains("UTF-8") || l.contains("utf8"));
    if !lang_ok && !lc_ctype_ok {
        // Prefer en_US.UTF-8 (always available on macOS); fall back to C.UTF-8.
        env.push(("LANG".to_string(), "en_US.UTF-8".to_string()));
    }

    let plan = Integration::from_shell(shell);
    if !plan.is_supported() {
        return env;
    }
    let Some(cache_root) = weft_cache_dir() else {
        warn!("HOME/XDG_CACHE_HOME unset — shell integration disabled");
        return env;
    };
    let base_len = env.len();
    let orig_zdotdir = std::env::var("ZDOTDIR").ok();
    env.extend(
        plan.child_env(orig_zdotdir.as_deref())
            .into_iter()
            .map(|(k, v)| (k.to_string(), v)),
    );

    // zsh: write the generated .zshenv and redirect ZDOTDIR at its directory.
    if let Some((redirect_var, file)) = plan.rc_redirect() {
        let dir = cache_root.join("zsh");
        if let Err(e) = std::fs::create_dir_all(&dir)
            .and_then(|_| std::fs::write(dir.join(file.filename), file.body))
        {
            warn!(error = %e, "failed to write zsh integration .zshenv; integration disabled");
            env.truncate(base_len);
            return env;
        }
        env.push((redirect_var.to_string(), dir.to_string_lossy().into_owned()));
    }

    // bash: ship the snippet so users can source it.
    if let Some(snippet) = plan.sourceable_snippet() {
        let path = cache_root.join("bash-integration.sh");
        if let Err(e) =
            std::fs::create_dir_all(&cache_root).and_then(|_| std::fs::write(&path, snippet))
        {
            warn!(error = %e, "failed to write bash integration snippet");
        } else {
            info!(
                path = %path.display(),
                "bash integration snippet written — add to ~/.bashrc: \
                 `[ -n \"$WEFT_SHELL_INTEGRATION\" ] && . \"{}\"`",
                path.display(),
            );
        }
    }

    env
}

fn main() {
    app_runtime::install_runtime_diagnostics();
    info!("Starting Weft v1.0 \"Weave\"");

    // Create a tokio runtime for PTY async operations.
    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
    let _guard = rt.enter();

    let event_loop = EventLoop::<AppEvent>::with_user_event().build().unwrap();
    let proxy = event_loop.create_proxy();
    let mut app = App::new(proxy);
    event_loop.run_app(&mut app).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that mutate `XDG_CONFIG_HOME` — env vars are
    /// process-global, so parallel tests that touch the same var would
    /// clobber each other's values.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn word_at_picks_token_left_of_cursor() {
        assert_eq!(word_at("ls -l", 5), Some((3, 5))); // "-l"
        assert_eq!(word_at("ls", 2), Some((0, 2))); // "ls"
    }

    #[test]
    fn word_at_none_on_whitespace_or_empty() {
        assert_eq!(word_at("ls ", 3), None); // cursor on trailing space
        assert_eq!(word_at("", 0), None);
    }

    #[test]
    fn is_command_position_first_word_or_after_operator() {
        assert!(is_command_position("ls", 0));
        assert!(is_command_position("a | b", 4)); // "b" after pipe
        assert!(!is_command_position("ls -l", 3)); // "-l" is an arg
    }

    #[test]
    fn strip_prompt_prefix_removes_cwd_and_marker() {
        assert_eq!(strip_prompt_prefix("~/projects/foo ❯ ls -la"), "ls -la");
        assert_eq!(strip_prompt_prefix("❯ echo hi"), "echo hi");
    }

    #[test]
    fn strip_prompt_prefix_keeps_plain_commands() {
        assert_eq!(strip_prompt_prefix("git status"), "git status");
        assert_eq!(strip_prompt_prefix("ls"), "ls");
    }

    #[test]
    fn strip_prompt_prefix_handles_root_prompts() {
        assert_eq!(strip_prompt_prefix("# whoami"), "whoami");
        assert_eq!(strip_prompt_prefix("user@host:~$ ls"), "ls");
    }

    #[test]
    fn strip_prompt_prefix_keeps_dollar_in_command() {
        // `$HOME` should NOT be stripped (no space after $, it's part of cmd).
        assert_eq!(strip_prompt_prefix("echo $HOME"), "echo $HOME");
    }

    // ── T5: first_run marker logic + supplementary prompt tests ─────────

    /// Verify the `.first_run` marker existence logic that
    /// `first_run_welcome()` relies on: on a fresh config dir the marker is
    /// absent (→ welcome should show); after the function runs once, the
    /// marker exists (→ welcome should not show again).
    ///
    /// This calls the private `first_run_welcome()` directly (accessible
    /// from the test submodule) with `XDG_CONFIG_HOME` pointed at a unique
    /// temp dir so the user's real config is never touched. The env var is
    /// saved and restored around the test to avoid affecting parallel tests.
    #[test]
    fn first_run_marker_created_on_first_call_only() {
        let _env = ENV_LOCK.lock().unwrap();
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let tmp = std::env::temp_dir().join(format!("weft-first-run-{pid}-{id}"));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let old_xdg = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", &tmp);

        // Before first call: marker should not exist.
        let marker = tmp.join("weft").join(".first_run");
        assert!(!marker.exists(), "marker should not exist before first run");

        // First call: should return a welcome banner and create the marker.
        let first = first_run_welcome();
        assert!(first.is_some(), "first run should return a welcome banner");
        let banner = first.unwrap();
        assert!(
            banner.contains("printf"),
            "banner should be a printf command, got: {banner:?}"
        );
        assert!(
            banner.contains("Welcome"),
            "banner should contain welcome text"
        );
        assert!(marker.exists(), "marker should be created after first run");

        // Second call: marker now exists → should return None.
        let second = first_run_welcome();
        assert!(
            second.is_none(),
            "second run should not return welcome (marker exists)"
        );

        // Restore env and clean up.
        match old_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn strip_prompt_prefix_marker_with_no_command_keeps_input() {
        // When the line is just the marker (no command after it), the
        // function should fall back to the original input rather than
        // returning an empty string.
        let result = strip_prompt_prefix("❯ ");
        // The trimmed-after string is empty → returns the original `s`.
        assert_eq!(result, "❯ ");
    }

    #[test]
    fn strip_prompt_prefix_picks_last_marker_in_line() {
        // When the line contains multiple markers (e.g. a path with `$`),
        // the function picks the LAST one so the command after it is kept.
        // Here `$ ` appears in `a$ b` and again as the real prompt `$ ls`.
        assert_eq!(strip_prompt_prefix("echo a$ b $ ls"), "ls");
    }

    #[test]
    fn word_at_handles_multibyte_boundaries() {
        // word_at operates on chars, so multibyte positions are safe.
        // "héllo" — é is one char (two UTF-8 bytes).
        let line = "héllo";
        // Cursor at end (char index 5).
        assert_eq!(word_at(line, 5), Some((0, 5)));
        // Cursor at char index 2 (the 'l').
        assert_eq!(word_at(line, 2), Some((0, 2)));
    }

    // ── Keybindings tab: chord_label formatting ──────────────────────

    #[test]
    fn chord_label_cmd_plus_char() {
        use weft_core::input::{KeyCode, Modifiers};
        assert_eq!(chord_label(KeyCode::Char('c'), Modifiers::SUPER), "cmd+c");
    }

    #[test]
    fn chord_label_cmd_shift_t() {
        use weft_core::input::{KeyCode, Modifiers};
        assert_eq!(
            chord_label(KeyCode::Char('t'), Modifiers::SUPER | Modifiers::SHIFT),
            "cmd+shift+t"
        );
    }

    #[test]
    fn chord_label_shift_page_up() {
        use weft_core::input::{KeyCode, Modifiers};
        assert_eq!(
            chord_label(KeyCode::PageUp, Modifiers::SHIFT),
            "shift+page_up"
        );
    }

    #[test]
    fn chord_label_bare_enter() {
        use weft_core::input::{KeyCode, Modifiers};
        assert_eq!(chord_label(KeyCode::Enter, Modifiers::empty()), "enter");
    }

    #[test]
    fn chord_label_ctrl_a() {
        use weft_core::input::{KeyCode, Modifiers};
        assert_eq!(
            chord_label(KeyCode::Char('a'), Modifiers::CONTROL),
            "ctrl+a"
        );
    }

    #[test]
    fn chord_label_alt_plus_char() {
        use weft_core::input::{KeyCode, Modifiers};
        assert_eq!(chord_label(KeyCode::Char('x'), Modifiers::ALT), "alt+x");
    }

    // ── F7 context menu: hit-test geometry ───────────────────────────

    #[test]
    fn context_menu_items_count_is_four() {
        assert_eq!(CONTEXT_MENU_ITEMS.len(), 4);
    }

    #[test]
    fn context_menu_actions_are_unique() {
        let actions: Vec<&str> = CONTEXT_MENU_ITEMS.iter().map(|(_, a)| *a).collect();
        let mut sorted = actions.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), actions.len(), "duplicate action strings");
    }

    // ── config mtime hot-reload: external file change is picked up ──
    //
    // The 1Hz mtime poller in `spawn_threads` calls `reload_config()` →
    // `Config::load()` when it detects a change. This test verifies the
    // data source: after an external edit (simulating the user editing
    // config.toml in another editor), `Config::load()` returns the new
    // values. The thread-scheduling layer is not exercised here.

    #[test]
    fn config_load_picks_up_external_theme_change() {
        let _env = ENV_LOCK.lock().unwrap();
        use weft_core::config::Config;
        let tmp = unique_temp_dir("weft-mtime-theme");
        let old_xdg = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", &tmp);

        let cfg_dir = tmp.join("weft");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let cfg_path = cfg_dir.join("config.toml");

        // Initial: theme = weft-warm.
        std::fs::write(&cfg_path, "[theme]\nname = \"weft-warm\"\n").unwrap();
        let first = Config::load();
        assert_eq!(first.theme.name, "weft-warm");

        // External edit: theme → weft-light (simulating user editing the file).
        std::fs::write(&cfg_path, "[theme]\nname = \"weft-light\"\n").unwrap();
        let second = Config::load();
        assert_eq!(second.theme.name, "weft-light");

        restore_xdg(old_xdg, &tmp);
    }

    #[test]
    fn config_load_picks_up_external_font_change() {
        let _env = ENV_LOCK.lock().unwrap();
        use weft_core::config::Config;
        let tmp = unique_temp_dir("weft-mtime-font");
        let old_xdg = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", &tmp);

        let cfg_dir = tmp.join("weft");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let cfg_path = cfg_dir.join("config.toml");

        std::fs::write(&cfg_path, "[font]\nsize = 12.0\n").unwrap();
        let first = Config::load();
        assert!((first.font.size - 12.0).abs() < 1e-6);

        std::fs::write(&cfg_path, "[font]\nsize = 16.0\n").unwrap();
        let second = Config::load();
        assert!((second.font.size - 16.0).abs() < 1e-6);

        restore_xdg(old_xdg, &tmp);
    }

    #[test]
    fn config_load_returns_default_when_file_deleted() {
        let _env = ENV_LOCK.lock().unwrap();
        use weft_core::config::Config;
        let tmp = unique_temp_dir("weft-mtime-del");
        let old_xdg = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", &tmp);

        let cfg_dir = tmp.join("weft");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let cfg_path = cfg_dir.join("config.toml");

        // File exists → load reads it.
        std::fs::write(&cfg_path, "[font]\nsize = 18.0\n").unwrap();
        let with_file = Config::load();
        assert!((with_file.font.size - 18.0).abs() < 1e-6);

        // File deleted → load falls back to defaults.
        std::fs::remove_file(&cfg_path).unwrap();
        let without_file = Config::load();
        assert!(
            (without_file.font.size - 14.0).abs() < 1e-6,
            "default font size"
        );

        restore_xdg(old_xdg, &tmp);
    }

    /// Helper: create a unique temp dir for an env-var-scoped test.
    fn unique_temp_dir(prefix: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let tmp = std::env::temp_dir().join(format!("{prefix}-{pid}-{id}"));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        tmp
    }

    /// Helper: restore XDG_CONFIG_HOME and clean up the temp dir.
    fn restore_xdg(old: Option<std::ffi::OsString>, tmp: &std::path::Path) {
        match old {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        let _ = std::fs::remove_dir_all(tmp);
    }
}
