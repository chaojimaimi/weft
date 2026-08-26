//! Winit application lifecycle and runtime scheduling.

use super::*;

mod resize_transaction;
#[cfg(test)]
mod retention_tests;
use resize_transaction::commit_pty_resize_result;

pub(crate) fn install_runtime_diagnostics() {
    // v1.10.4: write to ~/Library/Logs/Weft/weft.log (macOS standard location)
    // and APPEND across launches so a crash/relaunch doesn't wipe the prior
    // session's diagnostics. Previously this truncated /tmp/weft.log on every
    // start, which erased the exact现场 dogfood bugs need.
    // v1.4.0: honor RUST_LOG, default to `info`.
    let log_path = runtime_log_path(std::env::var_os("HOME").as_deref());
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(f) => f,
        Err(_) => {
            // v1.10.6: local-time timestamps even in the stderr fallback.
            tracing_subscriber::fmt()
                .with_timer(tracing_subscriber::fmt::time::LocalTime::rfc_3339())
                .init();
            let default_hook = std::panic::take_hook();
            setup_panic_hook(default_hook);
            return;
        }
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    // v1.10.6: timestamp in LOCAL time (CST) instead of the default UTC.
    // The default SystemTime formatter emits RFC3339 UTC, which read as
    // "8 hours in the past" for Asia/Shanghai users and made logs confusing.
    let subscriber = tracing_subscriber::fmt()
        .with_writer(std::sync::Arc::new(file))
        .with_ansi(false)
        .with_timer(tracing_subscriber::fmt::time::LocalTime::rfc_3339())
        .with_env_filter(filter)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);
    // v1.10.4: session separator so multiple appended launches stay readable.
    tracing::info!(
        pid = std::process::id(),
        version = env!("CARGO_PKG_VERSION"),
        "=== Weft session start ==="
    );
    setup_panic_hook(std::panic::take_hook());
}

