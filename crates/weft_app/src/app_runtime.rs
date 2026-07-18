//! Winit application lifecycle and runtime scheduling.

use super::*;

pub(crate) fn install_runtime_diagnostics() {
    tracing_subscriber::fmt::init();
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let path = panic_log_path(std::env::var_os("HOME").as_deref());
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
        {
            use std::io::Write;
            let _ = writeln!(
                file,
                "\n=== Weft panic pid={} at {:?} ===\n{}\n{}",
                std::process::id(),
                std::time::SystemTime::now(),
                info,
                std::backtrace::Backtrace::force_capture()
            );
        }
        default_hook(info);
    }));
}

fn invalidate_primary_tui_frame(terminal: &mut Terminal) -> bool {
    if !terminal.primary_screen_repaint_capable() {
        return false;
    }
    terminal.grid_mut().clear_screen_all();
    true
}

fn should_clear_pending_resize(resize_failed: bool) -> bool {
    !resize_failed
}

pub(super) fn terminal_capability_env() -> Vec<(String, String)> {
    vec![
        ("TERM".into(), "xterm-256color".into()),
        ("COLORTERM".into(), "truecolor".into()),
        ("TERM_PROGRAM".into(), "Weft".into()),
        (
            "TERM_PROGRAM_VERSION".into(),
            env!("CARGO_PKG_VERSION").into(),
        ),
    ]
}

fn panic_log_path(home: Option<&std::ffi::OsStr>) -> std::path::PathBuf {
    home.map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Library/Logs/Weft/panic.log")
}

fn schedule_synchronized_output_watchdog(
    pending: std::sync::Arc<std::sync::atomic::AtomicBool>,
    delay: std::time::Duration,
    wake: impl FnOnce() + Send + 'static,
) -> bool {
    if pending.swap(true, Ordering::AcqRel) {
        return false;
    }
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        pending.store(false, Ordering::Release);
        wake();
    });
    true
}

