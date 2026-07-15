//! Winit application lifecycle and runtime scheduling.

use super::*;

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
                self.request_redraw();
            }
            AppEvent::ConfigReload => {
                self.reload_config();
                self.request_redraw();
            }
            AppEvent::TabsAutoSave => {
                self.drain_effects(vec![Effect::PersistTabs]);
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
        let block_store = weft_cache_dir().and_then(|cache| {
            let path = cache.join("blocks.db");
            match BlockStore::open(&path) {
                Ok(store) => {
                    if let Some(terminal) = self.sessions.active_mut().terminal.as_mut() {
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
        self.sessions.set_block_store(block_store);

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
            // Clear saved tabs now so the immutable borrow of self.sessions
            // ends before the mutations below. The periodic auto-save will
            // re-persist the live state.
            let _ = store.clear_tabs();
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
                    self.sessions.set_active(0);
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
