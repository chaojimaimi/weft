//! Weft v0.2 "Weave" — Metal GPU-rendered terminal emulator
//!
//! Full pipeline: PTY → VT parser → Grid → Metal renderer
//! Features: scrollback, selection, clipboard, CJK, mouse, IME, shell integration

mod glyph;
mod overlay;
mod renderer;

use renderer::{block_matches_query, MetalRenderer, PanelDrawParams, PromptDrawParams};
use weft_core::blocks::{BlockId, ShellPhase};
use weft_core::complete::{complete, CompleteCtx, CompletePosition};
use weft_core::config::{Action, Config, KeyBindings};
use weft_core::input::{
    encode_paste, InputHandler, KeyCode, Modifiers, MouseAction, MouseButton, MouseProtocol,
};
use weft_core::persistence::BlockStore;
use weft_core::pty::{Pty, PtyEvent};
use weft_core::selection::{GridPos, SelectionHandler, SelectionMode};
use weft_core::shell::Integration;
use weft_core::vt::Terminal;

use crossbeam_channel::{Receiver, Sender};
use tracing::{error, info, warn};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode as WinitKeyCode, PhysicalKey};
use winit::window::{Window, WindowAttributes};

// ── Messages between threads ─────────────────────────────────────────

enum AppMsg {
    PtyOutput(Vec<u8>),
    PtyExit(Result<i32, String>),
}

/// Cross-thread wake-up for the winit event loop.
///
/// Sent by the PTY reader thread (new output) and the cursor-blink timer so
/// the loop redraws only when there is actual work — instead of busy-looping
/// at vsync and pinning a CPU core.
#[derive(Debug)]
enum AppEvent {
    Wake,
    /// Config file changed on disk — reload and re-apply live.
    ConfigReload,
}

// ── Application ──────────────────────────────────────────────────────

struct App {
    window: Option<Window>,
    renderer: Option<MetalRenderer>,
    terminal: Option<Terminal>,
    input_handler: InputHandler,
    selection_handler: SelectionHandler,
    msg_rx: Receiver<AppMsg>,
    msg_tx: Sender<AppMsg>,
    pty: Option<Pty>,
    /// Current keyboard modifier state, updated by ModifiersChanged events.
    mods: winit::event::Modifiers,
    /// Cursor blink state.
    cursor_blink_on: bool,
    /// Last cursor blink toggle time.
    cursor_blink_time: std::time::Instant,
    /// IME preedit string (for CJK input).
    ime_preedit: String,
    /// Last known mouse position for click/scroll handling.
    last_mouse_x: f64,
    last_mouse_y: f64,
    /// Debounced PTY resize: (rows, cols) waiting to be sent to the PTY
    /// after the resize animation cascade settles. The grid is resized
    /// immediately on each Resized event for smooth animation; only the
    /// PTY SIGWINCH is debounced to prevent the shell from fighting cursor
    /// position during rapid cascades.
    pending_pty_resize: Option<(usize, usize)>,
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
    /// Block-view scroll offset (rows from the bottom). Independent of
    /// `grid.scroll_offset` (which is for the grid/alt-screen path and
    /// clamped to grid scrollback — the wrong proxy for block content).
    /// This is what the mouse wheel / PgUp modifies in block view.
    block_scroll_offset: usize,
}

impl App {
    fn new(proxy: EventLoopProxy<AppEvent>) -> Self {
        let config = Config::load();
        info!(
            theme = %config.theme.name,
            font = %config.font.family,
            size = config.font.size,
            "config loaded"
        );
        let keybindings = config.keybindings();
        let (msg_tx, msg_rx) = crossbeam_channel::bounded(1024);
        Self {
            window: None,
            renderer: None,
            terminal: None,
            input_handler: InputHandler::new(),
            selection_handler: SelectionHandler::new(),
            msg_rx,
            msg_tx,
            mods: winit::event::Modifiers::default(),
            pty: None,
            cursor_blink_on: true,
            cursor_blink_time: std::time::Instant::now(),
            ime_preedit: String::new(),
            last_mouse_x: 0.0,
            last_mouse_y: 0.0,
            pending_pty_resize: None,
            last_resize_instant: std::time::Instant::now(),
            path_bins: scan_path_bins(),
            proxy,
            config,
            keybindings,
            block_store: None,
            panel_open: false,
            panel_query: String::new(),
            panel_selection: 0,
            panel_expanded: None,
            block_scroll_offset: 0,
        }
    }

