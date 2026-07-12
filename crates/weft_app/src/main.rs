//! Weft v1.0 "Weave" — Metal GPU-rendered terminal emulator
//!
//! Full pipeline: PTY → VT parser → Grid → Metal renderer
//! Features: scrollback, selection, clipboard, CJK, mouse, IME, shell integration

mod app_state;
mod completion_component;
mod context_menu_component;
mod editor_controller;
mod effect;
mod find_component;
mod find_controller;
mod find_worker;
mod geometry_controller;
mod glyph;
mod ime;
mod ime_event_controller;
mod input_router;
mod layout;
mod lifecycle_controller;
mod menu;
mod mouse_controller;
mod mouse_press_controller;
mod overlay;
mod palette_component;
mod palette_controller;
mod palette_state;
mod panel_controller;
mod redraw_controller;
mod renderer;
mod scene;
mod settings_controller;
mod tab;
mod tab_bar_component;
mod terminal_geometry;
mod ui_tokens;
mod window_event_controller;

use app_state::{
    ConfigState, ContextMenu, DragState, DragTarget, FindState, InteractionState, PanelState,
    SessionState, SettingsState, TabBarState, WindowRuntimeState,
};
use effect::Effect;
use input_router::{OverlayInputContext, OverlayInputOwner};
use palette_state::{
    BuiltinCmd, CreateStep, PaletteEntry, PaletteState, PaletteSubMode, WorkflowForm,
};
use renderer::{
    block_matches_query, configure_titlebar, visible_panel_rows, FindDrawState, MetalRenderer,
    TabBarDrawState,
};
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
    /// v1.0 H4: Periodic 30s timer fired — persist tab snapshots.
    TabsAutoSave,
    /// v1.1: A native menu item was clicked — dispatch the action on the
    /// main thread (where `App` is borrowed during `user_event`).
    MenuAction(weft_core::config::Action),
}

// ── Application ──────────────────────────────────────────────────────

