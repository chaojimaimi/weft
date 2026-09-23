//! Window event controller. The ApplicationHandler trait forwards here.

use super::*;

impl App {
    pub(super) fn dispatch_window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                self.request_application_close(event_loop);
            }
            WindowEvent::Resized(physical_size) => {
                // PLAN_zoom (field run): a PROGRAMMATIC zoom (double-click)
                // shows up here as a one-shot size jump with inLiveResize
                // already FALSE (the zoom's internal live-resize window has
                // closed before winit dispatches Resized) -- while a drag
                // streams small steps with inLiveResize true. v4 (field run
                // 2): the zoom ANIMATES -- a stream of small-step Resized
                // events (~100 ms total) with inLiveResize false throughout,
                // so no single frame ever exceeds a jump threshold. Arm on
                // ANY size change; the inLiveResize guard below separates it
                // from drags.
                let size_changed = self.window_runtime.last_resized_physical
                    != Some((physical_size.width, physical_size.height));
                self.window_runtime.last_resized_physical =
                    Some((physical_size.width, physical_size.height));
                // Grid/PTY tracks the renderer's visible terminal content
                // rectangle. The editor box is an overlay, but title/tab
                // chrome is outside that rectangle and must be subtracted.
                if let (Some(renderer), Some(window)) = (&mut self.renderer, &self.window) {
                    // Responsive geometry depends on the new viewport, so
                    // update the renderer before asking for sidebar/chrome
                    // metrics or terminal rows/cols.
                    renderer.resize(window, physical_size);
                    // v0.9 W5: subtract sidebar width when the panel is open so
                    // the grid reflows beside the sidebar (mirrors grid_dims).
                    let chrome_left = if self.panel.open {
                        renderer.sidebar_push_width() as f64
                    } else {
                        0.0
                    };
                    let base_layout =
                        terminal_layout_for_renderer(renderer, physical_size, chrome_left);

                    if base_layout.cols > 0 && base_layout.rows > 0 {
                        // Queue every pane's latest target geometry. The active
                        // pane commits PTY then Grid on this redraw; background
                        // panes coalesce the cascade and commit both together
                        // after it settles. Keeping the pair transactional
                        // prevents old-width output from wrapping in a Grid
                        // that has already adopted the new width.
                        //
                        // v1.3 Batch 6: resize ALL panes per tab according to
                        // their split-tree rects. For single-pane tabs this is
                        // equivalent to the old `resize_terminal_and_queue`.
                        let header_rows = renderer.block_header_rows();
                        let layout_ctx = base_layout.layout_ctx();
                        let content_rect: weft_core::pane_layout::Rect = [
                            base_layout.content.left as f32,
                            base_layout.content.top as f32,
                            base_layout.content.right as f32,
                            base_layout.content.bottom as f32,
                        ];
                        let cell_w = base_layout.cell_width as f32;
                        let cell_h = base_layout.cell_height as f32;
                        for (tab_index, tab) in self.sessions.tabs_mut().iter_mut().enumerate() {
                            if tab.terminal.is_some() {
                                tab.resize_all_panes_for_rect(content_rect, cell_w, cell_h);

                                // `block_scroll_offset` is measured from the
                                // bottom of a width-dependent document. A
                                // larger viewport usually wraps fewer rows and
                                // shows more of them, so an offset that was
                                // valid before maximize can exceed the new
                                // range and make the transcript tail
                                // unreachable. Reconcile against the detached
                                // snapshot now, before SIGWINCH causes the TUI
                                // to repaint asynchronously.
                                let previous = tab.block_scroll();
                                let reconciliation = tab.terminal.as_ref().and_then(|terminal| {
                                    crate::block_component::reconciled_terminal_block_scroll(
                                        terminal,
                                        &layout_ctx,
                                        header_rows,
                                        previous,
                                    )
                                });
                                if let Some((reconciled, total, visible)) = reconciliation {
                                    if previous != reconciled {
                                        info!(
                                            tab = tab_index,
                                            previous,
                                            reconciled,
                                            total,
                                            visible,
                                            "reconciled block scroll during resize"
                                        );
                                        tab.set_block_scroll(reconciled);
                                    }
                                }
                            }
                        }
                        info!(
                            rows = base_layout.rows,
                            cols = base_layout.cols,
                            "all tabs resized (event)"
                        );
                        self.window_runtime.last_resize_instant = std::time::Instant::now();
                        // v1.10.25 Batch 3 (FIX_SELECTION_AND_RESIZE_REMAINING)
                        // DEBUG probe (stage 1/4): the Resized event — anchor
                        // for the ioctl-commit / first-pty-output / first-present
                        // RESIZE_PROBE chain that quantifies the resize blank
                        // interval and the omp repaint latency.
                        tracing::debug!(
                            rows = base_layout.rows,
                            cols = base_layout.cols,
                            "RESIZE_PROBE window_resized",
                        );
                        // `renderer.resize()` above already armed the stage-4
                        // first-present probe.
                        // v1.2-fix: re-clamp tab scroll offset after resize.
                        // The window may have grown/shrunk, changing max_scroll.
                        // Without this, a stale scroll_offset can leave tabs
                        // culled (invisible) after resize.
                        self.clamp_tab_scroll();
                        self.scroll_active_tab_into_view();
                    }
                }
                // v1.11.10 (PLAN_v11110 M-B/D-d): synchronous same-tick draw
                // while live resizing. The bounds change stretches the
                // previous drawable the instant AppKit commits it; a queued
                // RedrawRequested lands a beat later, so
                // presentsWithTransaction never saw the intermediate sizes.
                // Polling here (before the draw) plus the forced path
                // (bypassing the sync-output / route-consume early returns)
                // gives every resize tick an atomic frame — the missing two
                // of Warp's three-piece guarantee. Unconditional
                // set_live_resize: the false reset must not depend on a
                // later RedrawRequested arriving (the last drag event can be
                // a Resized).
                if let (Some(renderer), window) = (&mut self.renderer, self.window.as_ref()) {
                    // PLAN_zoom Z-d: the zoom-sequence marker is armed by
                    // `is_programmatic_resize_jump` outside a live-resize
                    // gesture (arm block below); when hot it extends the
                    // same-tick draw to the programmatic zoom.
                    // HIGH-1 (round 3): arm the zoom channel ONLY outside a
                    // live-resize gesture -- a fast drag coalesces Resized
                    // events with >120 physical-px deltas (2x screen: 60 pt),
                    // which would re-arm it (per-frame CA flush through the
                    // drag plus a 300 ms tail, the cost PLAN §四 froze). The
                    // zoom's own Resized arrives with inLiveResize already
                    // false, so the guard separates the two cleanly. Arm
                    // BEFORE the zoom_jump_hot read: the zoom's own final
                    // Resized then engages the channel same-frame.
                    // v4/Z-f: arm on EVERY non-gesture Resized with a size
                    // change (the zoom animates in small steps). Z-f retires
                    // the per-step forced DRAW below -- the arming survives
                    // as observation/degrade marking only; animation-step
                    // frames are supplied by the displayLayer pull
                    // (paint/zoom_render.rs). Drags stream with inLiveResize
                    // true and are excluded by the same guard (their sync
                    // draw comes from the inLiveResize half).
                    let in_live_resize_now =
                        window.is_some_and(crate::macos_window::window_in_live_resize);
                    if crate::paint::zoom_render::zoom_jump_should_arm(
                        size_changed,
                        in_live_resize_now,
                    ) {
                        self.window_runtime.zoom_jump_until =
                            Some(std::time::Instant::now() + std::time::Duration::from_millis(300));
                        // Appendix F-3C: per-step watch update -- the first
                        // zoom-channel step opens the window, later steps
                        // extend it; the verdict runs at the deterministic
                        // WaitUntil expiry (zoom_wait_policy), not here.
                        crate::paint::zoom_render::note_zoom_step();
                    }
                    // PLAN_zoom_drawable_stall Phase E: set_live_resize is fed
                    // the gesture-only `in_live_resize_now` poll. The retired
                    // `|| zoom_jump_hot` leg (Z-c/Z-d tx-release timing, dead
                    // since the 1.12.10 binding removal) would arm the
                    // serialized-present regime on animated programmatic
                    // resizes (snap/tiling) where nothing presents new-size
                    // frames until the <=300 ms flush.
                    renderer.set_live_resize(in_live_resize_now);
                    // PLAN_zoom Z-f (Appendix E-3): the v4 per-step forced
                    // draw is RETIRED for the zoom channel -- a 5-9 ms
                    // synchronous draw per animation step blocked the
                    // system's zoom animation itself, which stretched the
                    // distortion window (Appendix D-0). Only a real drag
                    // gesture (inLiveResize true at dispatch) keeps the
                    // same-tick synchronous draw.
                    if crate::paint::zoom_render::forced_sync_draw_for_resize(in_live_resize_now) {
                        self.handle_redraw_requested_forced();
                    }
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // Moving between Retina and non-Retina displays changes every
                // physical metric used by the renderer: glyph cells, padding,
                // title/tab chrome and sidebar width. Refresh those first,
                // then recompute Grid/PTY dimensions from the same geometry.
                // Winit follows this event with Resized on macOS; doing the
                // recompute here also covers a retained physical inner size.
                let padding = (
                    self.config_state.config.window.padding_x,
                    self.config_state.config.window.padding_y,
                );
                let changed = self
                    .renderer
                    .as_mut()
                    .is_some_and(|renderer| renderer.update_scale(scale_factor, padding));
                if changed {
                    self.recompute_layout();
                    self.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                // v1.11.6 (PLAN_v1116 M2 step 3): poll the macOS live-resize
                // state right before the redraw. While true the renderer
                // presents through the current Core Animation transaction
                // (presentsWithTransaction), committing the resized layer
                // bounds and the new frame atomically instead of CA stretching
                // the previous drawable. `self.window` / `self.renderer` are
                // disjoint fields, so the borrows coexist. Polled here (the
                // winit dispatch point) rather than at the draw call site in
                // redraw_controller.rs because that file sits at its audited
                // 866-line ceiling; same frame, same result.
                // PLAN_zoom_drawable_stall Phase E: this poll feeds
                // set_live_resize GESTURE-ONLY — the retired Z-d/v4
                // `|| zoom_jump_hot` leg armed the present-mode machinery on
                // animated programmatic resizes (snap/tiling), where nothing
                // would present new-size frames until the <=300 ms flush.
                let in_live_resize = self
                    .window
                    .as_ref()
                    .is_some_and(crate::macos_window::window_in_live_resize);
                let zoom_jump_hot = self.window_runtime.zoom_jump_hot();
                if let Some(renderer) = self.renderer.as_mut() {
                    renderer.set_live_resize(in_live_resize);
                }
                // PLAN_zoom appendix F-3B: `zoom_jump_hot` is only the
                // NECESSARY half of "the pull should supply this frame"; the
                // tightened gate adds the cache-side sufficiency (lever
                // armed, cache populated, stale watermark, no pull in flight
                // -- see pull_can_freshen). While it holds, the displayLayer
                // pull is this frame's ONLY supplier: a full draw here would
                // restamp the watermark with this step's drawable
                // (apply_stash) and dedupe the pull away -- the 1.12.11
                // "every step races the pull" defect (F-2). No draw happens,
                // so the probe records nothing.
                // PLAN_zoom_drawable_stall Phase C: the Appendix I-6
                // main-path deferral is RETIRED. Its premise -- the shrink
                // direction's reflow costs 0.4-1 s/step (field: a 1.0 s
                // stall at cols~130) -- was falsified by the G6 bench
                // (resize_commit_bench G4/G6: 2.0-2.6 ms at 37k lines); the
                // real mid-animation stall was nextDrawable's <=1 s block
                // (PLAN section 0), now bounded by Fix A's present-rate
                // limiter. During a self-zoom the main draw is therefore
                // UNCONDITIONAL: live tracking in BOTH directions through
                // the drag-validated pipeline (the pull stands down for the
                // animation inside redraw_cached_frame). Outside a
                // self-zoom (drags, single programmatic resizes) the F-3B
                // `hot && pull_can_freshen` suppression above is preserved
                // verbatim.
                let self_zoom_active = crate::macos_zoom::zoom_anim_active();
                let pull_supplies_frame = zoom_jump_hot
                    && !self_zoom_active
                    && self.window.as_ref().is_some_and(|window| {
                        let inner = window.inner_size();
                        crate::paint::zoom_render::pull_can_freshen(
                            inner.width as f32,
                            inner.height as f32,
                        )
                    });
                if !pull_supplies_frame {
                    let started = std::time::Instant::now();
                    self.handle_redraw_requested();
                    self.performance_probe.record_redraw(started.elapsed());
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                // v1.11.4 (PLAN_v1114 §2.2, L2 pipe): winit 0.30 delivers
                // Pressed (first), Pressed+repeat (macOS hold-to-repeat) and
                // Released. The kind flows to the InputHandler so a kitty
                // event-types (0b10) app receives `:N` sub-segments — the
                // handling below stays behavior-identical at flags=0 (the
                // encoder ignores the kind; Releases were dropped before).
                let kind = match event.state {
                    winit::event::ElementState::Pressed if event.repeat => {
                        weft_core::input::KittyEventKind::Repeat
                    }
                    winit::event::ElementState::Pressed => weft_core::input::KittyEventKind::Press,
                    winit::event::ElementState::Released => {
                        weft_core::input::KittyEventKind::Release
                    }
                };
                if let PhysicalKey::Code(key_code) = event.physical_key {
                    // `event.text` already reflects Shift (and the keymap),
                    // e.g. Shift+A -> "A", Shift+1 -> "!". The editor uses it
                    // so typed commands keep their case / shifted symbols.
                    self.handle_key_event(
                        kind,
                        key_code,
                        self.interaction.mods,
                        event.text.as_deref(),
                    );
                }
            }
            WindowEvent::ModifiersChanged(new_mods) => {
                self.interaction.mods = new_mods;
            }
            WindowEvent::MouseInput { state, button, .. } => match state {
                winit::event::ElementState::Pressed => {
                    let modal_open = self.palette.open
                        || self.settings.open
                        || self.interaction.context_menu.is_some();
                    let capture_active = self.interaction.modal_mouse_capture.is_active();
                    if modal_open || capture_active {
                        self.interaction
                            .modal_mouse_capture
                            .capture_press(true, button);
                    } else if let Some(session) = self.sessions.tab(self.sessions.active_idx()) {
                        self.interaction.modal_mouse_capture.capture_terminal_press(
                            button,
                            session.session_id,
                            false,
                        );
                    }
                    // A newly modal-owned press is delivered to that modal.
                    // Extra presses during an already captured gesture stay
                    // captured without falling through to the terminal.
                    if modal_open || !capture_active {
                        self.handle_mouse_press(
                            self.interaction.last_mouse_x,
                            self.interaction.last_mouse_y,
                            button,
                        );
                    }
                    // A terminal-routed press can itself open ContextMenu.
                    // Record that ownership after the handler as well: the
                    // menu consumed the press without emitting PTY bytes, so
                    // its later release belongs to the same modal gesture.
                    self.interaction.modal_mouse_capture.capture_press(
                        self.palette.open
                            || self.settings.open
                            || self.interaction.context_menu.is_some(),
                        button,
                    );
                }
                winit::event::ElementState::Released => {
                    match self.interaction.modal_mouse_capture.consume_release(button) {
                        Some(crate::input_router::MouseGestureOwner::Modal)
                        | Some(crate::input_router::MouseGestureOwner::Suppressed) => {
                            // v1.11.13: a press that fell through to the tab
                            // bar while a modal (palette/settings) was open
                            // may have set `tab_drag`; its release is consumed
                            // here, so cancel the gesture or it would pin the
                            // ghost pill and swallow later CursorMoved events.
                            self.cancel_tab_drag();
                        }
                        Some(crate::input_router::MouseGestureOwner::TerminalSession(
                            session_id,
                        )) => {
                            self.handle_mouse_release(
                                self.interaction.last_mouse_x,
                                self.interaction.last_mouse_y,
                                button,
                                Some(session_id),
                                true,
                            );
                        }
                        Some(crate::input_router::MouseGestureOwner::LocalSession(session_id)) => {
                            self.handle_mouse_release(
                                self.interaction.last_mouse_x,
                                self.interaction.last_mouse_y,
                                button,
                                Some(session_id),
                                false,
                            );
                        }
                        None if crate::input_router::route_modal_pointer(
                            self.palette.open,
                            self.settings.open,
                            self.interaction.context_menu.is_some(),
                            self.interaction.modal_mouse_capture.is_active(),
                            !self.sessions.is_empty(),
                        ) == crate::input_router::SessionInputRoute::Dispatch =>
                        {
                            self.handle_mouse_release(
                                self.interaction.last_mouse_x,
                                self.interaction.last_mouse_y,
                                button,
                                None,
                                false,
                            );
                        }
                        None => {}
                    }
                }
            },
            WindowEvent::CursorMoved { position, .. } => {
                self.interaction.last_mouse_x = position.x;
                self.interaction.last_mouse_y = position.y;
                // v1.11: tab drag-to-reorder takes priority over all other
                // pointer routing. Must run before the modal/terminal routing
                // because a tab press switches tabs, which changes the active
                // session — the normal routing would then send CursorMoved to
                // the wrong session and skip handle_mouse_move entirely.
                if self.interaction.tab_drag.is_some() {
                    tracing::debug!(
                        "TAB_DRAG_DIAG: CursorMoved at ({}, {}), tab_drag is some, calling handle_tab_drag_move",
                        position.x, position.y
                    );
                    self.handle_tab_drag_move(position.x, position.y);
                    return;
                }
                // v1.10.34: context menu hover — update the highlighted item
                // as the pointer slides over the menu. The menu is modal
                // (route_modal_pointer returns Consume below), so this is the
                // only pointer handling that runs while it is open.
                if self.interaction.context_menu.is_some() {
                    self.update_context_menu_hover(position.x, position.y);
                    return;
                }
                if crate::input_router::route_modal_pointer(
                    self.palette.open,
                    self.settings.open,
                    self.interaction.context_menu.is_some(),
                    self.interaction.modal_mouse_capture.is_active(),
                    !self.sessions.is_empty(),
                ) == crate::input_router::SessionInputRoute::Dispatch
                {
                    let active_session = self
                        .sessions
                        .tab(self.sessions.active_idx())
                        .map(|tab| tab.session_id);
                    let owner = self
                        .interaction
                        .modal_mouse_capture
                        .terminal_move_owner()
                        .map(|(_, owner)| owner);
                    match crate::input_router::route_owned_pointer_move(active_session, owner) {
                        crate::input_router::OwnedPointerMoveRoute::TerminalOwner(_) => {
                            if self.terminal_content_contains(position.x, position.y) {
                                let pos = self.pixel_to_grid(position.x, position.y);
                                self.send_mouse_event(MouseButton::Left, MouseAction::Move, pos);
                            }
                        }
                        crate::input_router::OwnedPointerMoveRoute::Suppress => {}
                        crate::input_router::OwnedPointerMoveRoute::ActiveSession => {
                            self.handle_mouse_move(position.x, position.y);
                        }
                    }
                }
            }
            WindowEvent::CursorLeft { .. } => {
                // v1.11.13: a drag that ends with the pointer outside the
                // window is cancelled (not committed) — the release event may
                // never arrive, and a stale tab_drag would pin the ghost pill
                // to the screen edge and swallow every later CursorMoved.
                self.cancel_tab_drag();
                if self.interaction.scrollbar_hovered && self.interaction.scrollbar_drag.is_none() {
                    self.interaction.scrollbar_hovered = false;
                    if let Some(window) = &self.window {
                        window.set_cursor(winit::window::CursorIcon::Default);
                    }
                    self.request_redraw();
                }
                if self.tab_bar.hovered_tab.is_some()
                    || self.tab_bar.plus_hovered
                    || self.tab_bar.arrow_left_hovered
                    || self.tab_bar.arrow_right_hovered
                {
                    self.tab_bar.clear_hover();
                    self.request_redraw();
                }
            }
            WindowEvent::MouseWheel { delta, phase, .. } => {
                if crate::input_router::route_modal_pointer(
                    self.palette.open,
                    self.settings.open,
                    self.interaction.context_menu.is_some(),
                    self.interaction.modal_mouse_capture.is_active(),
                    !self.sessions.is_empty(),
                ) == crate::input_router::SessionInputRoute::Dispatch
                {
                    self.handle_scroll(
                        delta,
                        phase,
                        self.interaction.last_mouse_x,
                        self.interaction.last_mouse_y,
                    );
                }
            }
            WindowEvent::Ime(ime_event) => self.handle_ime_event(ime_event),
            WindowEvent::Focused(focused) => {
                // v1.11.5 (PLAN_v1115 §M2): focus drives the notification
                // gate — long commands notify only while the window is out
                // of focus. Initial state is `true` (see `App::new`).
                self.window_focused = focused;
                // v1.11.15 (FIX C, PLAN_v11115_EXIT_RACE_MOUSE_LEAK §3):
                // winit only delivers ModifiersChanged while this window is
                // the event target — switching apps mid-press can leave
                // stale mods behind (the incident's Cb=48 Move+CONTROL
                // hover report). Reset to the same default InteractionState
                // constructs with (app_state.rs) on BOTH the gain- and
                // lose-focus branches; the failure direction is dropping
                // modifier bits, which is safe.
                self.interaction.mods = winit::event::Modifiers::default();
                // Reset blink timer on focus change
                if focused {
                    self.window_runtime.cursor_blink_on = true;
                    self.window_runtime.cursor_blink_time = std::time::Instant::now();
                } else {
                    // v1.11.13: switching apps mid-drag (Cmd+Tab) can drop the
                    // mouse release — cancel the drag so the ghost doesn't
                    // stay pinned and future moves aren't swallowed.
                    self.cancel_tab_drag();
                    self.interaction.modal_mouse_capture.suspend_active();
                    self.reset_ime_context("window focus lost");
                }
            }
            _ => {}
        }
        // v1.0 H4: check should_exit flag (set by close_tab on last tab).
        // v1.6.3 review M8: mark clean shutdown here too — the should_exit
        // path fires when the last tab is closed or the shell exits. Without
        // this, the next launch shows a false recovery prompt. (The same flag
        // is also checked in dispatch_app_event, but a WindowEvent may arrive
        // first; calling mark_clean_shutdown twice is harmless.)
        if self.should_exit {
            self.recovery.mark_clean_shutdown();
            event_loop.exit();
        }
    }

    /// PLAN_zoom appendix F-3B: deterministic zoom-window expiry handling,
    /// driven from `about_to_wait`. While the programmatic-zoom window is
    /// hot, re-arm a short `WaitUntil` wake (expiry + 2ms) so the
    /// post-expiry flush does not depend on PTY output; at the first tick
    /// after it lapses, run the one-shot flush: the zoom-window verdict
    /// (F-3C) plus a redraw request -- by then the suppression gate no
    /// longer holds (the pull stamped the watermark to the final drawable),
    /// so the request lands as the normal full draw at the final size. The
    /// expiry needs no PTY traffic and re-runs after a modal suspension
    /// (runModal) because about_to_wait always gets a catch-up tick.
    /// Appendix H: the zoom animation batches winit `Resized` delivery
    /// until after its last `setFrameSize` callback (field: 41 events in
    /// one 0.4ms burst), so the reflow pipeline cannot track the animation
    /// from the event path alone. The IMP pings an `AppEvent::Wake` per
    /// callback; this synthesizes the standard resize dispatch whenever the
    /// live window size has moved past the last applied one. Size-deduped:
    /// drags and ordinary worker wakes land here as no-ops.
    pub(crate) fn reflow_if_live_size_moved(&mut self, event_loop: &ActiveEventLoop) {
        let live_size = self.window.as_ref().map(|window| window.inner_size());
        if let Some(size) = live_size {
            if self.window_runtime.last_resized_physical != Some((size.width, size.height)) {
                self.dispatch_window_event(event_loop, WindowEvent::Resized(size));
            }
        }
    }

    pub(crate) fn zoom_wait_policy(&mut self, event_loop: &ActiveEventLoop) {
        let hot = self.window_runtime.zoom_jump_hot();
        let pending = self.window_runtime.zoom_flush_pending;
        match crate::paint::zoom_render::zoom_flush_action(hot, pending) {
            crate::paint::zoom_render::ZoomFlushAction::Arm => {
                // Hot implies zoom_jump_until is Some (the predicate is
                // `now < until`); the guard keeps the invariant explicit.
                if let Some(until) = self.window_runtime.zoom_jump_until {
                    event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(
                        until + std::time::Duration::from_millis(2),
                    ));
                }
                self.window_runtime.zoom_flush_pending = true;
            }
            crate::paint::zoom_render::ZoomFlushAction::Flush => {
                // H1 (review round 1): restore the loop's resting flow. The
                // WaitUntil deadline is past by construction here; leaving
                // it armed makes the macOS runloop timer fire immediately
                // on every pass (EventLoopWaker::start_at -> start()), a
                // permanent post-zoom busy loop. Arm re-covers it on the
                // next hot window, so resetting here is race-free.
                event_loop.set_control_flow(winit::event_loop::ControlFlow::Wait);
                self.window_runtime.zoom_flush_pending = false;
                crate::paint::zoom_render::zoom_window_finished();
                self.request_redraw();
            }
            crate::paint::zoom_render::ZoomFlushAction::None => {}
        }
    }

    /// PLAN_zoom appendix I: advance the self-managed zoom animation by one
    /// event-loop turn. Called from `about_to_wait` every pass; idle cost is
    /// the zero-allocation `zoom_anim_peek` (Copy snapshot or None).
    ///
    /// Each step applies the eased intermediate frame through the SAME
    /// winit setters a user drag exercises, so the whole pipeline runs live:
    /// `request_inner_size` -> injected `setFrameSize:` IMP (drawable sync +
    /// inline pull) -> Resized dispatched same-pass -> tab/PTY reflow -> full
    /// draw. That is the drag semantics, which is why the animation tracks
    /// live instead of freezing like the system zoom does (I-1: AppKit's own
    /// animation suspends winit dispatch entirely).
    pub(crate) fn step_self_zoom(&mut self) {
        // Appendix I acceptance rig: fires one `zoom:` ~2s after startup
        // when WEFT_SELF_ZOOM_TEST=1 (inert otherwise).
        if let Some(window) = self.window.as_ref() {
            crate::macos_zoom::self_zoom_test_tick(window);
        }
        let Some(anim) = crate::macos_zoom::zoom_anim_peek() else {
            return;
        };
        let Some(window) = self.window.as_ref() else {
            crate::macos_zoom::zoom_anim_cancel();
            return;
        };
        // Cancel semantics (review P1-3): a live resize gesture owns the
        // geometry; the animation must not fight the user's drag.
        if crate::macos_window::window_in_live_resize(window) {
            crate::macos_zoom::zoom_anim_cancel();
            tracing::debug!("self-zoom cancelled: live resize in progress");
            return;
        }
        // Pacing floor (PLAN_zoom_drawable_stall B): refuse to apply a step
        // sooner than STEP_MIN_INTERVAL after the previous one. The refusal
        // MUST keep the sustain chain alive — request_redraw here — or the
        // loop would sleep until the 300 ms hot-window expiry (the exact
        // field-freeze shape this plan fixes). Each refused pass costs one
        // anim peek + one flag check; the due pass then folds the skipped
        // interval in (time-driven interpolation).
        if !crate::macos_zoom::zoom_step_due(anim.last_step.elapsed()) {
            self.request_redraw();
            return;
        }
        let progress = (anim.start.elapsed().as_secs_f64() / anim.duration.as_secs_f64()).min(1.0);
        let eased = crate::macos_zoom::ease_in_out_quad(progress);
        let x = crate::macos_zoom::lerp(anim.origin_start.0, anim.origin_target.0, eased);
        let y = crate::macos_zoom::lerp(anim.origin_start.1, anim.origin_target.1, eased);
        let w = crate::macos_zoom::lerp(anim.size_start.0, anim.size_target.0, eased);
        let h = crate::macos_zoom::lerp(anim.size_start.1, anim.size_target.1, eased);
        // Cancel check (review P1-3): actual geometry vs the last applied
        // step. Window dragging / Mission Control mid-animation must abort
        // the interpolation; the 1pt tolerance (same epsilon as the zoomed
        // test) keeps winit's physical read-back rounding from false-tripping.
        let scale = window.scale_factor();
        let drifted = match window.outer_position() {
            Ok(position) => {
                let position = position.to_logical::<f64>(scale);
                let size = window.outer_size().to_logical::<f64>(scale);
                let eps = crate::macos_zoom::ZOOM_EPSILON_PT;
                (position.x - anim.last_applied.0).abs() > eps
                    || (position.y - anim.last_applied.1).abs() > eps
                    || (size.width - anim.last_applied.2).abs() > eps
                    || (size.height - anim.last_applied.3).abs() > eps
            }
            Err(_) => true,
        };
        if drifted {
            crate::macos_zoom::zoom_anim_cancel();
            tracing::warn!("self-zoom cancelled: window moved externally mid-animation");
            return;
        }
        // Setter order (review P1-2, pinned): inner size FIRST, outer
        // position SECOND -- under either anchor interpretation the step
        // lands at exactly (x, y, w, h), and winit's position flip reads the
        // already-updated frame size.
        let _ = window.request_inner_size(winit::dpi::LogicalSize::new(w, h));
        window.set_outer_position(winit::dpi::LogicalPosition::new(x, y));
        crate::macos_zoom::zoom_anim_advance((x, y, w, h));
        crate::macos_zoom::zoom_anim_mark_stepped();
        if progress >= 1.0 {
            let steps = crate::macos_zoom::zoom_anim_finish().map_or(0, |finished| finished.steps);
            tracing::info!(steps, "self-zoom complete");
        } else {
            self.request_redraw();
        }
    }
}