// `PanicInfo` was renamed to `PanicHookInfo` in Rust 1.81, but our MSRV is
// 1.75 (`rust-version` in Cargo.toml). Stay on the old name until the MSRV
// bump and silence the deprecation at the use sites below.
#[allow(deprecated)]
fn setup_panic_hook(default_hook: Box<dyn Fn(&std::panic::PanicInfo<'_>) + Send + Sync + 'static>) {
    std::panic::set_hook(Box::new(move |info: &std::panic::PanicInfo<'_>| {
        let path = panic_log_path(std::env::var_os("HOME").as_deref());
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // v1.10.4: append (not truncate) so multiple panics across launches
        // don't overwrite each other's backtrace — essential for dogfood.
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
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

pub(super) fn terminal_capability_env() -> Vec<(String, String)> {
    vec![
        ("TERM".into(), "xterm-256color".into()),
        ("COLORTERM".into(), "truecolor".into()),
        // v1.10: BSD `ls` only colorizes with `-G` or CLICOLOR=1. Setting it
        // here makes plain `ls` color directories (SGR 34 → theme palette[4])
        // while regular files keep the default foreground, so directory tint
        // follows the active theme automatically. LSCOLORS is deliberately
        // NOT set: the macOS default already matches, keeping us minimally
        // invasive — user dotfiles stay free to override either variable.
        ("CLICOLOR".into(), "1".into()),
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

/// v1.10.4: runtime tracing log path under the macOS-standard location.
/// Falls back to `/tmp/weft.log` only when `$HOME` is unavailable.
fn runtime_log_path(home: Option<&std::ffi::OsStr>) -> std::path::PathBuf {
    match home {
        Some(h) => std::path::PathBuf::from(h).join("Library/Logs/Weft/weft.log"),
        None => std::path::PathBuf::from("/tmp/weft.log"),
    }
}

fn schedule_synchronized_output_watchdog(
    pending: std::sync::Arc<std::sync::atomic::AtomicBool>,
    delay: std::time::Duration,
    wake: impl FnOnce() + Send + 'static,
) -> bool {
    if pending.swap(true, Ordering::AcqRel) {
        return false;
    }
    std::thread::Builder::new()
        .name(String::from("weft-timer"))
        .spawn(move || {
            std::thread::sleep(delay);
            pending.store(false, Ordering::Release);
            wake();
        })
        .ok();
    true
}

fn schedule_primary_history_refresh_wakes(
    tabs: &mut [Tab],
    proxy: &winit::event_loop::EventLoopProxy<AppEvent>,
) {
    for delay in tabs
        .iter_mut()
        .filter_map(Tab::take_primary_history_refresh_wake_delay)
    {
        let proxy = proxy.clone();
        std::thread::Builder::new()
            .name(String::from("weft-tab-wake"))
            .spawn(move || {
                std::thread::sleep(delay);
                let _ = proxy.send_event(AppEvent::Wake);
            })
            .ok();
    }
}

/// v1.7.6 → FIX ② (2026-08-19): Hydrate a terminal with persisted history.
///
/// The editor's ↑-key recall gets ONLY this tab's commands (filtered by
/// `tab_block_ids`), matching Warp's per-tab session-scoped history. The block
/// tracker similarly gets ONLY this tab's blocks. See FIX_EDITOR_HISTORY_PER_TAB.md
/// for Warp comparison and rationale.
///
/// Before the fix, editor ↑-key recall incorrectly used GLOBAL history (all tabs
/// mixed together), while the block tracker was correctly per-tab — this mismatch
/// meant "Cmd+Shift+B panel is per-tab, but ↑ arrows cross-tab".
fn hydrate_persisted_history(
    terminal: &mut Terminal,
    global_newest_first: &[weft_core::blocks::Block],
    tab_block_ids: &[u64],
    block_id_allocator: std::sync::Arc<std::sync::atomic::AtomicU64>,
) {
    // Editor ↑-key recall: per-tab (this tab's own blocks), consistent with
    // the block tracker / Cmd+Shift+B panel below and Warp's session-scoped
    // up-arrow history. v1.7.6's global hydration mixed all tabs' commands
    // into every tab's ↑ recall (user-visible cross-tab mixing).
    let commands = global_newest_first
        .iter()
        .rev()
        .filter(|b| tab_block_ids.contains(&b.id.0))
        .map(|block| strip_prompt_prefix(&block.command))
        .filter(|command| !command.trim().is_empty())
        .collect();
    terminal.editor_mut().load_history(commands);

    // Block tracker: only this tab's blocks (per-tab isolation).
    // SQLite returns newest→oldest; BlockTracker expects chronological.
    let tab_blocks: Vec<_> = global_newest_first
        .iter()
        .rev()
        .filter(|b| tab_block_ids.contains(&b.id.0))
        .cloned()
        .collect();
    terminal.block_tracker_mut().load_blocks(tab_blocks);
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
                self.poll_completion_results();
                self.poll_runbook_results();
                self.poll_ai_results();
                schedule_primary_history_refresh_wakes(self.sessions.tabs_mut(), &self.proxy);
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
                let active_synchronized = self
                    .sessions
                    .active()
                    .terminal
                    .as_ref()
                    .is_some_and(Terminal::synchronized_output);
                let any_synchronized = self
                    .sessions
                    .tabs()
                    .iter()
                    .any(crate::tab::Tab::any_synchronized_output);
                if !active_synchronized {
                    self.request_redraw();
                }
                if any_synchronized {
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
                // v1.10.23: while the recovery prompt is up (deferred
                // runModal keeps this runloop alive), the 1 Hz autosave
                // must not run: it would DELETE-and-replace the tabs
                // table and overwrite the on-disk crash snapshot with
                // the fresh single-tab session before the user chooses,
                // destroying both recovery sources.
                //
                // Body (save + recovery snapshot + X6 timing metric +
                // paste-toast expiry) lives in effect_dispatch.rs next to
                // `expire_paste_toast_tick` — same 1 Hz tick domain.
                self.run_tabs_autosave_tick();
            }
            AppEvent::PerformanceProbeStart => self.performance_probe.start(),
            AppEvent::PerformanceProbeFinish => {
                if let Some(report) = self.performance_probe.finish() {
                    println!("{}", report.line());
                }
                // v1.6.3: Mark clean shutdown for the performance probe
                // exit path (test mode).
                self.recovery.mark_clean_shutdown();
                event_loop.exit();
            }
            AppEvent::MenuAction(action) => {
                // v1.1: native menu click → reuse the same dispatch as
                // keybindings. execute_action redraws where needed.
                self.execute_action(action);
            }
            AppEvent::QuitRequested => {
                self.request_application_close(event_loop);
            }
            AppEvent::RecoveryChosen(choice) => {
                // v1.10.23: the deferred recovery prompt (see
                // `run_startup_recovery`) closed off the winit handler and
                // sent the user's choice back through the proxy. Consumes
                // `pending_recovery`; a duplicate/spurious event is a
                // no-op. The window/tab state may have changed since
                // `resumed()` — the handler operates on the state as it
                // actually is.
                self.apply_recovery_choice(choice);
            }
            AppEvent::PasteDecided(response) => {
                // v1.11.1: deferred paste prompt closed (same
                // FIX_RECOVERY_MODAL_SPIN discipline as RecoveryChosen).
                self.apply_paste_decision(response);
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
            // v1.6.3: Mark clean shutdown for the should_exit path (last
            // tab closed, shell exit, etc.).
            self.recovery.mark_clean_shutdown();
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
            // Window-level transparency at creation. v1.2.11: runtime opacity
            // changes now also flip NSWindow.setOpaque: + backgroundColor
            // (see `macos_window::set_window_opaque`), so this flag only
            // governs the *initial* compositor hint. Lowering opacity below
            // 1.0 at runtime will work even when the window started opaque.
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
            self.config_state.config.theme.minimum_contrast,
            self.config_state.config.theme.semantic_output_enabled(),
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
                Ok(store) => Some(store),
                Err(e) => {
                    warn!(error = %e, "failed to open block store; persistence disabled");
                    None
                }
            }
        });
        self.sessions.set_block_store(block_store);

        // v1.7.3-C: Open the annotation sidecar store (bookmark/note/tags)
        // sharing the same `blocks.db` file. Best-effort — if it fails to
        // open, annotation actions are silently disabled (treated as no-ops
        // by the controller). Opened after BlockStore so the file exists.
        let annotation_store = weft_cache_dir().and_then(|cache| {
            let path = cache.join("blocks.db");
            match weft_core::blocks::annotations::AnnotationStore::open(&path) {
                Ok(store) => Some(store),
                Err(e) => {
                    warn!(error = %e, "failed to open annotation store; annotations disabled");
                    None
                }
            }
        });
        self.sessions.set_annotation_store(annotation_store);
        self.bookmarked_blocks = self
            .sessions
            .annotation_store()
            .and_then(|store| store.bookmarked_ids().ok())
            .unwrap_or_default();

        // v1.7.1: Open the search index (sidecar to blocks.db) for main-thread
        // upsert/delete. The background PaletteSearchWorker owns its own
        // SearchIndex for queries. If the index is empty, rebuild from
        // BlockStore so palette search has content on first launch.
        // v1.7.3-D: also pass AnnotationStore so bookmarked annotations
        // (notes/tags) are indexed on cold start and become searchable via
        // the Palette.
        self.search_index = crate::palette_search_worker::open_search_index(
            weft_cache_dir().map(|c| c.join("blocks.db")),
            self.sessions.block_store(),
            self.sessions.annotation_store(),
        );

        // v1.7.1: Spawn the palette search worker on a background thread.
        if let Some(cache) = weft_cache_dir() {
            self.palette.search_worker = crate::palette_search_worker::spawn_worker(
                cache.join("blocks.db"),
                self.proxy.clone(),
            );
        }

        // v1.6.3: Crash recovery detection. Check for an unclean shutdown
        // and offer to restore from a recovery snapshot if one exists.
        // This runs BEFORE the normal tab-snapshot restore so that a
        // successful recovery replaces the normal restore path.
        //
        // v1.10.23 (FIX_RECOVERY_MODAL_SPIN): detection stays synchronous
        // here, but when an unclean-shutdown snapshot exists the prompt is
        // deferred to a main-queue block — `runModal` must never run inside
        // the winit handler (the EventLoopWaker 0.1µs timer is then never
        // disarmed and the modal spins at ~84% CPU). The choice arrives
        // later as `AppEvent::RecoveryChosen` and
        // `apply_recovery_choice` restores + hydrates then.
        match self.run_startup_recovery() {
            recovery_controller::StartupRecoveryOutcome::Normal => {
                // v1.0 H4: restore saved tab snapshots (cwd + editor drafts)
                // so the session layout survives restarts. The first tab
                // (spawned above by spawn_pty) is replaced if saved
                // snapshots exist; otherwise it stays as a fresh shell. The
                // PTY itself is NOT revived — each restored tab gets a fresh
                // shell, with the editor draft rehydrated.
                //
                // v1.0 fix: cwd is restored via `chdir` in the child
                // process before exec (Pty::spawn_with_args `cwd` param),
                // NOT by sending a `cd` command. Sending `cd` polluted the
                // terminal, shell history, and block tracker with a
                // spurious `cd <cwd>` block. With chdir the shell starts in
                // the right directory silently — the initial tab stays
                // clean. If the saved cwd equals the weft process's cwd
                // (the common case when launching from the same directory),
                // no rebuild is needed — the initial tab already has the
                // right cwd.
                self.restore_tab_snapshots();
                // Restore history only after the tab topology is final.
                // Hydrating the initial terminal before cwd-based
                // replacement discarded the loaded history, and additional
                // restored tabs never received it at all.
                self.hydrate_tabs_from_history_store();
            }
            recovery_controller::StartupRecoveryOutcome::Pending => {
                // v1.10.23: the recovery prompt is on screen (deferred off
                // the winit handler). No synchronous restore here —
                // `apply_recovery_choice` rebuilds the topology and
                // hydrates history when the user's choice arrives.
                info!("recovery prompt pending; tab restore deferred to RecoveryChosen event");
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
        if let (Some(index), Some(store)) = (&self.search_index, &self.palette.store) {
            if let Err(e) = crate::palette_search_worker::sync_workflow_documents(index, store) {
                warn!(error = %e, "failed to index workflows");
            }
        }

        // Cursor-blink timer: wake the loop ~2x/sec so the caret toggles
        // without a vsync busy-loop. Exits when the event loop drops the proxy.
        // Flicker fix (Step 2): only wake when a cursor/caret is actually
        // animating. The main thread sets `cursor_anim_active` after each
        // redraw — when false (no prompt in block view, cursor hidden in
        // grid view, or window unfocused), the timer skips the wake, which
        // avoids pointless full redraws that caused idle flicker.
        let blink_proxy = self.proxy.clone();
        let blink_flag = self.window_runtime.cursor_anim_active.clone();
        std::thread::Builder::new()
            .name(String::from("weft-cursor"))
            .spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_millis(530));
                if !blink_flag.load(Ordering::Relaxed) {
                    continue; // no cursor to animate — skip this wake
                }
                if blink_proxy.send_event(AppEvent::Wake).is_err() {
                    break; // event loop exited
                }
            })
            .ok();

        // F3-2: Spinner timer — wake the loop ~every 80ms while a command is
        // running so the braille activity indicator animates smoothly even
        // when no PTY output is streaming (e.g. `sleep 10`). The main thread
        // sets `spinner_anim_active` after each redraw based on the shell phase.
        let spinner_proxy = self.proxy.clone();
        let spinner_flag = self.window_runtime.spinner_anim_active.clone();
        std::thread::Builder::new()
            .name(String::from("weft-spinner"))
            .spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_millis(80));
                if !spinner_flag.load(Ordering::Relaxed) {
                    continue; // no running command — skip this wake
                }
                if spinner_proxy.send_event(AppEvent::Wake).is_err() {
                    break; // event loop exited
                }
            })
            .ok();

        // Drag-selection autoscroll timer — wake the loop ~every 40ms while
        // a block-view drag selection is held past the content edge, so the
        // viewport keeps scrolling toward the pointer even when it doesn't
        // move. The main thread sets `selection_autoscroll_active` inside
        // `pump_selection_autoscroll` from the edge detection; when false
        // (no edge-held drag) the timer skips the wake.
        let autoscroll_proxy = self.proxy.clone();
        let autoscroll_flag = self.window_runtime.selection_autoscroll_active.clone();
        std::thread::Builder::new()
            .name(String::from("weft-autoscroll"))
            .spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_millis(40));
                if !autoscroll_flag.load(Ordering::Relaxed) {
                    continue; // no edge-held drag — skip this wake
                }
                if autoscroll_proxy.send_event(AppEvent::Wake).is_err() {
                    break; // event loop exited
                }
            })
            .ok();

        if self.performance_probe.enabled() {
            let probe_proxy = self.proxy.clone();
            std::thread::Builder::new()
                .name(String::from("weft-probe"))
                .spawn(move || {
                    // v1.4.0: honor WEFT_GUI_PROBE_*_SECS env overrides.
                    std::thread::sleep(performance_probe::warmup_duration());
                    if probe_proxy
                        .send_event(AppEvent::PerformanceProbeStart)
                        .is_err()
                    {
                        return;
                    }
                    std::thread::sleep(performance_probe::sample_duration());
                    let _ = probe_proxy.send_event(AppEvent::PerformanceProbeFinish);
                })
                .ok();
        }

        // Config file watcher: poll the config's mtime ~1/sec and reload live
        // on change (theme/font/keybindings/scrollback re-apply instantly).
        // Zero dependencies — mtime polling is cheap for a single file.
        if let Some(path) = Config::config_path() {
            let reload_proxy = self.proxy.clone();
            std::thread::Builder::new()
                .name(String::from("weft-config"))
                .spawn(move || {
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
                })
                .ok();
        }

        // D5: compare complete snapshots once per second so unmarked cwd or
        // scroll changes cannot bypass recovery, without writing unchanged
        // state. Best-effort failures retry on the next tick.
        // Runs on a background thread, wakes the loop
        // via AppEvent::TabsAutoSave (handled synchronously on the main
        // thread, which owns `&mut self`).
        let save_proxy = self.proxy.clone();
        std::thread::Builder::new()
            .name(String::from("weft-autosave"))
            .spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                if save_proxy.send_event(AppEvent::TabsAutoSave).is_err() {
                    break; // event loop exited
                }
            })
            .ok();

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
    pub(super) fn apply_pty_resize_effect(
        &mut self,
        tab: usize,
        pane_id: weft_core::pane_layout::PaneId,
        rows: usize,
        cols: usize,
    ) {
        // v1.10.19: the TIOCSWINSZ is deduped against the last-sent winsize
        // (Pane::apply_winsize_ioctl). Re-requesting a size the PTY already
        // has must not re-issue the ioctl — each redundant one SIGWINCHes
        // the foreground app, keeping a resize feedback loop alive. The
        // in-memory Grid is still committed below so grid dims never drift
        // from the PTY's.
        //
        // v1.10.25 Batch 3 (FIX_SELECTION_AND_RESIZE_REMAINING): the resize
        // path no longer clears the primary-screen frame. `Terminal::resize`
        // is dimension-only and keeps the old content, so the sequence is
        // old-frame stretch (CAMetalLayer keeps presenting the stale drawable)
        // → dimension-only old content (briefly misaligned, never blank) →
        // omp's repaint at the new size. `clear_screen_all` used to render a
        // blank intermediate frame that killed the CA stretch transition.
        let (ioctl_sent, committed) = {
            let Some(session) = self.sessions.tab_mut(tab) else {
                return;
            };
            // v1.3 Batch 6: target the specific pane by id (not just the active
            // pane). Pre-v1.3 callers always passed the active pane's id, so
            // behavior is unchanged for single-pane tabs.
            let Some(pane) = session.pane_mut(pane_id) else {
                return;
            };
            let (ioctl_sent, resize_succeeded) = match pane.apply_winsize_ioctl(rows, cols) {
                Ok(sent) => (sent, true),
                Err(error) => {
                    warn!(%error, tab, pane_id = %pane_id, rows, cols, "failed to apply PTY resize effect");
                    (false, false)
                }
            };
            let mut committed = false;
            if let Some(terminal) = &mut pane.terminal {
                committed = commit_pty_resize_result(
                    terminal,
                    &mut pane.pending_pty_resize,
                    (rows, cols),
                    resize_succeeded,
                );
            }
            (ioctl_sent, committed)
        };
        // DEBUG probe (stage 2/4): ioctl committed — time from the Resized
        // event to the TIOCSWINSZ taking effect; also arm the PTY-output
        // probe so the next output for this tab (stage 3/4) logs how long
        // until omp starts repainting.
        if committed && ioctl_sent {
            tracing::debug!(
                tab,
                pane_id = %pane_id,
                rows,
                cols,
                since_resize_ms = self.window_runtime.last_resize_instant.elapsed().as_millis(),
                "RESIZE_PROBE ioctl_commit",
            );
            if let Some(session) = self.sessions.tab_mut(tab) {
                session.arm_resize_output_probe();
            }
        }
    }

    /// v1.10.23: Hydrate per-tab history from the SQLite block store.
    ///
    /// Extracted from `resumed()` so BOTH startup paths run it exactly
    /// once, only after the tab topology is final:
    ///
    /// - Synchronous path (no recovery / clean shutdown): right after
    ///   `restore_tab_snapshots()` in `resumed()`.
    /// - Event-driven recovery path: inside `apply_recovery_choice`,
    ///   after the chosen restore path rebuilt the tabs.
    ///
    /// v1.7.6: Per-tab isolation — each tab's block tracker receives ONLY
    /// the blocks it produced last session (from its TabSnapshot's
    /// `block_ids`). The editor's ↑-key recall still gets the full global
    /// history. This prevents all tabs from showing the same mixed history.
    pub(super) fn hydrate_tabs_from_history_store(&mut self) {
        let Some(block_id_allocator) = self
            .sessions
            .block_store()
            .map(BlockStore::block_id_allocator)
        else {
            return;
        };
        let persisted_history = match self.sessions.block_store() {
            Some(store) => match store.recent(1000) {
                Ok(history) => history,
                Err(e) => {
                    warn!(error = %e, "failed to load block history");
                    Vec::new()
                }
            },
            None => Vec::new(),
        };
        for tab in self.sessions.tabs_mut() {
            // Clone first to avoid borrow conflict with terminal.as_mut().
            let tab_block_ids: Vec<u64> = tab
                .restored_snapshot
                .as_ref()
                .map(|snap| snap.block_ids.clone())
                .unwrap_or_default();
            if let Some(terminal) = tab.terminal.as_mut() {
                hydrate_persisted_history(
                    terminal,
                    &persisted_history,
                    &tab_block_ids,
                    block_id_allocator.clone(),
                );
            }
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
    fn resize_commit_keeps_primary_screen_content_after_dimension_only_resize() {
        // v1.10.25 Batch 3 (FIX_SELECTION_AND_RESIZE_REMAINING): the resize
        // path used to clear_screen_all() before the omp repaint, rendering a
        // blank frame (killing CA's stretch transition). Resize is now
        // dimension-only — the old content must survive the commit untouched.
        let mut terminal = Terminal::new(8, 40);
        terminal.process(b"\x1b]7;file://localhost/Users/me/Claude\x07");
        terminal.process(b"\x1b]133;A\x07claude\x1b]133;B\x07\x1b]133;C\x07");
        terminal.process(b"\x1b[?2026h\x1b[2J\x1b[HNEW ICON\x1b[?2026l");
        assert!(terminal.primary_screen_repaint_capable());
        let mut pending = None;
        assert!(commit_pty_resize_result(
            &mut terminal,
            &mut pending,
            (10, 50),
            true
        ));
        assert_eq!(
            (terminal.grid().num_rows, terminal.grid().num_cols),
            (10, 50),
            "dimension-only resize must still commit"
        );
        assert_eq!(terminal.grid().row_text(0), "NEW ICON");
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
    fn terminal_capabilities_override_a_bad_launcher_environment() {
        let env = terminal_capability_env();
        let value = |key: &str| env.iter().find(|(name, _)| name == key).map(|(_, v)| v);
        assert_eq!(value("TERM").map(String::as_str), Some("xterm-256color"));
        assert_eq!(value("COLORTERM").map(String::as_str), Some("truecolor"));
        assert_eq!(value("CLICOLOR").map(String::as_str), Some("1"));
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
        assert!(env
            .iter()
            .any(|pair| pair == &("CLICOLOR".into(), "1".into())));
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
            screen_origin: false,
        }
    }

    #[test]
    fn hydrate_gives_editor_and_tracker_only_tab_blocks() {
        // FIX ② (2026-08-19): editor ↑-key recall is per-tab (consistent with
        // block tracker and Warp semantics), not global.
        let mut newest_first = vec![block(2, "❯ echo newest"), block(1, "❯ echo oldest")];
        newest_first[0].output = "large persisted output".repeat(1024).into();
        let allocator = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(3));
        let mut terminal_a = Terminal::new(24, 80);
        let mut terminal_b = Terminal::new(24, 80);

        // Tab A produced blocks [1, 2]; tab B produced only block [1].
        hydrate_persisted_history(&mut terminal_a, &newest_first, &[1, 2], allocator.clone());
        hydrate_persisted_history(&mut terminal_b, &newest_first, &[1], allocator);

        // Both editors now see only their tab's own commands (per-tab isolation).
        assert_eq!(
            terminal_a.editor().history(),
            ["echo newest", "echo oldest"]
        );
        assert_eq!(
            terminal_b.editor().history(),
            ["echo oldest"] // block 2 is NOT in tab B's history
        );

        // Block trackers are per-tab isolated.
        let ids_a: Vec<_> = terminal_a
            .block_tracker()
            .blocks()
            .iter()
            .map(|b| b.id.0)
            .collect();
        let ids_b: Vec<_> = terminal_b
            .block_tracker()
            .blocks()
            .iter()
            .map(|b| b.id.0)
            .collect();
        assert_eq!(ids_a, [1, 2]);
        assert_eq!(ids_b, [1]);

        // Shared Arc<str> output (no clone of the large string).
        assert!(std::sync::Arc::ptr_eq(
            &terminal_a.block_tracker().blocks()[0].output,
            &terminal_b.block_tracker().blocks()[0].output,
        ));
    }

    #[test]
    fn hydrate_with_empty_block_ids_loads_nothing_into_tracker() {
        // FIX ② (2026-08-19): legacy snapshots (no block_ids) → both block tracker
        // and editor history stay empty (per-tab isolation).
        let newest_first = vec![block(1, "❯ ls"), block(2, "❯ echo hi")];
        let allocator = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(3));
        let mut terminal = Terminal::new(24, 80);
        hydrate_persisted_history(&mut terminal, &newest_first, &[], allocator);
        assert!(terminal.block_tracker().blocks().is_empty());
        assert_eq!(terminal.editor().history().len(), 0); // no block_ids → empty history
    }

    /// v1.8.9 regression test for the recovery-restore history bug.
    ///
    /// Before the fix, `restore_workspace` (recovery Restore button) rebuilt
    /// tabs + cwds but left `restored_snapshot = None`, so the hydration loop
    /// passed empty `block_ids` and the block tracker (sidebar history) was
    /// empty — while the Ignore path (which goes through
    /// `restore_tab_snapshots`) loaded the full history. This test verifies
    /// the core invariant: when `restored_snapshot` carries `block_ids`,
    /// hydrate populates the block tracker; when it's absent, it doesn't.
    #[test]
    fn hydrate_with_recovered_block_ids_populates_tracker() {
        let newest_first = vec![block(10, "❯ git status"), block(20, "❯ make test")];
        let allocator = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(30));

        // Simulate the post-fix recovery path: restored_snapshot was attached
        // with this tab's block_ids = [10, 20].
        let mut terminal = Terminal::new(24, 80);
        hydrate_persisted_history(&mut terminal, &newest_first, &[10, 20], allocator.clone());
        assert_eq!(
            terminal.block_tracker().blocks().len(),
            2,
            "recovery restore with attached block_ids should hydrate the tracker"
        );
        assert_eq!(
            terminal.editor().history().len(),
            2,
            "editor history should also be populated (per-tab isolation)"
        );

        // Contrast with the pre-fix recovery path: restored_snapshot was None,
        // yielding empty block_ids and an empty tracker.
        let mut terminal_bare = Terminal::new(24, 80);
        hydrate_persisted_history(&mut terminal_bare, &newest_first, &[], allocator);
        assert!(
            terminal_bare.block_tracker().blocks().is_empty(),
            "empty block_ids (pre-fix recovery path) leaves the tracker empty"
        );
        assert_eq!(
            terminal_bare.editor().history().len(),
            0,
            "empty block_ids (pre-fix recovery path) also leaves editor history empty"
        );
    }

    /// v1.10.24 B1 chain regression: the real Restore order (workspace
    /// restore sets the cwd fallback, then `attach_recovery_tab_snapshots`
    /// attaches the persisted snapshot) must deliver `block_ids` to the
    /// hydration loop — the exact connection the v1.8.9 stub-snapshot no-op
    /// broke. Extends the hydrate-only test above with the Tab-level chain:
    /// `attach_recovery_snapshot` must succeed after
    /// `set_restored_cwd_fallback` so `hydrate_tabs_from_history_store`
    /// reads the real `block_ids` off `restored_snapshot`.
    #[test]
    fn cwd_fallback_then_attach_hydrates_tracker_from_injected_block_ids() {
        let mut tab = crate::tab::Tab::empty();
        tab.terminal = Some(Terminal::with_scrollback(24, 80, 1000));

        // Workspace restore: cwd fallback only — no stub snapshot.
        tab.set_restored_cwd_fallback(Some("/saved".into()));
        assert!(
            tab.restored_snapshot.is_none(),
            "v1.8.9 no-op regression: no stub snapshot from the cwd fallback"
        );

        // attach_recovery_tab_snapshots: the real attach must succeed.
        assert!(
            tab.attach_recovery_snapshot(&weft_core::persistence::TabSnapshot {
                position: 0,
                active: false,
                cwd: Some("/saved".into()),
                block_scroll_offset: 0,
                editor_buffer: String::new(),
                shell_phase: "AtPrompt".into(),
                block_ids: vec![10, 20],
            })
        );

        // hydrate_tabs_from_history_store reads block_ids off the snapshot.
        let tab_block_ids: Vec<u64> = tab
            .restored_snapshot
            .as_ref()
            .map(|snap| snap.block_ids.clone())
            .unwrap_or_default();
        assert_eq!(
            tab_block_ids,
            vec![10, 20],
            "the attached snapshot must inject block_ids (v1.8.9 chain)"
        );

        let newest_first = vec![block(10, "❯ git status"), block(20, "❯ make test")];
        let allocator = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(30));
        hydrate_persisted_history(
            tab.terminal.as_mut().unwrap(),
            &newest_first,
            &tab_block_ids,
            allocator,
        );
        assert_eq!(
            tab.terminal
                .as_ref()
                .unwrap()
                .block_tracker()
                .blocks()
                .len(),
            2,
            "injected block_ids must hydrate the tracker (v1.8.9 chain)"
        );
    }

    /// FIX ② (2026-08-19): Verify that per-tab filtering preserves newest-first order.
    /// When a tab has blocks [2, 5, 10] from a global list [1..20], the editor history
    /// must still show them newest-first (10 → 5 → 2), not chronological (2 → 5 → 10).
    #[test]
    fn hydrate_preserves_newest_first_order_after_filtering() {
        // Global history: blocks 1 (oldest) through 10 (newest)
        let mut newest_first = Vec::new();
        for i in (1..=10).rev() {
            newest_first.push(block(i, &format!("❯ cmd{}", i)));
        }

        let allocator = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(11));
        let mut terminal = Terminal::new(24, 80);

        // Tab only produced blocks [2, 5, 10] (non-contiguous, not the newest globally)
        hydrate_persisted_history(&mut terminal, &newest_first, &[2, 5, 10], allocator);

        // Editor history must preserve newest-first order among the tab's blocks:
        // global order is 10→5→2 (since 10 is newest, 5 is middle, 2 is oldest among them)
        assert_eq!(
            terminal.editor().history(),
            ["cmd10", "cmd5", "cmd2"],
            "filtering must preserve newest-first order (not chronological)"
        );

        // Block tracker should also have the correct order
        let ids: Vec<_> = terminal
            .block_tracker()
            .blocks()
            .iter()
            .map(|b| b.id.0)
            .collect();
        assert_eq!(ids, [2, 5, 10], "block tracker expects chronological order");
    }
}