struct App {
    window: Option<Window>,
    renderer: Option<MetalRenderer>,
    /// v0.9 H1: per-tab session state. The active tab is `tabs[active_tab]`.
    /// Currently always has exactly one tab; Stage 2 adds Cmd+T/W multi-tab.
    sessions: SessionState,
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
        &self.sessions.tabs[self.sessions.active_tab]
    }

    /// Mutable borrow of the active tab.
    fn tab_mut(&mut self) -> &mut Tab {
        &mut self.sessions.tabs[self.sessions.active_tab]
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
        Self {
            window: None,
            renderer: None,
            sessions: SessionState::new(),
            tab_bar: TabBarState::default(),
            window_runtime: WindowRuntimeState::new(),
            interaction: InteractionState::new(),
            proxy,
            config_state,
            panel: PanelState::default(),
            palette: PaletteState::new(),
            find: FindState::new(),
            settings: SettingsState::new(),
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
        self.sessions.tabs.push(tab);
    }

    /// Non-blocking drain of PTY events into channel. Drains ALL tabs per
    /// frame (v0.9 H1 decision: background tabs keep their PTY buffers
    /// flushed so switching to them is instant; only the active tab is
    /// rendered).
    fn pump_pty(&mut self) {
        for tab in &mut self.sessions.tabs {
            tab.pump_pty();
        }
    }

    fn process_messages(&mut self) -> bool {
        let mut any_redraw = false;
        let mut had_pty_output = false;
        let mut deferred_local_scroll = 0_i32;
        let mut drained_blocks: Vec<weft_core::blocks::Block> = Vec::new();
        for i in 0..self.sessions.tabs.len() {
            let (alive, drained, need_redraw) = self.sessions.tabs[i].process_messages();
            if !alive {
                // Shell exited on tab `i`. For now (Stage 2) we only exit
                // the app when the LAST tab's shell exits. A closed tab
                // via Cmd+W is handled by `close_tab`, not here.
                if self.sessions.tabs.len() <= 1 {
                    self.should_exit = true;
                    break;
                }
                // Otherwise: remove the exited tab and switch to the prev.
                self.sessions.tabs.remove(i);
                if self.sessions.active_tab >= self.sessions.tabs.len() {
                    self.sessions.active_tab = self.sessions.tabs.len() - 1;
                }
                info!(
                    closed = i,
                    active = self.sessions.active_tab,
                    "tab shell exited"
                );
                break;
            }
            drained_blocks.extend(drained);
            if let Some(resolution) = self.sessions.tabs[i].resolve_pending_tui_scroll() {
                match resolution {
                    TuiScrollResolution::PtyBytes(bytes) => {
                        if let Some(pty) = &self.sessions.tabs[i].pty {
                            if let Err(e) = pty.write_sync(&bytes) {
                                warn!(error = %e, tab = i, "failed to replay queued TUI scroll");
                            }
                        }
                    }
                    TuiScrollResolution::LocalRows(rows) if i == self.sessions.active_tab => {
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
        if !drained_blocks.is_empty() {
            self.persist_blocks(&drained_blocks);
        }
        if deferred_local_scroll != 0 {
            self.scroll_local_view(deferred_local_scroll);
        }
        if any_redraw {
            self.request_redraw();
        }
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
                    if let Some(pty) = self.sessions.tabs.get(tab).and_then(|tab| tab.pty.as_ref())
                    {
                        if let Err(error) = pty.write_sync(&bytes) {
                            warn!(%error, tab, "failed to apply PTY write effect");
                        }
                    }
                }
                Effect::InterruptPty { tab } => {
                    if let Some(pty) = self.sessions.tabs.get(tab).and_then(|tab| tab.pty.as_ref())
                    {
                        pty.send_interrupt();
                    }
                }
                Effect::FlushPtyOutput { tab } => {
                    if let Some(tab) = self.sessions.tabs.get_mut(tab) {
                        tab.flush_pty_output();
                    }
                }
                Effect::ResizePty { tab, rows, cols } => {
                    if let Some(session) = self.sessions.tabs.get_mut(tab) {
                        if let Some(pty) = &session.pty {
                            if let Err(error) = pty.resize(rows as u16, cols as u16) {
                                warn!(%error, tab, rows, cols, "failed to apply PTY resize effect");
                            }
                        }
                        if session.pending_pty_resize == Some((rows, cols)) {
                            session.pending_pty_resize = None;
                        }
                    }
                }
                Effect::CopyClipboard { text } => clipboard_copy(&text),
                Effect::PersistTabs => self.save_all_tabs(),
                Effect::PersistBlocks { blocks } => self.persist_blocks(&blocks),
                Effect::Paste { tab } => self.apply_paste(tab),
                Effect::Exit => self.should_exit = true,
                Effect::RequestRedraw => self.request_redraw(),
            }
        }
    }

    /// Persist a batch of drained command blocks to the BlockStore. Best-effort:
    /// each failure is logged but does not abort the remaining inserts. Extracted
    /// from `process_messages` so the same logic serves the `PersistBlocks` effect.
    fn persist_blocks(&self, blocks: &[weft_core::blocks::Block]) {
        let Some(store) = &self.sessions.block_store else {
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
    /// bracketed-paste wrapping. Shared by `Effect::Paste` and the legacy
    /// `Action::Paste` / find-bar Cmd+V paths.
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
            .tabs
            .get(tab)
            .and_then(|t| t.terminal.as_ref())
            .map(|t| t.effective_input_mode())
            .unwrap_or(weft_core::input::InputMode::Passthrough);

        if mode == weft_core::input::InputMode::Editor {
            // Editor takeover: paste into the input box. Multi-line text is
            // split on \n (insert_char rejects control chars including \n,
            // so we must drive split_newline explicitly to preserve line
            // breaks). \r is dropped to handle CRLF paste from external apps.
            if let Some(t) = self
                .sessions
                .tabs
                .get_mut(tab)
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
                .tabs
                .get(tab)
                .and_then(|t| t.terminal.as_ref())
                .map(|t| t.bracketed_paste)
                .unwrap_or(false);
            let bytes = encode_paste(text, bracketed);
            if let Some(pty) = self.sessions.tabs.get(tab).and_then(|t| t.pty.as_ref()) {
                if let Err(e) = pty.write_sync(&bytes) {
                    warn!(error = %e, tab, "failed to paste to PTY");
                }
            }
        }
    }

    /// Cancel native marked text before keyboard ownership changes. macOS
    /// keeps this state on the window rather than on an individual Weft tab.
    fn reset_ime_context(&mut self, reason: &'static str) {
        for tab in &mut self.sessions.tabs {
            tab.ime_preedit.clear();
        }
        if let Some(window) = &self.window {
            ime::discard_marked_text(window);
        }
        tracing::debug!(reason, "native IME context reset");
    }

    fn overlay_input_owner(&self) -> Option<OverlayInputOwner> {
        OverlayInputOwner::resolve(OverlayInputContext {
            palette_open: self.palette.open,
            settings_open: self.settings.open,
            find_open: self.find.open,
            panel_search_focused: self.panel.open && self.panel.search_focused,
        })
    }

    fn handle_key_event(
        &mut self,
        key_code: WinitKeyCode,
        mods: winit::event::Modifiers,
        text: Option<&str>,
    ) {
        if self.tab().terminal.is_none() {
            return;
        }

        let key = match key_code {
            WinitKeyCode::Enter => KeyCode::Enter,
            WinitKeyCode::Backspace => KeyCode::Backspace,
            WinitKeyCode::Tab => KeyCode::Tab,
            WinitKeyCode::Escape => KeyCode::Escape,
            WinitKeyCode::ArrowUp => KeyCode::Up,
            WinitKeyCode::ArrowDown => KeyCode::Down,
            WinitKeyCode::ArrowLeft => KeyCode::Left,
            WinitKeyCode::ArrowRight => KeyCode::Right,
            WinitKeyCode::Home => KeyCode::Home,
            WinitKeyCode::End => KeyCode::End,
            WinitKeyCode::PageUp => KeyCode::PageUp,
            WinitKeyCode::PageDown => KeyCode::PageDown,
            WinitKeyCode::Delete => KeyCode::Delete,
            WinitKeyCode::Insert => KeyCode::Insert,
            WinitKeyCode::F1 => KeyCode::F(1),
            WinitKeyCode::F2 => KeyCode::F(2),
            WinitKeyCode::F3 => KeyCode::F(3),
            WinitKeyCode::F4 => KeyCode::F(4),
            WinitKeyCode::F5 => KeyCode::F(5),
            WinitKeyCode::F6 => KeyCode::F(6),
            WinitKeyCode::F7 => KeyCode::F(7),
            WinitKeyCode::F8 => KeyCode::F(8),
            WinitKeyCode::F9 => KeyCode::F(9),
            WinitKeyCode::F10 => KeyCode::F(10),
            WinitKeyCode::F11 => KeyCode::F(11),
            WinitKeyCode::F12 => KeyCode::F(12),
            WinitKeyCode::Space => KeyCode::Char(' '),
            WinitKeyCode::KeyA => KeyCode::Char('a'),
            WinitKeyCode::KeyB => KeyCode::Char('b'),
            WinitKeyCode::KeyC => KeyCode::Char('c'),
            WinitKeyCode::KeyD => KeyCode::Char('d'),
            WinitKeyCode::KeyE => KeyCode::Char('e'),
            WinitKeyCode::KeyF => KeyCode::Char('f'),
            WinitKeyCode::KeyG => KeyCode::Char('g'),
            WinitKeyCode::KeyH => KeyCode::Char('h'),
            WinitKeyCode::KeyI => KeyCode::Char('i'),
            WinitKeyCode::KeyJ => KeyCode::Char('j'),
            WinitKeyCode::KeyK => KeyCode::Char('k'),
            WinitKeyCode::KeyL => KeyCode::Char('l'),
            WinitKeyCode::KeyM => KeyCode::Char('m'),
            WinitKeyCode::KeyN => KeyCode::Char('n'),
            WinitKeyCode::KeyO => KeyCode::Char('o'),
            WinitKeyCode::KeyP => KeyCode::Char('p'),
            WinitKeyCode::KeyQ => KeyCode::Char('q'),
            WinitKeyCode::KeyR => KeyCode::Char('r'),
            WinitKeyCode::KeyS => KeyCode::Char('s'),
            WinitKeyCode::KeyT => KeyCode::Char('t'),
            WinitKeyCode::KeyU => KeyCode::Char('u'),
            WinitKeyCode::KeyV => KeyCode::Char('v'),
            WinitKeyCode::KeyW => KeyCode::Char('w'),
            WinitKeyCode::KeyX => KeyCode::Char('x'),
            WinitKeyCode::KeyY => KeyCode::Char('y'),
            WinitKeyCode::KeyZ => KeyCode::Char('z'),
            WinitKeyCode::Digit0 => KeyCode::Char('0'),
            WinitKeyCode::Digit1 => KeyCode::Char('1'),
            WinitKeyCode::Digit2 => KeyCode::Char('2'),
            WinitKeyCode::Digit3 => KeyCode::Char('3'),
            WinitKeyCode::Digit4 => KeyCode::Char('4'),
            WinitKeyCode::Digit5 => KeyCode::Char('5'),
            WinitKeyCode::Digit6 => KeyCode::Char('6'),
            WinitKeyCode::Digit7 => KeyCode::Char('7'),
            WinitKeyCode::Digit8 => KeyCode::Char('8'),
            WinitKeyCode::Digit9 => KeyCode::Char('9'),
            WinitKeyCode::Minus => KeyCode::Char('-'),
            WinitKeyCode::Equal => KeyCode::Char('='),
            WinitKeyCode::BracketLeft => KeyCode::Char('['),
            WinitKeyCode::BracketRight => KeyCode::Char(']'),
            WinitKeyCode::Backslash => KeyCode::Char('\\'),
            WinitKeyCode::Semicolon => KeyCode::Char(';'),
            WinitKeyCode::Quote => KeyCode::Char('\''),
            WinitKeyCode::Backquote => KeyCode::Char('`'),
            WinitKeyCode::Comma => KeyCode::Char(','),
            WinitKeyCode::Period => KeyCode::Char('.'),
            WinitKeyCode::Slash => KeyCode::Char('/'),
            _ => return,
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
                    self.find.last_key = Some(std::time::Instant::now());
                    self.request_redraw();
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
        if let Some(action) = self.config_state.keybindings.lookup(key, m) {
            if self.execute_action(action) {
                return;
            }
        }

        let overlay_owner = self.overlay_input_owner();
        let overlay_consumed = match overlay_owner {
            Some(OverlayInputOwner::Palette) => self.handle_palette_key(key, m, text),
            Some(OverlayInputOwner::Settings) => self.handle_settings_key(key, m, text),
            Some(OverlayInputOwner::Find) => self.handle_find_key(key, m, text),
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
            .map(|t| t.app_cursor_keys)
            .unwrap_or(false);
        self.tab_mut().input_handler.app_cursor_keys = app_cursor_keys;

        let bytes = ime::encode_passthrough_key(&self.tab().input_handler, key, m, text);
        // Diagnostic (set RUST_LOG=weft_app=debug to see): the exact bytes we
        // send for each key, including whether DECCKM/app-cursor mode is on.
        tracing::debug!(
            ?key,
            ?m,
            app_cursor_keys = self.tab().input_handler.app_cursor_keys,
            ?bytes,
            "key → pty"
        );
        let effects = effect::passthrough_key_effects(self.sessions.active_tab, bytes);
        self.drain_effects(effects);
    }

    /// Dispatch a weft action resolved from a keybinding. Returns true if the
    /// key was consumed (must not be forwarded to the PTY).
    fn execute_action(&mut self, action: Action) -> bool {
        match action {
            Action::Copy => {
                self.copy_selection();
                true
            }
            Action::Paste => {
                self.drain_effects(vec![Effect::Paste {
                    tab: self.sessions.active_tab,
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
                } else {
                    // v0.9 fix: opening the palette closes the find bar (and
                    // vice versa) so only one modal owns keyboard input at a
                    // time. Without this, Cmd+F then Cmd+P leaves both
                    // popups open and keystrokes go to the wrong one.
                    // v1.0 S1: also close the Settings panel.
                    self.close_find();
                    self.close_settings();
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
                } else {
                    // v0.9 fix: opening find closes the palette (see above).
                    // v1.0 S1: also close the Settings panel.
                    self.close_palette();
                    self.close_settings();
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
                true
            }
            Action::CloseTab => self.close_tab(),
            Action::NextTab => {
                self.next_tab();
                true
            }
            Action::PrevTab => {
                self.prev_tab();
                true
            }
            Action::ToggleSettings => {
                self.reset_ime_context("settings toggled");
                if self.settings.open {
                    self.settings.close();
                } else {
                    // Mutual exclusion: close other modals.
                    self.close_palette();
                    self.close_find();
                    self.settings.open_from(&self.config_state.config);
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
    }

    /// v0.9: close the command palette and reset its state. Used when
    /// another modal (find bar, …) opens so only one owns keyboard input.
    fn close_palette(&mut self) {
        if !self.palette.open {
            return;
        }
        self.reset_ime_context("palette closed");
        self.palette.close();
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
            let Some(terminal) = &self.sessions.tabs[self.sessions.active_tab].terminal else {
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
                        self.sessions.tabs[self.sessions.active_tab]
                            .selection_handler
                            .block_view_text()
                    } else {
                        self.sessions.tabs[self.sessions.active_tab]
                            .selection_handler
                            .selected_text(terminal.grid())
                    }
                })
        };
        self.drain_effects(effect::copy_clipboard_effects(text));
    }

    /// Paste from system clipboard.
    ///
    /// Two-path dispatch mirrors `Ime::Commit` (main.rs ~L2721): in Editor mode
    /// the shell is taken over by weft and does not echo, so pasted bytes sent
    /// to the PTY would vanish. Instead we insert the text directly into the
    /// editor buffer. Passthrough mode forwards to the PTY as before (with
    /// bracketed-paste wrapping when the shell supports it).
    fn paste_from_clipboard(&mut self) {
        self.apply_paste(self.sessions.active_tab);
    }

    /// Update cursor blink state.
    ///
    /// Two independent mechanisms share the `cursor_blink_time` anchor:
    /// - **Grid view**: hard on/off toggle every 530ms (unchanged v0.7 logic).
    /// - **Prompt (Editor mode)**: smooth `sin()` breath over a 2400ms period
    ///   (v0.8 §0.3 signature). The phase advances continuously and wraps at
    ///   2π; the renderer maps it to an alpha curve 0.25↔1.0 + amber glow.
    fn update_cursor_blink(&mut self) {
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
}

/// v0.9 U-D1: Query macOS system appearance via `NSUserDefaults`.
/// Returns `true` when the user has Dark mode selected in System
/// Settings, `false` for Light (the macOS default — `AppleInterfaceStyle`
/// is absent/empty when Light is active). Used by `poll_system_appearance`
/// to follow the system appearance live (throttled to 1Hz by the caller).
///
/// Reads `AppleInterfaceStyle` from `NSUserDefaults.standardUserDefaults`,
/// which is kept in sync by the OS across `AppleInterfaceThemeChangedNotification`.
/// We poll rather than register a distributed-notification observer because
/// winit owns the `NSApplication` and its delegate, making selector-based
/// callbacks awkward; a 1Hz poll is cheap and matches the existing
/// config-mtime poller pattern.
unsafe fn system_appearance_is_dark() -> bool {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let defaults_cls = objc2::ffi::objc_getClass(c"NSUserDefaults".as_ptr());
    let str_cls = objc2::ffi::objc_getClass(c"NSString".as_ptr());
    if defaults_cls.is_null() || str_cls.is_null() {
        return false;
    }
    let defaults: *mut AnyObject =
        msg_send![defaults_cls as *const AnyObject, standardUserDefaults];
    if defaults.is_null() {
        return false;
    }
    let c_key = std::ffi::CString::new("AppleInterfaceStyle").unwrap_or_default();
    let key_ns: *mut AnyObject =
        msg_send![str_cls as *const AnyObject, stringWithUTF8String: c_key.as_ptr()];
    if key_ns.is_null() {
        return false;
    }
    // stringForKey: returns nil for absent keys (Light mode default).
    let value_ns: *mut AnyObject = msg_send![defaults, stringForKey: key_ns];
    if value_ns.is_null() {
        return false;
    }
    let c_str: *const i8 = msg_send![value_ns, UTF8String];
    if c_str.is_null() {
        return false;
    }
    let raw = std::ffi::CStr::from_ptr(c_str);
    let s = raw.to_str().unwrap_or("").trim().to_ascii_lowercase();
    s == "dark"
}

/// Copy text to macOS system clipboard using NSPasteboard.
fn clipboard_copy(text: &str) {
    unsafe {
        use objc2::msg_send;
        use objc2::runtime::AnyObject;

        let pb_cls = objc2::ffi::objc_getClass(c"NSPasteboard".as_ptr());
        let str_cls = objc2::ffi::objc_getClass(c"NSString".as_ptr());
        if pb_cls.is_null() || str_cls.is_null() {
            return;
        }
        let pasteboard: *mut AnyObject = msg_send![pb_cls as *const AnyObject, generalPasteboard];
        if pasteboard.is_null() {
            return;
        }

        // NSPasteboardTypeString == "public.utf8-plain-text". Build NSStrings
        // for the value and the type, then use the real setters (the old code
        // called non-existent `setString:` and `string` selectors, so the
        // clipboard never actually worked).
        let c_text = std::ffi::CString::new(text).unwrap_or_default();
        let value_ns: *mut AnyObject =
            msg_send![str_cls as *const AnyObject, stringWithUTF8String: c_text.as_ptr()];
        let c_type = std::ffi::CString::new("public.utf8-plain-text").unwrap();
        let type_ns: *mut AnyObject =
            msg_send![str_cls as *const AnyObject, stringWithUTF8String: c_type.as_ptr()];
        if value_ns.is_null() || type_ns.is_null() {
            return;
        }

        // clearContents returns NSInteger (objc2 verifies the return type code
        // against the method signature at runtime in debug, so this must be
        // `isize` = 'q', not `()`).
        let _: isize = msg_send![pasteboard, clearContents];
        // `setString:forType:` returns BOOL (arm64 macOS: `_Bool` = type code
        // 'B', matching Rust `bool`); we ignore it.
        let _: bool = msg_send![pasteboard, setString: value_ns forType: type_ns];
    }
}

/// Paste text from macOS system clipboard using NSPasteboard.
fn clipboard_paste() -> Option<String> {
    unsafe {
        use objc2::msg_send;
        use objc2::runtime::AnyObject;

        let pb_cls = objc2::ffi::objc_getClass(c"NSPasteboard".as_ptr());
        let str_cls = objc2::ffi::objc_getClass(c"NSString".as_ptr());
        if pb_cls.is_null() || str_cls.is_null() {
            return None;
        }
        let pasteboard: *mut AnyObject = msg_send![pb_cls as *const AnyObject, generalPasteboard];
        if pasteboard.is_null() {
            return None;
        }

        let c_type = std::ffi::CString::new("public.utf8-plain-text").unwrap();
        let type_ns: *mut AnyObject =
            msg_send![str_cls as *const AnyObject, stringWithUTF8String: c_type.as_ptr()];

        // stringForType: returns a nullable NSString (nil if no string of that
        // type is on the pasteboard).
        let ns_string: *mut AnyObject = msg_send![pasteboard, stringForType: type_ns];
        if ns_string.is_null() {
            return None;
        }

        let c_str: *const i8 = msg_send![ns_string, UTF8String];
        if c_str.is_null() {
            return None;
        }

        std::ffi::CStr::from_ptr(c_str)
            .to_str()
            .ok()
            .map(|s| s.to_owned())
    }
}

impl ApplicationHandler<AppEvent> for App {
    /// Cross-thread wake-up (PTY output or blink timer): pump + process
    /// immediately, then schedule a redraw for rendering.
    ///
    /// v1.0 perf: Previously this only called `request_redraw()`, deferring
    /// all PTY processing to `RedrawRequested` (next vsync). That added up
    /// to 16ms latency per batch — for `seq 1 100000` (~500KB), ~30 vsync
    /// cycles were needed just for the data to flow through, on top of the
    /// VT parse + render time. Processing here means data is drained from
    /// the PTY channel on arrival, and the subsequent `RedrawRequested`
    /// only needs to render (the heavy work is already done).
    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: AppEvent) {
        match event {
            AppEvent::Wake => {
                self.pump_pty();
                self.process_messages();
                self.request_redraw();
            }
            AppEvent::ConfigReload => {
                self.reload_config();
                self.request_redraw();
            }
            AppEvent::TabsAutoSave => {
                self.save_all_tabs();
            }
            AppEvent::MenuAction(action) => {
                // v1.1: native menu click → reuse the same dispatch as
                // keybindings. execute_action redraws where needed.
                self.execute_action(action);
            }
        }
        if self.should_exit {
            event_loop.exit();
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let win = &self.config_state.config.window;
        // v0.9 U-D1: resolve the startup theme honoring `follow_system` so
        // the window opens with the correct color from frame 0 (no dark→light
        // flash). `Config::theme()` ignores follow_system; this mirrors the
        // logic in `apply_config` / `poll_system_appearance`.
        let startup_theme = if self.config_state.config.theme.follow_system {
            let dark = unsafe { system_appearance_is_dark() };
            let name = if dark {
                self.config_state
                    .config
                    .theme
                    .dark_name
                    .clone()
                    .unwrap_or_else(|| "weft-warm".to_string())
            } else {
                self.config_state
                    .config
                    .theme
                    .light_name
                    .clone()
                    .unwrap_or_else(|| "weft-light".to_string())
            };
            weft_core::config::Theme::resolve_named(&name, &self.config_state.config.theme)
        } else {
            self.config_state.config.theme()
        };
        let attrs = WindowAttributes::default()
            .with_title(&win.title)
            .with_inner_size(winit::dpi::LogicalSize::new(
                win.width as f64,
                win.height as f64,
            ))
            .with_min_inner_size(winit::dpi::LogicalSize::new(
                crate::ui_tokens::MIN_WINDOW_WIDTH,
                crate::ui_tokens::MIN_WINDOW_HEIGHT,
            ))
            // Window-level transparency is fixed at creation; the layer opaque
            // flag + bg alpha still update live, but crossing the 1.0 boundary
            // (opaque ↔ see-through) needs a relaunch.
            .with_transparent(win.opacity < 1.0)
            // Runtime window icon (shows in the Dock during `cargo run` and
            // in the app switcher). The .icns in the .app bundle takes over
            // once packaged — see v1.0 Phase 3 V9-b.
            .with_window_icon(load_window_icon());

        let window = event_loop.create_window(attrs).unwrap();
        // v1.1: Warp-style transparent titlebar. Must run AFTER create_window
        // (needs the NSView/NSWindow to exist) and BEFORE renderer attaches the
        // Metal layer (so FullSizeContentView is in effect when the layer is
        // sized → it extends under the titlebar). configure_titlebar reaches
        // the NSWindow via the raw-window-handle AppKit handle and sets the
        // style mask + transparency + movable-by-background.
        configure_titlebar(&window);
        // v1.1: Install the native macOS menu bar (Weft/File/Edit/View/Find/
        // Window). Runs on the main thread; replaces winit's default menu.
        // Must be after `create_window` (NSApplication is up by `resumed`).
        // MainThreadMarker is sound here — `resumed` always runs on main.
        if let Some(mtm) = objc2_foundation::MainThreadMarker::new() {
            menu::install(mtm, self.proxy.clone());
        }
        let renderer = MetalRenderer::new(
            &window,
            self.config_state.config.font.clone(),
            startup_theme,
            (win.padding_x, win.padding_y),
            win.opacity,
        );

        // Enable IME so CJK input methods compose/commit into the PTY. Without
        // this winit delivers raw keystrokes (e.g. pinyin letters) instead of
        // composed text — Chinese wouldn't type in the shell or in TUI apps.
        window.set_ime_allowed(true);

        // Spawn the PTY at the window's actual cell size from the start (not a
        // hardcoded 24×80). Otherwise the program reads 24×80, renders, then
        // gets a late SIGWINCH to the real size and re-renders — a race that
        // desyncs its cursor model from the grid (seen in claude: cursor/text
        // land offset from the drawn UI).
        let win_size = window.inner_size();
        let (init_rows, init_cols) = dimensions_for_renderer(&renderer, win_size, 0.0);
        let init_rows = init_rows.max(1);
        let init_cols = init_cols.max(1);
        self.spawn_pty(init_rows, init_cols);
        self.window = Some(window);
        self.renderer = Some(renderer);

        // v1.0 Logo: apply the configured Dock icon variant on startup.
        // `with_window_icon` sets the window title-bar icon; this sets the
        // Dock / app-switcher icon. For `cargo run` both show; in a .app
        // bundle the .icns takes over unless overridden here.
        self.window_runtime.current_logo_variant = self.config_state.config.logo.variant;
        unsafe {
            set_dock_icon(self.window_runtime.current_logo_variant);
        }

        // v0.9 U-D1: seed the appearance tracker so the first
        // `poll_system_appearance` (1s after launch) doesn't re-apply the
        // same theme and cause a flicker. The startup theme above already
        // queried the system appearance, so we record it as "known".
        if self.config_state.config.theme.follow_system {
            let dark = unsafe { system_appearance_is_dark() };
            self.window_runtime.last_system_appearance_dark = Some(dark);
            self.config_state.theme_is_dark = dark;
        }

        // Open the command-block DB (best-effort) and hydrate the tracker with
        // recent history so the panel has content on first show.
        self.sessions.block_store = weft_cache_dir().and_then(|cache| {
            let path = cache.join("blocks.db");
            match BlockStore::open(&path) {
                Ok(store) => {
                    if let Some(terminal) =
                        &mut self.sessions.tabs[self.sessions.active_tab].terminal
                    {
                        match store.recent(1000) {
                            Ok(history) => {
                                // Hydrate editor history from persisted commands so
                                // ↑/↓ navigation works immediately on startup.
                                // Blocks are oldest→newest; load_history reverses
                                // to newest-first. Skip empty commands and strip
                                // prompt artifacts (cwd path + ❯ marker) that
                                // snapshot_command_line may have captured for
                                // passthrough / non-editor sessions.
                                let cmds: Vec<String> = history
                                    .iter()
                                    .map(|b| strip_prompt_prefix(&b.command))
                                    .filter(|c| !c.trim().is_empty())
                                    .collect();
                                terminal.editor_mut().load_history(cmds);
                                terminal.block_tracker_mut().load_blocks(history);
                            }
                            Err(e) => warn!(error = %e, "failed to load block history"),
                        }
                    }
                    Some(store)
                }
                Err(e) => {
                    warn!(error = %e, "failed to open block store; persistence disabled");
                    None
                }
            }
        });

        // v1.0 H4: restore saved tab snapshots (cwd + editor drafts) so the
        // session layout survives restarts. The first tab (spawned above by
        // spawn_pty) is replaced if saved snapshots exist; otherwise it stays
        // as a fresh shell. The PTY itself is NOT revived — each restored tab
        // gets a fresh shell, with the editor draft rehydrated.
        //
        // v1.0 fix: cwd is restored via `chdir` in the child process before
        // exec (Pty::spawn_with_args `cwd` param), NOT by sending a `cd`
        // command. Sending `cd` polluted the terminal, shell history, and
        // block tracker with a spurious `cd <cwd>` block. With chdir the
        // shell starts in the right directory silently — the initial tab
        // stays clean. If the saved cwd equals the weft process's cwd (the
        // common case when launching from the same directory), no rebuild
        // is needed — the initial tab already has the right cwd.
        if let Some(store) = &self.sessions.block_store {
            match store.load_tabs() {
                Ok(snaps) if !snaps.is_empty() => {
                    info!(count = snaps.len(), "restoring saved tab snapshots");
                    let (rows, cols) = self.current_size();
                    let total = snaps.len();
                    let home = std::env::var("HOME").unwrap_or_default();
                    let weft_cwd = std::env::current_dir()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default();
                    for (i, snap) in snaps.iter().enumerate() {
                        let saved_cwd = snap.cwd.clone();
                        // Filter: only apply cwd if non-empty, != $HOME, and
                        // != the weft process's current cwd (the last check
                        // avoids a needless tab rebuild in the common case of
                        // launching from the same directory).
                        let cwd_to_apply = saved_cwd
                            .as_deref()
                            .filter(|c| !c.is_empty() && *c != home && *c != weft_cwd);
                        if i == 0 {
                            // First tab: rebuild with chdir only if a different
                            // cwd is needed; otherwise reuse the existing tab
                            // (already spawned with weft's cwd).
                            if cwd_to_apply.is_some() {
                                self.sessions.tabs[0] = Tab::new(
                                    rows,
                                    cols,
                                    self.config_state.config.scrollback.lines,
                                    &self.proxy,
                                    cwd_to_apply,
                                );
                                if let Some(t) = &mut self.sessions.tabs[0].terminal {
                                    if let Some(r) = &self.renderer {
                                        t.set_palette(r.theme().palette);
                                    }
                                }
                            }
                            self.sessions.tabs[0].restore_from_snapshot(snap);
                        } else {
                            let mut tab = Tab::new(
                                rows,
                                cols,
                                self.config_state.config.scrollback.lines,
                                &self.proxy,
                                cwd_to_apply,
                            );
                            tab.restore_from_snapshot(snap);
                            if let Some(t) = &mut tab.terminal {
                                if let Some(r) = &self.renderer {
                                    t.set_palette(r.theme().palette);
                                }
                            }
                            self.sessions.tabs.push(tab);
                        }
                    }
                    // Clear saved tabs so a crash during the session doesn't
                    // re-restore stale state on the next launch — the periodic
                    // auto-save will re-persist the live state.
                    let _ = store.clear_tabs();
                    self.sessions.active_tab = 0;
                    info!(restored = total, "tab snapshots restored");
                }
                Ok(_) => {
                    // No saved tabs — fresh launch, keep the initial tab.
                }
                Err(e) => {
                    warn!(error = %e, "failed to load tab snapshots; starting fresh");
                }
            }
        }

        // Open the workflow DB (best-effort) and seed built-in templates on
        // first launch.
        self.palette.store = weft_cache_dir().and_then(|cache| {
            let path = cache.join("workflows.db");
            match weft_core::workflow::WorkflowStore::open(&path) {
                Ok(store) => {
                    // Seed built-in workflows on first launch (count == 0).
                    if let Ok(0) = store.count() {
                        seed_workflows(&store);
                    }
                    Some(store)
                }
                Err(e) => {
                    warn!(error = %e, "failed to open workflow store; workflows disabled");
                    None
                }
            }
        });

        // Cursor-blink timer: wake the loop ~2x/sec so the caret toggles
        // without a vsync busy-loop. Exits when the event loop drops the proxy.
        // Flicker fix (Step 2): only wake when a cursor/caret is actually
        // animating. The main thread sets `cursor_anim_active` after each
        // redraw — when false (no prompt in block view, cursor hidden in
        // grid view, or window unfocused), the timer skips the wake, which
        // avoids pointless full redraws that caused idle flicker.
        let blink_proxy = self.proxy.clone();
        let blink_flag = self.window_runtime.cursor_anim_active.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_millis(530));
            if !blink_flag.load(Ordering::Relaxed) {
                continue; // no cursor to animate — skip this wake
            }
            if blink_proxy.send_event(AppEvent::Wake).is_err() {
                break; // event loop exited
            }
        });

        // Config file watcher: poll the config's mtime ~1/sec and reload live
        // on change (theme/font/keybindings/scrollback re-apply instantly).
        // Zero dependencies — mtime polling is cheap for a single file.
        if let Some(path) = Config::config_path() {
            let reload_proxy = self.proxy.clone();
            std::thread::spawn(move || {
                let mut last = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    let cur = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
                    if cur != last {
                        last = cur;
                        if reload_proxy.send_event(AppEvent::ConfigReload).is_err() {
                            break; // event loop exited
                        }
                    }
                }
            });
        }

        // v1.0 H4: periodic auto-save (every 30s) so a crash doesn't lose
        // the tab layout + editor drafts. Best-effort — failures are logged
        // inside `save_all_tabs`. Runs on a background thread, wakes the loop
        // via AppEvent::TabsAutoSave (handled synchronously on the main
        // thread, which owns `&mut self`).
        let save_proxy = self.proxy.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(30));
            if save_proxy.send_event(AppEvent::TabsAutoSave).is_err() {
                break; // event loop exited
            }
        });

        self.request_redraw();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        self.dispatch_window_event(event_loop, event);
    }

    /// v1.0 fix: during macOS live-resize, winit may defer `RedrawRequested`
    /// until the mouse is released. The grid IS reflowed in the `Resized`
    /// handler, but without a redraw the old drawable is stretched to fit the
    /// new window bounds → "content squished together" artifact.
    ///
    /// `AboutToWait` fires when the event loop is about to block waiting for
    /// events. By requesting a redraw here while the resize cascade is active
    /// (within 100ms of the last `Resized`), we ensure the content is
    /// re-rendered on every intermediate size during live resize.
    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if self.window_runtime.last_resize_instant.elapsed() < std::time::Duration::from_millis(100)
        {
            self.request_redraw();
        }
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
/// Estimate the total block-view content rows and the visible viewport rows
/// for scroll clamping. This mirrors the row accounting in
/// `build_block_view_vertices`: per block = output lines (wrapped at `cols`)
/// + command line + header line + separator gap.
fn block_content_metrics(terminal: &Terminal, cols: usize) -> (usize, usize) {
    use weft_core::blocks::ShellPhase;

    let blocks = terminal.block_tracker().session_blocks();
    let mut total: usize = 0;
    for b in blocks {
        if !b.collapsed {
            for line in b.output.lines() {
                total += wrapped_row_count(line, cols);
            }
        }
        total += 1; // command line
        total += 1; // header line
        total += 1; // separator gap
    }
    // Live block during CommandExecuting: output + command + gap.
    if terminal.block_tracker().phase() == ShellPhase::CommandExecuting {
        if let Some(live) = terminal.block_tracker().in_flight() {
            for line in live.output.lines() {
                total += wrapped_row_count(line, cols);
            }
            total += 2; // command + gap
        }
    } else {
        // Editor mode: cwd header line.
        total += 1;
    }

    // Visible rows: the block region height / cell height.
    let renderer_cell_h = 1; // placeholder; computed from terminal grid rows
    let grid_rows = terminal.grid().num_rows;
    let visible = grid_rows.max(1);
    let _ = renderer_cell_h;
    (total, visible)
}

/// Count how many visual rows a text line occupies when wrapped at `cols`.
fn wrapped_row_count(text: &str, cols: usize) -> usize {
    if cols == 0 {
        return 1;
    }
    let mut rows = 1;
    let mut col = 0usize;
    for c in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w == 0 {
            continue;
        }
        if col + w > cols {
            rows += 1;
            col = 0;
        }
        col += w;
    }
    rows.max(1)
}

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

/// Open `url` using the system default handler (macOS `open`).
/// Used by OSC 8 Cmd+Click. Best-effort: errors are logged, not surfaced.
fn open_url(url: &str) {
    // Sanity-check the scheme before handing it to `open` — we don't want
    // `open file:///etc/passwd` surprises or arbitrary `open <path>` shells.
    let is_safe = url.starts_with("https://") || url.starts_with("http://");
    if !is_safe {
        tracing::warn!(url, "OSC 8 Cmd+Click refused non-http(s) URL");
        return;
    }
    match std::process::Command::new("open").arg(url).status() {
        Ok(status) if !status.success() => {
            tracing::warn!(?status, url, "open exited non-zero");
        }
        Err(e) => tracing::warn!(error = %e, url, "open spawn failed"),
        _ => {}
    }
}

/// Scan `$PATH` for executable names (files, not dirs). Best-effort: unreadable
/// / missing dirs are skipped. Deduped + sorted. Cached once at startup.
fn scan_path_bins() -> Vec<String> {
    let mut bins = std::collections::BTreeSet::new();
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                        if let Some(name) = entry.file_name().to_str() {
                            bins.insert(name.to_string());
                        }
                    }
                }
            }
        }
    }
    bins.into_iter().collect()
}

