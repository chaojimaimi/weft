//! Weft v0.1 — Metal GPU-rendered terminal emulator
//!
//! Full pipeline: PTY → VT parser → Grid → Metal renderer

mod glyph;
mod renderer;

use renderer::MetalRenderer;
use weft_core::input::{InputHandler, KeyCode, Modifiers};
use weft_core::pty::{Pty, PtyEvent};
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
    msg_rx: Receiver<AppMsg>,
    msg_tx: Sender<AppMsg>,
    pty: Option<Pty>,
    /// Current keyboard modifier state, updated by ModifiersChanged events.
    mods: winit::event::Modifiers,
}

impl App {
    fn new() -> Self {
        let (msg_tx, msg_rx) = crossbeam_channel::bounded(1024);
        Self {
            window: None,
            renderer: None,
            terminal: None,
            input_handler: InputHandler::new(),
            msg_rx,
            msg_tx,
            mods: winit::event::Modifiers::default(),
            pty: None,
        }
    }

    fn spawn_pty(&mut self) {
        let rows = 24;
        let cols = 80;

        let pty = match Pty::spawn(
            &std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into()),
            (rows as u16, cols as u16),
        ) {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to spawn PTY: {e}");
                return;
            }
        };

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
            if let (Some(window), Some(renderer), Some(terminal)) =
                (&self.window, &mut self.renderer, &self.terminal)
            {
                renderer.draw(terminal);
                window.request_redraw();
            }
        }

        true
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

        if let (Some(window), Some(renderer), Some(terminal)) =
            (&self.window, &self.renderer, &self.terminal)
        {
            renderer.draw(terminal);
            window.request_redraw();
        }
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
                    let scale = window.scale_factor();
                    let cell_w_logical =
                        renderer.cell_width() as f64 / scale;
                    let cell_h_logical =
                        renderer.cell_height() as f64 / scale;
                    let new_cols =
                        (physical_size.width as f64 / cell_w_logical) as usize;
                    let new_rows =
                        (physical_size.height as f64 / cell_h_logical) as usize;

                    if new_cols > 0 && new_rows > 0 {
                        terminal.resize(new_rows, new_cols);
                        if let Some(pty) = &self.pty {
                            let _ = pty.resize(new_rows as u16, new_cols as u16);
                        }
                    }

                    renderer.resize(window, physical_size);
                    renderer.draw(terminal);
                }
            }
            WindowEvent::RedrawRequested => {
                self.pump_pty();
                self.process_messages();
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
            _ => {}
        }
    }
}

fn main() {
    tracing_subscriber::fmt::init();
    info!("Starting Weft");

    // Create a tokio runtime for PTY async operations.
    // winit's event loop doesn't provide one, so we run our own.
    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
    let _guard = rt.enter();

    let event_loop = EventLoop::new().unwrap();
    let mut app = App::new();
    event_loop.run_app(&mut app).unwrap();
}
