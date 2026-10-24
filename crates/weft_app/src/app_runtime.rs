//! Winit application lifecycle and runtime scheduling.

use super::*;

#[cfg(test)]
mod log_rotate_tests;
mod resize_transaction;
#[cfg(test)]
mod retention_tests;
// Startup-diagnostics and hydration families live in child modules
// (v1.13.9 file-budget split, pure code motion; resize_transaction.rs
// precedent).
mod diagnostics;
mod hydrate;
// `terminal_capability_env` and `install_runtime_diagnostics` moved to
// `diagnostics.rs`; re-exported so the `crate::app_runtime::` paths in
// app/helpers.rs and main.rs keep resolving.
pub(crate) use diagnostics::{install_runtime_diagnostics, terminal_capability_env};
use resize_transaction::commit_pty_resize_result;
use std::time::Duration;

fn schedule_synchronized_output_watchdog(
    pending: std::sync::Arc<std::sync::atomic::AtomicBool>,
    delay: std::time::Duration,
    wake: impl FnOnce() + Send + 'static,
) -> bool {
    if pending.swap(true, Ordering::AcqRel) {
        return false;
    }
    // v1.12.23 audit batch 1 (bespoke): failure must reset the armed flag or
    // the watchdog stays latched; `pending` moves into the closure, so clone.
    let rearm = pending.clone();
    if let Err(e) = std::thread::Builder::new()
        .name(String::from("weft-timer"))
        .spawn(move || {
            std::thread::sleep(delay);
            pending.store(false, Ordering::Release);
            wake();
        })
    {
        // Re-arm so a future request can retry; today's wake is lost but the
        // watchdog is not permanently disabled (audit S-3).
        rearm.store(false, Ordering::Release);
        tracing::warn!(error = ?e, "weft-timer spawn failed");
    }
    true
}

