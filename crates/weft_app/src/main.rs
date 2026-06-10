//! Weft v0.2 "Weave" — Metal GPU-rendered terminal emulator
//!
//! Full pipeline: PTY → VT parser → Grid → Metal renderer
//! Features: scrollback, selection, clipboard, CJK, mouse, IME, shell integration

mod glyph;
mod renderer;

use renderer::MetalRenderer;
use weft_core::input::{
    encode_paste, MouseAction, MouseButton, MouseProtocol, InputHandler, KeyCode, Modifiers,
};
use weft_core::pty::{Pty, PtyEvent};
use weft_core::selection::{GridPos, SelectionHandler, SelectionMode};
use weft_core::vt::Terminal;

use crossbeam_channel::{Receiver, Sender};
use tracing::{error, info, warn};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{KeyCode as WinitKeyCode, PhysicalKey};
use winit::window::{Window, WindowAttributes};

// ── Messages between threads ─────────────────────────────────────────

enum AppMsg {
    PtyOutput(Vec<u8>),
    PtyExit(Result<i32, String>),
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
}

impl App {
    fn new() -> Self {
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
        }
    }

    fn spawn_pty(&mut self) {
        let rows = 24;
        let cols = 80;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        let pty = match Pty::spawn(&shell, (rows as u16, cols as u16)) {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to spawn PTY: {e}");
                return;
            }
        };

        // Shell integration hook injection is intentionally NOT done here. Injecting
        // the OSC 133 hook via PTY stdin into an *interactive* shell echoes the hook
        // source (and a buggy `eval '<multi-line>'` form drops zsh into a `quote>`
        // continuation). v0.3 will provide integration via an env var the user's rc
        // file sources instead. See shell::format_injection_command for the WIP form.

        self.terminal = Some(Terminal::new(rows, cols));
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
                    if let Some(terminal) = &mut self.terminal {
                        terminal.process(&data);
                        need_redraw = true;
                    }
                }
                AppMsg::PtyExit(code) => {
                    info!("Shell exited: {:?}", code);
                    return false;
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

    fn handle_key_event(&mut self, key_code: WinitKeyCode, mods: winit::event::Modifiers) {
        let Some(terminal) = &self.terminal else { return };

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

        // Cmd+C: copy selection to clipboard
        if mods.state().super_key() && key == KeyCode::Char('c') && !mods.state().control_key() {
            self.copy_selection();
            return;
        }

        // Cmd+V: paste from clipboard
        if mods.state().super_key() && key == KeyCode::Char('v') && !mods.state().control_key() {
            self.paste_from_clipboard();
            return;
        }

        self.input_handler.app_cursor_keys = terminal.app_cursor_keys;

        let bytes = self.input_handler.encode_key(key, m);
        if !bytes.is_empty() {
            if let Some(pty) = &self.pty {
                if let Err(e) = pty.write_sync(&bytes) {
                    warn!("Failed to write to PTY: {e}");
                }
            }
        }
    }

    /// Convert pixel coordinates to grid (row, col).
    fn pixel_to_grid(&self, x: f64, y: f64) -> GridPos {
        let Some(renderer) = &self.renderer else {
            return GridPos::new(0, 0);
        };
        let scale = self.window.as_ref().map(|w| w.scale_factor()).unwrap_or(1.0);
        let cell_w = renderer.cell_width() as f64 / scale;
        let cell_h = renderer.cell_height() as f64 / scale;
        let col = (x / cell_w) as usize;
        let row = (y / cell_h) as usize;
        GridPos::new(row, col)
    }

    fn handle_mouse_press(&mut self, x: f64, y: f64, button: winit::event::MouseButton) {
        let pos = self.pixel_to_grid(x, y);

        match button {
            winit::event::MouseButton::Left => {
                // Double-click: line selection; Triple-click: block selection
                // For simplicity: Shift+click = block, click = simple
                let mode = if self.mods.state().shift_key() {
                    SelectionMode::Block
                } else {
                    SelectionMode::Simple
                };
                self.selection_handler.start(pos, mode);

                // If mouse protocol is active, send mouse event to PTY
                self.send_mouse_event(MouseButton::Left, MouseAction::Press, pos);
            }
            winit::event::MouseButton::Middle => {
                // Middle click: paste
                self.paste_from_clipboard();
                self.send_mouse_event(MouseButton::Middle, MouseAction::Press, pos);
            }
            winit::event::MouseButton::Right => {
                // Right click: extend selection
                if self.selection_handler.selection.is_none() {
                    self.selection_handler.start(pos, SelectionMode::Simple);
                } else {
                    self.selection_handler.extend(pos);
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
                if v > 0.0 { v.ceil() as usize } else { v.floor().abs() as usize }
            }
            winit::event::MouseScrollDelta::PixelDelta(pos) => {
                let v = pos.y / 40.0; // approx 40px per line
                if v > 0.0 { v.ceil() as usize } else { v.floor().abs() as usize }
            }
        };

        if lines == 0 {
            return;
        }

        let Some(terminal) = &mut self.terminal else { return };

        // Check if mouse protocol is active — forward scroll to PTY
        if terminal.mouse_protocol != MouseProtocol::Off {
            let up = match delta {
                winit::event::MouseScrollDelta::LineDelta(_, v) => v > 0.0,
                winit::event::MouseScrollDelta::PixelDelta(pos) => pos.y > 0.0,
            };
            let pos = self.pixel_to_grid(x, y);
            let mut m = Modifiers::empty();
            if self.mods.state().shift_key() { m |= Modifiers::SHIFT; }
            if self.mods.state().alt_key() { m |= Modifiers::ALT; }
            if self.mods.state().control_key() { m |= Modifiers::CONTROL; }
            if let Some(bytes) = self.input_handler.encode_scroll(up, pos.col, pos.row, m) {
                if let Some(pty) = &self.pty {
                    let _ = pty.write_sync(&bytes);
                }
            }
            return;
        }

        // Otherwise, scroll the terminal viewport
        let grid = &mut terminal.grid_mut();
        let up = match delta {
            winit::event::MouseScrollDelta::LineDelta(_, v) => v > 0.0,
            winit::event::MouseScrollDelta::PixelDelta(pos) => pos.y > 0.0,
        };
        if up {
            grid.scroll_up_history(lines);
        } else {
            grid.scroll_down_history(lines);
        }
        self.request_redraw();
    }

    /// Send a mouse event to the PTY if mouse protocol is active.
    fn send_mouse_event(&self, button: MouseButton, action: MouseAction, pos: GridPos) {
        let Some(terminal) = &self.terminal else { return };
        if terminal.mouse_protocol == MouseProtocol::Off {
            return;
        }
        let mut m = Modifiers::empty();
        if self.mods.state().shift_key() { m |= Modifiers::SHIFT; }
        if self.mods.state().alt_key() { m |= Modifiers::ALT; }
        if self.mods.state().control_key() { m |= Modifiers::CONTROL; }
        if let Some(bytes) = self.input_handler.encode_mouse(button, action, pos.col, pos.row, m) {
            if let Some(pty) = &self.pty {
                let _ = pty.write_sync(&bytes);
            }
        }
    }

    /// Copy selection to system clipboard.
    fn copy_selection(&self) {
        let Some(terminal) = &self.terminal else { return };
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
            let bracketed = self.terminal.as_ref().map(|t| t.bracketed_paste).unwrap_or(false);
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

        let cls = objc2::ffi::objc_getClass("NSPasteboard\0".as_ptr() as *const i8);
        if cls.is_null() {
            return;
        }
        let pasteboard: *mut AnyObject = msg_send![cls as *const AnyObject, generalPasteboard];
        if pasteboard.is_null() {
            return;
        }

        let ns_string_cls = objc2::ffi::objc_getClass("NSString\0".as_ptr() as *const i8);
        if ns_string_cls.is_null() {
            return;
        }
        let ns_string: *mut AnyObject = msg_send![ns_string_cls as *const AnyObject, alloc];
        let c_str = std::ffi::CString::new(text).unwrap_or_default();
        let ns_string: *mut AnyObject = msg_send![ns_string,
            initWithBytes: c_str.as_ptr()
            length: text.len()
            encoding: 1u64 // NSUTF8StringEncoding
        ];

        let _: () = msg_send![pasteboard, clearContents];
        let _: () = msg_send![pasteboard, setString: ns_string];
    }
}

/// Paste text from macOS system clipboard using NSPasteboard.
fn clipboard_paste() -> Option<String> {
    unsafe {
        use objc2::msg_send;
        use objc2::runtime::AnyObject;

        let cls = objc2::ffi::objc_getClass("NSPasteboard\0".as_ptr() as *const i8);
        if cls.is_null() {
            return None;
        }
        let pasteboard: *mut AnyObject = msg_send![cls as *const AnyObject, generalPasteboard];
        if pasteboard.is_null() {
            return None;
        }

        let ns_string: *mut AnyObject = msg_send![pasteboard, string];
        if ns_string.is_null() {
            return None;
        }

        let len: usize = msg_send![ns_string, lengthOfBytesUsingEncoding: 1u64];
        if len == 0 {
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

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let attrs = WindowAttributes::default()
            .with_title("Weft")
            .with_inner_size(winit::dpi::LogicalSize::new(800.0, 600.0));

        let window = event_loop.create_window(attrs).unwrap();
        let renderer = MetalRenderer::new(&window);

        self.spawn_pty();
        self.window = Some(window);
        self.renderer = Some(renderer);

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
                if let (Some(renderer), Some(window), Some(terminal)) =
                    (&mut self.renderer, &self.window, &mut self.terminal)
                {
                    // cell_width()/cell_height() are in physical pixels (rasterized at the
                    // window scale); physical_size is also physical pixels. Divide in
                    // consistent units — mixing physical size with a logical cell size
                    // (cell / scale) double-counts the scale factor and over-sizes the grid 2×.
                    let new_cols =
                        (physical_size.width as f64 / renderer.cell_width() as f64) as usize;
                    let new_rows =
                        (physical_size.height as f64 / renderer.cell_height() as f64) as usize;

                    if new_cols > 0 && new_rows > 0 {
                        terminal.resize(new_rows, new_cols);
                        if let Some(pty) = &self.pty {
                            let _ = pty.resize(new_rows as u16, new_cols as u16);
                        }
                    }

                    renderer.resize(window, physical_size);
                    renderer.draw(terminal, &self.selection_handler, self.cursor_blink_on);
                }
            }
            WindowEvent::RedrawRequested => {
                self.pump_pty();
                self.process_messages();
                self.update_cursor_blink();

                if let (Some(renderer), Some(terminal)) =
                    (&self.renderer, &self.terminal)
                {
                    renderer.draw(terminal, &self.selection_handler, self.cursor_blink_on);
                }

                // Keep the render loop alive: pump_pty/process_messages above only
                // request a redraw when PTY data is sitting in the channel *at poll
                // time, but the shell echoes keystrokes asynchronously, after the
                // redraw triggered by the key. Without this, the screen freezes until
                // the next mouse/keyboard event ("must click to see output").
                // TODO: replace this vsync busy-loop with an EventLoopProxy wake from
                // the PTY reader thread for power efficiency.
                self.request_redraw();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == winit::event::ElementState::Pressed && !event.repeat {
                    if let PhysicalKey::Code(key_code) = event.physical_key {
                        self.handle_key_event(key_code, self.mods);
                    }
                }
            }
            WindowEvent::ModifiersChanged(new_mods) => {
                self.mods = new_mods;
            }
            WindowEvent::MouseInput { state, button, .. } => {
                match state {
                    winit::event::ElementState::Pressed => {
                        self.handle_mouse_press(self.last_mouse_x, self.last_mouse_y, button);
                    }
                    winit::event::ElementState::Released => {
                        self.handle_mouse_release(self.last_mouse_x, self.last_mouse_y, button);
                    }
                }
            }
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
                        // Send committed text to PTY
                        if !text.is_empty() {
                            let bracketed = self.terminal.as_ref().map(|t| t.bracketed_paste).unwrap_or(false);
                            let bytes = encode_paste(&text, bracketed);
                            if let Some(pty) = &self.pty {
                                let _ = pty.write_sync(&bytes);
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

fn main() {
    tracing_subscriber::fmt::init();
    info!("Starting Weft v0.2 \"Weave\"");

    // Create a tokio runtime for PTY async operations.
    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
    let _guard = rt.enter();

    let event_loop = EventLoop::new().unwrap();
    let mut app = App::new();
    event_loop.run_app(&mut app).unwrap();
}
