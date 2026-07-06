//! Weft v0.2 "Weave" — Metal GPU-rendered terminal emulator
//!
//! Full pipeline: PTY → VT parser → Grid → Metal renderer
//! Features: scrollback, selection, clipboard, CJK, mouse, IME, shell integration

mod find_worker;
mod glyph;
mod layout;
mod overlay;
mod renderer;
mod tab;

use renderer::{
    block_matches_query, visible_panel_rows, FindDrawState, MetalRenderer, TabBarDrawState,
};
use tab::Tab;
use weft_core::blocks::{BlockId, ShellPhase};
use weft_core::complete::{complete, CompleteCtx, CompletePosition};
use weft_core::config::{Action, Config, KeyBindings};
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
}

// ── Application ──────────────────────────────────────────────────────

struct App {
    window: Option<Window>,
    renderer: Option<MetalRenderer>,
    /// v0.9 H1: per-tab session state. The active tab is `tabs[active_tab]`.
    /// Currently always has exactly one tab; Stage 2 adds Cmd+T/W multi-tab.
    tabs: Vec<Tab>,
    active_tab: usize,
    /// v0.9 W1+: index of the tab currently hovered by the mouse, or `None`
    /// when the cursor is outside the tab bar. Drives the Warp-style
    /// hover-to-show close "×" button. Reset on tab close/switch and on
    /// `CursorLeft` (mouse leaves the window).
    hovered_tab: Option<usize>,
    /// Current keyboard modifier state, updated by ModifiersChanged events.
    mods: winit::event::Modifiers,
    /// Cursor blink state (grid view: hard on/off, period 1060ms).
    cursor_blink_on: bool,
    /// Cursor blink phase in radians [0, 2π) for the prompt signature breath
    /// (v0.8 §0.3). Drives a smooth sin() alpha curve (0.25↔1.0) over a
    /// 2400ms period, plus the amber glow halo. Updated per-frame from
    /// elapsed time since `cursor_blink_time`.
    cursor_blink_phase: f32,
    /// Last cursor blink toggle time (shared anchor for both the grid-view
    /// hard blink and the prompt signature breath).
    cursor_blink_time: std::time::Instant,
    /// Last known mouse position for click/scroll handling.
    last_mouse_x: f64,
    last_mouse_y: f64,
    /// Debounced PTY resize: (rows, cols) waiting to be sent to the PTY
    /// after the resize animation cascade settles. The grid is resized
    /// immediately on each Resized event for smooth animation; only the
    /// PTY SIGWINCH is debounced to prevent the shell from fighting cursor
    /// position during rapid cascades.
    last_resize_instant: std::time::Instant,
    /// Cached `$PATH` executable names for Tab completion (scanned once at
    /// startup; empty if completion is disabled).
    path_bins: Vec<String>,
    /// Proxy used by background threads (PTY reader, blink timer) to wake the
    /// event loop without a vsync busy-loop.
    proxy: EventLoopProxy<AppEvent>,
    /// User configuration (loaded at startup, reloaded live by the watcher).
    config: Config,
    /// Resolved keybindings (key + modifiers → action).
    keybindings: KeyBindings,
    /// Current theme state for ToggleTheme builtin command (true = dark).
    theme_is_dark: bool,
    /// v0.9 U-D1: last queried macOS system appearance (None = not yet
    /// queried). When `[theme] follow_system = true`, polled once per
    /// second in `poll_system_appearance` and the theme is hot-swapped
    /// when it changes.
    last_system_appearance_dark: Option<bool>,
    /// v0.9 U-D1: throttle for `poll_system_appearance` — queried at most
    /// once per second to avoid per-frame NSUserDefaults overhead.
    last_appearance_check: std::time::Instant,
    /// SQLite store for command blocks. `None` when the cache dir is
    /// unavailable or opening failed (persistence is best-effort).
    block_store: Option<BlockStore>,
    /// Whether the command-history sidebar panel is shown.
    panel_open: bool,
    /// Live search filter typed into the panel.
    panel_query: String,
    /// Selected row index within the newest-first filtered list.
    panel_selection: usize,
    /// Id of the block whose output is expanded inline in the panel.
    panel_expanded: Option<BlockId>,
    /// v0.9 fix: whether the panel search box has keyboard focus. When true,
    /// typed text goes to `panel_query` instead of the editor. Toggled by
    /// clicking the search box area at the top of the sidebar.
    panel_search_focused: bool,
    /// v0.9: when true, the next mouse drag in the prompt box should extend
    /// the editor selection (mouse is dragging inside the prompt box).
    prompt_dragging: bool,
    /// v0.9 W2: block currently highlighted in the terminal because the user
    /// clicked its row in the history panel. The renderer draws an accent
    /// border around this block. Cleared after 1.5s.
    panel_highlight: Option<BlockId>,
    /// When the panel highlight expires. Polled from the redraw path.
    panel_highlight_until: Option<std::time::Instant>,
    /// v0.9: timestamp + row of the last panel row click, for detecting
    /// double-clicks (which send the command to the prompt).
    panel_last_click: Option<(std::time::Instant, usize)>,

    // ── Command Palette (v0.7) ────────────────────────────────────────
    /// Whether the Cmd+P command palette overlay is shown.
    palette_open: bool,
    /// Popup width scale (0.5–1.0 of viewport). User-adjustable via border drag.
    popup_width_scale: f32,
    /// Popup max visible rows. User-adjustable via border drag.
    popup_max_rows: usize,
    /// Active drag operation on a popup border (None = no drag).
    drag_state: Option<DragState>,
    /// Right-click context menu (F7). None when closed.
    context_menu: Option<ContextMenu>,
    /// Live search query typed into the palette.
    palette_query: String,
    /// Selected index in the palette results.
    palette_selection: usize,
    /// v0.9: last click in the palette results list (time + row index) for
    /// double-click detection. Double-click runs the entry immediately.
    palette_last_click: Option<(std::time::Instant, usize)>,
    /// Cached search results (workflows + builtin commands).
    palette_results: Vec<PaletteEntry>,
    /// Active variable-fill form for a selected workflow (None = search mode).
    palette_form: Option<WorkflowForm>,
    /// Palette sub-mode (Search / CreateWorkflow / EditWorkflow / ConfirmDelete).
    palette_submode: PaletteSubMode,
    /// SQLite workflow store. `None` when the cache dir is unavailable.
    workflow_store: Option<weft_core::workflow::WorkflowStore>,

    /// Live font zoom factor (1.0 = configured base). Cmd+= / Cmd+- /
    /// Cmd+0 adjust this; `apply_font_scale` rebuilds the atlas with the
    /// scaled size. Clamped to [0.5, 3.0]. Persists across config reloads
    /// (re-applied in `apply_config`).
    font_scale: f32,

    // ── FindInGrid (Cmd+F, v0.8 B3) ───────────────────────────────────
    /// Whether the in-grid search bar is open.
    find_open: bool,
    /// Live search query (single-line input). Matches update after a 150ms
    /// debounce — see `find_last_key`.
    find_query: String,
    /// Last keystroke time. The actual search runs when `now - find_last_key
    /// >= 150ms`, checked from the redraw path. `None` when idle.
    find_last_key: Option<std::time::Instant>,
    /// Cached matches for `find_query`. Recomputed on debounce expiry.
    /// Empty when the query is empty or no results.
    find_matches: Vec<weft_core::find::FindMatch>,
    /// Index into `find_matches` of the currently highlighted match.
    find_index: usize,
    /// True when `find_matches` was truncated at MAX_MATCHES — surfaced in
    /// the UI as "too many matches, refine query".
    find_truncated: bool,
    /// Block-view matches (v0.8 B3 — block content search). When the user
    /// is in the Warp-style block view, the visible content comes from
    /// `Block.output` strings, not the live grid. `find_in_grid` alone
    /// returns "no matches" even when the text is on screen. We track
    /// block matches separately so the FindUI count reflects what the
    /// user actually sees; the renderer doesn't yet highlight block
    /// matches (that needs a layout lookup — future work).
    find_block_matches: Vec<weft_core::find::BlockMatch>,
    /// Current index into `find_block_matches` (for Enter cycling in block
    /// view). Grid view uses `find_index` into `find_matches`.
    find_block_index: usize,
    /// True when `find_block_matches` was truncated at MAX_MATCHES.
    find_block_truncated: bool,
    /// Regex mode toggle (Cmd+R while find is open). Visual-only for now —
    /// the actual regex search engine isn't wired yet, so toggling this
    /// doesn't change search behavior. The renderer shows a lit ".*"
    /// indicator when true.
    find_regex_mode: bool,
    /// Case-sensitive toggle (Cmd+I while find is open, or click "Aa" in
    /// the popup). When false (default), search is case-insensitive;
    /// when true, character case must match exactly.
    find_case_sensitive: bool,
    /// Background find worker (v0.9 U-P1). Runs `find_in_snapshot` on a
    /// dedicated thread so large scrollback searches don't block the render
    /// loop. Submit via `find_worker.submit(...)` and poll results via
    /// `find_worker.try_recv_result()` in the redraw path.
    find_worker: find_worker::FindWorker,
    /// Latest regex compile error message (None = no error / not regex mode).
    /// Surfaced in the FindUI as "invalid regex" so the user knows the query
    /// failed to compile (v0.9 U-P2).
    find_regex_error: Option<String>,
    /// True when a find query is in-flight on the worker. Used to suppress
    /// redundant submits while a scan is running.
    find_worker_busy: bool,
}

/// Which border is being dragged to resize a popup.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DragTarget {
    /// Right border — adjusts width.
    Right,
    /// Top border — adjusts height (max rows).
    Top,
}

/// Right-click context menu on a block (F7).
#[allow(dead_code)]
struct ContextMenu {
    /// Target block for the menu actions. `None` means the target is the
    /// in-flight (running) command, which has no finalized BlockId yet.
    block_id: Option<BlockId>,
    /// Menu popup position (physical pixels).
    x: f32,
    y: f32,
    /// Selected menu item index.
    selection: usize,
}

/// Context menu item labels.
const CONTEXT_MENU_ITEMS: &[(&str, &str)] = &[
    ("Copy Command", "copy_command"),
    ("Copy Output", "copy_output"),
    ("Toggle Fold", "toggle_fold"),
    // v0.9 W4: send the block's command to the input box for re-editing
    // (Warp-style "rerun" — user can tweak parameters before pressing Enter).
    ("Send to Input", "send_to_input"),
];

/// Active popup border drag state.
#[derive(Clone)]
struct DragState {
    target: DragTarget,
    /// Starting mouse position for delta calculation.
    start_x: f64,
    start_y: f64,
    /// Starting scale/rows for delta calculation.
    start_scale: f32,
    start_rows: usize,
    /// Cell width at drag start (for delta→cols/rows conversion).
    #[allow(dead_code)]
    cell_w: f32,
    cell_h: f32,
}

// ── Command Palette types ─────────────────────────────────────────────

/// A single entry in the palette results list.
#[derive(Clone)]
enum PaletteEntry {
    Workflow(weft_core::workflow::Workflow),
    Builtin(BuiltinCmd),
}

/// Built-in commands that appear in the palette.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BuiltinCmd {
    ToggleTheme,
    SelectTheme,
    ToggleBlockPanel,
    ReloadConfig,
}

impl BuiltinCmd {
    fn label(&self) -> &'static str {
        match self {
            BuiltinCmd::ToggleTheme => "Toggle Theme",
            BuiltinCmd::SelectTheme => "Select Theme",
            BuiltinCmd::ToggleBlockPanel => "Toggle History Panel",
            BuiltinCmd::ReloadConfig => "Reload Config",
        }
    }
}

/// Active variable-fill form for a selected workflow.
#[allow(dead_code)]
struct WorkflowForm {
    workflow_id: i64,
    workflow_name: String,
    workflow_description: String,
    var_names: Vec<String>,
    var_values: Vec<String>,
    current_field: usize,
}

/// Sub-modes of the palette beyond normal search.
enum PaletteSubMode {
    /// Normal search/filter mode.
    Search,
    /// Creating a new workflow: guided step-by-step entry.
    CreateWorkflow {
        step: CreateStep,
        buffer: String,
        // Partially built workflow fields:
        name: String,
        command: String,
    },
    /// Editing an existing workflow's command.
    EditWorkflow {
        id: i64,
        name: String,
        buffer: String,
    },
    /// Confirming deletion of a workflow.
    ConfirmDelete { id: i64, name: String },
    /// v0.9 W2+: Theme picker sub-mode. `themes` holds resolvable theme names
    /// (built-ins + custom files from ~/.config/weft/themes/). `buffer` is
    /// the filter query. `selection` reuses `palette_selection`.
    SelectTheme { buffer: String, themes: Vec<String> },
}