/// Load the weft window icon from the embedded 256×256 PNG. Returns `None`
/// (winit default icon) if decode fails — best-effort, not a hard error.
/// The PNG is embedded at compile time via `include_bytes!`, so there's no
/// runtime file dependency.
fn load_window_icon() -> Option<winit::window::Icon> {
    // Use the Cool variant (default) for the window title-bar icon. Set once
    // at window creation; runtime Dock icon switching via set_dock_icon() does
    // not update this (would need window recreation — acceptable trade-off).
    let png_bytes = include_bytes!("../../../assets/logo/variants/png/cool-256.png");
    let img = image::load_from_memory(png_bytes).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    winit::window::Icon::from_rgba(rgba.into_raw(), w, h).ok()
}

/// v1.0 Logo: PNG bytes for each logo variant. Each variant ships its own
/// 256×256 PNG (rendered from `assets/logo/variants/{variant}.svg`); the
/// runtime switches Dock icon by loading the matching bytes via NSImage.
fn logo_png_bytes(variant: weft_core::config::LogoVariant) -> &'static [u8] {
    use weft_core::config::LogoVariant;
    match variant {
        LogoVariant::Cool => include_bytes!("../../../assets/logo/variants/png/cool-256.png"),
        LogoVariant::Warm => include_bytes!("../../../assets/logo/variants/png/warm-256.png"),
        LogoVariant::Light => include_bytes!("../../../assets/logo/variants/png/light-256.png"),
        LogoVariant::Transparent => {
            include_bytes!("../../../assets/logo/variants/png/transparent-256.png")
        }
    }
}