fn hydrate_persisted_history(
    terminal: &mut Terminal,
    newest_first: &[weft_core::blocks::Block],
    block_id_allocator: std::sync::Arc<std::sync::atomic::AtomicU64>,
) {
    // SQLite returns newest→oldest. Both BlockTracker and Editor::load_history
    // accept chronological input; the editor reverses it internally for Up.
    let oldest_first: Vec<_> = newest_first.iter().rev().cloned().collect();
    let commands = oldest_first
        .iter()
        .map(|block| strip_prompt_prefix(&block.command))
        .filter(|command| !command.trim().is_empty())
        .collect();
    terminal.editor_mut().load_history(commands);
    terminal.block_tracker_mut().load_blocks(oldest_first);
    terminal
        .block_tracker_mut()
        .use_shared_id_allocator(block_id_allocator);
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
                self.performance_probe.record_wake();
                self.pump_pty();
                self.process_messages();
                if self.sessions.tabs().iter().any(|tab| {
                    tab.terminal
                        .as_ref()
                        .is_some_and(Terminal::primary_screen_exit_pending)
                }) {
                    let proxy = self.proxy.clone();
                    schedule_synchronized_output_watchdog(
                        self.screen_exit_watchdog_pending.clone(),
                        weft_core::vt::PRIMARY_SCREEN_EXIT_SETTLE_DELAY,
                        move || {
                            let _ = proxy.send_event(AppEvent::Wake);
                        },
                    );
                }
                let synchronized = self
                    .sessions
                    .active()
                    .terminal
                    .as_ref()
                    .is_some_and(Terminal::synchronized_output);
                if !synchronized {
                    self.request_redraw();
                } else {
                    let proxy = self.proxy.clone();
                    schedule_synchronized_output_watchdog(
                        self.window_runtime
                            .synchronized_output_watchdog_pending
                            .clone(),
                        weft_core::vt::SYNCHRONIZED_OUTPUT_TIMEOUT,
                        move || {
                            let _ = proxy.send_event(AppEvent::Wake);
                        },
                    );
                }
            }
            AppEvent::ConfigReload => {
                self.reload_config();
                self.request_redraw();
            }
            AppEvent::TabsAutoSave => {
                self.save_changed_tabs();
            }
            AppEvent::PerformanceProbeStart => self.performance_probe.start(),
            AppEvent::PerformanceProbeFinish => {
                if let Some(report) = self.performance_probe.finish() {
                    println!("{}", report.line());
                }
                event_loop.exit();
            }
            AppEvent::MenuAction(action) => {
                // v1.1: native menu click → reuse the same dispatch as
                // keybindings. execute_action redraws where needed.
                self.execute_action(action);
            }
            AppEvent::AccessibilityPress {
                generation,
                node_id,
            } => {
                if let Some(action) = self.accessibility.resolve_press(generation, node_id) {
                    self.perform_accessibility_action(action);
                }
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
        // style mask + transparency; tab-bar empty space starts native drag.
        configure_titlebar(&window);
        accessibility::install_event_proxy(self.proxy.clone());
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

        // F3-3: apply the persisted sidebar width override (if any) so the
        // first frame opens with the user's last-dragged width instead of the
        // responsive default.
        if let Some(r) = self.renderer.as_mut() {
            r.set_sidebar_width(self.config_state.config.window.sidebar_width);
        }

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
        let mut persisted_history = Vec::new();
        let block_store = weft_cache_dir().and_then(|cache| {
            let path = cache.join("blocks.db");
            match BlockStore::open(&path) {
                Ok(store) => {
                    match store.recent(1000) {
                        Ok(history) => persisted_history = history,
                        Err(e) => warn!(error = %e, "failed to load block history"),
                    }
                    Some(store)
                }
                Err(e) => {
                    warn!(error = %e, "failed to open block store; persistence disabled");
                    None
                }
            }
        });
        self.sessions.set_block_store(block_store);
        let block_id_allocator = self
            .sessions
            .block_store()
            .map(BlockStore::block_id_allocator);

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
        if let Some(store) = self.sessions.block_store() {
            let snaps_result = store.load_tabs();
            match snaps_result {
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
                                let mut tab = Tab::new(
                                    rows,
                                    cols,
                                    self.config_state.config.scrollback.lines,
                                    &self.proxy,
                                    cwd_to_apply,
                                );
                                if let Some(t) = &mut tab.terminal {
                                    if let Some(r) = &self.renderer {
                                        t.set_palette(r.theme().palette);
                                    }
                                }
                                tab.restore_from_snapshot(snap);
                                self.sessions.replace_tab(0, tab);
                            } else if let Some(t) = self.sessions.tab_mut(0) {
                                t.restore_from_snapshot(snap);
                            }
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
                            self.sessions.push_tab(tab);
                        }
                    }
                    let active = weft_core::persistence::TabSnapshot::restored_active_index(&snaps);
                    self.sessions.set_active(active);
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

        // Restore history only after the tab topology is final. Hydrating the
        // initial terminal before cwd-based replacement discarded the loaded
        // history, and additional restored tabs never received it at all.
        if let Some(block_id_allocator) = block_id_allocator {
            for tab in self.sessions.tabs_mut() {
                if let Some(terminal) = tab.terminal.as_mut() {
                    hydrate_persisted_history(
                        terminal,
                        &persisted_history,
                        block_id_allocator.clone(),
                    );
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

        // F3-2: Spinner timer — wake the loop ~every 80ms while a command is
        // running so the braille activity indicator animates smoothly even
        // when no PTY output is streaming (e.g. `sleep 10`). The main thread
        // sets `spinner_anim_active` after each redraw based on the shell phase.
        let spinner_proxy = self.proxy.clone();
        let spinner_flag = self.window_runtime.spinner_anim_active.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_millis(80));
            if !spinner_flag.load(Ordering::Relaxed) {
                continue; // no running command — skip this wake
            }
            if spinner_proxy.send_event(AppEvent::Wake).is_err() {
                break; // event loop exited
            }
        });

        if self.performance_probe.enabled() {
            let probe_proxy = self.proxy.clone();
            std::thread::spawn(move || {
                std::thread::sleep(performance_probe::WARMUP);
                if probe_proxy
                    .send_event(AppEvent::PerformanceProbeStart)
                    .is_err()
                {
                    return;
                }
                std::thread::sleep(performance_probe::SAMPLE);
                let _ = probe_proxy.send_event(AppEvent::PerformanceProbeFinish);
            });
        }

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

        // D5: compare complete snapshots once per second so unmarked cwd or
        // scroll changes cannot bypass recovery, without writing unchanged
        // state. Best-effort failures retry on the next tick.
        // Runs on a background thread, wakes the loop
        // via AppEvent::TabsAutoSave (handled synchronously on the main
        // thread, which owns `&mut self`).
        let save_proxy = self.proxy.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
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