fn schedule_primary_history_refresh_wakes(
    tabs: &mut [Tab],
    proxy: &winit::event_loop::EventLoopProxy<AppEvent>,
) {
    // v1.11.12 (PLAN_v11112 M-C): shared note for ALL wakeup-class send_event
    // sites (12×: app_runtime 154/234/260/681, mouse:943, config:445,
    // palette_search_worker:276, main.rs 463/466/472, app_state:673, pane:141)
    // plus the 5 timer sites below and the probe thread:
    // EventLoopClosed = loop exited, by design silent — no wake can ever be
    // delivered after exit, so a warn would be pure teardown noise.
    for delay in tabs
        .iter_mut()
        .flat_map(Tab::take_primary_history_refresh_wake_delays)
    {
        let proxy = proxy.clone();
        spawn_named("weft-tab-wake", move || {
            std::thread::sleep(delay);
            let _ = proxy.send_event(AppEvent::Wake);
        });
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
                // Appendix H: see reflow_if_live_size_moved (zoom burst).
                self.reflow_if_live_size_moved(event_loop);
                self.performance_probe.record_wake();
                // v1.13.6 T10 P2 (D6): no main-thread byte pump — the parse
                // workers own it; this drains their control events only.
                self.process_messages();
                // v1.11.15 (FIX B, PLAN_v11115_EXIT_RACE_MOUSE_LEAK §2):
                // process_messages → remove_dead can empty `tabs` when the
                // LAST shell exits (panic.log ×6). Everything below derefs
                // tabs[active] in one way or another, so when no tabs remain
                // the arm falls straight through to the shared should_exit
                // tail at the bottom of user_event (Effect::Exit was already
                // drained inside process_messages — semantics unchanged).
                if !self.sessions.tabs().is_empty() {
                    self.poll_completion_results();
                    self.poll_runbook_results();
                    self.poll_ai_results();
                    schedule_primary_history_refresh_wakes(self.sessions.tabs_mut(), &self.proxy);
                    if self.sessions.tabs().iter().any(|tab| {
                        tab.with_terminal(|t| t.primary_screen_exit_pending())
                            .unwrap_or(false)
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
                        .and_then(|tab| tab.with_terminal(|t| t.synchronized_output()))
                        .unwrap_or(false);
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
            }
            AppEvent::ConfigReload => {
                self.reload_config();
                self.request_redraw();
            }
            AppEvent::PathBinsResolved(bins) => {
                // v1.13.5 (T16c): async PATH backfill — requests before this
                // used the env-PATH fast subset; the thread already wrote the
                // cache, and reloads never touch path_bins (swap persists).
                self.config_state.path_bins = bins;
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
            AppEvent::PasteDecided { response, seq } => {
                // v1.11.1: deferred paste prompt closed (same
                // FIX_RECOVERY_MODAL_SPIN discipline as RecoveryChosen).
                // v1.11.11 (M-B): the reply pairs with the parked paste via
                // its seq — a stale dialog response never consumes a newer
                // park (OSC52 pattern).
                self.apply_paste_decision(response, seq);
            }
            AppEvent::Osc52ReadDecided { allowed, seq } => {
                // v1.11.5 (PLAN_v1115 §M3): deferred OSC 52 read permission
                // prompt closed. Peek-compare-take pairs it with the parked
                // request (stale decisions never consume newer slots).
                self.apply_osc52_read_decision(allowed, seq);
            }
            AppEvent::NotificationActivated(block_id) => {
                // v1.11.5 (PLAN_v1115 §M6): notification clicked — front +
                // jump (implemented in effect_dispatch).
                self.handle_notification_activated(block_id);
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

        // v1.11.5 (PLAN_v1115 §M4): build the notification sink once the
        // event loop is up (bundle-identity gate inside; NoopSink for dev
        // binaries). Requires a MainThreadMarker — we are on main here.
        if let Some(mtm) = objc2_foundation::MainThreadMarker::new() {
            self.notification_sink =
                crate::macos_notifications::build_sink(mtm, self.proxy.clone());
        } else {
            tracing::warn!("notification sink: not on main thread at resumed; keeping NoopSink");
        }
        // v1.11.12 (PLAN_v11112 M-B): notifications phase boundary.
        performance_probe::report_phase(performance_probe::StartupPhase::Notifications);

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
        // PLAN_zoom Z-d: add `setFrameSize:` to the winit view class
        // (0.30.13 lacks it) — the IMP syncs the drawable right after super
        // applies the new size (Warp host_view.m shape); the zoom-sequence
        // marker is armed by the size-jump detector on the Resized path.
        macos_zoom::install_zoom_sequence_hook(&window);
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
            // v1.12.2 B2 (PLAN_S2_render): default false — live-resize keeps
            // async present and skips the transaction flush; `true` in
            // config.toml restores the v1.11.6 flip+flush carrier.
            self.config_state
                .config
                .window
                .presents_with_transaction_live_resize,
        );
        // v1.11.12 (PLAN_v11112 M-B): renderer-init phase boundary (shader
        // compile + pipelines happen inside `MetalRenderer::new`).
        performance_probe::report_phase(performance_probe::StartupPhase::RendererInit);

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
        // v1.11.12 (PLAN_v11112 M-B): window/pane phase boundary.
        performance_probe::report_phase(performance_probe::StartupPhase::WindowPane);

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

        // v1.13.17 S4: seed the zoom Reduce-Motion bridge at startup. The
        // 1Hz poller's first query is delayed 1s (`last_appearance_check`
        // inits to `Instant::now()`), and the `follow_system` block above
        // governs the THEME only — reduce-motion polling is unconditional —
        // so this sync sits outside it, immediately after window creation.
        crate::macos_zoom::set_zoom_reduce_motion(unsafe { system_reduce_motion() });

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
        // v1.11 audit (PLAN_audit_fix_batch3 C3): Arc-shared with the renderer.
        self.bookmarked_blocks = self
            .sessions
            .annotation_store()
            // v1.13.2 (WP-D): `.ok()` used to swallow a failed startup load;
            // the empty-set fallback is unchanged, the failure is now logged.
            .and_then(|store| match store.bookmarked_ids() {
                Ok(ids) => Some(ids),
                Err(e) => {
                    warn!(error = %e, "bookmark ids load failed");
                    None
                }
            })
            .map(std::sync::Arc::new)
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
        // v1.11.12 (PLAN_v11112 M-B): stores-open phase boundary — covers the
        // blocks.db + annotation + search-index open/rebuild above (architect
        // P1-4: the missing "+63ms" segment boundary).
        performance_probe::report_phase(performance_probe::StartupPhase::StoresOpen);

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
                // v1.11.13 (PLAN_v11113 §M2): Normal-arm restore complete —
                // flip the activation gate + replay queued cold-start clicks
                // (NOT at the resumed() tail: the Pending arm must keep the
                // gate closed until apply_recovery_choice restores).
                self.finish_restore_notifications();
            }
            recovery_controller::StartupRecoveryOutcome::Pending => {
                // v1.10.23: the recovery prompt is on screen (deferred off
                // the winit handler). No synchronous restore here —
                // `apply_recovery_choice` rebuilds the topology and
                // hydrates history when the user's choice arrives.
                info!("recovery prompt pending; tab restore deferred to RecoveryChosen event");
            }
        }
        // v1.11.12 (PLAN_v11112 M-B): session-restore phase boundary —
        // recovery detection + tab snapshot restore + history hydration all
        // happened inside the match above.
        performance_probe::report_phase(performance_probe::StartupPhase::SessionRestore);

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
        spawn_named("weft-cursor", move || loop {
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
        spawn_named("weft-spinner", move || loop {
            std::thread::sleep(std::time::Duration::from_millis(80));
            if !spinner_flag.load(Ordering::Relaxed) {
                continue; // no running command — skip this wake
            }
            if spinner_proxy.send_event(AppEvent::Wake).is_err() {
                break; // event loop exited
            }
        });

        // Drag-selection autoscroll timer — wake the loop ~every 40ms while
        // a block-view drag selection is held past the content edge, so the
        // viewport keeps scrolling toward the pointer even when it doesn't
        // move. The main thread sets `selection_autoscroll_active` inside
        // `pump_selection_autoscroll` from the edge detection; when false
        // (no edge-held drag) the timer skips the wake.
        let autoscroll_proxy = self.proxy.clone();
        let autoscroll_flag = self.window_runtime.selection_autoscroll_active.clone();
        spawn_named("weft-autoscroll", move || loop {
            std::thread::sleep(std::time::Duration::from_millis(40));
            if !autoscroll_flag.load(Ordering::Relaxed) {
                continue; // no edge-held drag — skip this wake
            }
            if autoscroll_proxy.send_event(AppEvent::Wake).is_err() {
                break; // event loop exited
            }
        });

        // v1.14.0 (PLAN_v1140 T0): motion tick — drives every v1.14.0
        // animation (overlay reveal / sidebar width / tab indicator / hover
        // fade) by waking the loop at 16ms while any Tween is alive and
        // dropping to a 40ms idle probe otherwise (RAII counter in
        // app::motion; rust-reviewer P3: the shorter probe halves the
        // worst-case first-frame latency after an idle period, and an idle
        // probe only burns a counter read — no Wake is posted). Same
        // sleep-loop shape as the three timers above: the motor only posts
        // Wake — progress and drawing still ride the existing
        // request_redraw → draw chain.
        let motion_proxy = self.proxy.clone();
        spawn_named("weft-motion", move || loop {
            if crate::app::motion::live_tweens() > 0 {
                if motion_proxy.send_event(AppEvent::Wake).is_err() {
                    break; // event loop exited
                }
                std::thread::sleep(std::time::Duration::from_millis(16));
            } else {
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
        });

        if self.performance_probe.enabled() {
            let probe_proxy = self.proxy.clone();
            spawn_named("weft-probe", move || {
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
            });
        }

        // Config file watcher: poll the config's mtime ~1/sec and reload live
        // on change (theme/font/keybindings/scrollback re-apply instantly).
        // Zero dependencies — mtime polling is cheap for a single file.
        if let Some(path) = Config::config_path() {
            let reload_proxy = self.proxy.clone();
            spawn_named("weft-config", move || {
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
        spawn_named("weft-autosave", move || loop {
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
    /// until the mouse is released: the grid IS reflowed in the `Resized`
    /// handler, but without a redraw the old drawable is stretched to fit the
    /// new window bounds ("content squished together"). AboutToWait fires
    /// when the loop is about to block: a redraw within 100ms of the last
    /// `Resized` re-renders every intermediate live-resize size. Appendix
    /// F-3B: hot zoom windows skip the pump (it would only hit the
    /// suppression gate and self-wake); zoom_wait_policy owns all wakes.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.step_self_zoom();
        self.zoom_wait_policy(event_loop);
        let rt = &self.window_runtime;
        if !rt.zoom_jump_hot() && rt.last_resize_instant.elapsed() < Duration::from_millis(100) {
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
            // v1.14.0 (PLAN_v1140 T6): a geometry change invalidates the
            // sub-row scroll remainder (plan: resize 清零) — the baked row
            // coordinates the px shifts against are about to change.
            pane.grid_scroll_px = 0.0;
            let mut committed = false;
            // T10 P1: the guard is owned (`lock_arc`) so the disjoint
            // `pending_pty_resize` field stays mutable under the lock.
            if let Some(mut terminal) = pane.lock_terminal() {
                committed = commit_pty_resize_result(
                    &mut terminal,
                    &mut pane.pending_pty_resize,
                    (rows, cols),
                    resize_succeeded,
                );
            }
            // FIX-α: stamp on success only — a failed ioctl stays retryable.
            if committed {
                pane.last_resize_commit = Some(std::time::Instant::now());
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
}

/// Spawn a named background thread; a failed spawn is logged, never silent.
/// (v1.12.23 audit batch 1: the eight `std::thread::Builder ... .ok()` sites
/// silently dropped failures — autosave never running means a crash loses all
/// session data.)
fn spawn_named(name: &str, body: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new()
        .name(String::from(name))
        .spawn(body)
    {
        tracing::warn!(thread = name, error = ?e, "background thread spawn failed");
    }
}

// Tests live in the gate-exempt sibling module (repo test-module convention,
// pty/tests.rs precedent) so inline test lines stay out of the production
// budget.
#[cfg(test)]
#[path = "app_runtime/tests.rs"]
mod tests;