/// v1.0 Logo: set the macOS Dock app icon at runtime via
/// `NSApp.setApplicationIconImage:`. Constructs an NSImage from PNG bytes
/// using typed `objc2-app-kit` safe methods.
///
/// v1.2 fix: re-enabled using the same typed objc2-app-kit pattern proven
/// in `configure_titlebar` (renderer.rs). The original raw `msg_send!`
/// implementation panicked under an `extern "C"` boundary (nounwind →
/// abort). The typed `NSImage::initWithData:` + `NSApplication` setters
/// are safe and wrapped in `catch_unwind` as a belt-and-suspenders guard.
///
/// On failure (not on main thread, image decode, ObjC call), this is a
/// no-op: the Dock keeps whatever icon it currently has. Safe to call
/// repeatedly.
unsafe fn set_dock_icon(variant: weft_core::config::LogoVariant) {
    use objc2::ClassType;
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::{MainThreadMarker, NSData};

    // NSApplication::sharedApplication requires a MainThreadMarker. If we're
    // not on the main thread (shouldn't happen — both call sites are in the
    // event loop), silently skip.
    let Some(mtm) = MainThreadMarker::new() else {
        tracing::warn!(?variant, "set_dock_icon skipped — not on main thread");
        return;
    };

    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let png_bytes = logo_png_bytes(variant);
        let ns_data = NSData::dataWithBytes_length(
            png_bytes.as_ptr() as *mut std::ffi::c_void,
            png_bytes.len(),
        );
        let ns_image = NSImage::initWithData(NSImage::alloc(), &ns_data);
        if let Some(image) = ns_image {
            let app = NSApplication::sharedApplication(mtm);
            app.setApplicationIconImage(Some(&image));
            tracing::info!(?variant, "dock icon updated");
        } else {
            tracing::warn!(?variant, "NSImage::initWithData returned nil");
        }
    }));
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
/// Returns `KEY=VALUE` overrides to pass to the PTY. On any setup failure it
/// logs a warning and returns empty — the shell still launches, just without
/// integration.
pub(crate) fn shell_integration_env(shell: &str) -> Vec<(String, String)> {
    let plan = Integration::from_shell(shell);
    if !plan.is_supported() {
        return Vec::new();
    }

    let Some(cache_root) = weft_cache_dir() else {
        warn!("HOME/XDG_CACHE_HOME unset — shell integration disabled");
        return Vec::new();
    };

    // Base env: integration flag (+ forwarded original ZDOTDIR for zsh restore).
    let orig_zdotdir = std::env::var("ZDOTDIR").ok();
    let mut env: Vec<(String, String)> = plan
        .child_env(orig_zdotdir.as_deref())
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();

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

    // v1.0 fix: set TERM / COLORTERM / TERM_PROGRAM for the child shell.
    // When launched from Finder, GUI apps have no TERM set, so `less`,
    // `vim`, `top`, etc. can't query terminfo and degrade ("terminal is
    // not fully functional"). Weft implements xterm-256color semantics
    // (256-color SGR, cursor movement, alternate screen), so advertise
    // that capability. Don't override an existing TERM — the user may have
    // set a specialized one (e.g. tmux).
    if std::env::var("TERM").is_err() {
        env.push(("TERM".to_string(), "xterm-256color".to_string()));
    }
    env.push(("COLORTERM".to_string(), "truecolor".to_string()));
    env.push(("TERM_PROGRAM".to_string(), "Weft".to_string()));
    env.push((
        "TERM_PROGRAM_VERSION".to_string(),
        env!("CARGO_PKG_VERSION").to_string(),
    ));

    // zsh: write the generated .zshenv and redirect ZDOTDIR at its directory.
    if let Some((redirect_var, file)) = plan.rc_redirect() {
        let dir = cache_root.join("zsh");
        if let Err(e) = std::fs::create_dir_all(&dir)
            .and_then(|_| std::fs::write(dir.join(file.filename), file.body))
        {
            warn!(error = %e, "failed to write zsh integration .zshenv; integration disabled");
            return Vec::new();
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
    tracing_subscriber::fmt::init();
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