    fn spawn_pty(&mut self, rows: usize, cols: usize) {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());

        // Shell integration: redirect the shell's rc lookup so it sources our
        // OSC 133 hooks itself (no PTY-stdin injection → no echo). Returns the
        // env overrides to set on the child; falls back to empty on any error.
        let env = shell_integration_env(&shell);
        let env_refs: Vec<(&str, &str)> =
            env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

        let wake_proxy = self.proxy.clone();
        let pty = match Pty::spawn_with_args(
            &shell,
            &[],
            (rows as u16, cols as u16),
            &env_refs,
            move || {
                let _ = wake_proxy.send_event(AppEvent::Wake);
            },
        ) {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to spawn PTY: {e}");
                return;
            }
        };

        self.terminal = Some(Terminal::with_scrollback(
            rows,
            cols,
            self.config.scrollback.lines,
        ));
        info!(rows, cols, "initial terminal size");
        self.pty = Some(pty);
    }

    /// Non-blocking drain of PTY events into channel.
    fn pump_pty(&mut self) {
        let Some(pty) = &mut self.pty else { return };
        let tx = self.msg_tx.clone();
        loop {
            match pty.try_recv() {
                Ok(PtyEvent::Output(data)) => {
                    if tx.send(AppMsg::PtyOutput(data)).is_err() {
                        break;
                    }
                }
                Ok(PtyEvent::Exit(code)) => {
                    let _ = tx.send(AppMsg::PtyExit(code));
                    break;
                }
                Err(_) => break,
            }
        }
    }

    fn process_messages(&mut self) -> bool {
        let mut need_redraw = false;

        while let Ok(msg) = self.msg_rx.try_recv() {
            match msg {
                AppMsg::PtyOutput(data) => {
                    tracing::trace!("PTY output: {} bytes", data.len());
                    let mut response = Vec::new();
                    if let Some(terminal) = &mut self.terminal {
                        terminal.process(&data);
                        response = terminal.take_response();
                        need_redraw = true;
                    }
                    // Write any terminal-query responses (DA/DSR/size reports)
                    // back to the PTY so TUIs get their capability answers.
                    if !response.is_empty() {
                        if let Some(pty) = &self.pty {
                            if let Err(e) = pty.write_sync(&response) {
                                warn!(error = %e, "failed to write terminal response");
                            }
                        }
                    }
                }
                AppMsg::PtyExit(code) => {
                    info!("Shell exited: {:?}", code);
                    return false;
                }
            }
        }

        // Persist any command blocks finished during this batch (synchronous:
        // local SQLite inserts are fast; volume is one block per command).
        if let Some(store) = &self.block_store {
            if let Some(terminal) = &mut self.terminal {
                for block in terminal.block_tracker_mut().drain_unpersisted() {
                    if let Err(e) = store.insert(&block) {
                        warn!(error = %e, "failed to persist block");
                    }
                }
            }
        }

        if need_redraw {
            self.request_redraw();
        }

        true
    }

    fn request_redraw(&self) {
        if let (Some(window), Some(_renderer), Some(_terminal)) =
            (&self.window, &self.renderer, &self.terminal)
        {
            window.request_redraw();
        }
    }

    fn handle_key_event(
        &mut self,
        key_code: WinitKeyCode,
        mods: winit::event::Modifiers,
        text: Option<&str>,
    ) {
        if self.terminal.is_none() {
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
        if let Some(action) = self.keybindings.lookup(key, m) {
            if self.execute_action(action) {
                return;
            }
        }

        // When the history panel is open, navigation/typing/search go to it
        // (not the shell). Modifier chords (cmd/ctrl/alt) still fall through so
        // keybindings like cmd+shift+b (toggle) keep working.
        if self.panel_open && self.handle_panel_key(key, m) {
            return;
        }

        // Editor takeover: at the prompt with integration ready, keys drive the
        // input-box editor instead of being forwarded to the PTY. Enter submits
        // (writes the command); Shift+Enter grows the box. Drops back to
        // passthrough automatically in alt-screen / command-running / SSH.
        let input_mode = self
            .terminal
            .as_ref()
            .map(|t| t.effective_input_mode())
            .unwrap_or(weft_core::input::InputMode::Passthrough);
        if input_mode == weft_core::input::InputMode::Editor {
            let prev_lines = self
                .terminal
                .as_ref()
                .map(|t| t.editor().line_count())
                .unwrap_or(1);
            let consumed = self.handle_editor_key(key, m, text);
            let new_lines = self
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

        self.input_handler.app_cursor_keys = self
            .terminal
            .as_ref()
            .map(|t| t.app_cursor_keys)
            .unwrap_or(false);

        let bytes = self.input_handler.encode_key(key, m);
        // Diagnostic (set RUST_LOG=weft_app=debug to see): the exact bytes we
        // send for each key, including whether DECCKM/app-cursor mode is on.
        tracing::debug!(
            ?key,
            ?m,
            app_cursor_keys = self.input_handler.app_cursor_keys,
            ?bytes,
            "key → pty"
        );
        if !bytes.is_empty() {
            if let Some(pty) = &self.pty {
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
                }
                self.request_redraw();
                true
            }
        }
    }

    /// Handle a key while the history panel is open. Returns true if consumed
    /// (search typing / arrow nav / expand / close). Modifier chords fall
    /// through (returns false) so keybindings still work.
    fn handle_panel_key(&mut self, key: KeyCode, mods: Modifiers) -> bool {
        // Let cmd/ctrl/alt chords pass through to keybindings / PTY.
        if mods.intersects(Modifiers::SUPER | Modifiers::CONTROL | Modifiers::ALT) {
            return false;
        }
        match key {
            KeyCode::Escape => {
                self.panel_open = false;
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
                // Toggle inline expansion of the selected block's output.
                let selected_id = self.panel_selected_block_id();
                if let Some(id) = selected_id {
                    self.panel_expanded = if self.panel_expanded == Some(id) {
                        None
                    } else {
                        Some(id)
                    };
                    self.request_redraw();
                }
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

    /// Handle a key while in Editor input mode (the input box owns the prompt).
    /// Returns true if consumed. Ctrl chords that aren't editor ops fall through
    /// (returns false) so Ctrl+C etc. still reach the PTY.
    fn handle_editor_key(&mut self, key: KeyCode, mods: Modifiers, text: Option<&str>) -> bool {
        use weft_core::input::{KeyCode::*, Modifiers};
        let shift = mods.contains(Modifiers::SHIFT);

        // Ctrl editor ops (Ctrl+C / other Ctrl chords fall through to the PTY).
        if mods.contains(Modifiers::CONTROL) && !mods.contains(Modifiers::ALT) {
            let consumed = if let Some(t) = self.terminal.as_mut() {
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
        let searching = self
            .terminal
            .as_ref()
            .map(|t| t.editor().is_searching())
            .unwrap_or(false);
        if searching {
            if let Some(t) = self.terminal.as_mut() {
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
        let completing = self
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
                } else if let Some(t) = self.terminal.as_mut() {
                    t.editor_mut().buffer.split_newline();
                }
                true
            }
            Char(c) => {
                if let Some(t) = self.terminal.as_mut() {
                    t.editor_mut()
                        .buffer
                        .insert_char(resolve_text_char(text, c, shift));
                }
                true
            }
            Backspace => {
                if let Some(t) = self.terminal.as_mut() {
                    t.editor_mut().buffer.delete_backspace();
                }
                true
            }
            Delete => {
                if let Some(t) = self.terminal.as_mut() {
                    t.editor_mut().buffer.delete_forward();
                }
                true
            }
            Left => {
                if let Some(t) = self.terminal.as_mut() {
                    t.editor_mut().buffer.move_left();
                }
                true
            }
            Right => {
                if let Some(t) = self.terminal.as_mut() {
                    t.editor_mut().buffer.move_right();
                }
                true
            }
            Home => {
                if let Some(t) = self.terminal.as_mut() {
                    t.editor_mut().buffer.move_line_home();
                }
                true
            }
            End => {
                if let Some(t) = self.terminal.as_mut() {
                    t.editor_mut().buffer.move_line_end();
                }
                true
            }
            Up => {
                if let Some(t) = self.terminal.as_mut() {
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
                if let Some(t) = self.terminal.as_mut() {
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
        let (line_owned, col, cwd, history) = match self.terminal.as_ref() {
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
        let Some(t) = self.terminal.as_mut() else {
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
        if let Some(t) = self.terminal.as_mut() {
            t.editor_mut().completion_next();
        }
    }

    fn editor_completion_prev(&mut self) {
        if let Some(t) = self.terminal.as_mut() {
            t.editor_mut().completion_prev();
        }
    }

    fn editor_completion_accept(&mut self) {
        if let Some(t) = self.terminal.as_mut() {
            t.editor_mut().completion_accept();
        }
    }

    fn editor_completion_cancel(&mut self) {
        if let Some(t) = self.terminal.as_mut() {
            t.editor_mut().completion_cancel();
        }
    }

    /// Submit the editor's command: write PTY bytes (and any terminal query
    /// response) and locally block the editor through the Enter→preexec window.
    fn editor_submit(&mut self) {
        let bytes = self
            .terminal
            .as_mut()
            .map(|t| t.submit_command())
            .unwrap_or_default();
        if !bytes.is_empty() {
            if let Some(pty) = &self.pty {
                let _ = pty.write_sync(&bytes);
            }
        }
        let resp = self
            .terminal
            .as_mut()
            .map(|t| t.take_response())
            .unwrap_or_default();
        if !resp.is_empty() {
            if let Some(pty) = &self.pty {
                let _ = pty.write_sync(&resp);
            }
        }
    }

    /// Count of blocks visible in the panel (newest-first, query-filtered).
    fn panel_visible_count(&self) -> usize {
        let Some(terminal) = &self.terminal else {
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
        let terminal = self.terminal.as_ref()?;
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

    /// Local scrollback navigation (page up/down, top, bottom).
    fn scroll_action(&mut self, action: Action) {
        let Some(terminal) = &mut self.terminal else {
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
                    self.block_scroll_offset = self
                        .block_scroll_offset
                        .saturating_add(rows)
                        .min(max_scroll);
                }
                Action::ScrollPageDown => {
                    self.block_scroll_offset = self.block_scroll_offset.saturating_sub(rows);
                }
                Action::ScrollToTop => {
                    self.block_scroll_offset = max_scroll;
                }
                Action::ScrollToBottom => self.block_scroll_offset = 0,
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

    /// Apply a (possibly new) config: theme, font, keybindings, scrollback.
    /// Theme/font/scrollback changes take effect immediately; window size/title
    /// apply on the next launch.
    fn apply_config(&mut self, config: Config) {
        // Theme — renderer defaults + terminal palette reseed (recolors all
        // Palette-indexed cells on the next draw).
        let theme = config.theme();
        if let Some(r) = &mut self.renderer {
            r.set_theme(theme.clone());
        }
        if let Some(t) = &mut self.terminal {
            t.set_palette(theme.palette);
        }

        // Font — rebuild the atlas (cell dimensions may change → recompute).
        if self.config.font.family != config.font.family
            || self.config.font.size != config.font.size
            || self.config.font.line_height != config.font.line_height
        {
            if let Some(r) = &mut self.renderer {
                r.rebuild_atlas(config.font.clone());
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
        if let Some(t) = &mut self.terminal {
            let cols = t.grid().num_cols;
            t.grid_mut()
                .scrollback
                .set_max_lines(config.scrollback.lines, cols);
        }

        self.config = config;
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
        let usable_w = size.width as f64 - 2.0 * renderer.padding_x() as f64;
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
        if let Some(terminal) = &mut self.terminal {
            terminal.resize(new_rows, new_cols);
            info!(rows = new_rows, cols = new_cols, "terminal resized");
        }
        self.pending_pty_resize = Some((new_rows, new_cols));
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
        // Clamp to valid grid bounds. A click past the right/bottom edge (e.g.
        // a drag-to-select ending at the window margin) would otherwise yield
        // col == num_cols / row == num_rows and panic text_from_grid on copy.
        let (num_rows, num_cols) = self
            .terminal
            .as_ref()
            .map(|t| (t.grid().num_rows, t.grid().num_cols))
            .unwrap_or((1, 1));
        let col = (((x - renderer.padding_x() as f64) / cell_w).max(0.0) as usize)
            .min(num_cols.saturating_sub(1));
        let row = (((y - renderer.padding_y() as f64) / cell_h).max(0.0) as usize)
            .min(num_rows.saturating_sub(1));
        GridPos::new(row, col)
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
        self.terminal
            .as_ref()
            .map(|t| t.mouse_protocol != MouseProtocol::Off)
            .unwrap_or(false)
    }

    /// Which foldable block (if any) owns the physical-pixel y in the last
    /// rendered block view. `None` outside the block view or off every block.
    fn block_at(&self, y: f32) -> Option<BlockId> {
        let regions = self.renderer.as_ref()?.block_hit_regions.as_slice();
        regions
            .iter()
            .find(|(_, top, bottom)| y >= *top && y <= *bottom)
            .map(|(id, _, _)| *id)
    }

    fn handle_mouse_press(&mut self, x: f64, y: f64, button: winit::event::MouseButton) {
        // Editor-mode block view: clicking a foldable block's command line
        // toggles its collapse (instead of starting a grid selection).
        if button == winit::event::MouseButton::Left {
            if let Some(id) = self.block_at(y as f32) {
                if let Some(t) = self.terminal.as_mut() {
                    t.block_tracker_mut().toggle_collapse(id);
                    self.request_redraw();
                }
                return;
            }
        }

        let pos = self.pixel_to_grid(x, y);
        let selecting = !self.mouse_reporting_active();

        match button {
            winit::event::MouseButton::Left => {
                if selecting {
                    // Double-click: line selection; Triple-click: block selection
                    // For simplicity: Shift+click = block, click = simple
                    let mode = if self.mods.state().shift_key() {
                        SelectionMode::Block
                    } else {
                        SelectionMode::Simple
                    };
                    self.selection_handler.start(pos, mode);
                }

                // If mouse protocol is active, send mouse event to PTY
                self.send_mouse_event(MouseButton::Left, MouseAction::Press, pos);
            }
            winit::event::MouseButton::Middle => {
                // Middle click: paste
                self.paste_from_clipboard();
                self.send_mouse_event(MouseButton::Middle, MouseAction::Press, pos);
            }
            winit::event::MouseButton::Right => {
                if selecting {
                    // Right click: extend selection
                    if self.selection_handler.selection.is_none() {
                        self.selection_handler.start(pos, SelectionMode::Simple);
                    } else {
                        self.selection_handler.extend(pos);
                    }
                }
                self.send_mouse_event(MouseButton::Right, MouseAction::Press, pos);
            }
            _ => {}
        }

        self.request_redraw();
    }

    /// Handle mouse release.
    fn handle_mouse_release(&mut self, x: f64, y: f64, button: winit::event::MouseButton) {
        let pos = self.pixel_to_grid(x, y);
        self.selection_handler.end();

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
        let pos = self.pixel_to_grid(x, y);

        if self.selection_handler.selecting {
            self.selection_handler.extend(pos);
            self.request_redraw();
        }

        self.send_mouse_event(MouseButton::Left, MouseAction::Move, pos);
    }

    /// Handle scroll wheel.
    fn handle_scroll(&mut self, delta: winit::event::MouseScrollDelta, x: f64, y: f64) {
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

        let Some(terminal) = &mut self.terminal else {
            return;
        };

        // Check if mouse protocol is active — forward scroll to PTY
        if terminal.mouse_protocol != MouseProtocol::Off {
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
            if let Some(bytes) = self.input_handler.encode_scroll(up, pos.col, pos.row, m) {
                if let Some(pty) = &self.pty {
                    let _ = pty.write_sync(&bytes);
                }
            }
            return;
        }

        // Alt-screen apps (less, vim, man, etc.) don't use mouse protocol but
        // still benefit from wheel scroll: translate to Up/Down arrow key
        // sequences so the pager scrolls its content natively.
        if terminal.is_alt_screen_active() {
            let up = match delta {
                winit::event::MouseScrollDelta::LineDelta(_, v) => v > 0.0,
                winit::event::MouseScrollDelta::PixelDelta(pos) => pos.y > 0.0,
            };
            let key = if up { KeyCode::Up } else { KeyCode::Down };
            let mut m = Modifiers::empty();
            if self.mods.state().shift_key() {
                m |= Modifiers::SHIFT;
            }
            let single = self.input_handler.encode_key(key, m);
            if !single.is_empty() {
                let mut batch = Vec::with_capacity(single.len() * lines);
                for _ in 0..lines {
                    batch.extend_from_slice(&single);
                }
                if let Some(pty) = &self.pty {
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
        if terminal.show_block_view() {
            // Cap scroll speed at 1 row per wheel notch in the block view.
            // macOS trackpad inertia can send 3-4 lines per tick, which skips
            // past content too fast for comfortable reading.
            let scroll_lines = lines.min(1);
            if up {
                self.block_scroll_offset = self.block_scroll_offset.saturating_add(scroll_lines);
                // Clamp: don't scroll past the oldest content. The visible
                // viewport already holds `visible_rows` rows; total content
                // is `total_rows`. Max scroll = total - visible.
                let cols = terminal.grid().num_cols;
                let (total, _) = block_content_metrics(terminal, cols);
                // Compute visible rows from the renderer's actual geometry
                // (pitch = ch * 1.1, region = viewport minus prompt box).
                // The old code used grid().num_rows which overcounts because
                // the block view uses a 10% taller line pitch and doesn't
                // occupy the full viewport (prompt box eats space).
                let prompt_lines = if let Some(t) = &self.terminal {
                    t.editor().buffer.lines.len()
                } else {
                    1
                };
                let visible = self
                    .renderer
                    .as_ref()
                    .map(|r| r.block_visible_rows(prompt_lines))
                    .unwrap_or(1);
                let max_scroll = total.saturating_sub(visible);
                self.block_scroll_offset = self.block_scroll_offset.min(max_scroll);
            } else {
                self.block_scroll_offset = self.block_scroll_offset.saturating_sub(scroll_lines);
            }
        } else {
            let grid = &mut terminal.grid_mut();
            if up {
                grid.scroll_up_history(lines);
            } else {
                grid.scroll_down_history(lines);
            }
        }
        self.request_redraw();
    }

    /// Send a mouse event to the PTY if mouse protocol is active.
    fn send_mouse_event(&self, button: MouseButton, action: MouseAction, pos: GridPos) {
        let Some(terminal) = &self.terminal else {
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
        if let Some(bytes) = self
            .input_handler
            .encode_mouse(button, action, pos.col, pos.row, m)
        {
            if let Some(pty) = &self.pty {
                let _ = pty.write_sync(&bytes);
            }
        }
    }

    /// Copy selection to system clipboard.
    fn copy_selection(&self) {
        let Some(terminal) = &self.terminal else {
            return;
        };
        if let Some(text) = self.selection_handler.selected_text(terminal.grid()) {
            if !text.is_empty() {
                clipboard_copy(&text);
            }
        }
    }

    /// Paste from system clipboard.
    fn paste_from_clipboard(&self) {
        if let Some(text) = clipboard_paste() {
            if text.is_empty() {
                return;
            }
            let bracketed = self
                .terminal
                .as_ref()
                .map(|t| t.bracketed_paste)
                .unwrap_or(false);
            let bytes = encode_paste(&text, bracketed);
            if let Some(pty) = &self.pty {
                if let Err(e) = pty.write_sync(&bytes) {
                    warn!("Failed to paste to PTY: {e}");
                }
            }
        }
    }

    /// Update cursor blink state.
    fn update_cursor_blink(&mut self) {
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.cursor_blink_time);
        if elapsed >= std::time::Duration::from_millis(530) {
            self.cursor_blink_on = !self.cursor_blink_on;
            self.cursor_blink_time = now;
        }
    }
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
            self.config.theme(),
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

        // Open the command-block DB (best-effort) and hydrate the tracker with
        // recent history so the panel has content on first show.
        self.block_store = weft_cache_dir().and_then(|cache| {
            let path = cache.join("blocks.db");
            match BlockStore::open(&path) {
                Ok(store) => {
                    if let Some(terminal) = &mut self.terminal {
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
                    let usable_w = physical_size.width as f64 - 2.0 * pad_x;
                    let usable_h = (physical_size.height as f64 - 2.0 * pad_y).max(0.0);
                    let new_cols = (usable_w / renderer.cell_width() as f64).max(0.0) as usize;
                    let new_rows = (usable_h / renderer.cell_height() as f64).max(0.0) as usize;

                    if new_cols > 0 && new_rows > 0 {
                        // Update renderer viewport immediately
                        renderer.resize(window, physical_size);

                        // Resize grid immediately for smooth animation.
                        // The rewrap is fast (<1ms) so doing it on every
                        // intermediate event is fine.
                        if let Some(terminal) = &mut self.terminal {
                            terminal.resize(new_rows, new_cols);
                            info!(rows = new_rows, cols = new_cols, "terminal resized (event)");
                        }

                        // Debounce only the PTY SIGWINCH to prevent the
                        // shell from fighting cursor position during rapid
                        // resize cascades (~40 events during maximize).
                        self.pending_pty_resize = Some((new_rows, new_cols));
                        self.last_resize_instant = std::time::Instant::now();
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                self.pump_pty();
                let had_output = self.process_messages();
                self.update_cursor_blink();

                // During command execution, new output streams in — snap the
                // block view to the bottom so the user sees fresh content.
                // (At prompt / idle, preserve the user's scroll position.)
                if had_output {
                    if let Some(t) = &self.terminal {
                        if t.block_tracker().phase() == ShellPhase::CommandExecuting {
                            self.block_scroll_offset = 0;
                        }
                    }
                }

                // If the grid row count drifted from what the terminal holds
                // (font/padding/window-size change, or the one-time convergence
                // from the spawn size to the padded size) — recompute. Mode
                // transitions no longer cause drift: the grid is always
                // full-window and the input box is a non-resizing overlay.
                let desired_rows = self.grid_dims().0;
                let current_rows = self
                    .terminal
                    .as_ref()
                    .map(|t| t.grid().num_rows)
                    .unwrap_or(0);
                if desired_rows != 0 && desired_rows != current_rows {
                    self.recompute_layout();
                }

                // Flush debounced PTY resize after the cascade settles
                // (100ms of no new resize events). The grid was already
                // resized immediately in the Resized handler.
                if let Some((rows, cols)) = self.pending_pty_resize {
                    if self.last_resize_instant.elapsed() > std::time::Duration::from_millis(100) {
                        if let Some(pty) = &self.pty {
                            if let Err(e) = pty.resize(rows as u16, cols as u16) {
                                warn!("PTY resize failed: {e}");
                            }
                        }
                        self.pending_pty_resize = None;
                    }
                }

                if let (Some(renderer), Some(terminal)) = (&mut self.renderer, &self.terminal) {
                    let panel = if self.panel_open {
                        let width_px =
                            (renderer.viewport_width() * 0.38).min(460.0 * renderer.scale() as f32);
                        Some(PanelDrawParams {
                            blocks: terminal.block_tracker().blocks(),
                            width_px,
                            query: &self.panel_query,
                            selection: self.panel_selection,
                            expanded_id: self.panel_expanded,
                        })
                    } else {
                        None
                    };
                    // Editor input box: built only in Editor mode (hidden in
                    // passthrough). cwd/lines/search borrow `terminal`; the
                    // preedit borrows `self.ime_preedit` (a disjoint field).
                    let prompt =
                        if terminal.effective_input_mode() == weft_core::input::InputMode::Editor {
                            let search = terminal.editor().search_view();
                            Some(PromptDrawParams {
                                cwd: terminal.cwd(),
                                lines: &terminal.editor().buffer.lines,
                                cursor: terminal.editor().buffer.cursor,
                                preedit: if self.ime_preedit.is_empty() {
                                    None
                                } else {
                                    Some(self.ime_preedit.as_str())
                                },
                                search,
                            })
                        } else {
                            None
                        };
                    // Completion popup: passed separately from prompt (overlay
                    // refactor commit 2 — split out from PromptDrawParams).
                    let completions =
                        if terminal.effective_input_mode() == weft_core::input::InputMode::Editor {
                            let c = terminal.editor().completion_view();
                            // Only show if search is not active (completion and
                            // Ctrl+R search are mutually exclusive).
                            if terminal.editor().search_view().is_some() {
                                None
                            } else {
                                c
                            }
                        } else {
                            None
                        };
                    // Hide the grid (shell) cursor while the editor owns input:
                    // a blinking caret at the shell prompt misreads as "type
                    // here". The input-box cursor is drawn by build_prompt_vertices.
                    renderer.draw(
                        terminal,
                        &self.selection_handler,
                        self.cursor_blink_on,
                        panel.as_ref(),
                        prompt.as_ref(),
                        completions,
                        self.block_scroll_offset,
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
            WindowEvent::MouseWheel { delta, .. } => {
                self.handle_scroll(delta, self.last_mouse_x, self.last_mouse_y);
            }
            WindowEvent::Ime(ime_event) => {
                match ime_event {
                    winit::event::Ime::Preedit(text, _cursor) => {
                        self.ime_preedit = text;
                    }
                    winit::event::Ime::Commit(text) => {
                        self.ime_preedit.clear();
                        if !text.is_empty() {
                            let mode = self
                                .terminal
                                .as_ref()
                                .map(|t| t.effective_input_mode())
                                .unwrap_or(weft_core::input::InputMode::Passthrough);
                            if mode == weft_core::input::InputMode::Editor {
                                // Editor takeover: composed text goes into the box.
                                if let Some(t) = self.terminal.as_mut() {
                                    for c in text.chars() {
                                        t.editor_mut().buffer.insert_char(c);
                                    }
                                }
                                self.request_redraw();
                            } else {
                                // Passthrough: send committed text to the PTY.
                                let bracketed = self
                                    .terminal
                                    .as_ref()
                                    .map(|t| t.bracketed_paste)
                                    .unwrap_or(false);
                                let bytes = encode_paste(&text, bracketed);
                                if let Some(pty) = &self.pty {
                                    let _ = pty.write_sync(&bytes);
                                }
                            }
                        }
                    }
                    winit::event::Ime::Disabled => {
                        self.ime_preedit.clear();
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
fn shell_integration_env(shell: &str) -> Vec<(String, String)> {
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