/// Steps in the create-workflow guided entry.
#[derive(PartialEq, Eq)]
enum CreateStep {
    Name,
    Command,
    Done,
}

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
        &self.tabs[self.active_tab]
    }

    /// Mutable borrow of the active tab.
    fn tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active_tab]
    }

    fn new(proxy: EventLoopProxy<AppEvent>) -> Self {
        let config = Config::load();
        info!(
            theme = %config.theme.name,
            font = %config.font.family,
            size = config.font.size,
            "config loaded"
        );
        let keybindings = config.keybindings();
        Self {
            window: None,
            renderer: None,
            tabs: Vec::new(),
            active_tab: 0,
            hovered_tab: None,
            mods: winit::event::Modifiers::default(),
            cursor_blink_on: true,
            cursor_blink_phase: 0.0,
            cursor_blink_time: std::time::Instant::now(),
            last_mouse_x: 0.0,
            last_mouse_y: 0.0,
            last_resize_instant: std::time::Instant::now(),
            path_bins: scan_path_bins(),
            proxy,
            config,
            keybindings,
            theme_is_dark: true, // default to dark theme
            last_system_appearance_dark: None,
            last_appearance_check: std::time::Instant::now(),
            block_store: None,
            panel_open: false,
            panel_query: String::new(),
            panel_selection: 0,
            panel_expanded: None,
            panel_search_focused: false,
            prompt_dragging: false,
            panel_highlight: None,
            panel_highlight_until: None,
            panel_last_click: None,
            palette_open: false,
            popup_width_scale: 0.6,
            popup_max_rows: 8,
            drag_state: None,
            context_menu: None,
            palette_query: String::new(),
            palette_selection: 0,
            palette_last_click: None,
            palette_results: Vec::new(),
            palette_form: None,
            palette_submode: PaletteSubMode::Search,
            workflow_store: None,
            font_scale: 1.0,
            find_open: false,
            find_query: String::new(),
            find_last_key: None,
            find_matches: Vec::new(),
            find_index: 0,
            find_truncated: false,
            find_block_matches: Vec::new(),
            find_block_index: 0,
            find_block_truncated: false,
            find_regex_mode: false,
            find_case_sensitive: false,
            find_worker: find_worker::FindWorker::spawn(),
            find_regex_error: None,
            find_worker_busy: false,
        }
    }

    fn spawn_pty(&mut self, rows: usize, cols: usize) {
        let tab = Tab::new(rows, cols, self.config.scrollback.lines, &self.proxy);
        self.tabs.push(tab);
    }

    /// Non-blocking drain of PTY events into channel. Drains ALL tabs per
    /// frame (v0.9 H1 decision: background tabs keep their PTY buffers
    /// flushed so switching to them is instant; only the active tab is
    /// rendered).
    fn pump_pty(&mut self) {
        for tab in &mut self.tabs {
            tab.pump_pty();
        }
    }

    fn process_messages(&mut self) -> bool {
        let mut any_redraw = false;
        let mut drained_blocks: Vec<weft_core::blocks::Block> = Vec::new();
        for i in 0..self.tabs.len() {
            let (alive, drained, need_redraw) = self.tabs[i].process_messages();
            if !alive {
                // Shell exited on tab `i`. For now (Stage 2) we only exit
                // the app when the LAST tab's shell exits. A closed tab
                // via Cmd+W is handled by `close_tab`, not here.
                if self.tabs.len() <= 1 {
                    return false;
                }
                // Otherwise: remove the exited tab and switch to the prev.
                self.tabs.remove(i);
                if self.active_tab >= self.tabs.len() {
                    self.active_tab = self.tabs.len() - 1;
                }
                info!(closed = i, active = self.active_tab, "tab shell exited");
                break;
            }
            drained_blocks.extend(drained);
            if need_redraw {
                any_redraw = true;
            }
        }
        if !drained_blocks.is_empty() {
            if let Some(store) = &self.block_store {
                for block in &drained_blocks {
                    if let Err(e) = store.insert(block) {
                        warn!(error = %e, "failed to persist block");
                    }
                }
            }
        }
        if any_redraw {
            self.request_redraw();
        }
        true
    }

    fn request_redraw(&self) {
        if let (Some(window), Some(_renderer)) = (&self.window, &self.renderer) {
            window.request_redraw();
        }
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
        if self.find_open
            && m.contains(Modifiers::SUPER)
            && matches!(key, KeyCode::Char('v') | KeyCode::Char('a'))
        {
            if key == KeyCode::Char('v') {
                if let Some(text) = clipboard_paste() {
                    self.find_query.push_str(&text);
                    self.find_last_key = Some(std::time::Instant::now());
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
        if let Some(action) = self.keybindings.lookup(key, m) {
            if self.execute_action(action) {
                return;
            }
        }

        // Command Palette (highest overlay priority — captures all keys when open).
        if self.palette_open && self.handle_palette_key(key, m, text) {
            return;
        }

        // FindInGrid bar (just below palette in priority — both close on Esc
        // and capture typed text into their respective inputs).
        if self.find_open && self.handle_find_key(key, m, text) {
            return;
        }

        // v0.9 fix: panel search box has click-to-focus. When focused, typed
        // text goes to the panel query (filtering the history list) instead
        // of the editor. Esc unfocuses; clicking elsewhere also unfocuses.
        // This preserves sidebar-mode editor input while making the search
        // box functional (bug 6: "no search box / can't search").
        if self.panel_open && self.panel_search_focused && self.handle_panel_key(key, m) {
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

        let bytes = self.tab().input_handler.encode_key(key, m);
        // Diagnostic (set RUST_LOG=weft_app=debug to see): the exact bytes we
        // send for each key, including whether DECCKM/app-cursor mode is on.
        tracing::debug!(
            ?key,
            ?m,
            app_cursor_keys = self.tab().input_handler.app_cursor_keys,
            ?bytes,
            "key → pty"
        );
        if !bytes.is_empty() {
            if let Some(pty) = &self.tab().pty {
                if let Err(e) = pty.write_sync(&bytes) {
                    warn!("Failed to write to PTY: {e}");
                }
            }
        }
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
                self.paste_from_clipboard();
                true
            }
            Action::ReloadConfig => {
                self.reload_config();
                true
            }
            Action::ScrollPageUp
            | Action::ScrollPageDown
            | Action::ScrollToTop
            | Action::ScrollToBottom => {
                self.scroll_action(action);
                true
            }
            Action::ToggleBlockPanel => {
                self.panel_open = !self.panel_open;
                if self.panel_open {
                    // Fresh search/selection each time the panel opens.
                    self.panel_query.clear();
                    self.panel_selection = 0;
                    self.panel_expanded = None;
                    self.panel_search_focused = false;
                }
                // v0.9 W5: resize grid for sidebar so the terminal content
                // reflows beside the panel instead of being covered by it.
                self.recompute_layout();
                self.request_redraw();
                true
            }
            Action::ToggleCommandPalette => {
                self.palette_open = !self.palette_open;
                if self.palette_open {
                    // v0.9 fix: opening the palette closes the find bar (and
                    // vice versa) so only one modal owns keyboard input at a
                    // time. Without this, Cmd+F then Cmd+P leaves both
                    // popups open and keystrokes go to the wrong one.
                    self.close_find();
                    self.palette_query.clear();
                    self.palette_selection = 0;
                    self.palette_form = None;
                    self.palette_submode = PaletteSubMode::Search;
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
                self.find_open = !self.find_open;
                if self.find_open {
                    // v0.9 fix: opening find closes the palette (see above).
                    self.close_palette();
                    self.find_query.clear();
                    self.find_matches.clear();
                    self.find_index = 0;
                    self.find_truncated = false;
                    self.find_block_matches.clear();
                    self.find_block_index = 0;
                    self.find_block_truncated = false;
                    self.find_regex_mode = false;
                    self.find_case_sensitive = false;
                    self.find_last_key = None;
                    self.find_regex_error = None;
                    self.find_worker_busy = false;
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
                self.panel_search_focused = false;
                self.request_redraw();
                true
            }
            KeyCode::Up => {
                self.panel_selection = self.panel_selection.saturating_sub(1);
                self.clamp_panel_selection();
                self.request_redraw();
                true
            }
            KeyCode::Down => {
                self.panel_selection = self.panel_selection.saturating_add(1);
                self.clamp_panel_selection();
                self.request_redraw();
                true
            }
            KeyCode::Backspace => {
                self.panel_query.pop();
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
                self.panel_query.push(c);
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
        if !self.find_open {
            return;
        }
        self.find_open = false;
        self.find_query.clear();
        self.find_matches.clear();
        self.find_index = 0;
        self.find_truncated = false;
        self.find_block_matches.clear();
        self.find_block_index = 0;
        self.find_block_truncated = false;
        self.find_regex_mode = false;
        self.find_case_sensitive = false;
        self.find_last_key = None;
        self.find_regex_error = None;
        self.find_worker_busy = false;
    }

    /// v0.9: close the command palette and reset its state. Used when
    /// another modal (find bar, …) opens so only one owns keyboard input.
    fn close_palette(&mut self) {
        if !self.palette_open {
            return;
        }
        self.palette_open = false;
        self.palette_query.clear();
        self.palette_selection = 0;
        self.palette_form = None;
        self.palette_submode = PaletteSubMode::Search;
    }

    /// Refresh the palette search results from the workflow store + builtin commands.
    fn refresh_palette_results(&mut self) {
        let mut results = Vec::new();

        // Workflows from the store.
        if let Some(store) = &self.workflow_store {
            let workflows = if self.palette_query.is_empty() {
                store.list().unwrap_or_default()
            } else {
                store.search(&self.palette_query, 50).unwrap_or_default()
            };
            for wf in workflows {
                results.push(PaletteEntry::Workflow(wf));
            }
        }

        // Builtin commands (filtered by query if non-empty).
        let builtins = [
            BuiltinCmd::ToggleTheme,
            BuiltinCmd::SelectTheme,
            BuiltinCmd::ToggleBlockPanel,
            BuiltinCmd::ReloadConfig,
        ];
        for b in &builtins {
            let label = b.label();
            if self.palette_query.is_empty()
                || label
                    .to_lowercase()
                    .contains(&self.palette_query.to_lowercase())
            {
                results.push(PaletteEntry::Builtin(*b));
            }
        }

        self.palette_results = results;
        // Clamp selection.
        if self.palette_selection >= self.palette_results.len() {
            self.palette_selection = 0;
        }
    }

    /// Handle a key while the Command Palette is open. Returns true if consumed.
    fn handle_palette_key(&mut self, key: KeyCode, mods: Modifiers, text: Option<&str>) -> bool {
        // Let modifier chords fall through (so cmd+p can toggle closed).
        if mods.intersects(Modifiers::SUPER | Modifiers::CONTROL | Modifiers::ALT) {
            return false;
        }

        // If we're in form mode (filling workflow variables), route differently.
        if self.palette_form.is_some() {
            return self.handle_palette_form_key(key, mods, text);
        }

        // Route to sub-mode handler if not in Search.
        match &self.palette_submode {
            PaletteSubMode::CreateWorkflow { .. } => {
                return self.handle_palette_create_key(key, text);
            }
            PaletteSubMode::EditWorkflow { .. } => {
                return self.handle_palette_edit_key(key, text);
            }
            PaletteSubMode::ConfirmDelete { .. } => {
                return self.handle_palette_delete_key(key);
            }
            PaletteSubMode::SelectTheme { .. } => {
                return self.handle_palette_select_theme_key(key, text);
            }
            PaletteSubMode::Search => {}
        }

        match key {
            KeyCode::Escape => {
                self.palette_open = false;
                self.request_redraw();
                true
            }
            KeyCode::Up => {
                if self.palette_selection > 0 {
                    self.palette_selection -= 1;
                }
                self.request_redraw();
                true
            }
            KeyCode::Down => {
                if self.palette_selection + 1 < self.palette_results.len() {
                    self.palette_selection += 1;
                }
                self.request_redraw();
                true
            }
            KeyCode::Enter => {
                if let Some(entry) = self.palette_results.get(self.palette_selection).cloned() {
                    self.activate_palette_entry(entry);
                }
                true
            }
            KeyCode::Backspace => {
                // If query is empty and we were typing '>', clear it.
                if self.palette_query.is_empty() {
                    self.palette_submode = PaletteSubMode::Search;
                } else {
                    self.palette_query.pop();
                    self.palette_selection = 0;
                    self.refresh_palette_results();
                }
                self.request_redraw();
                true
            }
            _ => {
                let c = resolve_text_char(text, '\0', false);
                if c == '\0' || c.is_control() {
                    return false;
                }

                // Check for action shortcuts when a workflow is selected and
                // query is empty (single-char commands).
                if self.palette_query.is_empty() {
                    let action = match c {
                        '>' => Some("create"),
                        'e' | 'E' => Some("edit"),
                        'd' | 'D' => Some("delete"),
                        'x' | 'X' => Some("export"),
                        _ => None,
                    };
                    if let Some(act) = action {
                        return self.handle_palette_action(act);
                    }
                }

                self.palette_query.push(c);
                self.palette_selection = 0;
                self.refresh_palette_results();
                self.request_redraw();
                true
            }
        }
    }

    /// Handle a single-key palette action (create/edit/delete/export).
    fn handle_palette_action(&mut self, action: &str) -> bool {
        match action {
            "create" => {
                self.palette_submode = PaletteSubMode::CreateWorkflow {
                    step: CreateStep::Name,
                    buffer: String::new(),
                    name: String::new(),
                    command: String::new(),
                };
                self.request_redraw();
                true
            }
            "edit" => {
                if let Some(PaletteEntry::Workflow(wf)) =
                    self.palette_results.get(self.palette_selection).cloned()
                {
                    self.palette_submode = PaletteSubMode::EditWorkflow {
                        id: wf.id,
                        name: wf.name.clone(),
                        buffer: wf
                            .steps
                            .first()
                            .map(|s| s.command.clone())
                            .unwrap_or_default(),
                    };
                    self.request_redraw();
                    true
                } else {
                    false
                }
            }
            "delete" => {
                if let Some(PaletteEntry::Workflow(wf)) =
                    self.palette_results.get(self.palette_selection).cloned()
                {
                    self.palette_submode = PaletteSubMode::ConfirmDelete {
                        id: wf.id,
                        name: wf.name.clone(),
                    };
                    self.request_redraw();
                    true
                } else {
                    false
                }
            }
            "export" => {
                if let Some(store) = &self.workflow_store {
                    if let Some(PaletteEntry::Workflow(wf)) =
                        self.palette_results.get(self.palette_selection)
                    {
                        match store.export_yaml(wf.id) {
                            Ok(yaml) => {
                                info!(workflow = %wf.name, "workflow YAML exported to log");
                                tracing::debug!(yaml = %yaml, "exported workflow YAML");
                            }
                            Err(e) => warn!(error = %e, "failed to export workflow YAML"),
                        }
                    }
                }
                // Export doesn't change mode — stay in search.
                false
            }
            _ => false,
        }
    }

    /// Handle keys while the FindInGrid bar is open. The bar consumes all
    /// non-modifier keystrokes into the query input; Esc closes, Enter /
    /// Shift+Enter navigate next/prev match, Cmd+F toggles closed (handled
    /// by the keybinding resolution above, so it never reaches here).
    fn handle_find_key(&mut self, key: KeyCode, mods: Modifiers, text: Option<&str>) -> bool {
        // Cmd+R: toggle regex mode (visual indicator only — actual regex
        // search engine not yet wired, so this doesn't change results yet).
        if mods.contains(Modifiers::SUPER) && key == KeyCode::Char('r') {
            self.find_regex_mode = !self.find_regex_mode;
            self.request_redraw();
            return true;
        }
        // Cmd+I: toggle case-sensitive search.
        if mods.contains(Modifiers::SUPER) && key == KeyCode::Char('i') {
            self.find_case_sensitive = !self.find_case_sensitive;
            // Re-run the search immediately so the toggle is reflected.
            self.find_last_key = Some(std::time::Instant::now());
            self.request_redraw();
            return true;
        }
        if mods.intersects(Modifiers::SUPER | Modifiers::CONTROL | Modifiers::ALT) {
            return false;
        }
        match key {
            KeyCode::Escape => {
                self.find_open = false;
                self.request_redraw();
                true
            }
            KeyCode::Enter => {
                // Shift+Enter = previous, Enter = next.
                self.find_cycle_next_prev(!mods.contains(Modifiers::SHIFT));
                true
            }
            KeyCode::Up | KeyCode::Down => {
                // Arrow keys cycle prev/next, mirroring Warp's find popup.
                self.find_cycle_next_prev(key == KeyCode::Down);
                true
            }
            KeyCode::Backspace => {
                if self.find_query.pop().is_some() {
                    self.find_last_key = Some(std::time::Instant::now());
                    self.request_redraw();
                }
                true
            }
            _ => {
                // v0.9 fix: use resolve_text_char to fall back to the key
                // char when `text` is None (winit doesn't populate text for
                // all printable keys, e.g. `-` on some layouts). This
                // matches the editor's behavior.
                if let KeyCode::Char(c) = key {
                    let resolved = resolve_text_char(text, c, mods.contains(Modifiers::SHIFT));
                    if !resolved.is_control() {
                        self.find_query.push(resolved);
                        self.find_last_key = Some(std::time::Instant::now());
                        self.request_redraw();
                        return true;
                    }
                }
                if let Some(t) = text {
                    if !t.is_empty() {
                        self.find_query.push_str(t);
                        self.find_last_key = Some(std::time::Instant::now());
                        self.request_redraw();
                        return true;
                    }
                }
                false
            }
        }
    }

    /// Cycle the find popup's current match forward (`next = true`) or
    /// backward (`next = false`). Used by Enter / Shift+Enter, Up/Down
    /// arrow keys, and the up/down buttons in the popup. In block view,
    /// cycles through block matches; in grid view, cycles through grid
    /// matches. No-op when there are no matches.
    fn find_cycle_next_prev(&mut self, next: bool) {
        if !self.find_block_matches.is_empty() && self.block_view_active() {
            let len = self.find_block_matches.len();
            if next {
                self.find_block_index = (self.find_block_index + 1) % len;
            } else if self.find_block_index == 0 {
                self.find_block_index = len - 1;
            } else {
                self.find_block_index -= 1;
            }
            // v0.9 fix: auto-expand the block containing the current match so
            // the highlighted hit is visible. If a match is inside a folded
            // block's output, expanding it reveals the matching line. Command
            // matches are always visible (the command line shows even when
            // folded), so only expand for output matches.
            let need_expand = self
                .find_block_matches
                .get(self.find_block_index)
                .map(|bm| !bm.is_command)
                .unwrap_or(false);
            if need_expand {
                if let Some(bm) = self.find_block_matches.get(self.find_block_index) {
                    if let Some(term) = self.tabs[self.active_tab].terminal.as_mut() {
                        let block = term
                            .block_tracker()
                            .session_blocks()
                            .iter()
                            .find(|b| b.id == bm.block_id)
                            .cloned();
                        if let Some(b) = block {
                            if b.collapsed {
                                term.block_tracker_mut().toggle_collapse(b.id);
                            }
                        }
                    }
                }
            }
            self.scroll_to_current_find_match();
            self.request_redraw();
        } else if !self.find_matches.is_empty() {
            let len = self.find_matches.len();
            if next {
                self.find_index = (self.find_index + 1) % len;
            } else if self.find_index == 0 {
                self.find_index = len - 1;
            } else {
                self.find_index -= 1;
            }
            self.scroll_to_current_find_match();
            self.request_redraw();
        }
    }

    /// Run the search if the debounce window has elapsed. Called from the
    /// redraw path; safe to call every frame — it no-ops when no search is
    /// pending or the debounce hasn't expired.
    ///
    /// v0.9 U-P1: grid search is now async — we submit a `FindSnapshot` to
    /// the background `FindWorker` and drain results in `poll_find_worker_results`.
    /// This avoids blocking the render thread on large scrollbacks. Block
    /// search stays synchronous (block output is plain `String` — scanning
    /// is O(text size), not O(grid cells × flags), and is rarely the bottleneck).
    fn maybe_refresh_find_results(&mut self) {
        let Some(t) = self.find_last_key else {
            return;
        };
        if t.elapsed() < std::time::Duration::from_millis(150) {
            return;
        }
        self.find_last_key = None;

        // Access the active tab's terminal via direct field indexing so the
        // borrow is split to `self.tabs` — the find_* fields below can then
        // be mutated without a borrow conflict (going through `self.tab()`
        // would borrow all of `self`).
        let Some(term) = self.tabs[self.active_tab].terminal.as_ref() else {
            return;
        };

        // Clear any stale regex error when starting a new search.
        self.find_regex_error = None;

        // Submit grid search to the background worker (async, non-blocking).
        // The snapshot creation (~2-3ms for 10K rows) is the only main-thread
        // cost; the scan itself runs on the worker thread.
        if !self.find_query.is_empty() {
            let snapshot = std::sync::Arc::new(term.grid().find_snapshot());
            self.find_worker.submit(
                self.find_query.clone(),
                self.find_case_sensitive,
                self.find_regex_mode,
                snapshot,
            );
            self.find_worker_busy = true;
        } else {
            // Empty query → no matches. Clear immediately (no need to wait
            // for the worker).
            self.find_matches.clear();
            self.find_truncated = false;
            self.find_index = 0;
            self.find_worker_busy = false;
        }

        // Search block history + in-flight block synchronously when in
        // block view. This is fast (String scanning) and the matches are
        // needed immediately for the FindUI count.
        // v0.9 U-P2: pass is_regex so regex mode works in block view too.
        if term.show_block_view() {
            let blocks = term.block_tracker().session_blocks();
            let mut block_matches = match weft_core::find::find_in_blocks(
                blocks,
                &self.find_query,
                self.find_case_sensitive,
                self.find_regex_mode,
            ) {
                Ok(m) => m,
                Err(e) => {
                    self.find_regex_error = Some(e.0);
                    self.find_block_matches.clear();
                    self.find_block_truncated = false;
                    self.find_block_index = 0;
                    self.request_redraw();
                    return;
                }
            };
            if let Some(live) = term.block_tracker().in_flight() {
                match weft_core::find::find_in_flight(
                    &live,
                    &self.find_query,
                    self.find_case_sensitive,
                    self.find_regex_mode,
                ) {
                    Ok(mut live_matches) => block_matches.append(&mut live_matches),
                    Err(e) => {
                        self.find_regex_error = Some(e.0);
                    }
                }
            }
            self.find_block_truncated =
                block_matches.len() >= weft_core::find::MAX_MATCHES && !self.find_query.is_empty();
            self.find_block_matches = block_matches;
            if !self.find_block_matches.is_empty() {
                self.find_block_index =
                    self.find_block_index.min(self.find_block_matches.len() - 1);
                self.scroll_to_current_find_match();
            } else {
                self.find_block_index = 0;
            }
        } else {
            self.find_block_matches.clear();
            self.find_block_truncated = false;
            self.find_block_index = 0;
        }

        self.request_redraw();
    }

    /// Drain pending find-worker results (v0.9 U-P1). Called every frame
    /// from the redraw path. When a `Complete` or `Partial` result arrives,
    /// updates `find_matches` / `find_truncated` and scrolls to the current
    /// match. When `RegexInvalid` arrives, surfaces the error in the FindUI.
    fn poll_find_worker_results(&mut self) {
        if !self.find_worker_busy {
            return;
        }
        while let Some(result) = self.find_worker.try_recv_result() {
            match result {
                find_worker::FindResult::Partial { matches } => {
                    // Incremental results — paint them so the user sees
                    // matches appear as the scan progresses.
                    // v0.9 fix: update find_matches regardless of block view —
                    // grid matches are needed for the FindUI count and for
                    // scrolling when the user navigates. Block matches are a
                    // separate field and don't conflict.
                    if !matches.is_empty() {
                        self.find_index = self.find_index.min(matches.len() - 1);
                    } else {
                        self.find_index = 0;
                    }
                    self.find_matches = matches;
                    if !self.block_view_active() {
                        self.scroll_to_current_find_match();
                    }
                    self.request_redraw();
                }
                find_worker::FindResult::Complete { matches, truncated } => {
                    if !matches.is_empty() {
                        self.find_index = self.find_index.min(matches.len() - 1);
                    } else {
                        self.find_index = 0;
                    }
                    self.find_matches = matches;
                    self.find_truncated = truncated;
                    if !self.block_view_active() {
                        self.scroll_to_current_find_match();
                    }
                    self.find_worker_busy = false;
                    self.request_redraw();
                    // Done — break out of the drain loop.
                    break;
                }
                find_worker::FindResult::RegexInvalid(msg) => {
                    self.find_regex_error = Some(msg);
                    self.find_matches.clear();
                    self.find_truncated = false;
                    self.find_index = 0;
                    self.find_worker_busy = false;
                    self.request_redraw();
                    break;
                }
                find_worker::FindResult::Cancelled => {
                    // A newer query is in flight — keep `find_worker_busy`
                    // true; the newer query's results will arrive soon.
                }
            }
        }
    }

    /// Scroll the viewport so the current find match is visible. In grid
    /// view, adjusts `grid.scroll_offset` to bring the match to the middle
    /// viewport row. In block view, adjusts `block_scroll_offset` to bring
    /// the matching block into the visible region.
    fn scroll_to_current_find_match(&mut self) {
        // Block view: scroll to the block containing the current block match.
        if self.block_view_active() && !self.find_block_matches.is_empty() {
            let bm = self.find_block_matches.get(self.find_block_index).cloned();
            let Some(bm) = bm else { return };
            let Some(term) = self.tabs[self.active_tab].terminal.as_ref() else {
                return;
            };
            // Find the block's index in session_blocks to compute its row
            // offset from the bottom. Blocks are laid out bottom-to-top:
            // the newest (highest index) is at the bottom. The row offset
            // from the bottom = sum of rows of all blocks BELOW it + its
            // own offset within. We approximate by scrolling to bring the
            // block's command line to the middle of the viewport.
            let blocks = term.block_tracker().session_blocks();
            let block_idx = blocks.iter().position(|b| b.id == bm.block_id);
            let Some(block_idx) = block_idx else { return };
            // Count rows from the bottom up to this block's matching line.
            // Actual layout (bottom→top within a block):
            //   Output[N-1] (last printed)  → row 1 from bottom
            //   Output[N-2]                 → row 2
            //   …
            //   Output[0] (first printed)   → row N
            //   Command                     → row N+1
            //   Header                      → row N+2
            //   Separator                   → row N+3
            // where N = trimmed output line count. So for an output match at
            // `bm.line`, the in-block offset from the bottom is `N - bm.line`.
            // For a command match, it's `N + 1`.
            // (The previous code used `bm.line + 3` which treated the layout
            // as top-to-bottom — that was inverted, causing the viewport to
            // jump to the wrong position and the highlight to land off-screen.)
            let trim_output_lines = |b: &weft_core::blocks::Block| -> usize {
                if b.collapsed {
                    return 0;
                }
                let mut lines: Vec<&str> = b.output.lines().collect();
                while lines.last().is_some_and(|l| {
                    let t = l.trim();
                    t.is_empty() || matches!(t, "%" | "$" | "#")
                }) {
                    lines.pop();
                }
                lines.len()
            };
            let mut rows_from_bottom = 0usize;
            for (i, b) in blocks.iter().enumerate().rev() {
                if i == block_idx {
                    break;
                }
                rows_from_bottom += 3 + trim_output_lines(b);
            }
            let matching_output_lines = trim_output_lines(&blocks[block_idx]);
            let line_in_block = if bm.is_command {
                matching_output_lines + 1
            } else {
                matching_output_lines.saturating_sub(bm.line)
            };
            rows_from_bottom += line_in_block;
            // Bring it to roughly the middle of the viewport.
            let Some(renderer) = self.renderer.as_ref() else {
                return;
            };
            let visible = renderer.block_visible_rows(1);
            // Scroll so the matching row lands at ~visible/2 from the bottom
            // of the viewport. block_scroll_offset is "rows scrolled up from
            // the bottom", so target = rows_from_bottom - visible/2.
            // (Previously this was ADDING visible/2, which scrolled PAST the
            // match — the highlight was drawn but outside the clip region.)
            let cols = term.grid().num_cols;
            let (total, _) = block_content_metrics(term, cols);
            let max_scroll = total.saturating_sub(visible);
            let target = rows_from_bottom.saturating_sub(visible / 2).min(max_scroll);
            self.tabs[self.active_tab].block_scroll_offset = target;
            return;
        }
        // Grid view: scroll grid to bring the match to the middle row.
        let Some(m) = self.find_matches.get(self.find_index).copied() else {
            return;
        };
        let Some(term) = self.tabs[self.active_tab].terminal.as_mut() else {
            return;
        };
        let grid = term.grid_mut();
        let sb_len = grid.scrollback_len();
        let mid = grid.num_rows / 2;
        let target_offset = if m.row >= sb_len {
            0
        } else {
            (sb_len + mid).saturating_sub(m.row).min(sb_len)
        };
        if grid.scroll_offset != target_offset {
            grid.scroll_offset = target_offset;
            term.clear_hyperlink_cell_map();
        }
    }

    /// Handle keys in CreateWorkflow sub-mode (guided step-by-step entry).
    fn handle_palette_create_key(&mut self, key: KeyCode, text: Option<&str>) -> bool {
        let PaletteSubMode::CreateWorkflow {
            step,
            buffer,
            name,
            command,
        } = &mut self.palette_submode
        else {
            return false;
        };

        match key {
            KeyCode::Escape => {
                self.palette_submode = PaletteSubMode::Search;
                self.request_redraw();
                true
            }
            KeyCode::Enter => {
                match step {
                    CreateStep::Name => {
                        if buffer.is_empty() {
                            return true; // ignore empty name
                        }
                        *name = std::mem::take(buffer);
                        *step = CreateStep::Command;
                        self.request_redraw();
                        true
                    }
                    CreateStep::Command => {
                        if buffer.is_empty() {
                            return true;
                        }
                        *command = std::mem::take(buffer);
                        *step = CreateStep::Done;

                        // Create the workflow in the store.
                        let wf = weft_core::workflow::Workflow {
                            id: 0,
                            name: name.clone(),
                            description: "User-created workflow".into(),
                            steps: vec![weft_core::workflow::WorkflowStep {
                                command: command.clone(),
                            }],
                            variables: vec![],
                            source: weft_core::workflow::WorkflowSource::Manual,
                            use_count: 0,
                            last_used_ms: 0,
                        };
                        if let Some(store) = &self.workflow_store {
                            if let Err(e) = store.insert(&wf) {
                                warn!(error = %e, "failed to save new workflow");
                            } else {
                                info!(name = %wf.name, "workflow created");
                            }
                        }

                        // Return to search mode and refresh.
                        self.palette_submode = PaletteSubMode::Search;
                        self.palette_query.clear();
                        self.refresh_palette_results();
                        self.request_redraw();
                        true
                    }
                    CreateStep::Done => true,
                }
            }
            KeyCode::Backspace => {
                buffer.pop();
                self.request_redraw();
                true
            }
            _ => {
                let c = resolve_text_char(text, '\0', false);
                if c != '\0' && !c.is_control() {
                    buffer.push(c);
                    self.request_redraw();
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Handle keys in EditWorkflow sub-mode.
    fn handle_palette_edit_key(&mut self, key: KeyCode, text: Option<&str>) -> bool {
        let (id, name) = match &self.palette_submode {
            PaletteSubMode::EditWorkflow { id, name, .. } => (*id, name.clone()),
            _ => return false,
        };

        match key {
            KeyCode::Escape => {
                self.palette_submode = PaletteSubMode::Search;
                self.request_redraw();
                true
            }
            KeyCode::Enter => {
                let new_command = match &self.palette_submode {
                    PaletteSubMode::EditWorkflow { buffer, .. } => buffer.clone(),
                    _ => return false,
                };

                // Update the workflow in the store.
                if let Some(store) = &self.workflow_store {
                    if let Some(mut wf) = store.find_by_name(&name).unwrap_or(None) {
                        if let Some(step) = wf.steps.first_mut() {
                            step.command = new_command.clone();
                        } else {
                            // No steps — append the new command as the first step.
                            wf.steps.push(weft_core::workflow::WorkflowStep {
                                command: new_command.clone(),
                            });
                        }
                        if let Err(e) = store.update(&wf) {
                            warn!(error = %e, "failed to update workflow");
                        } else {
                            info!(name = %name, "workflow updated");
                        }
                    }
                }
                let _ = id; // id already used via find_by_name
                self.palette_submode = PaletteSubMode::Search;
                self.palette_query.clear();
                self.refresh_palette_results();
                self.request_redraw();
                true
            }
            KeyCode::Backspace => {
                if let PaletteSubMode::EditWorkflow { buffer, .. } = &mut self.palette_submode {
                    buffer.pop();
                }
                self.request_redraw();
                true
            }
            _ => {
                let c = resolve_text_char(text, '\0', false);
                if c != '\0' && !c.is_control() {
                    if let PaletteSubMode::EditWorkflow { buffer, .. } = &mut self.palette_submode {
                        buffer.push(c);
                    }
                    self.request_redraw();
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Handle keys in ConfirmDelete sub-mode.
    fn handle_palette_delete_key(&mut self, key: KeyCode) -> bool {
        match key {
            KeyCode::Escape => {
                self.palette_submode = PaletteSubMode::Search;
                self.request_redraw();
                true
            }
            KeyCode::Enter => {
                let (id, name) = match &self.palette_submode {
                    PaletteSubMode::ConfirmDelete { id, name } => (*id, name.clone()),
                    _ => return false,
                };
                if let Some(store) = &self.workflow_store {
                    if let Err(e) = store.delete(id) {
                        warn!(error = %e, "failed to delete workflow");
                    } else {
                        info!(name = %name, "workflow deleted");
                    }
                }
                self.palette_submode = PaletteSubMode::Search;
                self.palette_query.clear();
                self.refresh_palette_results();
                self.request_redraw();
                true
            }
            _ => true, // consume all other keys in confirm mode
        }
    }

    /// Handle keys while in the workflow variable form sub-mode.
    fn handle_palette_form_key(
        &mut self,
        key: KeyCode,
        _mods: Modifiers,
        text: Option<&str>,
    ) -> bool {
        match key {
            KeyCode::Escape => {
                // Return to search mode (keep palette open).
                self.palette_form = None;
                self.request_redraw();
                true
            }
            KeyCode::Tab => {
                if let Some(form) = &mut self.palette_form {
                    if form.current_field + 1 < form.var_names.len() {
                        form.current_field += 1;
                    } else {
                        form.current_field = 0; // wrap
                    }
                }
                self.request_redraw();
                true
            }
            KeyCode::Enter => {
                // Execute the workflow with the filled variables.
                let form = self.palette_form.take();
                if let Some(form) = form {
                    self.execute_workflow(form);
                }
                self.palette_open = false;
                self.request_redraw();
                true
            }
            KeyCode::Backspace => {
                if let Some(form) = &mut self.palette_form {
                    if form.current_field < form.var_values.len() {
                        form.var_values[form.current_field].pop();
                    }
                }
                self.request_redraw();
                true
            }
            _ => {
                let c = resolve_text_char(text, '\0', false);
                if c != '\0' && !c.is_control() {
                    if let Some(form) = &mut self.palette_form {
                        if form.current_field < form.var_values.len() {
                            form.var_values[form.current_field].push(c);
                        }
                    }
                    self.request_redraw();
                    return true;
                }
                false
            }
        }
    }

    /// Activate a palette entry: workflow → enter form mode, builtin → execute.
    fn activate_palette_entry(&mut self, entry: PaletteEntry) {
        match entry {
            PaletteEntry::Workflow(wf) => {
                let var_names = wf.all_var_names();
                if var_names.is_empty() {
                    // No variables — execute immediately (skip the form).
                    let form = WorkflowForm {
                        workflow_id: wf.id,
                        workflow_name: wf.name.clone(),
                        workflow_description: wf.description.clone(),
                        var_names: Vec::new(),
                        var_values: Vec::new(),
                        current_field: 0,
                    };
                    self.execute_workflow(form);
                    self.palette_open = false;
                    self.request_redraw();
                } else {
                    // Has variables — enter form-fill mode.
                    let var_count = var_names.len();
                    self.palette_form = Some(WorkflowForm {
                        workflow_id: wf.id,
                        workflow_name: wf.name.clone(),
                        workflow_description: wf.description.clone(),
                        var_names,
                        var_values: vec![String::new(); var_count],
                        current_field: 0,
                    });
                    self.request_redraw();
                }
            }
            PaletteEntry::Builtin(cmd) => {
                match cmd {
                    BuiltinCmd::ToggleTheme => {
                        self.toggle_theme();
                        self.palette_open = false;
                    }
                    BuiltinCmd::SelectTheme => {
                        // v0.9 W2+: enter theme picker sub-mode instead of
                        // closing the palette. List built-in themes + custom
                        // theme files from ~/.config/weft/themes/.
                        let themes = self.available_theme_names();
                        self.palette_submode = PaletteSubMode::SelectTheme {
                            buffer: String::new(),
                            themes,
                        };
                        self.palette_query.clear();
                        self.palette_selection = 0;
                        self.refresh_theme_picker_results();
                        // NOTE: do NOT close the palette — user must pick.
                    }
                    BuiltinCmd::ToggleBlockPanel => {
                        self.execute_action(Action::ToggleBlockPanel);
                        self.palette_open = false;
                    }
                    BuiltinCmd::ReloadConfig => {
                        self.execute_action(Action::ReloadConfig);
                        self.palette_open = false;
                    }
                }
                self.request_redraw();
            }
        }
    }

    /// Execute a workflow: render variables → submit commands to PTY.
    fn execute_workflow(&mut self, form: WorkflowForm) {
        let Some(store) = &self.workflow_store else {
            return;
        };
        let wf = store.find_by_name(&form.workflow_name).ok().flatten();
        let Some(wf) = wf else {
            return;
        };

        // Build variable values map.
        let mut values = std::collections::HashMap::new();
        for (name, val) in form.var_names.iter().zip(form.var_values.iter()) {
            values.insert(name.clone(), val.clone());
        }

        match wf.render(&values) {
            Ok(commands) => {
                for cmd in &commands {
                    let tab = &mut self.tabs[self.active_tab];
                    if let Some(terminal) = &mut tab.terminal {
                        // Set the command text and submit via the editor path.
                        terminal.editor_mut().buffer.set_text(cmd);
                        let bytes = terminal.submit_command();
                        if !bytes.is_empty() {
                            if let Some(pty) = &tab.pty {
                                let _ = pty.write_sync(&bytes);
                            }
                        }
                    }
                }
                // Update use count.
                let _ = store.bump_use_count(wf.id);
            }
            Err(e) => {
                warn!(error = %e, "workflow render failed");
            }
        }
    }
    /// Returns true if consumed. Ctrl chords that aren't editor ops fall through
    /// (returns false) so Ctrl+C etc. still reach the PTY.
    fn handle_editor_key(&mut self, key: KeyCode, mods: Modifiers, text: Option<&str>) -> bool {
        use weft_core::input::{KeyCode::*, Modifiers};
        let shift = mods.contains(Modifiers::SHIFT);

        // v0.9 fix: Cmd (SUPER) chords are app-level shortcuts (copy/paste/
        // tab/panel…), not editor input. If a Cmd chord reaches here it
        // means no keybinding matched — drop it instead of inserting the
        // character into the editor (e.g. Cmd+Shift+V was inserting 'V').
        if mods.contains(Modifiers::SUPER) {
            return false;
        }

        // v0.9: any non-Cmd editor key clears the mouse-drag selection so
        // typing replaces the selection. Cmd+C is handled above (returns
        // false) so it won't clear the selection — copy still works.
        if let Some(t) = self.tabs[self.active_tab].terminal.as_mut() {
            if t.editor().buffer.has_selection() {
                t.editor_mut().buffer.clear_selection();
                self.request_redraw();
            }
        }
        self.prompt_dragging = false;

        // Ctrl editor ops (Ctrl+C / other Ctrl chords fall through to the PTY).
        if mods.contains(Modifiers::CONTROL) && !mods.contains(Modifiers::ALT) {
            let consumed = if let Some(t) = self.tabs[self.active_tab].terminal.as_mut() {
                let e = t.editor_mut();
                match key {
                    Char('a') => {
                        e.buffer.move_line_home();
                        true
                    }
                    Char('e') => {
                        e.buffer.move_line_end();
                        true
                    }
                    Char('w') => {
                        e.buffer.delete_word_back();
                        true
                    }
                    Char('u') => {
                        e.buffer.clear_line();
                        true
                    }
                    Char('k') => {
                        e.buffer.delete_to_end();
                        true
                    }
                    Char('r') => {
                        if e.is_searching() {
                            e.search_next();
                        } else {
                            e.search_start();
                        }
                        true
                    }
                    _ => false,
                }
            } else {
                false
            };
            return consumed;
        }

        // Ctrl+R search mode intercepts printable/backspace/enter/esc/arrows.
        let searching = self.tabs[self.active_tab]
            .terminal
            .as_ref()
            .map(|t| t.editor().is_searching())
            .unwrap_or(false);
        if searching {
            if let Some(t) = self.tabs[self.active_tab].terminal.as_mut() {
                let e = t.editor_mut();
                match key {
                    Char(c) => {
                        e.search_input(resolve_text_char(text, c, shift));
                        return true;
                    }
                    Backspace => {
                        e.search_backspace();
                        return true;
                    }
                    Enter => {
                        e.search_accept();
                        return true;
                    }
                    Escape => {
                        e.search_cancel();
                        return true;
                    }
                    Up => {
                        e.search_prev();
                        return true;
                    }
                    Down => {
                        e.search_next();
                        return true;
                    }
                    _ => {}
                }
            }
            return false;
        }

        // Tab-completion mode: Tab cycles, Enter accepts (no submit), Up/Down
        // navigate, Esc cancels. Any other key cancels and falls through to
        // normal editing (so typing/deleting ends the session).
        let completing = self.tabs[self.active_tab]
            .terminal
            .as_ref()
            .map(|t| t.editor().is_completing())
            .unwrap_or(false);
        if completing {
            let consumed = match key {
                Tab => {
                    self.editor_completion_next();
                    true
                }
                Enter => {
                    self.editor_completion_accept();
                    true
                }
                Up => {
                    self.editor_completion_prev();
                    true
                }
                Down => {
                    self.editor_completion_next();
                    true
                }
                Escape => {
                    self.editor_completion_cancel();
                    true
                }
                _ => false,
            };
            if consumed {
                self.request_redraw();
                return true;
            }
            self.editor_completion_cancel();
        }

        match key {
            Tab => {
                self.editor_start_completion();
                true
            }
            Enter => {
                // submit_on_ctrl_enter: Ctrl+Enter submits, plain Enter newlines
                // (Warp default). Otherwise plain Enter submits, Shift+Enter
                // newlines.
                let ctrl = mods.contains(Modifiers::CONTROL);
                let do_submit = if self.config.editor.submit_on_ctrl_enter {
                    ctrl
                } else {
                    !shift
                };
                if do_submit {
                    self.editor_submit();
                } else if let Some(t) = self.tabs[self.active_tab].terminal.as_mut() {
                    t.editor_mut().buffer.split_newline();
                }
                true
            }
            Char(c) => {
                if let Some(t) = self.tabs[self.active_tab].terminal.as_mut() {
                    t.editor_mut()
                        .buffer
                        .insert_char(resolve_text_char(text, c, shift));
                }
                true
            }
            Backspace => {
                if let Some(t) = self.tabs[self.active_tab].terminal.as_mut() {
                    t.editor_mut().buffer.delete_backspace();
                }
                true
            }
            Delete => {
                if let Some(t) = self.tabs[self.active_tab].terminal.as_mut() {
                    t.editor_mut().buffer.delete_forward();
                }
                true
            }
            Left => {
                if let Some(t) = self.tabs[self.active_tab].terminal.as_mut() {
                    t.editor_mut().buffer.move_left();
                }
                true
            }
            Right => {
                if let Some(t) = self.tabs[self.active_tab].terminal.as_mut() {
                    t.editor_mut().buffer.move_right();
                }
                true
            }
            Home => {
                if let Some(t) = self.tabs[self.active_tab].terminal.as_mut() {
                    t.editor_mut().buffer.move_line_home();
                }
                true
            }
            End => {
                if let Some(t) = self.tabs[self.active_tab].terminal.as_mut() {
                    t.editor_mut().buffer.move_line_end();
                }
                true
            }
            Up => {
                if let Some(t) = self.tabs[self.active_tab].terminal.as_mut() {
                    let e = t.editor_mut();
                    if e.buffer.cursor.0 == 0 {
                        e.history_prev();
                    } else {
                        e.buffer.cursor.0 -= 1;
                        let len = e.buffer.lines[e.buffer.cursor.0].chars().count();
                        e.buffer.cursor.1 = e.buffer.cursor.1.min(len);
                    }
                }
                true
            }
            Down => {
                if let Some(t) = self.tabs[self.active_tab].terminal.as_mut() {
                    let e = t.editor_mut();
                    let last = e.buffer.line_count() - 1;
                    if e.buffer.cursor.0 == last {
                        e.history_next();
                    } else {
                        e.buffer.cursor.0 += 1;
                        let len = e.buffer.lines[e.buffer.cursor.0].chars().count();
                        e.buffer.cursor.1 = e.buffer.cursor.1.min(len);
                    }
                }
                true
            }
            Escape => true, // swallow stray Esc in editor mode
            _ => false,
        }
    }

    // ── Tab completion (drives Editor's completion state machine) ───────────

    fn editor_start_completion(&mut self) {
        // Gather context under an immutable borrow, then mutate the editor.
        let (line_owned, col, cwd, history) = match self.tabs[self.active_tab].terminal.as_ref() {
            Some(t) => {
                let line_idx = t.editor().buffer.cursor.0;
                let col = t.editor().buffer.cursor.1;
                let line = t.editor().buffer.lines.get(line_idx).cloned();
                let cwd = t.cwd().unwrap_or("").to_string();
                let history = t.editor().history().to_vec();
                (line, col, cwd, history)
            }
            None => return,
        };
        let Some(line_str) = line_owned.as_deref() else {
            return;
        };

        // Determine the word range and prefix. Normally this is the token left
        // of the cursor. But when the cursor sits on whitespace after a command
        // (e.g. `cd |`), word_at returns None — in that case, if we're at an
        // argument position, treat it as an empty-prefix path completion so
        // Tab lists all files/dirs in the cwd (matching Warp's behavior).
        let (ws, we, prefix, is_cmd_pos): (usize, usize, String, bool) =
            match word_at(line_str, col) {
                Some((ws, we)) => {
                    let prefix: String = line_str.chars().skip(ws).take(we - ws).collect();
                    let is_cmd = is_command_position(line_str, ws);
                    (ws, we, prefix, is_cmd)
                }
                None => {
                    // Cursor on whitespace. Check if there's a command token
                    // before the cursor (making this an argument position).
                    // If so, start an empty-prefix path completion.
                    let is_cmd = is_command_position(line_str, col);
                    if is_cmd {
                        return; // blank line or after operator — nothing to complete
                    }
                    (col, col, String::new(), false)
                }
            };

        let position = if is_cmd_pos {
            CompletePosition::Command
        } else {
            CompletePosition::Argument
        };

        // Skip empty prefix at command position (nothing to match).
        if prefix.is_empty() && is_cmd_pos {
            return;
        }

        let path_bins: Vec<String> = if is_cmd_pos {
            self.path_bins.clone()
        } else {
            Vec::new()
        };
        let ctx = CompleteCtx {
            cwd: &cwd,
            history: &history,
            path_bins: &path_bins,
        };
        let matches = complete(&prefix, &ctx, position);
        if matches.is_empty() {
            return;
        }
        let Some(t) = self.tabs[self.active_tab].terminal.as_mut() else {
            return;
        };
        let e = t.editor_mut();
        if matches.len() == 1 {
            // Single candidate: accept immediately (replace the word).
            e.start_completion(matches, ws, we);
            e.completion_accept();
        } else {
            e.start_completion(matches, ws, we);
        }
    }

    fn editor_completion_next(&mut self) {
        if let Some(t) = self.tabs[self.active_tab].terminal.as_mut() {
            t.editor_mut().completion_next();
        }
    }

    fn editor_completion_prev(&mut self) {
        if let Some(t) = self.tabs[self.active_tab].terminal.as_mut() {
            t.editor_mut().completion_prev();
        }
    }

    fn editor_completion_accept(&mut self) {
        if let Some(t) = self.tabs[self.active_tab].terminal.as_mut() {
            t.editor_mut().completion_accept();
        }
    }

    fn editor_completion_cancel(&mut self) {
        if let Some(t) = self.tabs[self.active_tab].terminal.as_mut() {
            t.editor_mut().completion_cancel();
        }
    }

    /// Submit the editor's command: write PTY bytes (and any terminal query
    /// response) and locally block the editor through the Enter→preexec window.
    fn editor_submit(&mut self) {
        let bytes = self.tabs[self.active_tab]
            .terminal
            .as_mut()
            .map(|t| t.submit_command())
            .unwrap_or_default();
        if !bytes.is_empty() {
            if let Some(pty) = &self.tabs[self.active_tab].pty {
                let _ = pty.write_sync(&bytes);
            }
        }
        let resp = self.tabs[self.active_tab]
            .terminal
            .as_mut()
            .map(|t| t.take_response())
            .unwrap_or_default();
        if !resp.is_empty() {
            if let Some(pty) = &self.tabs[self.active_tab].pty {
                let _ = pty.write_sync(&resp);
            }
        }
        // Snap the block view to the bottom so the user sees the new
        // command's output. Without this, a fast command (e.g. `echo hi`)
        // finishes before the next redraw's `had_output && phase ==
        // CommandExecuting` check fires — the phase is already back to
        // AtPrompt by then, so the existing snap logic never triggers and
        // the view stays scrolled up on history. Snapping here, at submit
        // time, guarantees the user sees the result regardless of how fast
        // the command completes.
        self.tabs[self.active_tab].block_scroll_offset = 0;
        self.request_redraw();
    }

    /// Count of blocks visible in the panel (newest-first, query-filtered).
    fn panel_visible_count(&self) -> usize {
        let Some(terminal) = &self.tabs[self.active_tab].terminal else {
            return 0;
        };
        let blocks = terminal.block_tracker().blocks();
        let visible = terminal.grid().num_rows;
        blocks
            .iter()
            .rev()
            .filter(|b| block_matches_query(b, &self.panel_query))
            .take(visible)
            .count()
    }

    /// Keep the selection inside the filtered, visible list.
    fn clamp_panel_selection(&mut self) {
        let max = self.panel_visible_count();
        if max == 0 {
            self.panel_selection = 0;
        } else {
            self.panel_selection = self.panel_selection.min(max - 1);
        }
    }

    /// The [`BlockId`] of the currently selected panel row, if any.
    fn panel_selected_block_id(&self) -> Option<BlockId> {
        let terminal = self.tabs[self.active_tab].terminal.as_ref()?;
        let visible = terminal.grid().num_rows;
        terminal
            .block_tracker()
            .blocks()
            .iter()
            .rev()
            .filter(|b| block_matches_query(b, &self.panel_query))
            .take(visible)
            .nth(self.panel_selection)
            .map(|b| b.id)
    }

    /// v0.9 fix: send the panel's currently-selected command to the prompt
    /// editor (Warp-style "click/Enter to rerun"). Looks up the block by id,
    /// strips any prompt prefix, and sets the editor buffer. Silently no-ops
    /// when not at the prompt (command running / alt-screen active) to avoid
    /// stashing text that would resurface unexpectedly.
    fn send_panel_selection_to_input(&mut self) {
        let block_id = match self.panel_selected_block_id() {
            Some(id) => id,
            None => return,
        };
        // Borrow the terminal immutably to find the command, then release
        // before mutating the editor.
        let cmd: Option<String> = {
            let Some(t) = &self.tabs[self.active_tab].terminal else {
                return;
            };
            if t.effective_input_mode() != weft_core::input::InputMode::Editor {
                return;
            }
            t.block_tracker()
                .blocks()
                .iter()
                .find(|b| b.id == block_id)
                .map(|b| strip_prompt_prefix(&b.command))
        };
        if let Some(cmd) = cmd {
            if !cmd.is_empty() {
                if let Some(t) = self.tabs[self.active_tab].terminal.as_mut() {
                    t.editor_mut().buffer.set_text(&cmd);
                    // v0.9: select all so Cmd+C copies the command without
                    // needing a drag-select first. The user can still adjust
                    // the selection by clicking in the prompt box.
                    t.editor_mut().buffer.select_all();
                }
                // Unfocus the panel so the editor gets subsequent keystrokes.
                self.panel_search_focused = false;
                self.request_redraw();
            }
        }
    }

    /// v0.9 W2: scroll the terminal's block view so the panel-selected block
    /// is visible, and arm a 1.5s accent highlight. Called when the user
    /// clicks a row in the history panel (or presses Cmd+Enter while the
    /// panel is open).
    fn scroll_to_panel_selection(&mut self) {
        let block_id = match self.panel_selected_block_id() {
            Some(id) => id,
            None => return,
        };
        // Only meaningful in block view (grid view has no block layout).
        if !self.block_view_active() {
            return;
        }
        let Some(term) = self.tabs[self.active_tab].terminal.as_ref() else {
            return;
        };
        let blocks = term.block_tracker().session_blocks();
        let block_idx = blocks.iter().position(|b| b.id == block_id);
        let Some(block_idx) = block_idx else { return };

        // Count rows from the bottom up to the target block's command line.
        // Layout (bottom→top): Output[N-1] at row 0, …, Output[0] at row
        // N-1, Command at row N, Header at row N+1, Separator at row N+2.
        // For each block BELOW the target (i.e. with higher index), add its
        // full height (3 + output_lines).
        let trim_output_lines = |b: &weft_core::blocks::Block| -> usize {
            if b.collapsed {
                return 0;
            }
            let mut lines: Vec<&str> = b.output.lines().collect();
            while lines.last().is_some_and(|l| {
                let t = l.trim();
                t.is_empty() || matches!(t, "%" | "$" | "#")
            }) {
                lines.pop();
            }
            lines.len()
        };
        let mut rows_from_bottom = 0usize;
        for (i, b) in blocks.iter().enumerate().rev() {
            if i == block_idx {
                break;
            }
            rows_from_bottom += 3 + trim_output_lines(b);
        }
        // Position the block's command line at ~1/3 from the bottom of the
        // viewport so the user sees the command + most of its output above.
        let Some(renderer) = self.renderer.as_ref() else {
            return;
        };
        let visible = renderer.block_visible_rows(1);
        let target = rows_from_bottom.saturating_sub(visible / 3).max(0);
        self.tabs[self.active_tab].block_scroll_offset = target;

        // Arm the highlight: accent border around the block for 1.5s.
        self.panel_highlight = Some(block_id);
        self.panel_highlight_until =
            Some(std::time::Instant::now() + std::time::Duration::from_millis(1500));
        self.request_redraw();
    }

    /// Local scrollback navigation (page up/down, top, bottom).
    fn scroll_action(&mut self, action: Action) {
        let tab = &mut self.tabs[self.active_tab];
        let Some(terminal) = &mut tab.terminal else {
            return;
        };
        let rows = terminal.grid().num_rows;
        let cols = terminal.grid().num_cols;
        // Block view uses a dedicated scroll offset.
        if terminal.show_block_view() {
            let (total, _) = block_content_metrics(terminal, cols);
            // Compute visible rows from the renderer's actual geometry.
            let prompt_lines = terminal.editor().buffer.lines.len();
            let visible = self
                .renderer
                .as_ref()
                .map(|r| r.block_visible_rows(prompt_lines))
                .unwrap_or(rows);
            let max_scroll = total.saturating_sub(visible);
            match action {
                Action::ScrollPageUp => {
                    tab.block_scroll_offset =
                        tab.block_scroll_offset.saturating_add(rows).min(max_scroll);
                }
                Action::ScrollPageDown => {
                    tab.block_scroll_offset = tab.block_scroll_offset.saturating_sub(rows);
                }
                Action::ScrollToTop => {
                    tab.block_scroll_offset = max_scroll;
                }
                Action::ScrollToBottom => tab.block_scroll_offset = 0,
                _ => {}
            }
        } else {
            let grid = terminal.grid_mut();
            match action {
                Action::ScrollPageUp => grid.scroll_up_history(rows),
                Action::ScrollPageDown => grid.scroll_down_history(rows),
                Action::ScrollToTop => grid.scroll_to_top(),
                Action::ScrollToBottom => grid.scroll_to_bottom(),
                _ => {}
            }
        }
        self.request_redraw();
    }

    /// Re-read config from disk and apply it live. Triggered by the
    /// reload-config keybinding (and the file watcher).
    fn reload_config(&mut self) {
        let config = Config::load();
        self.apply_config(config);
        info!("config reloaded");
    }

    // ── v0.9 H1: Tab management ────────────────────────────────────────

    /// Get the current (rows, cols) of the active tab's terminal, falling
    /// back to the renderer's viewport estimate if the terminal is not yet
    /// initialized.
    fn current_size(&self) -> (usize, usize) {
        if let Some(t) = &self.tabs[self.active_tab].terminal {
            let grid = t.grid();
            return (grid.num_rows, grid.num_cols);
        }
        // Fallback: derive from renderer viewport.
        if let Some(r) = &self.renderer {
            let (w, h) = r.viewport();
            let cw = r.cell_size();
            let cols = (w / cw.0).max(1.0) as usize;
            let rows = (h / cw.1).max(1.0) as usize;
            return (rows, cols);
        }
        (24, 80)
    }

    /// Cmd+T — open a new tab with a fresh shell session and switch to it.
    fn new_tab(&mut self) {
        let (rows, cols) = self.current_size();
        let tab = Tab::new(rows, cols, self.config.scrollback.lines, &self.proxy);
        self.tabs.push(tab);
        self.active_tab = self.tabs.len() - 1;
        // Apply the current theme palette to the new terminal so it matches
        // the window's renderer theme (the atlas is shared per-window, not
        // per-tab — no atlas rebuild needed).
        if let Some(t) = &mut self.tabs[self.active_tab].terminal {
            if let Some(r) = &self.renderer {
                t.set_palette(r.theme().palette);
            }
        }
        info!(tab_idx = self.active_tab, "new tab created");
        self.refresh_find_for_active_tab();
        self.request_redraw();
    }

    /// Cmd+W — close the current tab. Returns `false` (exit app) if this
    /// was the last tab; otherwise switches to the previous tab and
    /// returns `true`.
    fn close_tab(&mut self) -> bool {
        if self.tabs.len() <= 1 {
            // Last tab closed → exit the app.
            info!("closing last tab, exiting app");
            return false;
        }
        let removed_idx = self.active_tab;
        let _tab = self.tabs.remove(removed_idx);
        // v0.9 W1+: clear hover state — tab indices shift after removal, so
        // a stale hovered_tab would point at the wrong tab. The next
        // CursorMoved will recompute it.
        self.hovered_tab = None;
        // Switch to the previous tab (or wrap to last).
        if self.active_tab > 0 {
            self.active_tab -= 1;
        } else {
            self.active_tab = self.tabs.len() - 1;
        }
        info!(closed = removed_idx, active = self.active_tab, "tab closed");
        self.refresh_find_for_active_tab();
        self.request_redraw();
        true
    }

    /// v0.9 H1 Stage 4: re-bind the global find state to the active tab.
    ///
    /// Find query / regex / case-sensitive flags stay global (convenient for
    /// searching the same term across tabs), but the matches must come from
    /// the active tab's content — otherwise switching tabs shows the previous
    /// tab's match coordinates, which point at the wrong rows/blocks.
    ///
    /// Called from `new_tab` / `close_tab` / `next_tab` / `prev_tab`. When
    /// the find bar is closed or the query is empty this is a no-op.
    fn refresh_find_for_active_tab(&mut self) {
        if !self.find_open || self.find_query.is_empty() {
            return;
        }
        // Drop any results still queued from the old tab's scan. Without this
        // drain, `poll_find_worker_results` would apply the stale `Partial`/
        // `Complete` results to the new active tab on the next frame.
        while self.find_worker.try_recv_result().is_some() {}
        self.find_worker_busy = false;
        // Clear stale matches so the FindUI count resets to 0 until the new
        // search completes.
        self.find_matches.clear();
        self.find_truncated = false;
        self.find_index = 0;
        self.find_block_matches.clear();
        self.find_block_truncated = false;
        self.find_block_index = 0;
        self.find_regex_error = None;
        // Arm the debounce so `maybe_refresh_find_results` re-submits the
        // query against the new active tab's snapshot on the next redraw.
        self.find_last_key = Some(std::time::Instant::now());
    }

    /// Cmd+Shift+] — switch to the next tab (wraps around).
    fn next_tab(&mut self) {
        if self.tabs.len() <= 1 {
            return;
        }
        self.active_tab = (self.active_tab + 1) % self.tabs.len();
        info!(active = self.active_tab, "switched to next tab");
        self.refresh_find_for_active_tab();
        self.request_redraw();
    }

    /// Cmd+Shift+[ — switch to the previous tab (wraps around).
    fn prev_tab(&mut self) {
        if self.tabs.len() <= 1 {
            return;
        }
        self.active_tab = if self.active_tab == 0 {
            self.tabs.len() - 1
        } else {
            self.active_tab - 1
        };
        info!(active = self.active_tab, "switched to prev tab");
        self.refresh_find_for_active_tab();
        self.request_redraw();
    }

    /// Build the `TabBarDrawState` for the renderer from the current tab list.
    /// Tab labels are the cwd basename (or "Tab N" when no cwd is set).
    fn tab_bar_state(&self) -> TabBarDrawState {
        let labels: Vec<String> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, tab)| {
                // v0.9 fix: handle root path "/" — rsplit('/').next() on "/"
                // returns "" (empty string). Fall back to "/" for root.
                let cwd = tab.terminal.as_ref().and_then(|t| t.cwd()).map(|c| {
                    let base = c.rsplit('/').next().unwrap_or("");
                    if base.is_empty() {
                        "/".to_string()
                    } else {
                        base.to_string()
                    }
                });
                // v0.9 W1: when a command is running, show "cwd · cmd". When
                // idle, show only the cwd basename. (Previous version showed
                // the last completed command, but that kept the label stuck
                // on "cwd · cmd" forever after any command — e.g. "sleep 5"
                // never reverted, and "cd /" showed "/ · cd /". The running
                // indicator is enough; completed commands live in the history
                // panel and block view.)
                let cmd: Option<String> = tab
                    .terminal
                    .as_ref()
                    .and_then(|t| t.block_tracker().in_flight())
                    .map(|f| f.command.to_string());
                match (cwd, cmd) {
                    (Some(cwd), Some(cmd)) => {
                        let cmd_short: String = cmd.chars().take(16).collect();
                        let suffix = if cmd.chars().count() > 16 { "…" } else { "" };
                        format!("{} · {}{}", cwd, cmd_short, suffix)
                    }
                    (Some(cwd), None) => cwd,
                    (None, Some(cmd)) => {
                        let cmd_short: String = cmd.chars().take(16).collect();
                        let suffix = if cmd.chars().count() > 16 { "…" } else { "" };
                        format!("Tab {} · {}{}", i + 1, cmd_short, suffix)
                    }
                    (None, None) => format!("Tab {}", i + 1),
                }
            })
            .collect();
        TabBarDrawState {
            tab_count: self.tabs.len(),
            active_tab: self.active_tab,
            labels,
            hovered_tab: self.hovered_tab,
        }
    }

    fn toggle_theme(&mut self) {
        // v0.9 U-D1: when `follow_system` is enabled, manual toggle is a
        // no-op — the system appearance wins on the next poll.
        if self.config.theme.follow_system {
            info!("toggle_theme ignored: follow_system is enabled");
            return;
        }
        self.theme_is_dark = !self.theme_is_dark;
        // v0.9 W2+ fix: route through resolve_named (not the raw constructor)
        // so inline `[theme]` overrides AND non-default theme names (e.g.
        // `name = "warp"`) survive a Cmd+Shift+T toggle. Previously this
        // called Theme::weft_dark()/weft_light() directly, which silently
        // dropped user customizations on every toggle.
        let cfg = &self.config.theme;
        let name = if self.theme_is_dark {
            cfg.dark_name.as_deref().unwrap_or("weft-warm")
        } else {
            cfg.light_name.as_deref().unwrap_or("weft-light")
        };
        let theme = weft_core::config::Theme::resolve_named(name, cfg);
        if let Some(r) = &mut self.renderer {
            r.set_theme(theme.clone());
        }
        // Reseed the terminal's ANSI palette so existing cells recolor.
        if let Some(t) = &mut self.tabs[self.active_tab].terminal {
            t.set_palette(theme.palette);
        }
        info!(dark = self.theme_is_dark, name, "theme toggled");
        self.request_redraw();
    }

    /// v0.9 U-D1: Apply a theme by name (light or dark), respecting the
    /// `[theme]` overrides from the loaded config. Updates `theme_is_dark`
    /// and reseeds the terminal palette so existing cells recolor on the
    /// next draw.
    fn apply_theme_by_name(&mut self, name: &str, dark: bool) {
        let theme = weft_core::config::Theme::resolve_named(name, &self.config.theme);
        if let Some(r) = &mut self.renderer {
            r.set_theme(theme.clone());
        }
        if let Some(t) = &mut self.tabs[self.active_tab].terminal {
            t.set_palette(theme.palette);
        }
        self.theme_is_dark = dark;
        info!(dark, name, "theme applied");
        self.request_redraw();
    }

    /// v0.9 W2+: List all resolvable theme names for the picker.
    ///
    /// Returns built-in names (sorted by display order, not alphabetically)
    /// followed by custom theme files discovered in
    /// `~/.config/weft/themes/` (stem of `.toml`/`.yaml`/`.yml` files).
    /// Built-ins are returned in a curated order (defaults first, then
    /// classic themes, then community themes) so the picker shows the most
    /// useful themes at the top.
    fn available_theme_names(&self) -> Vec<String> {
        let mut names: Vec<String> = [
            "weft-warm",
            "weft-light",
            "warp",
            "dracula",
            "solarized-dark",
            "gruvbox-dark",
            "nord",
            "tokyo-night",
            "catppuccin",
            "one-dark",
            "monokai-pro",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        // Append custom theme file stems from the themes dir (if any).
        if let Some(dir) = weft_core::config::Theme::themes_dir() {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                let mut customs: Vec<String> = Vec::new();
                for entry in entries.flatten() {
                    if let Some(stem) = entry.path().file_stem().and_then(|s| s.to_str()) {
                        // Skip names that collide with built-ins.
                        if !names.iter().any(|n| n == stem) {
                            customs.push(stem.to_string());
                        }
                    }
                }
                customs.sort();
                names.extend(customs);
            }
        }
        names
    }

    /// v0.9 W2+: Rebuild `palette_results` for the theme picker sub-mode.
    ///
    /// Filters `themes` by the buffer (case-insensitive substring) and
    /// pushes each as a `PaletteEntry::Builtin(BuiltinCmd::SelectTheme)` —
    /// but the renderer projection (in `draw()`) maps each to a
    /// `(theme_name, "", "Theme")` tuple via the sub-mode check, so the
    /// picker rows show theme names with a "Theme" suffix label.
    fn refresh_theme_picker_results(&mut self) {
        let (buffer, themes) = match &self.palette_submode {
            PaletteSubMode::SelectTheme { buffer, themes } => (buffer.clone(), themes.clone()),
            _ => return,
        };
        let q = buffer.to_lowercase();
        let mut results: Vec<PaletteEntry> = Vec::new();
        for name in &themes {
            if q.is_empty() || name.to_lowercase().contains(&q) {
                // We reuse PaletteEntry::Builtin(SelectTheme) as a sentinel;
                // the renderer projection distinguishes themes by sub-mode.
                results.push(PaletteEntry::Builtin(BuiltinCmd::SelectTheme));
            }
        }
        self.palette_results = results;
        if self.palette_selection >= self.palette_results.len() {
            self.palette_selection = 0;
        }
    }

    /// v0.9 W2+: Handle keyboard input in the theme picker sub-mode.
    ///
    /// - Up/Down: navigate the filtered theme list.
    /// - Enter: apply the selected theme (dark heuristic: name ends with
    ///   `-dark`/`-night` or contains "dracula"/"nord"/"tokyo"/"monokai"
    ///   → dark; otherwise treat as light). The palette closes after apply.
    /// - Escape: return to Search sub-mode (does not close the palette).
    /// - Backspace: pop the filter buffer; if empty, return to Search.
    /// - Printable char: append to the filter buffer and refresh.
    fn handle_palette_select_theme_key(&mut self, key: KeyCode, text: Option<&str>) -> bool {
        let (buffer, themes) = match &self.palette_submode {
            PaletteSubMode::SelectTheme { buffer, themes } => (buffer.clone(), themes.clone()),
            _ => return false,
        };

        // Helper: given the current filtered selection index, resolve back to
        // the actual theme name from the full `themes` list.
        let selected_name = |sel: usize, buf: &str| -> Option<String> {
            let q = buf.to_lowercase();
            themes
                .iter()
                .filter(|n| q.is_empty() || n.to_lowercase().contains(&q))
                .nth(sel)
                .cloned()
        };

        match key {
            KeyCode::Escape => {
                // Return to search mode (keep palette open).
                self.palette_submode = PaletteSubMode::Search;
                self.palette_query.clear();
                self.palette_selection = 0;
                self.refresh_palette_results();
                self.request_redraw();
                true
            }
            KeyCode::Up => {
                if self.palette_selection > 0 {
                    self.palette_selection -= 1;
                }
                self.request_redraw();
                true
            }
            KeyCode::Down => {
                if self.palette_selection + 1 < self.palette_results.len() {
                    self.palette_selection += 1;
                }
                self.request_redraw();
                true
            }
            KeyCode::Enter => {
                if let Some(name) = selected_name(self.palette_selection, &buffer) {
                    // Heuristic: classify as dark unless the name clearly
                    // indicates a light theme. Affects follow_system parity.
                    let dark = !matches!(
                        name.as_str(),
                        "weft-light" | "solarized-light" | "gruvbox-light"
                    );
                    self.apply_theme_by_name(&name, dark);
                    self.palette_open = false;
                    self.palette_submode = PaletteSubMode::Search;
                    self.palette_query.clear();
                    self.request_redraw();
                }
                true
            }
            KeyCode::Backspace => {
                if buffer.is_empty() {
                    // Exit sub-mode back to search.
                    self.palette_submode = PaletteSubMode::Search;
                    self.palette_query.clear();
                    self.palette_selection = 0;
                    self.refresh_palette_results();
                } else {
                    // Pop filter char.
                    if let PaletteSubMode::SelectTheme { buffer, .. } = &mut self.palette_submode {
                        buffer.pop();
                        let buf = buffer.clone();
                        self.palette_selection = 0;
                        self.refresh_theme_picker_results();
                        // refresh_theme_picker_results reads buffer from self.
                        let _ = buf; // silence unused
                    }
                }
                self.request_redraw();
                true
            }
            _ => {
                let c = resolve_text_char(text, '\0', false);
                if c == '\0' || c.is_control() {
                    return false;
                }
                if let PaletteSubMode::SelectTheme { buffer, .. } = &mut self.palette_submode {
                    buffer.push(c);
                    self.palette_selection = 0;
                    self.refresh_theme_picker_results();
                }
                self.request_redraw();
                true
            }
        }
    }

    /// v0.9 U-D1: Poll the macOS system appearance and switch theme if it
    /// has changed since the last poll. Throttled to one query per second
    /// to avoid per-frame `NSUserDefaults` overhead. No-op when
    /// `[theme] follow_system = false`.
    fn poll_system_appearance(&mut self) {
        if !self.config.theme.follow_system {
            return;
        }
        // Throttle: at most one query per second.
        if self.last_appearance_check.elapsed() < std::time::Duration::from_secs(1) {
            return;
        }
        self.last_appearance_check = std::time::Instant::now();
        let dark = unsafe { system_appearance_is_dark() };
        if Some(dark) != self.last_system_appearance_dark {
            self.last_system_appearance_dark = Some(dark);
            let name = if dark {
                self.config
                    .theme
                    .dark_name
                    .clone()
                    .unwrap_or_else(|| "weft-warm".to_string())
            } else {
                self.config
                    .theme
                    .light_name
                    .clone()
                    .unwrap_or_else(|| "weft-light".to_string())
            };
            self.apply_theme_by_name(&name, dark);
        }
    }

    /// Apply a (possibly new) config: theme, font, keybindings, scrollback.
    /// Theme/font/scrollback changes take effect immediately; window size/title
    /// apply on the next launch.
    fn apply_config(&mut self, config: Config) {
        // Theme — renderer defaults + terminal palette reseed (recolors all
        // Palette-indexed cells on the next draw).
        //
        // v0.9 U-D1: when `[theme] follow_system = true`, the system
        // appearance picks the theme (via `light_name` / `dark_name`,
        // defaulting to `weft-light` / `weft-warm`). The `name` field is
        // ignored in this mode.
        let theme = if config.theme.follow_system {
            let dark = unsafe { system_appearance_is_dark() };
            let name = if dark {
                config
                    .theme
                    .dark_name
                    .clone()
                    .unwrap_or_else(|| "weft-warm".to_string())
            } else {
                config
                    .theme
                    .light_name
                    .clone()
                    .unwrap_or_else(|| "weft-light".to_string())
            };
            weft_core::config::Theme::resolve_named(&name, &config.theme)
        } else {
            config.theme()
        };
        if let Some(r) = &mut self.renderer {
            r.set_theme(theme.clone());
        }
        if let Some(t) = &mut self.tabs[self.active_tab].terminal {
            t.set_palette(theme.palette);
        }

        // Font — rebuild the atlas (cell dimensions may change → recompute).
        // The active `font_scale` (Cmd+/- zoom) is re-applied on top of the
        // freshly loaded config, so a reload doesn't lose the user's zoom.
        if self.config.font.family != config.font.family
            || self.config.font.size != config.font.size
            || self.config.font.line_height != config.font.line_height
            || self.font_scale != 1.0
        {
            if let Some(r) = &mut self.renderer {
                let mut scaled = config.font.clone();
                scaled.size *= self.font_scale;
                r.rebuild_atlas(scaled);
            }
            self.recompute_layout();
        }

        // Window background opacity (layer-level transparency; text stays
        // opaque). Recolors the next frame. Window-level transparency is
        // startup-only — see `resumed`.
        if (self.config.window.opacity - config.window.opacity).abs() > f32::EPSILON {
            if let Some(r) = &mut self.renderer {
                r.set_opacity(config.window.opacity);
            }
        }

        // Content padding (changes usable rows/cols → recompute layout).
        if self.config.window.padding_x != config.window.padding_x
            || self.config.window.padding_y != config.window.padding_y
        {
            if let Some(r) = &mut self.renderer {
                r.set_padding((config.window.padding_x, config.window.padding_y));
            }
            self.recompute_layout();
        }

        // Keybindings.
        self.keybindings = config.keybindings();

        // Scrollback capacity.
        if let Some(t) = &mut self.tabs[self.active_tab].terminal {
            let cols = t.grid().num_cols;
            t.grid_mut()
                .scrollback
                .set_max_lines(config.scrollback.lines, cols);
        }

        self.config = config;
        self.request_redraw();
    }

    /// Adjust `font_scale` for a zoom action (Cmd+= / Cmd+- / Cmd+0) and
    /// rebuild the glyph atlas with the scaled size. Each ZoomIn/Out step
    /// multiplies/divides by 1.1; `font_scale` is clamped to [0.5, 3.0] so
    /// the cell dimensions stay sane. ZoomReset restores 1.0.
    fn zoom_action(&mut self, action: Action) {
        let new_scale = match action {
            Action::ZoomIn => (self.font_scale * 1.1).min(3.0),
            Action::ZoomOut => (self.font_scale / 1.1).max(0.5),
            Action::ZoomReset => 1.0,
            _ => return,
        };
        if (new_scale - self.font_scale).abs() < f32::EPSILON && action != Action::ZoomReset {
            return;
        }
        self.font_scale = new_scale;
        if let Some(r) = &mut self.renderer {
            let mut scaled = self.config.font.clone();
            scaled.size *= self.font_scale;
            r.rebuild_atlas(scaled);
        }
        self.recompute_layout();
        self.request_redraw();
    }

    /// Compute grid (rows, cols) from the window size minus content padding and
    /// the current cell dimensions. Returns (0, 0) until the window/renderer are
    /// ready. Centralizes the padding-aware geometry used by both resize paths.
    fn grid_dims(&self) -> (usize, usize) {
        let (Some(window), Some(renderer)) = (&self.window, &self.renderer) else {
            return (0, 0);
        };
        let size = window.inner_size();
        // v0.9 W5: when the history panel is open it becomes a left sidebar
        // that pushes terminal content right, so the grid/PTY must shrink by
        // the sidebar width (chrome_left).
        let chrome_left = if self.panel_open {
            renderer.sidebar_width() as f64
        } else {
            0.0
        };
        let usable_w = size.width as f64 - 2.0 * renderer.padding_x() as f64 - chrome_left;
        // The grid/PTY is always the FULL window. The editor input box is an
        // overlay that covers the bottom rows in Editor mode — it never
        // changes the grid size, so editor↔passthrough transitions don't fire
        // a SIGWINCH/reflow storm (which was clearing prior output + the
        // command echo). The shell's blank prompt sits under the box.
        let usable_h = (size.height as f64 - 2.0 * renderer.padding_y() as f64).max(0.0);
        let cols = (usable_w / renderer.cell_width() as f64).max(0.0) as usize;
        let rows = (usable_h / renderer.cell_height() as f64).max(0.0) as usize;
        (rows, cols)
    }

    /// Recompute grid rows/cols from the current window + cell dimensions and
    /// resize the terminal / queue a PTY SIGWINCH. Used after a font or padding
    /// change (cell size or usable area changes) and on window resize.
    fn recompute_layout(&mut self) {
        let (new_rows, new_cols) = self.grid_dims();
        if new_cols == 0 || new_rows == 0 {
            return;
        }
        let Some(window) = &self.window else {
            return;
        };
        let size = window.inner_size();
        if let Some(renderer) = &mut self.renderer {
            renderer.resize(window, size);
        }
        // v0.9 W5: resize every tab's terminal so non-active tabs also pick
        // up the new chrome_left (sidebar open/close shifts the grid). Only
        // the active tab sends a PTY resize immediately; background tabs get
        // their PTY resize on activation (refresh_grid_for_active_tab).
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            if let Some(terminal) = &mut tab.terminal {
                terminal.resize(new_rows, new_cols);
                if i == self.active_tab {
                    info!(rows = new_rows, cols = new_cols, "terminal resized");
                }
            }
            if i == self.active_tab {
                tab.pending_pty_resize = Some((new_rows, new_cols));
            }
        }
        self.last_resize_instant = std::time::Instant::now();
    }

    /// Convert pixel coordinates to grid (row, col).
    fn pixel_to_grid(&self, x: f64, y: f64) -> GridPos {
        let Some(renderer) = &self.renderer else {
            return GridPos::new(0, 0);
        };
        // CursorMoved position is in physical pixels; cell_width/height are
        // also in physical pixels — divide directly without scale conversion.
        // Subtract content padding first so clicks map to the padded grid.
        let cell_w = renderer.cell_width() as f64;
        let cell_h = renderer.cell_height() as f64;
        // v0.9 H1: subtract tab bar height (chrome_top) so clicks map to the
        // correct grid row. The grid is rendered at
        // `padding_y + chrome_top + row * cell_h`, so the inverse is
        // `(y - padding_y - chrome_top) / cell_h`. Without this, a click on
        // row N would resolve to N + ~1.5 (off-by-one-down) when the tab bar
        // is visible. Read from layout_ctx so it matches the renderer's
        // conditional (chrome_top = 0 when single tab hides the bar).
        let chrome_top = renderer.layout_ctx.map(|c| c.chrome_top).unwrap_or(0.0) as f64;
        // v0.9 W5: subtract sidebar width (chrome_left) so clicks map to the
        // correct column when the history panel pushes the grid right.
        let chrome_left = if self.panel_open {
            renderer.sidebar_width() as f64
        } else {
            0.0
        };
        // Clamp to valid grid bounds. A click past the right/bottom edge (e.g.
        // a drag-to-select ending at the window margin) would otherwise yield
        // col == num_cols / row == num_rows and panic text_from_grid on copy.
        let (num_rows, num_cols) = self.tabs[self.active_tab]
            .terminal
            .as_ref()
            .map(|t| (t.grid().num_rows, t.grid().num_cols))
            .unwrap_or((1, 1));
        let col = (((x - renderer.padding_x() as f64 - chrome_left) / cell_w).max(0.0) as usize)
            .min(num_cols.saturating_sub(1));
        let row = (((y - renderer.padding_y() as f64 - chrome_top) / cell_h).max(0.0) as usize)
            .min(num_rows.saturating_sub(1));
        GridPos::new(row, col)
    }

    /// Resolve the OSC 8 hyperlink URL at pixel coordinates `(x, y)`, if any.
    /// Returns `None` when the click misses a HYPERLINK-tagged cell or when
    /// the cell_map has been invalidated by a scroll (MVP trade-off: links
    /// in scrolled-off content aren't clickable).
    fn hyperlink_at_pixel(&self, x: f64, y: f64) -> Option<String> {
        let terminal = self.tabs[self.active_tab].terminal.as_ref()?;
        // Block view uses a separate scrollable layout — skip OSC 8 there.
        if self.block_view_active() {
            return None;
        }
        let pos = self.pixel_to_grid(x, y);
        terminal
            .hyperlinks()
            .url_at(pos.row, pos.col)
            .map(str::to_string)
    }

    /// Convert pixel coordinates to a block-view position.
    ///
    /// Used in place of `pixel_to_grid` when `show_block_view()` is true: the
    /// classic grid division (`y / cell_h`) does not match the block view's
    /// `pitch = cell_h * 1.1` row spacing, inserted Header/Separator rows, the
    /// pinned CWD bar, or the scroll offset, so a grid-coordinate copy landed
    /// on the wrong line (the "复制错位" bug). This walks the renderer's cached
    /// `block_view_rows` (scroll-adjusted y bands + visible text) and maps the
    /// click to a char index in the matched row, honoring CJK double-width.
    ///
    /// Returns `None` if no row band contains `y` (e.g. on the CWD bar / input
    /// box / outside the scroll region) or the matched row isn't selectable.
    fn pixel_to_block_view_pos(&self, x: f64, y: f64) -> Option<BlockViewPos> {
        let renderer = self.renderer.as_ref()?;
        let cw = renderer.cell_width() as f64;
        if cw <= 0.0 {
            return None;
        }
        // v0.9 W5: account for the left sidebar offset (chrome_left) so
        // block-view clicks map to the correct char when the panel is open.
        let chrome_left = if self.panel_open {
            renderer.sidebar_width() as f64
        } else {
            0.0
        };
        let left = renderer.padding_x() as f64 + chrome_left;
        let rows = renderer.block_view_rows.as_slice();
        if rows.is_empty() {
            return None;
        }
        // Find the row whose [y_top, y_bottom) contains y.
        let row_index = rows.iter().position(|r| r.contains_y(y as f32))?;
        let row = &rows[row_index];
        if !matches!(
            row.kind,
            BlockViewRowKind::Output | BlockViewRowKind::Command | BlockViewRowKind::LiveCommand
        ) {
            return None;
        }
        // Map pixel x → char index by accumulating each char's display width.
        // A click in the right half of a double-width cell rounds to that
        // cell's index (so dragging across it selects the whole CJK char).
        let mut col_cursor = 0usize; // column units consumed so far
        let target_col = ((x - left) / cw).max(0.0) as usize;
        for (ci, c) in row.text.chars().enumerate() {
            let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            if w == 0 {
                // Zero-width (combining mark): belongs to the previous cell,
                // don't advance the column cursor.
                continue;
            }
            // Click lands in this char if it's before the far edge of the cell.
            // For double-width, the char occupies [col_cursor, col_cursor+2);
            // a click anywhere in that range maps to this char.
            if target_col < col_cursor + w {
                return Some(BlockViewPos {
                    row_index,
                    char_index: ci,
                });
            }
            col_cursor += w;
        }
        // Past the last char: clamp to end.
        Some(BlockViewPos {
            row_index,
            char_index: row.text.chars().count(),
        })
    }

    /// v0.9: map a physical-pixel click (inside the prompt input box) to an
    /// editor buffer position `(line, char_col)`. Returns None when the
    /// renderer/prompt isn't available or the click is outside the box.
    ///
    /// The prompt box layout (from `layout_prompt`):
    ///   - line 0 starts at `first_line_text_x` (after the "❯ " glyph)
    ///   - lines 1+ start at `left` (= `box_x0`)
    ///   - each line is `cell_h` tall, starting at `text_y0`
    fn pixel_to_editor_pos(&self, x: f64, y: f64) -> Option<(usize, usize)> {
        use unicode_width::UnicodeWidthChar;
        let renderer = self.renderer.as_ref()?;
        let ctx = renderer.layout_ctx?;
        let cw = ctx.cell_w as f64;
        let ch = ctx.cell_h as f64;
        if cw <= 0.0 || ch <= 0.0 {
            return None;
        }
        let Some(terminal) = &self.tabs[self.active_tab].terminal else {
            return None;
        };
        if terminal.effective_input_mode() != weft_core::input::InputMode::Editor {
            return None;
        }
        let lines = &terminal.editor().buffer.lines;
        if lines.is_empty() {
            return None;
        }
        // Recompute the prompt geometry (matches layout_prompt).
        let n_lines = lines.len().max(1);
        let box_h = ch * (n_lines as f64 + 2.0);
        let box_y1 = (ctx.viewport.1 as f64 - ctx.padding_y as f64).max(0.0);
        let box_y0 = (box_y1 - box_h).max(0.0);
        let text_y0 = box_y0 + ch;
        let left = ctx.left() as f64;
        let prompt_chars = 2usize;
        let first_line_text_x = left + prompt_chars as f64 * cw;
        // Which line was clicked? (clamped to [0, n_lines-1])
        let mut line = ((y - text_y0) / ch) as isize;
        if line < 0 {
            line = 0;
        }
        let line = (line as usize).min(n_lines - 1);
        // X origin for this line.
        let text_x = if line == 0 { first_line_text_x } else { left };
        // Column offset in display units.
        let disp_col = ((x - text_x) / cw).max(0.0) as usize;
        // Walk the line's chars, accumulating display widths, to find the
        // char index whose cumulative width first exceeds disp_col.
        let line_str = &lines[line];
        let mut col_cursor = 0usize;
        for (ci, c) in line_str.chars().enumerate() {
            let w = UnicodeWidthChar::width(c).unwrap_or(0);
            if w == 0 {
                continue;
            }
            if disp_col < col_cursor + w {
                // For double-width chars, clicking the right half advances
                // past the char (so drag-select lands after it).
                let char_idx = if disp_col > col_cursor { ci + 1 } else { ci };
                return Some((line, char_idx));
            }
            col_cursor += w;
        }
        // Past the last char: clamp to end of line.
        Some((line, line_str.chars().count()))
    }

    /// True when the block view is the active renderer (Editor mode, not in
    /// an alt-screen app). Centralises the dispatch so mouse/copy paths stay
    /// consistent.
    fn block_view_active(&self) -> bool {
        self.tabs[self.active_tab]
            .terminal
            .as_ref()
            .map(|t| t.show_block_view())
            .unwrap_or(false)
    }

    /// True when the foreground program has grabbed the mouse (mouse reporting
    /// on) and the user is NOT holding Shift to force a selection. While true,
    /// clicks/drags are forwarded to the program and we must NOT start a visual
    /// selection (otherwise a stray blue cell follows the click — e.g. inside
    /// `claude`/`vim`). Standard xterm/Alacritty behavior.
    fn mouse_reporting_active(&self) -> bool {
        if self.mods.state().shift_key() {
            return false; // Shift = force terminal selection
        }
        self.tabs[self.active_tab]
            .terminal
            .as_ref()
            .map(|t| t.mouse_protocol != MouseProtocol::Off)
            .unwrap_or(false)
    }

    /// Which foldable block (if any) owns the physical-pixel y in the last
    /// rendered block view. `None` outside the block view or off every block.
    /// Find the block at vertical position `y`. Returns `Some(id)` for
    /// completed blocks, `Some(None)` for the in-flight (running) command,
    /// or `None` when not on a block row.
    fn block_at(&self, y: f32) -> Option<Option<BlockId>> {
        let rows = &self.renderer.as_ref()?.block_view_rows;
        // Find the row whose y-range contains `y`. Prefer Command/LiveCommand
        // rows; Output rows fall back to their owning block.
        for row in rows {
            if y >= row.y_top && y < row.y_bottom {
                use weft_core::selection::BlockViewRowKind;
                match row.kind {
                    BlockViewRowKind::Command => return Some(row.block_id),
                    BlockViewRowKind::LiveCommand => return Some(None),
                    BlockViewRowKind::Output => return Some(row.block_id),
                    _ => {}
                }
            }
        }
        None
    }

    /// Hit-test the find popup's clickable buttons. Returns the action the
    /// click should trigger, or `None` when the click landed outside any
    /// button (or the find popup isn't open). Reads the rects stored by the
    /// renderer in the last `build_find_vertices` pass.
    fn find_button_at(&self, x: f32, y: f32) -> Option<FindButtonAction> {
        let buttons = self.renderer.as_ref()?.find_buttons.as_ref()?;
        let hit = |r: &[f32; 4]| x >= r[0] && x < r[2] && y >= r[1] && y < r[3];
        // Order matters: check arrows first (they're nested between the
        // toggles on the right side), then the toggles. In practice the
        // rects don't overlap so any order works, but this is defensive.
        if let Some(r) = buttons.up {
            if hit(&r) {
                return Some(FindButtonAction::Prev);
            }
        }
        if let Some(r) = buttons.down {
            if hit(&r) {
                return Some(FindButtonAction::Next);
            }
        }
        if hit(&buttons.case_sensitive) {
            return Some(FindButtonAction::ToggleCase);
        }
        if hit(&buttons.regex) {
            return Some(FindButtonAction::ToggleRegex);
        }
        None
    }

    /// v0.9: map a physical-pixel click to a palette results-list row
    /// index. Returns `Some(idx)` when the click lands inside a visible
    /// results row, `None` otherwise (outside the popup, on the query/banner
    /// row, in workflow form mode, or below the last visible row). Border-drag
    /// clicks are handled earlier by `check_popup_border_drag`, so they never
    /// reach here.
    ///
    /// Geometry is recomputed via `layout_palette_search` to match
    /// `build_palette_vertices` exactly — `palette_popup_rect` alone isn't
    /// enough because we need `results_y` and the visible `start..end` window.
    fn palette_row_at(&self, x: f64, y: f64) -> Option<usize> {
        // Form mode (workflow variable fill) has no results list to click.
        if self.palette_form.is_some() {
            return None;
        }
        let renderer = self.renderer.as_ref()?;
        // Popup must have been rendered last frame.
        renderer.palette_popup_rect?;
        let ctx = renderer.layout_ctx?;
        let cw = ctx.cell_w;
        let ch = ctx.cell_h;
        if cw <= 0.0 || ch <= 0.0 {
            return None;
        }
        let layout = crate::layout::layout_palette_search(
            &ctx,
            self.palette_results.len(),
            self.palette_selection,
            self.popup_max_rows,
            self.popup_width_scale,
        );
        let [px0, _py0, px1, _py1] = layout.popup_rect;
        let xf = x as f32;
        let yf = y as f32;
        // Click must be inside the popup horizontally and below the separator
        // (i.e. on the results list, not the query/banner row above it).
        if xf < px0 || xf >= px1 || yf < layout.results_y {
            return None;
        }
        let row = ((yf - layout.results_y) / ch) as usize;
        let idx = layout.start + row;
        if idx >= layout.end {
            return None;
        }
        Some(idx)
    }

    fn handle_mouse_press(&mut self, x: f64, y: f64, button: winit::event::MouseButton) {
        // v0.9 H1: Tab bar click handling — check before everything else so
        // tab clicks work even inside TUI apps that captured the mouse. Only
        // left-clicks on the tab bar are handled here, and only when more
        // than one tab is open (single tab hides the bar).
        if button == winit::event::MouseButton::Left && self.tabs.len() > 1 {
            if let Some(renderer) = &self.renderer {
                let bar_h = renderer.tab_bar_height();
                if y as f32 <= bar_h {
                    // Click is in the tab bar region. Check hit-test rects.
                    let xf = x as f32;
                    let yf = y as f32;
                    for hit in &renderer.tab_hits {
                        // Check close button first (it's inside the tab rect).
                        let [cx0, cy0, cx1, cy1] = hit.close_rect;
                        if xf >= cx0 && xf < cx1 && yf >= cy0 && yf < cy1 {
                            // Close this tab.
                            let idx = hit.index;
                            // If closing the active tab, switch first.
                            if idx == self.active_tab {
                                if self.close_tab() {
                                    // App continues with remaining tabs.
                                }
                            } else {
                                // Close a background tab — remove and adjust index.
                                self.tabs.remove(idx);
                                if idx < self.active_tab {
                                    self.active_tab -= 1;
                                }
                                self.hovered_tab = None;
                                self.request_redraw();
                            }
                            return;
                        }
                        // Check tab label rect.
                        let [tx0, ty0, tx1, ty1] = hit.tab_rect;
                        if xf >= tx0 && xf < tx1 && yf >= ty0 && yf < ty1 {
                            if self.active_tab != hit.index {
                                self.active_tab = hit.index;
                                // v0.9 H1 Stage 4 fix: re-bind find state to
                                // the new active tab so matches come from its
                                // content, not the previous tab's.
                                self.refresh_find_for_active_tab();
                            }
                            self.hovered_tab = None;
                            self.request_redraw();
                            return;
                        }
                    }
                    return; // Click in tab bar but not on any tab — consume.
                }
            }
        }

        // v0.9 W2: history panel click → select row + scroll terminal to block.
        // Handled before PTY mouse reporting so panel clicks work even inside
        // TUI apps that captured the mouse.
        if button == winit::event::MouseButton::Left && self.panel_open {
            if let Some(renderer) = &self.renderer {
                // v0.9 W5: panel is now a LEFT sidebar anchored at x = 0 with
                // width = sidebar_width().
                let width_px = renderer.sidebar_width();
                let panel_x = 0.0;
                let ch = renderer.cell_height() as f64;
                // v0.9 fix: list_top must include chrome_top (tab bar height)
                // to match the renderer's panel content offset. Without this,
                // row clicks were misaligned by one tab-bar height.
                let chrome_top = renderer.layout_ctx.map(|c| c.chrome_top).unwrap_or(0.0) as f64;
                let xf = x as f32;
                let yf = y;
                if xf >= panel_x && xf < panel_x + width_px && yf > 0.0 {
                    // v0.9 fix: match the renderer's Warp-style panel layout:
                    //   header  at chrome_top + ch*0.4
                    //   search  at chrome_top + ch*1.6, height ch*1.4
                    //   list    at chrome_top + ch*1.6 + ch*1.4 + ch*0.4
                    let field_pad_y = ch * 1.6;
                    let field_h = ch * 1.4;
                    let search_top = chrome_top + field_pad_y;
                    let search_bottom = chrome_top + field_pad_y + field_h;
                    let list_top = chrome_top + field_pad_y + field_h + ch * 0.4;
                    let row_h = ch * 1.1;
                    if yf >= list_top {
                        // Click on a history row: select it AND focus the
                        // panel so Up/Down keys navigate the list (Warp-style).
                        // Single click only selects + scrolls + highlights
                        // the block; double-click (or Enter) sends the command
                        // to the prompt editor.
                        self.panel_search_focused = true;
                        let clicked = ((yf - list_top) / row_h) as usize;
                        let max_rows =
                            visible_panel_rows(renderer.viewport().1, renderer.cell_height());
                        if clicked < max_rows {
                            // v0.9: detect double-click on the same row.
                            let now = std::time::Instant::now();
                            let is_double = self
                                .panel_last_click
                                .map(|(t, row)| {
                                    t.elapsed() < std::time::Duration::from_millis(400)
                                        && row == clicked
                                })
                                .unwrap_or(false);
                            self.panel_last_click = Some((now, clicked));
                            self.panel_selection = clicked;
                            self.clamp_panel_selection();
                            // Scroll terminal to the selected block + highlight.
                            self.scroll_to_panel_selection();
                            if is_double {
                                // Double-click: send the command to the prompt.
                                self.send_panel_selection_to_input();
                            }
                            return;
                        }
                    } else if yf >= search_top && yf < search_bottom {
                        // Click in the search input field: focus it so keyboard
                        // input goes to panel_query (bug 6 fix).
                        self.panel_search_focused = true;
                        self.request_redraw();
                        return;
                    }
                } else {
                    // Click outside the panel: unfocus search (but keep panel open).
                    if self.panel_search_focused {
                        self.panel_search_focused = false;
                        self.request_redraw();
                    }
                }
            }
        }

        // v0.9 W3 (revised): block collapse/expand — only clicking the chevron
        // (▸/▾ in the first cell of a Command row) toggles fold. Clicking the
        // rest of the command line starts a normal text selection instead, so
        // the user can select/copy command text. This reverts the earlier
        // "click anywhere on the command line folds" behavior.
        if button == winit::event::MouseButton::Left && self.block_view_active() {
            if let Some(renderer) = &self.renderer {
                let chrome_left = if self.panel_open {
                    renderer.sidebar_width()
                } else {
                    0.0
                };
                let content_left = renderer.padding_x() + chrome_left;
                let cw = renderer.cell_width() as f32;
                let xf = x as f32;
                let yf = y as f32;
                // Chevron occupies the first cell [content_left, content_left + cw).
                if xf >= content_left && xf < content_left + cw {
                    for row in &renderer.block_view_rows {
                        if row.kind == weft_core::selection::BlockViewRowKind::Command
                            && yf >= row.y_top
                            && yf < row.y_bottom
                        {
                            if let Some(bid) = row.block_id {
                                if let Some(term) = self.tabs[self.active_tab].terminal.as_mut() {
                                    term.block_tracker_mut().toggle_collapse(bid);
                                    self.request_redraw();
                                }
                            }
                            return;
                        }
                    }
                }
            }
        }

        // OSC 8 hyperlink Cmd+Click: open the URL tagged on the clicked cell
        // via the registry's side-map. Bypasses normal selection / PTY mouse
        // reporting so Cmd+Click works even inside TUI apps that captured the
        // mouse (opencode, claude, vim) — same escape hatch as Shift+drag.
        if button == winit::event::MouseButton::Left && self.mods.state().super_key() {
            if let Some(url) = self.hyperlink_at_pixel(x, y) {
                open_url(&url);
                return;
            }
        }

        // Find popup button clicks (regex / case / up / down). Bypasses the
        // normal selection / PTY mouse path so the buttons work even inside
        // TUI apps that captured the mouse — same rationale as Cmd+Click.
        if button == winit::event::MouseButton::Left && self.find_open {
            if let Some(action) = self.find_button_at(x as f32, y as f32) {
                match action {
                    FindButtonAction::ToggleRegex => {
                        self.find_regex_mode = !self.find_regex_mode;
                        // Force immediate re-search so toggle is reflected
                        // (matches ToggleCase behavior — without this, typing
                        // the regex first and then toggling .* won't apply
                        // the regex mode to the existing query).
                        self.find_last_key = Some(std::time::Instant::now());
                        self.request_redraw();
                    }
                    FindButtonAction::ToggleCase => {
                        self.find_case_sensitive = !self.find_case_sensitive;
                        // Force immediate re-search so toggle is reflected.
                        self.find_last_key = Some(std::time::Instant::now());
                        self.request_redraw();
                    }
                    FindButtonAction::Next => self.find_cycle_next_prev(true),
                    FindButtonAction::Prev => self.find_cycle_next_prev(false),
                }
                return;
            }
        }

        // Check for popup border drag (completion or palette).
        if button == winit::event::MouseButton::Left {
            if let Some(drag) = self.check_popup_border_drag(x, y) {
                self.drag_state = Some(drag);
                return;
            }
        }

        // v0.9: Command Palette mouse interaction — click inside the popup
        // (but not on the border drag zone) selects the entry; double-click
        // runs it immediately. Mirrors the history panel's click/double-click
        // pattern so the user doesn't have to press Enter.
        if button == winit::event::MouseButton::Left && self.palette_open {
            if let Some(clicked_idx) = self.palette_row_at(x, y) {
                let now = std::time::Instant::now();
                let is_double = self
                    .palette_last_click
                    .map(|(t, row)| {
                        t.elapsed() < std::time::Duration::from_millis(400) && row == clicked_idx
                    })
                    .unwrap_or(false);
                self.palette_last_click = Some((now, clicked_idx));
                if is_double {
                    if let Some(entry) = self.palette_results.get(clicked_idx).cloned() {
                        self.activate_palette_entry(entry);
                    }
                } else {
                    self.palette_selection = clicked_idx;
                    self.request_redraw();
                }
                return;
            }
        }

        // If context menu is open, handle click as menu selection.
        // (v0.9 fix: removed the "click any block to fold" handler that
        // prevented text selection on block output. Folding is now solely
        // via the chevron click handler above — W3.)
        if button == winit::event::MouseButton::Left {
            if let Some(menu) = self.context_menu.take() {
                self.execute_context_menu(&menu, x as f32, y as f32);
                return;
            }
        }

        // v0.9: click inside the prompt input box → position the editor
        // cursor at the clicked char and start a mouse-drag selection (so
        // the user can select/copy part of the command). Clicks outside the
        // prompt box clear any active editor selection.
        if button == winit::event::MouseButton::Left {
            let in_prompt = self
                .renderer
                .as_ref()
                .and_then(|r| r.prompt_box_rect.get())
                .map(|[x0, y0, x1, y1]| {
                    let xf = x as f32;
                    let yf = y as f32;
                    xf >= x0 && xf <= x1 && yf >= y0 && yf <= y1
                })
                .unwrap_or(false);
            if in_prompt {
                if let Some(pos) = self.pixel_to_editor_pos(x, y) {
                    if let Some(t) = self.tabs[self.active_tab].terminal.as_mut() {
                        t.editor_mut().buffer.start_selection(pos);
                    }
                    self.prompt_dragging = true;
                    // Clear any block/grid selection so Cmd+C targets the editor.
                    self.tabs[self.active_tab].selection_handler.clear();
                    self.request_redraw();
                }
                return;
            } else if let Some(t) = self.tabs[self.active_tab].terminal.as_mut() {
                if t.editor().buffer.has_selection() {
                    t.editor_mut().buffer.clear_selection();
                    self.request_redraw();
                }
            }
            self.prompt_dragging = false;
        }

        let selecting = !self.mouse_reporting_active();
        let block_view = self.block_view_active();

        match button {
            winit::event::MouseButton::Left => {
                if selecting {
                    if block_view {
                        // Block view: hit-test against the cached visible-row
                        // snapshot and start a block-view selection. The row
                        // snapshot is cloned so the selection stays consistent
                        // with what the user saw at drag start, even if a PTY
                        // update re-lays-out the view mid-drag.
                        if let Some(bv_pos) = self.pixel_to_block_view_pos(x, y) {
                            let rows_snapshot = self
                                .renderer
                                .as_ref()
                                .map(|r| r.block_view_rows.clone())
                                .unwrap_or_default();
                            self.tabs[self.active_tab]
                                .selection_handler
                                .start_block_view(bv_pos, rows_snapshot);
                        } else {
                            // Click missed every selectable row (e.g. on the
                            // prompt box, CWD bar, or empty padding). Clear the
                            // existing selection so the user gets visual
                            // feedback that the previous selection is gone.
                            self.tabs[self.active_tab].selection_handler.clear();
                        }
                    } else {
                        // Grid view (alt-screen): classic grid selection.
                        let pos = self.pixel_to_grid(x, y);
                        let mode = if self.mods.state().shift_key() {
                            SelectionMode::Block
                        } else {
                            SelectionMode::Simple
                        };
                        self.tabs[self.active_tab]
                            .selection_handler
                            .start(pos, mode);
                    }
                }

                // If mouse protocol is active, send mouse event to PTY.
                // Always compute a grid pos for PTY mouse reporting (the
                // foreground program speaks grid coordinates, not block rows).
                let grid_pos = self.pixel_to_grid(x, y);
                self.send_mouse_event(MouseButton::Left, MouseAction::Press, grid_pos);
            }
            winit::event::MouseButton::Middle => {
                // Middle click: paste
                self.paste_from_clipboard();
                let pos = self.pixel_to_grid(x, y);
                self.send_mouse_event(MouseButton::Middle, MouseAction::Press, pos);
            }
            winit::event::MouseButton::Right => {
                // If context menu is open, right-click closes it.
                if self.context_menu.is_some() {
                    self.context_menu = None;
                    self.request_redraw();
                    return;
                }

                // Block view: open context menu on a block. block_at now
                // supports both completed blocks (including those with no
                // output) and the in-flight (running) command.
                if let Some(id) = self.block_at(y as f32) {
                    self.context_menu = Some(ContextMenu {
                        block_id: id,
                        x: x as f32,
                        y: y as f32,
                        selection: 0,
                    });
                    self.request_redraw();
                    return;
                }

                if selecting {
                    // Right click: extend selection.
                    if block_view {
                        if let Some(bv_pos) = self.pixel_to_block_view_pos(x, y) {
                            if self.tabs[self.active_tab]
                                .selection_handler
                                .block_view_selection
                                .is_none()
                            {
                                let rows_snapshot = self
                                    .renderer
                                    .as_ref()
                                    .map(|r| r.block_view_rows.clone())
                                    .unwrap_or_default();
                                self.tabs[self.active_tab]
                                    .selection_handler
                                    .start_block_view(bv_pos, rows_snapshot);
                            } else {
                                self.tabs[self.active_tab]
                                    .selection_handler
                                    .extend_block_view(bv_pos);
                            }
                        }
                    } else {
                        let pos = self.pixel_to_grid(x, y);
                        if self.tabs[self.active_tab]
                            .selection_handler
                            .selection
                            .is_none()
                        {
                            self.tabs[self.active_tab]
                                .selection_handler
                                .start(pos, SelectionMode::Simple);
                        } else {
                            self.tabs[self.active_tab].selection_handler.extend(pos);
                        }
                    }
                }
                let pos = self.pixel_to_grid(x, y);
                self.send_mouse_event(MouseButton::Right, MouseAction::Press, pos);
            }
            _ => {}
        }

        self.request_redraw();
    }

    /// Handle mouse release.
    fn handle_mouse_release(&mut self, _x: f64, _y: f64, button: winit::event::MouseButton) {
        // End popup border drag if active.
        if button == winit::event::MouseButton::Left && self.drag_state.is_some() {
            self.drag_state = None;
            return;
        }

        // v0.9: end editor drag-selection (the selection itself stays so
        // Cmd+C can copy it).
        if button == winit::event::MouseButton::Left && self.prompt_dragging {
            self.prompt_dragging = false;
            // A click without drag (anchor == cursor) leaves an empty
            // selection — clear it so the caret shows normally.
            if let Some(t) = self.tabs[self.active_tab].terminal.as_mut() {
                if !t.editor().buffer.has_selection() {
                    // has_selection returns false when anchor==cursor, so
                    // explicitly clear the anchor to drop the empty selection.
                    t.editor_mut().buffer.clear_selection();
                }
            }
            self.request_redraw();
        }

        let pos = self.pixel_to_grid(_x, _y);
        self.tabs[self.active_tab].selection_handler.end();

        let btn = match button {
            winit::event::MouseButton::Left => MouseButton::Left,
            winit::event::MouseButton::Middle => MouseButton::Middle,
            winit::event::MouseButton::Right => MouseButton::Right,
            _ => return,
        };
        self.send_mouse_event(btn, MouseAction::Release, pos);
    }

    /// Handle mouse movement.
    fn handle_mouse_move(&mut self, x: f64, y: f64) {
        // Update popup drag if active (clone to avoid borrow conflict).
        if let Some(drag) = self.drag_state.clone() {
            self.update_popup_drag(x, y, &drag);
            return;
        }

        // v0.9 W1+: tab bar hover detection — show close "×" on the hovered
        // tab (Warp-style). Only active when more than one tab is open (the
        // bar is hidden for a single tab). Reads `tab_hits` populated during
        // the last draw; layout is stable between mouse moves at rest.
        if self.tabs.len() > 1 {
            let new_hover: Option<usize> = if let Some(renderer) = &self.renderer {
                let bar_h = renderer.tab_bar_height();
                let yf = y as f32;
                if yf <= bar_h {
                    let xf = x as f32;
                    renderer
                        .tab_hits
                        .iter()
                        .find(|h| xf >= h.tab_rect[0] && xf < h.tab_rect[2])
                        .map(|h| h.index)
                } else {
                    None
                }
            } else {
                None
            };
            if new_hover != self.hovered_tab {
                self.hovered_tab = new_hover;
                self.request_redraw();
            }
        }

        // v0.9: extend editor drag-selection inside the prompt box.
        if self.prompt_dragging {
            if let Some(pos) = self.pixel_to_editor_pos(x, y) {
                if let Some(t) = self.tabs[self.active_tab].terminal.as_mut() {
                    t.editor_mut().buffer.extend_selection(pos);
                    self.request_redraw();
                }
            }
        }

        if self.tabs[self.active_tab].selection_handler.selecting {
            if self.block_view_active() {
                if let Some(bv_pos) = self.pixel_to_block_view_pos(x, y) {
                    self.tabs[self.active_tab]
                        .selection_handler
                        .extend_block_view(bv_pos);
                    self.request_redraw();
                }
            } else {
                let pos = self.pixel_to_grid(x, y);
                self.tabs[self.active_tab].selection_handler.extend(pos);
                self.request_redraw();
            }
        }

        // PTY mouse reporting always speaks grid coordinates.
        let pos = self.pixel_to_grid(x, y);
        self.send_mouse_event(MouseButton::Left, MouseAction::Move, pos);
    }

    /// Check if a click (x, y) lands on a popup border drag handle.
    /// Returns a DragState if so, enabling resize-drag.
    /// Uses the actual popup rectangles stored by the renderer (not
    /// approximations), so hot-zone detection is accurate.
    fn check_popup_border_drag(&self, x: f64, y: f64) -> Option<DragState> {
        let renderer = self.renderer.as_ref()?;
        let (cw, ch) = (renderer.cell_width() as f32, renderer.cell_height() as f32);
        let hot_zone = 8.0; // px from the border (wider for usability)

        // Gather all active popup rects (completion + palette).
        let mut rects: Vec<[f32; 4]> = Vec::new();
        if let Some(r) = renderer.completion_popup_rect {
            rects.push(r);
        }
        if let Some(r) = renderer.palette_popup_rect {
            rects.push(r);
        }

        let xf = x as f32;
        let yf = y as f32;

        for &[rx0, ry0, rx1, ry1] in &rects {
            // Right border: x near rx1, y within [ry0, ry1].
            let on_right = (xf - rx1).abs() < hot_zone && yf >= ry0 && yf <= ry1;
            // Top border: y near ry0, x within [rx0, rx1].
            let on_top = (yf - ry0).abs() < hot_zone && xf >= rx0 && xf <= rx1;

            let target = if on_right {
                DragTarget::Right
            } else if on_top {
                DragTarget::Top
            } else {
                continue;
            };

            return Some(DragState {
                target,
                start_x: x,
                start_y: y,
                start_scale: self.popup_width_scale,
                start_rows: self.popup_max_rows,
                cell_w: cw,
                cell_h: ch,
            });
        }

        None
    }

    /// Update popup dimensions during a border drag.
    fn update_popup_drag(&mut self, x: f64, y: f64, drag: &DragState) {
        match drag.target {
            DragTarget::Right => {
                // Width: delta-x adjusts the popup width scale.
                let dx = (x - drag.start_x) as f32;
                let vp_w = self
                    .renderer
                    .as_ref()
                    .map(|r| r.viewport_width())
                    .unwrap_or(800.0);
                let scale_delta = dx / vp_w;
                self.popup_width_scale = (drag.start_scale + scale_delta).clamp(0.3, 0.95);
            }
            DragTarget::Top => {
                // Height: delta-y (upward = more rows).
                let dy = (drag.start_y - y) as f32;
                let row_delta = (dy / drag.cell_h) as i32;
                let new_rows = (drag.start_rows as i32 + row_delta).clamp(3, 20) as usize;
                self.popup_max_rows = new_rows;
            }
        }
        self.request_redraw();
    }

    /// Execute a context menu action based on click position.
    fn execute_context_menu(&mut self, menu: &ContextMenu, click_x: f32, click_y: f32) {
        let ch = self
            .renderer
            .as_ref()
            .map(|r| r.cell_height() as f32)
            .unwrap_or(16.0);
        let item_h = ch * 1.2;
        let top_inset = ch * 0.2; // matches renderer's `menu_y0 + ch * 0.2`

        // Check if click is on a menu item.
        for (i, _label) in CONTEXT_MENU_ITEMS.iter().enumerate() {
            let item_y = menu.y + top_inset + i as f32 * item_h;
            if click_y >= item_y && click_y < item_y + item_h && click_x >= menu.x {
                // Execute the action.
                let action = CONTEXT_MENU_ITEMS[i].1;
                self.run_context_action(menu.block_id, action);
                self.request_redraw();
                return;
            }
        }
        // Click outside menu items — just close (already taken).
        self.request_redraw();
    }

    /// Run a context menu action on the target block.
    /// `block_id` is `None` for the in-flight (running) command.
    fn run_context_action(&mut self, block_id: Option<BlockId>, action: &str) {
        let Some(terminal) = &mut self.tabs[self.active_tab].terminal else {
            return;
        };

        match action {
            "copy_command" | "copy_output" => {
                // For in-flight blocks, copy from the live command/output.
                if block_id.is_none() {
                    if let Some(live) = terminal.block_tracker().in_flight() {
                        let text = if action == "copy_command" {
                            live.command.to_string()
                        } else {
                            live.output.to_string()
                        };
                        clipboard_copy(&text);
                        info!(len = text.len(), "copied in-flight to clipboard");
                    }
                    return;
                }
                let bid = block_id.unwrap();
                let block = terminal
                    .block_tracker()
                    .session_blocks()
                    .iter()
                    .find(|b| b.id == bid);
                if let Some(b) = block {
                    let text = if action == "copy_command" {
                        &b.command
                    } else {
                        &b.output
                    };
                    clipboard_copy(text);
                    info!(len = text.len(), "copied to clipboard");
                }
            }
            "toggle_fold" => {
                if let Some(bid) = block_id {
                    terminal.block_tracker_mut().toggle_collapse(bid);
                }
                // In-flight blocks can't be folded (no finalized block yet).
            }
            // W4: copy the block's command into the editor buffer so the user
            // can tweak parameters and re-submit (Warp-style "rerun"). Only
            // takes effect at the prompt — when a command is running or an
            // alt-screen app is active, the editor isn't the effective input
            // mode, so we silently no-op rather than stashing text the user
            // would see resurface unexpectedly when the prompt returns.
            "send_to_input" => {
                if terminal.effective_input_mode() == weft_core::input::InputMode::Editor {
                    // Clone first to release the immutable borrow before editor_mut().
                    let cmd = if let Some(bid) = block_id {
                        terminal
                            .block_tracker()
                            .session_blocks()
                            .iter()
                            .find(|b| b.id == bid)
                            .map(|b| b.command.clone())
                    } else {
                        terminal
                            .block_tracker()
                            .in_flight()
                            .map(|f| f.command.to_string())
                    };
                    if let Some(cmd) = cmd {
                        if !cmd.is_empty() {
                            terminal.editor_mut().buffer.set_text(&cmd);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Handle scroll wheel.
    fn handle_scroll(&mut self, delta: winit::event::MouseScrollDelta, x: f64, y: f64) {
        // v0.9 fix: drain pending PTY messages BEFORE checking alt-screen
        // state. When `less` (or any alt-screen app) starts, the enter
        // sequence (`\x1b[?1049h`) is still in the channel until the next
        // `process_messages` call. Without this drain, the first wheel
        // events see `alt_active = false` and fall through to the
        // viewport-scroll branch (which does nothing on alt screen). After
        // a keyboard event triggers a redraw → process_messages → alt_active
        // becomes true, the wheel starts working — which matches the user
        // report "scrolling works only after pressing a key".
        self.pump_pty();
        self.process_messages();

        let lines = match delta {
            winit::event::MouseScrollDelta::LineDelta(_, v) => {
                if v > 0.0 {
                    v.ceil() as usize
                } else {
                    v.floor().abs() as usize
                }
            }
            winit::event::MouseScrollDelta::PixelDelta(pos) => {
                let v = pos.y / 40.0; // approx 40px per line
                if v > 0.0 {
                    v.ceil() as usize
                } else {
                    v.floor().abs() as usize
                }
            }
        };

        if lines == 0 {
            return;
        }

        // Short-lived immutable borrow to read the mode flags up-front —
        // avoids holding a long-lived mutable borrow of `terminal` across
        // later accesses to `block_scroll_offset`, `renderer`, etc.
        let (mouse_protocol_active, alt_screen_active, block_view) = {
            let Some(t) = &self.tabs[self.active_tab].terminal else {
                return;
            };
            (
                t.mouse_protocol != MouseProtocol::Off,
                t.is_alt_screen_active(),
                t.show_block_view(),
            )
        };

        // Check if mouse protocol is active — forward scroll to PTY
        if mouse_protocol_active {
            let up = match delta {
                winit::event::MouseScrollDelta::LineDelta(_, v) => v > 0.0,
                winit::event::MouseScrollDelta::PixelDelta(pos) => pos.y > 0.0,
            };
            let pos = self.pixel_to_grid(x, y);
            let mut m = Modifiers::empty();
            if self.mods.state().shift_key() {
                m |= Modifiers::SHIFT;
            }
            if self.mods.state().alt_key() {
                m |= Modifiers::ALT;
            }
            if self.mods.state().control_key() {
                m |= Modifiers::CONTROL;
            }
            if let Some(bytes) = self.tabs[self.active_tab]
                .input_handler
                .encode_scroll(up, pos.col, pos.row, m)
            {
                if let Some(pty) = &self.tabs[self.active_tab].pty {
                    let _ = pty.write_sync(&bytes);
                }
            }
            return;
        }

        // Alt-screen apps (less, vim, man, etc.) don't use mouse protocol but
        // still benefit from wheel scroll: translate to Up/Down arrow key
        // sequences so the pager scrolls its content natively.
        if alt_screen_active {
            let up = match delta {
                winit::event::MouseScrollDelta::LineDelta(_, v) => v > 0.0,
                winit::event::MouseScrollDelta::PixelDelta(pos) => pos.y > 0.0,
            };
            let key = if up { KeyCode::Up } else { KeyCode::Down };
            let mut m = Modifiers::empty();
            if self.mods.state().shift_key() {
                m |= Modifiers::SHIFT;
            }
            let single = self.tabs[self.active_tab].input_handler.encode_key(key, m);
            if !single.is_empty() {
                let mut batch = Vec::with_capacity(single.len() * lines);
                for _ in 0..lines {
                    batch.extend_from_slice(&single);
                }
                if let Some(pty) = &self.tabs[self.active_tab].pty {
                    let _ = pty.write_sync(&batch);
                }
            }
            return;
        }

        // Otherwise, scroll the terminal viewport
        let up = match delta {
            winit::event::MouseScrollDelta::LineDelta(_, v) => v > 0.0,
            winit::event::MouseScrollDelta::PixelDelta(pos) => pos.y > 0.0,
        };
        // Block view uses a dedicated scroll offset (not grid.scroll_offset,
        // which is clamped to grid scrollback — the wrong proxy for block
        // content like headers/commands/separators).
        if block_view {
            // Cap scroll speed at 1 row per wheel notch in the block view.
            // macOS trackpad inertia can send 3-4 lines per tick, which skips
            // past content too fast for comfortable reading.
            let scroll_lines = lines.min(1);
            // Compute metrics via a short-lived immutable borrow of `terminal`
            // so we can later mutate `block_scroll_offset` (same Tab, but a
            // disjoint field — allowed once the immutable borrow ends).
            let (total, prompt_lines) = {
                let Some(t) = &self.tabs[self.active_tab].terminal else {
                    return;
                };
                let cols = t.grid().num_cols;
                let (total, _) = block_content_metrics(t, cols);
                (total, t.editor().buffer.lines.len())
            };
            // Compute visible rows from the renderer's actual geometry
            // (pitch = ch * 1.1, region = viewport minus prompt box).
            // The old code used grid().num_rows which overcounts because
            // the block view uses a 10% taller line pitch and doesn't
            // occupy the full viewport (prompt box eats space).
            let visible = self
                .renderer
                .as_ref()
                .map(|r| r.block_visible_rows(prompt_lines))
                .unwrap_or(1);
            let max_scroll = total.saturating_sub(visible);
            if up {
                self.tabs[self.active_tab].block_scroll_offset = self.tabs[self.active_tab]
                    .block_scroll_offset
                    .saturating_add(scroll_lines)
                    .min(max_scroll);
            } else {
                self.tabs[self.active_tab].block_scroll_offset = self.tabs[self.active_tab]
                    .block_scroll_offset
                    .saturating_sub(scroll_lines);
            }
        } else {
            // Grid view scroll — needs mutable terminal.
            if let Some(terminal) = &mut self.tabs[self.active_tab].terminal {
                let grid = &mut terminal.grid_mut();
                if up {
                    grid.scroll_up_history(lines);
                } else {
                    grid.scroll_down_history(lines);
                }
            }
        }
        self.request_redraw();
    }

    /// Send a mouse event to the PTY if mouse protocol is active.
    fn send_mouse_event(&self, button: MouseButton, action: MouseAction, pos: GridPos) {
        let Some(terminal) = &self.tabs[self.active_tab].terminal else {
            return;
        };
        if terminal.mouse_protocol == MouseProtocol::Off {
            return;
        }
        let mut m = Modifiers::empty();
        if self.mods.state().shift_key() {
            m |= Modifiers::SHIFT;
        }
        if self.mods.state().alt_key() {
            m |= Modifiers::ALT;
        }
        if self.mods.state().control_key() {
            m |= Modifiers::CONTROL;
        }
        if let Some(bytes) = self.tabs[self.active_tab]
            .input_handler
            .encode_mouse(button, action, pos.col, pos.row, m)
        {
            if let Some(pty) = &self.tabs[self.active_tab].pty {
                let _ = pty.write_sync(&bytes);
            }
        }
    }

    /// Copy selection to system clipboard.
    ///
    /// Dispatches on the active view: block view copies from the captured
    /// `BlockViewSelection` row snapshot (what the user actually sees), grid
    /// view copies from the terminal Grid. This split fixes the "复制错位"
    /// bug where a grid-coordinate copy landed on the wrong line because the
    /// block view's pitch/scroll/layout don't map 1:1 to grid rows.
    fn copy_selection(&self) {
        let Some(terminal) = &self.tabs[self.active_tab].terminal else {
            return;
        };
        // v0.9: editor drag-selection (or select-all after double-click→send-
        // to-prompt) takes priority — Cmd+C copies the selected editor text.
        if let Some(text) = terminal.editor().buffer.selected_text() {
            if !text.is_empty() {
                clipboard_copy(&text);
                return;
            }
        }
        let text = if terminal.show_block_view() {
            self.tabs[self.active_tab]
                .selection_handler
                .block_view_text()
        } else {
            self.tabs[self.active_tab]
                .selection_handler
                .selected_text(terminal.grid())
        };
        if let Some(text) = text {
            if !text.is_empty() {
                clipboard_copy(&text);
            }
        }
    }

    /// Paste from system clipboard.
    ///
    /// Two-path dispatch mirrors `Ime::Commit` (main.rs ~L2721): in Editor mode
    /// the shell is taken over by weft and does not echo, so pasted bytes sent
    /// to the PTY would vanish. Instead we insert the text directly into the
    /// editor buffer. Passthrough mode forwards to the PTY as before (with
    /// bracketed-paste wrapping when the shell supports it).
    fn paste_from_clipboard(&mut self) {
        let Some(text) = clipboard_paste() else {
            return;
        };
        if text.is_empty() {
            return;
        }

        let mode = self.tabs[self.active_tab]
            .terminal
            .as_ref()
            .map(|t| t.effective_input_mode())
            .unwrap_or(weft_core::input::InputMode::Passthrough);

        if mode == weft_core::input::InputMode::Editor {
            // Editor takeover: paste into the input box. Multi-line text is
            // split on \n (insert_char rejects control chars including \n,
            // so we must drive split_newline explicitly to preserve line
            // breaks). \r is dropped to handle CRLF paste from external apps.
            if let Some(t) = self.tabs[self.active_tab].terminal.as_mut() {
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
            let bracketed = self.tabs[self.active_tab]
                .terminal
                .as_ref()
                .map(|t| t.bracketed_paste)
                .unwrap_or(false);
            let bytes = encode_paste(&text, bracketed);
            if let Some(pty) = &self.tabs[self.active_tab].pty {
                if let Err(e) = pty.write_sync(&bytes) {
                    warn!("Failed to paste to PTY: {e}");
                }
            }
        }
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
        let elapsed = now.duration_since(self.cursor_blink_time);

        // Grid-view hard blink: toggle every 530ms (anchor reset on toggle).
        if elapsed >= std::time::Duration::from_millis(530) {
            self.cursor_blink_on = !self.cursor_blink_on;
            self.cursor_blink_time = now;
        }

        // Prompt signature breath: advance phase continuously.
        // Period 2400ms → one full sin() cycle; phase stored in radians.
        const PERIOD_MS: f64 = 2400.0;
        let elapsed_ms = elapsed.as_millis() as f64;
        // Each update advances phase by (elapsed_ms / PERIOD_MS) * 2π.
        let delta = (elapsed_ms / PERIOD_MS) * std::f64::consts::TAU;
        self.cursor_blink_phase += delta as f32;
        // Wrap into [0, 2π) to avoid float drift over long sessions.
        if self.cursor_blink_phase >= std::f32::consts::TAU {
            self.cursor_blink_phase -= std::f32::consts::TAU;
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
    /// Cross-thread wake-up (PTY output or blink timer): schedule one redraw.
    /// The pump/process/draw happens in `WindowEvent::RedrawRequested`, so we
    /// avoid the vsync busy-loop while still reacting promptly to output.
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: AppEvent) {
        match event {
            AppEvent::Wake => self.request_redraw(),
            AppEvent::ConfigReload => {
                self.reload_config();
                self.request_redraw();
            }
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let win = &self.config.window;
        // v0.9 U-D1: resolve the startup theme honoring `follow_system` so
        // the window opens with the correct color from frame 0 (no dark→light
        // flash). `Config::theme()` ignores follow_system; this mirrors the
        // logic in `apply_config` / `poll_system_appearance`.
        let startup_theme = if self.config.theme.follow_system {
            let dark = unsafe { system_appearance_is_dark() };
            let name = if dark {
                self.config
                    .theme
                    .dark_name
                    .clone()
                    .unwrap_or_else(|| "weft-warm".to_string())
            } else {
                self.config
                    .theme
                    .light_name
                    .clone()
                    .unwrap_or_else(|| "weft-light".to_string())
            };
            weft_core::config::Theme::resolve_named(&name, &self.config.theme)
        } else {
            self.config.theme()
        };
        let attrs = WindowAttributes::default()
            .with_title(&win.title)
            .with_inner_size(winit::dpi::LogicalSize::new(
                win.width as f64,
                win.height as f64,
            ))
            // Window-level transparency is fixed at creation; the layer opaque
            // flag + bg alpha still update live, but crossing the 1.0 boundary
            // (opaque ↔ see-through) needs a relaunch.
            .with_transparent(win.opacity < 1.0);

        let window = event_loop.create_window(attrs).unwrap();
        let renderer = MetalRenderer::new(
            &window,
            self.config.font.clone(),
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
        let init_rows =
            ((win_size.height as f64) / renderer.cell_height() as f64).max(1.0) as usize;
        let init_cols = ((win_size.width as f64) / renderer.cell_width() as f64).max(1.0) as usize;
        self.spawn_pty(init_rows, init_cols);
        self.window = Some(window);
        self.renderer = Some(renderer);

        // v0.9 U-D1: seed the appearance tracker so the first
        // `poll_system_appearance` (1s after launch) doesn't re-apply the
        // same theme and cause a flicker. The startup theme above already
        // queried the system appearance, so we record it as "known".
        if self.config.theme.follow_system {
            let dark = unsafe { system_appearance_is_dark() };
            self.last_system_appearance_dark = Some(dark);
            self.theme_is_dark = dark;
        }

        // Open the command-block DB (best-effort) and hydrate the tracker with
        // recent history so the panel has content on first show.
        self.block_store = weft_cache_dir().and_then(|cache| {
            let path = cache.join("blocks.db");
            match BlockStore::open(&path) {
                Ok(store) => {
                    if let Some(terminal) = &mut self.tabs[self.active_tab].terminal {
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

        // Open the workflow DB (best-effort) and seed built-in templates on
        // first launch.
        self.workflow_store = weft_cache_dir().and_then(|cache| {
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
        let blink_proxy = self.proxy.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_millis(530));
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

        self.request_redraw();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                info!("Window closed");
                event_loop.exit();
            }
            WindowEvent::Resized(physical_size) => {
                // Grid/PTY tracks the FULL window (the editor input box is an
                // overlay, never a grid resize) — see grid_dims.
                if let (Some(renderer), Some(window)) = (&mut self.renderer, &self.window) {
                    let pad_x = renderer.padding_x() as f64;
                    let pad_y = renderer.padding_y() as f64;
                    // v0.9 H1: subtract tab bar height from usable height when
                    // more than one tab is open. With a single tab the bar is
                    // hidden (matches the previous design).
                    let tab_bar_h = if self.tabs.len() > 1 {
                        renderer.tab_bar_height() as f64
                    } else {
                        0.0
                    };
                    // v0.9 W5: subtract sidebar width when the panel is open so
                    // the grid reflows beside the sidebar (mirrors grid_dims).
                    let chrome_left = if self.panel_open {
                        renderer.sidebar_width() as f64
                    } else {
                        0.0
                    };
                    let usable_w = physical_size.width as f64 - 2.0 * pad_x - chrome_left;
                    let usable_h = (physical_size.height as f64 - 2.0 * pad_y - tab_bar_h).max(0.0);
                    let new_cols = (usable_w / renderer.cell_width() as f64).max(0.0) as usize;
                    let new_rows = (usable_h / renderer.cell_height() as f64).max(0.0) as usize;

                    if new_cols > 0 && new_rows > 0 {
                        // Update renderer viewport immediately
                        renderer.resize(window, physical_size);

                        // Resize ALL tabs' grids immediately for smooth
                        // animation. The rewrap is fast (<1ms) so doing it
                        // on every intermediate event is fine. Background
                        // tabs also need resizing so their content wraps
                        // correctly when switched to.
                        for tab in &mut self.tabs {
                            if let Some(terminal) = &mut tab.terminal {
                                terminal.resize(new_rows, new_cols);
                            }
                            // Debounce the PTY SIGWINCH for each tab.
                            tab.pending_pty_resize = Some((new_rows, new_cols));
                        }
                        info!(rows = new_rows, cols = new_cols, "all tabs resized (event)");
                        self.last_resize_instant = std::time::Instant::now();
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                self.pump_pty();
                let had_output = self.process_messages();
                self.update_cursor_blink();
                // FindInGrid debounce: when 150ms have elapsed since the last
                // keystroke, run the search and update `find_matches`.
                self.maybe_refresh_find_results();
                // v0.9 U-P1: drain pending find-worker results (async grid search).
                self.poll_find_worker_results();
                // v0.9 U-D1: poll macOS system appearance (throttled to 1Hz).
                self.poll_system_appearance();

                // During command execution, new output streams in — snap the
                // block view to the bottom so the user sees fresh content.
                // (At prompt / idle, preserve the user's scroll position.)
                if had_output {
                    if let Some(t) = &self.tabs[self.active_tab].terminal {
                        if t.block_tracker().phase() == ShellPhase::CommandExecuting {
                            self.tabs[self.active_tab].block_scroll_offset = 0;
                        }
                    }
                }

                // If the grid row count drifted from what the terminal holds
                // (font/padding/window-size change, or the one-time convergence
                // from the spawn size to the padded size) — recompute. Mode
                // transitions no longer cause drift: the grid is always
                // full-window and the input box is a non-resizing overlay.
                let desired_rows = self.grid_dims().0;
                let current_rows = self.tabs[self.active_tab]
                    .terminal
                    .as_ref()
                    .map(|t| t.grid().num_rows)
                    .unwrap_or(0);
                if desired_rows != 0 && desired_rows != current_rows {
                    self.recompute_layout();
                }

                // Flush debounced PTY resize for ALL tabs after the cascade
                // settles (100ms of no new resize events). The grids were
                // already resized immediately in the Resized handler —
                // this only sends the SIGWINCH to each shell.
                if self.last_resize_instant.elapsed() > std::time::Duration::from_millis(100) {
                    for tab in &mut self.tabs {
                        if let Some((rows, cols)) = tab.pending_pty_resize.take() {
                            if let Some(pty) = &tab.pty {
                                if let Err(e) = pty.resize(rows as u16, cols as u16) {
                                    warn!("PTY resize failed: {e}");
                                }
                            }
                        }
                    }
                }

                // v0.9 H1: borrow the active Tab once and access its fields
                // (terminal / ime_preedit / selection_handler / block_scroll_offset)
                // as disjoint field borrows. Indexing `self.tabs[i]` repeatedly
                // would prevent Rust from splitting borrows across the
                // `&tab.terminal` (immutable) and `&mut tab.selection_handler`
                // (mutable) needed by `renderer.draw`. `self.tabs` and
                // `self.renderer` are disjoint fields of `App`, so both can be
                // mutably borrowed at once.
                let active = self.active_tab;
                // Compute the tab bar state BEFORE borrowing `self.tabs`
                // mutably below: `tab_bar_state()` reads `self.tabs[*]`
                // labels and would conflict with `&mut self.tabs[active]`.
                // The returned `TabBarDrawState` is owned and lives for the
                // whole draw call.
                let tab_bar = self.tab_bar_state();
                let tab = &mut self.tabs[active];
                if let (Some(renderer), Some(terminal)) = (&mut self.renderer, &tab.terminal) {
                    // Sync popup dimensions to renderer (user-adjustable via border drag).
                    renderer.set_popup_size(self.popup_width_scale, self.popup_max_rows);
                    // Sync context menu target to renderer.
                    renderer.context_menu_target =
                        self.context_menu.as_ref().map(|m| (m.x, m.y, m.block_id));

                    // Build palette entries as (label, description, kind_label) tuples.
                    // v0.9 W2+: in SelectTheme sub-mode, project theme names
                    // (filtered from the full list) instead of the generic
                    // "Select Theme" builtin label.
                    let palette_entries: Vec<(String, String, &str)> =
                        if matches!(self.palette_submode, PaletteSubMode::SelectTheme { .. }) {
                            let (buffer, themes) = match &self.palette_submode {
                                PaletteSubMode::SelectTheme { buffer, themes } => {
                                    (buffer.clone(), themes.clone())
                                }
                                _ => unreachable!(),
                            };
                            let q = buffer.to_lowercase();
                            themes
                                .iter()
                                .filter(|n| q.is_empty() || n.to_lowercase().contains(&q))
                                .map(|n| (n.clone(), String::new(), "Theme"))
                                .collect()
                        } else {
                            self.palette_results
                                .iter()
                                .map(|e| match e {
                                    PaletteEntry::Workflow(wf) => {
                                        (wf.name.clone(), wf.description.clone(), "Workflow")
                                    }
                                    PaletteEntry::Builtin(b) => {
                                        (b.label().to_string(), String::new(), "Builtin")
                                    }
                                })
                                .collect()
                        };

                    // Compute palette banner + submode input from the sub-mode state.
                    let (palette_banner, palette_submode_input) = match &self.palette_submode {
                        PaletteSubMode::Search => (String::new(), String::new()),
                        PaletteSubMode::CreateWorkflow { step, buffer, .. } => {
                            let label = match step {
                                CreateStep::Name => "New workflow — name:",
                                CreateStep::Command => "New workflow — command (use {{var}}):",
                                CreateStep::Done => "Creating...",
                            };
                            (label.to_string(), buffer.clone())
                        }
                        PaletteSubMode::EditWorkflow { name, buffer, .. } => {
                            (format!("Edit '{name}':"), buffer.clone())
                        }
                        PaletteSubMode::ConfirmDelete { name, .. } => {
                            (format!("Delete '{name}'? (y/n)"), String::new())
                        }
                        PaletteSubMode::SelectTheme { buffer, .. } => {
                            ("Select theme:".to_string(), buffer.clone())
                        }
                    };

                    let overlays = crate::overlay::build_overlay_stack(
                        terminal,
                        renderer.viewport_width(),
                        renderer.scale(),
                        self.panel_open,
                        &self.panel_query,
                        self.panel_selection,
                        self.panel_expanded,
                        self.panel_search_focused,
                        &tab.ime_preedit,
                        self.palette_open,
                        &self.palette_query,
                        self.palette_selection,
                        &palette_entries,
                        &palette_banner,
                        &palette_submode_input,
                        terminal.editor().buffer.selection_range(),
                    );
                    // v0.8 U6: compute block-content metrics for the dynamic
                    // scrollbar thumb (total/visible/max_scroll). None in grid
                    // view — the scrollbar only shows in block view anyway.
                    let scroll_metrics = if terminal.show_block_view() {
                        let cols = terminal.grid().num_cols;
                        let (total, _) = block_content_metrics(terminal, cols);
                        let prompt_lines = terminal.editor().buffer.lines.len();
                        let visible = renderer.block_visible_rows(prompt_lines);
                        let max_scroll = total.saturating_sub(visible);
                        Some((total, visible, max_scroll))
                    } else {
                        None
                    };
                    // v0.8 B3: populate find overlay state before draw. None
                    // when the bar is closed so the renderer skips the overlay.
                    // The total includes block-view matches so the count
                    // reflects what the user actually sees (block content
                    // isn't in the grid).
                    // Compute find state values BEFORE the mutable borrow
                    // on `renderer` (renderer.find_state = ...).
                    let find_state = if self.find_open {
                        let grid_total = self.find_matches.len();
                        let block_total = self.find_block_matches.len();
                        let block_view = terminal.show_block_view();
                        let (total, current) = if block_view {
                            (block_total, self.find_block_index + 1)
                        } else {
                            (
                                grid_total,
                                if grid_total == 0 {
                                    0
                                } else {
                                    self.find_index + 1
                                },
                            )
                        };
                        let truncated = self.find_truncated || self.find_block_truncated;
                        let highlight = if !block_view {
                            let grid = terminal.grid();
                            let sb_len = grid.scrollback_len();
                            let offset = grid.scroll_offset.min(sb_len);
                            let unified_base = sb_len - offset;
                            self.find_matches
                                .get(self.find_index)
                                .map(|m| (m.row.saturating_sub(unified_base), m.col, m.len))
                        } else {
                            None
                        };
                        let block_highlight = if block_view {
                            self.find_block_matches
                                .get(self.find_block_index)
                                .map(|m| (m.block_id.0, m.line, m.is_command, m.col, m.len))
                        } else {
                            None
                        };
                        Some(FindDrawState {
                            query: self.find_query.clone(),
                            current,
                            total,
                            truncated,
                            highlight,
                            block_highlight,
                            block_matches: if block_view { 0 } else { block_total },
                            regex_mode: self.find_regex_mode,
                            case_sensitive: self.find_case_sensitive,
                            regex_error: self.find_regex_error.clone(),
                        })
                    } else {
                        None
                    };
                    renderer.find_state = find_state;
                    // v0.9 W2: expire panel highlight after 1.5s.
                    if let Some(until) = self.panel_highlight_until {
                        if std::time::Instant::now() >= until {
                            self.panel_highlight = None;
                            self.panel_highlight_until = None;
                        }
                    }
                    renderer.panel_highlight = self.panel_highlight;
                    // Pause cursor blink while the user is actively selecting
                    // OR while a selection is visible (not yet cleared). A
                    // moving or persistent selection is the focus of attention;
                    // a blinking caret distracts. Resumes when the selection
                    // is cleared (click on empty area / prompt / Esc).
                    let has_selection = tab.selection_handler.selecting
                        || tab.selection_handler.block_view_selection.is_some()
                        || tab.selection_handler.selection.is_some();
                    let blink_on = self.cursor_blink_on && !has_selection;
                    renderer.draw(
                        terminal,
                        &mut tab.selection_handler,
                        blink_on,
                        self.cursor_blink_phase,
                        &overlays,
                        tab.block_scroll_offset,
                        scroll_metrics,
                        &tab_bar,
                    );
                }

                // No busy-loop redraw here: the PTY reader thread and the
                // cursor-blink timer wake the loop via `AppEvent::Wake`
                // whenever there is work (see `user_event`). This lets the CPU
                // idle instead of spinning at vsync.
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == winit::event::ElementState::Pressed {
                    if let PhysicalKey::Code(key_code) = event.physical_key {
                        // `event.text` already reflects Shift (and the keymap),
                        // e.g. Shift+A -> "A", Shift+1 -> "!". The editor uses it
                        // so typed commands keep their case / shifted symbols.
                        self.handle_key_event(key_code, self.mods, event.text.as_deref());
                    }
                }
            }
            WindowEvent::ModifiersChanged(new_mods) => {
                self.mods = new_mods;
            }
            WindowEvent::MouseInput { state, button, .. } => match state {
                winit::event::ElementState::Pressed => {
                    self.handle_mouse_press(self.last_mouse_x, self.last_mouse_y, button);
                }
                winit::event::ElementState::Released => {
                    self.handle_mouse_release(self.last_mouse_x, self.last_mouse_y, button);
                }
            },
            WindowEvent::CursorMoved { position, .. } => {
                self.last_mouse_x = position.x;
                self.last_mouse_y = position.y;
                self.handle_mouse_move(position.x, position.y);
            }
            WindowEvent::CursorLeft { .. } => {
                // v0.9 W1+: clear tab hover state when the mouse leaves the
                // window, so the close "×" doesn't stay visible on a tab that
                // is no longer hovered.
                if self.hovered_tab.is_some() {
                    self.hovered_tab = None;
                    self.request_redraw();
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                self.handle_scroll(delta, self.last_mouse_x, self.last_mouse_y);
            }
            WindowEvent::Ime(ime_event) => {
                match ime_event {
                    winit::event::Ime::Preedit(text, _cursor) => {
                        // v0.9 fix: when a modal input (find/palette/panel) is
                        // active, suppress the terminal preedit so the IME
                        // composition doesn't render in the prompt editor.
                        if self.find_open || self.palette_open || self.panel_search_focused {
                            self.tabs[self.active_tab].ime_preedit.clear();
                        } else {
                            self.tabs[self.active_tab].ime_preedit = text;
                        }
                    }
                    winit::event::Ime::Commit(text) => {
                        self.tabs[self.active_tab].ime_preedit.clear();
                        if !text.is_empty() {
                            // v0.9 fix: when the Find bar is open, IME
                            // committed text goes into the find query, not
                            // the shell editor / PTY. This lets CJK users
                            // search with Chinese input.
                            if self.find_open {
                                self.find_query.push_str(&text);
                                self.find_last_key = Some(std::time::Instant::now());
                                self.request_redraw();
                            } else if self.palette_open {
                                // v0.9 fix: route IME commit to the command
                                // palette query so CJK input works there too.
                                self.palette_query.push_str(&text);
                                self.palette_selection = 0;
                                self.refresh_palette_results();
                                self.request_redraw();
                            } else if self.panel_open && self.panel_search_focused {
                                // v0.9 fix: route IME commit to the panel
                                // (sidebar) search box when it's focused.
                                self.panel_query.push_str(&text);
                                self.clamp_panel_selection();
                                self.request_redraw();
                            } else {
                                let mode = self.tabs[self.active_tab]
                                    .terminal
                                    .as_ref()
                                    .map(|t| t.effective_input_mode())
                                    .unwrap_or(weft_core::input::InputMode::Passthrough);
                                if mode == weft_core::input::InputMode::Editor {
                                    // Editor takeover: composed text goes into the box.
                                    if let Some(t) = self.tabs[self.active_tab].terminal.as_mut() {
                                        for c in text.chars() {
                                            t.editor_mut().buffer.insert_char(c);
                                        }
                                        // v0.9: IME input clears the editor
                                        // selection (typing replaces it).
                                        t.editor_mut().buffer.clear_selection();
                                    }
                                    self.prompt_dragging = false;
                                    self.request_redraw();
                                } else {
                                    // Passthrough: send committed text to the PTY.
                                    let bracketed = self.tabs[self.active_tab]
                                        .terminal
                                        .as_ref()
                                        .map(|t| t.bracketed_paste)
                                        .unwrap_or(false);
                                    let bytes = encode_paste(&text, bracketed);
                                    if let Some(pty) = &self.tabs[self.active_tab].pty {
                                        let _ = pty.write_sync(&bytes);
                                    }
                                }
                            }
                        }
                    }
                    winit::event::Ime::Disabled => {
                        self.tabs[self.active_tab].ime_preedit.clear();
                    }
                    _ => {}
                }
            }
            WindowEvent::Focused(focused) => {
                // Reset blink timer on focus change
                if focused {
                    self.cursor_blink_on = true;
                    self.cursor_blink_time = std::time::Instant::now();
                }
            }
            _ => {}
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
    info!("Starting Weft v0.2 \"Weave\"");

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
}