impl App {
    pub(super) fn apply_pty_resize_effect(&mut self, tab: usize, rows: usize, cols: usize) {
        let Some(session) = self.sessions.tab_mut(tab) else {
            return;
        };
        let resize_failed = session.pty.as_ref().is_some_and(|pty| {
            pty.resize(rows as u16, cols as u16)
                .map(|()| {
                    if let Some(terminal) = &mut session.terminal {
                        invalidate_primary_tui_frame(terminal);
                    }
                    false
                })
                .unwrap_or_else(|error| {
                    warn!(%error, tab, rows, cols, "failed to apply PTY resize effect");
                    true
                })
        });
        if should_clear_pending_resize(resize_failed)
            && session.pending_pty_resize == Some((rows, cols))
        {
            session.pending_pty_resize = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panic_log_uses_the_user_library_logs_directory() {
        assert_eq!(
            panic_log_path(Some(std::ffi::OsStr::new("/Users/test"))),
            std::path::PathBuf::from("/Users/test/Library/Logs/Weft/panic.log")
        );
    }

    #[test]
    fn primary_tui_resize_invalidation_drops_stale_frame_but_keeps_shell_state() {
        let mut terminal = Terminal::new(8, 40);
        terminal.process(b"\x1b]7;file://localhost/Users/me/Claude\x07");
        terminal.process(b"\x1b]133;A\x07claude\x1b]133;B\x07\x1b]133;C\x07");
        terminal.process(b"\x1b[?2026h\x1b[2J\x1b[HOLD ICON\x1b[4;1HOLD HEADER\x1b[?2026l");
        let cursor_before = (
            terminal.grid().cursor.row,
            terminal.grid().cursor.col,
            terminal.grid().cursor.wrap_pending,
        );

        assert!(invalidate_primary_tui_frame(&mut terminal));
        assert_eq!(
            (
                terminal.grid().cursor.row,
                terminal.grid().cursor.col,
                terminal.grid().cursor.wrap_pending,
            ),
            cursor_before
        );
        terminal.process(b"\x1b[HNEW ICON");

        assert_eq!(terminal.grid().row_text(0), "NEW ICON");
        assert!(terminal.grid().row_text(3).is_empty());
        assert_eq!(terminal.cwd(), Some("/Users/me/Claude"));
        assert_eq!(
            terminal
                .block_tracker()
                .in_flight()
                .map(|live| live.command),
            Some("claude")
        );
    }

    #[test]
    fn resize_invalidation_does_not_clear_shell_or_alt_screen() {
        let mut shell = Terminal::new(4, 20);
        shell.process(b"shell output");
        assert!(!invalidate_primary_tui_frame(&mut shell));
        assert_eq!(shell.grid().row_text(0), "shell output");

        shell.process(b"\x1b[?1049hALT");
        assert!(!invalidate_primary_tui_frame(&mut shell));
        assert_eq!(shell.grid().row_text(0), "ALT");
    }

    #[test]
    fn cursor_addressing_without_synchronized_frames_is_not_destructively_cleared() {
        let mut terminal = Terminal::new(4, 24);
        terminal.process(b"\x1b]133;B\x07\x1b]133;C\x07");
        terminal.process(b"\x1b[Hprogress\x1b[2;1Hstill running");
        assert!(terminal.primary_screen_app_active());

        assert!(!invalidate_primary_tui_frame(&mut terminal));
        assert_eq!(terminal.grid().row_text(0), "progress");
        assert_eq!(terminal.grid().row_text(1), "still running");
    }

    #[test]
    fn failed_pty_resize_keeps_latest_dimensions_for_retry() {
        assert!(!should_clear_pending_resize(true));
        assert!(should_clear_pending_resize(false));
    }

    #[test]
    fn terminal_capabilities_override_a_bad_launcher_environment() {
        let env = terminal_capability_env();
        let value = |key: &str| env.iter().find(|(name, _)| name == key).map(|(_, v)| v);
        assert_eq!(value("TERM").map(String::as_str), Some("xterm-256color"));
        assert_eq!(value("COLORTERM").map(String::as_str), Some("truecolor"));
        assert_eq!(value("TERM_PROGRAM").map(String::as_str), Some("Weft"));
    }

    #[test]
    fn unsupported_shell_still_receives_terminal_capabilities() {
        let env = crate::shell_integration_env("/usr/local/bin/fish");
        assert!(env
            .iter()
            .any(|pair| pair == &("TERM".into(), "xterm-256color".into())));
        assert!(env
            .iter()
            .any(|pair| pair == &("COLORTERM".into(), "truecolor".into())));
    }

    use std::time::SystemTime;
    use weft_core::blocks::{Block, BlockId};

    #[test]
    fn synchronized_output_watchdog_wakes_without_another_pty_event() {
        let pending = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        assert!(schedule_synchronized_output_watchdog(
            pending.clone(),
            std::time::Duration::from_millis(1),
            move || tx.send(()).unwrap()
        ));
        assert!(!schedule_synchronized_output_watchdog(
            pending.clone(),
            std::time::Duration::from_millis(1),
            || {}
        ));
        rx.recv_timeout(std::time::Duration::from_millis(100))
            .unwrap();
        assert!(!pending.load(Ordering::Acquire));
    }

    fn block(id: u64, command: &str) -> Block {
        Block {
            id: BlockId(id),
            command: command.to_string(),
            cwd: None,
            output: String::new().into(),
            styled_output: None,
            exit_code: Some(0),
            started_at: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(id),
            finished_at: Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(id)),
            collapsed: false,
        }
    }

    #[test]
    fn restored_history_is_chronological_and_shares_output_across_tabs() {
        let mut newest_first = vec![block(2, "❯ echo newest"), block(1, "❯ echo oldest")];
        newest_first[0].output = "large persisted output".repeat(1024).into();
        let allocator = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(3));
        let mut terminal = Terminal::new(24, 80);
        let mut second_terminal = Terminal::new(24, 80);
        hydrate_persisted_history(&mut terminal, &newest_first, allocator.clone());
        hydrate_persisted_history(&mut second_terminal, &newest_first, allocator);

        assert_eq!(terminal.editor().history(), ["echo newest", "echo oldest"]);
        let ids: Vec<_> = terminal
            .block_tracker()
            .blocks()
            .iter()
            .map(|block| block.id.0)
            .collect();
        assert_eq!(ids, [1, 2]);
        assert!(std::sync::Arc::ptr_eq(
            &terminal.block_tracker().blocks()[1].output,
            &second_terminal.block_tracker().blocks()[1].output,
        ));
    }
}
