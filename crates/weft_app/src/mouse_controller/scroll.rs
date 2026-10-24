//! Scroll-wheel dispatch and drag-selection autoscroll for `App`, moved
//! verbatim out of `mouse_controller.rs` (v1.13.8 S3 zero-behavior
//! file-budget split; `impl App` cross-file block per the
//! mouse_press/settings_geometry submodule precedent). Method visibility
//! follows the access paths: the event-loop / pump callers sit outside
//! this module (`pub(crate)`), the intra-controller ones don't.

use crate::selection::{down_autoscroll_threshold, AutoscrollDir};
use crate::*;

impl App {
    /// Handle scroll wheel.
    pub(crate) fn handle_scroll(
        &mut self,
        delta: winit::event::MouseScrollDelta,
        phase: winit::event::TouchPhase,
        x: f64,
        y: f64,
    ) {
        // v1.10.23 (FIX_LIVE_BLOCK_SCROLL_PERF): the per-event
        // `process_messages` (v0.9 alt-screen-entry fix; the retired
        // main-thread `pump_pty` half is now the parse worker) was
        // removed — the redraw path (`handle_redraw_requested`) pumps the
        // PTY on every frame, so a wheel event sees terminal state at most
        // one frame stale (a single event during the CSI ?1049h transition
        // can still be misrouted, then self-corrects next frame). The tab /
        // panel / block-view branches below don't depend on freshly parsed
        // PTY state; no branch was found that requires the in-place drain.

        // v1.2: If the scroll event is over the tab bar, adjust the tab bar
        // horizontal scroll offset instead of scrolling the terminal. This
        // lets the user navigate overflowed tabs via trackpad / wheel.
        if let Some(renderer) = &self.renderer {
            let bar_h = renderer.tab_bar_height();
            if y as f32 <= bar_h && self.sessions.len() > 1 {
                let cw = renderer.cell_width() as f32;
                let scroll_step = cw * 15.0; // scroll ~1 tab width per notch
                let delta_px = match delta {
                    winit::event::MouseScrollDelta::LineDelta(_, v) => {
                        // Vertical wheel in tab bar → horizontal scroll.
                        -v * scroll_step
                    }
                    winit::event::MouseScrollDelta::PixelDelta(pos) => {
                        // Trackpad: use deltaX if significant, else convert deltaY.
                        if pos.x.abs() > pos.y.abs() {
                            -(pos.x as f32)
                        } else {
                            -(pos.y as f32) * 0.5
                        }
                    }
                };
                self.tab_bar.scroll_offset = (self.tab_bar.scroll_offset + delta_px).max(0.0);
                self.clamp_tab_scroll();
                self.request_redraw();
                return;
            }
        }

        // F3-4: When the scroll event is over the history sidebar, adjust the
        // panel's block-level scroll offset instead of scrolling the terminal.
        // v1.13.18 P2: the panel list is newest-first (`panel_display`), and
        // the invariant is content-follows-finger — the SAME gesture that
        // moves the main view's content down moves the panel's rows down.
        // Wheel-up / downward content → the viewport walks toward NEWER rows
        // → scroll_offset decreases; wheel-down → increases (toward older).
        let over_panel = self.panel.open
            && self
                .renderer
                .as_ref()
                .is_some_and(|r| x < r.sidebar_width() as f64);
        if over_panel {
            // v1.12.27a (P1-03) + v1.13.18 (P2): line count, direction and
            // sign all come from scroll_input's panel_scroll_step (pure,
            // unit-tested). The tab-bar branch above keeps its
            // horizontal-semantics match, and the PixelDelta type guard
            // further below stays — both are out of scope by plan.
            let step = crate::scroll_input::panel_scroll_step(delta);
            if step != 0 {
                // Floor at 0 here (newest end); the older end is bounded by
                // the existing clamp_panel_scroll.
                self.panel.scroll_offset =
                    (self.panel.scroll_offset as isize + step).max(0) as usize;
                self.clamp_panel_scroll();
                self.clamp_panel_selection();
                self.request_redraw();
            }
            return;
        }

        // Precise macOS trackpads emit many sub-row PixelDelta events for one
        // gesture. Accumulate them by physical cell height; treating every
        // non-zero event as a wheel notch makes Vim jump straight to an edge.
        let cell_height = self
            .renderer
            .as_ref()
            .map_or(40.0, |renderer| renderer.cell_height() as f64);
        let block_view = self
            .sessions
            .active()
            .and_then(|tab| tab.with_terminal(|t| t.show_block_view()))
            .unwrap_or(false);
        if block_view {
            if let winit::event::MouseScrollDelta::PixelDelta(pos) = delta {
                if phase == winit::event::TouchPhase::Cancelled {
                    return;
                }
                // v1.10.21: during an alt-screen history peek a plain
                // precise scroll means "interact with the app" — one flick
                // drops the peek and returns to the live TUI (Warp parity;
                // see FIX_ALT_PEEK_WARP_ALIGNMENT). Shift+scroll falls
                // through to the fractional block scroll below.
                let peeking = self
                    .sessions
                    .active()
                    .and_then(|tab| tab.with_terminal(|t| t.is_alt_screen_history_peek()))
                    .unwrap_or(false);
                if peeking && !self.interaction.mods.state().shift_key() {
                    // snap_to_bottom clears the flag and arms the gate's
                    // re-entry lockout (exit edge detected inside).
                    if let Some(tab) = self.sessions.active_mut() {
                        tab.snap_to_bottom();
                    }
                    self.request_redraw();
                    return;
                }
                // v1.10.19: A1-fix parity with scroll_local_view — compute
                // max_scroll LIVE from the layout cache instead of the
                // per-frame cached value. The cached metrics are None during
                // grid-mode execution (primary-screen TUI), which clamped to
                // 0 and kicked the PixelDelta fast path straight back to
                // FollowBottom — the trackpad's first flick then discarded
                // the scroll-up the user had just started.
                let max_scroll = self
                    .renderer
                    .as_ref()
                    .and_then(|renderer| {
                        let tab = self.sessions.active()?;
                        let terminal = tab.lock_terminal()?;
                        let (_total, _visible, max) =
                            renderer.block_scroll_metrics(&terminal, tab.pane_session_id);
                        Some(max)
                    })
                    .unwrap_or(0);
                if let Some(tab) = self.sessions.active_mut() {
                    tab.scroll_block_fractional((pos.y / cell_height.max(1.0)) as f32, max_scroll);
                }
                self.request_redraw();
                return;
            }
        }
        let rows = crate::scroll_input::terminal_scroll_rows(
            delta,
            phase,
            cell_height,
            &mut self.interaction.precise_scroll,
        );
        tracing::debug!(
            ?delta,
            ?phase,
            rows,
            residual = self.interaction.precise_scroll.residual_pixels(),
            "terminal scroll quantized"
        );
        if rows == 0 {
            return;
        }
        let up = rows > 0;
        let lines = rows.unsigned_abs() as usize;

        // Short-lived LOCK (T10 accessors) to read the mode flags up-front;
        // the encode path and pane-field writes below run lock-free.
        let tui_starting = self
            .sessions
            .active_mut()
            .is_some_and(|tab| tab.tui_scroll_window_active());
        let (mouse_protocol_active, alt_screen_active, app_cursor_keys, mouse_protocol, sgr_mouse) = {
            let Some(t) = self.sessions.active().and_then(|tab| tab.lock_terminal()) else {
                return;
            };
            // v1.0 fix: capture mouse_protocol here and sync it into the
            // InputHandler below (after this immutable borrow ends). Without
            // this sync, InputHandler.mouse_protocol stays `Off` forever (its
            // setters are test-only), so `encode_scroll`/`encode_mouse` hit
            // their `if Off { return None }` guards and silently drop every
            // mouse event — mouse-aware apps (vim `set mouse=a`, tmux, htop)
            // never receive wheel/click input. Mirrors the `app_cursor_keys`
            // sync in handle_key_event.
            let mp = t.mouse_protocol();
            let sgr = t.sgr_mouse();
            (
                t.accepts_mouse_reporting_input(),
                t.is_alt_screen_active(),
                t.app_cursor_keys(),
                mp,
                sgr,
            )
        };
        // Apply all terminal-controlled input modes before encoding this
        // gesture. Vim/less commonly enable DECCKM before the first wheel;
        // using a stale default would emit CSI arrows instead of SS3 arrows.
        if let Some(tab) = self.sessions.active_mut() {
            tab.input_handler.app_cursor_keys = app_cursor_keys;
            tab.input_handler.mouse_protocol = mouse_protocol;
            tab.input_handler.sgr_mouse = sgr_mouse;
        }

        // Shift disables mouse routing for this gesture on the PRIMARY screen so
        // the user can browse terminal history even while a primary-screen TUI
        // captures the mouse. (Alt-screen Shift is handled in the alt block
        // below as an arrow-key escape hatch for pagers like less/man.)
        if self.interaction.mods.state().shift_key() && !alt_screen_active {
            self.scroll_local_view(rows);
            return;
        }

        // Check if mouse protocol is active — forward scroll to PTY
        if mouse_protocol_active {
            // v1.11.15 (FIX A): the reader already saw this session's
            // mouse-disable — the wheel must not land in a shell that is
            // back in cooked mode (the teardown leak window).
            if self
                .sessions
                .active()
                .is_some_and(|tab| tab.mouse_suppressed())
            {
                return;
            }
            if !self.terminal_content_contains(x, y) {
                return;
            }
            let pos = self.pixel_to_grid(x, y);
            let mut m = Modifiers::empty();
            if self.interaction.mods.state().shift_key() {
                m |= Modifiers::SHIFT;
            }
            if self.interaction.mods.state().alt_key() {
                m |= Modifiers::ALT;
            }
            if self.interaction.mods.state().control_key() {
                m |= Modifiers::CONTROL;
            }
            if let Some(bytes) = self
                .sessions
                .active_mut()
                .and_then(|tab| tab.input_handler.encode_scroll(up, pos.col, pos.row, m))
            {
                let mut batch = Vec::with_capacity(bytes.len() * lines);
                for _ in 0..lines {
                    batch.extend_from_slice(&bytes);
                }
                if let Some(Err(e)) = self
                    .sessions
                    .active_mut()
                    .map(|tab| tab.write_user_input(&batch))
                {
                    tracing::debug!(?e, "tui wheel write failed"); // v1.12.23 batch 1: was silent `let _ =`
                }
            }
            return;
        }

        // A TUI launched from the editor can receive its first wheel gesture
        // before the PTY reader has delivered/parsing has reached CSI ?1049h.
        // Keep the gesture per-tab for one 50ms protocol grace period. It is
        // encoded for the TUI if alternate screen arrives, otherwise it falls
        // back to ordinary local viewport scrolling (so normal commands do not
        // lose their first gesture during the two-second launch window).
        if tui_starting && !alt_screen_active {
            // v1.11.15 (FIX A): same suppression gate as the reporting branch
            // — do not even queue a gesture that would be replayed into a
            // suppressed session (the parked resolution checks the flag too,
            // but not queueing keeps the wake timer off entirely).
            if self
                .sessions
                .active()
                .is_some_and(|tab| tab.mouse_suppressed())
            {
                return;
            }
            if !self.terminal_content_contains(x, y) {
                return;
            }
            let pos = self.pixel_to_grid(x, y);
            let mut m = Modifiers::empty();
            if self.interaction.mods.state().shift_key() {
                m |= Modifiers::SHIFT;
            }
            if self.interaction.mods.state().alt_key() {
                m |= Modifiers::ALT;
            }
            if self.interaction.mods.state().control_key() {
                m |= Modifiers::CONTROL;
            }
            let queued = self
                .sessions
                .active_mut()
                .map(|tab| tab.queue_tui_scroll(rows, pos.col, pos.row, m))
                .unwrap_or(false);
            if queued {
                if let Some(delay) = self
                    .sessions
                    .active_mut()
                    .and_then(|tab| tab.take_tui_scroll_wake_delay())
                {
                    let proxy = self.proxy.clone();
                    std::thread::Builder::new()
                        .name(String::from("weft-mouse"))
                        .spawn(move || {
                            std::thread::sleep(delay);
                            let _ = proxy.send_event(AppEvent::Wake);
                        })
                        .ok();
                }
                return;
            }
        }

        // Alt-screen apps (less, vim, man, omp, …) don't use mouse protocol
        // but own the screen. v1.10.21: wheel routing lives in
        // `alt_wheel_controller` (pure table in `alt_peek::route`, Warp
        // parity — see FIX_ALT_PEEK_WARP_ALIGNMENT): plain wheels forward
        // arrows to the TUI, Shift+wheel-up enters the history peek, a
        // plain wheel inside the peek returns to the live TUI instantly.
        // Mouse-reporting apps and the primary screen never reach here.
        if alt_screen_active {
            self.handle_alt_screen_wheel(rows, lines, up);
            return;
        }

        // Otherwise, scroll the terminal viewport.
        // v1.14.0 (PLAN_v1140 T6): grid (纯终端模式) trackpad gestures fold
        // into the sub-row px accumulator — whole rows feed the existing
        // line path and the remainder renders as a sub-row offset. LineDelta
        // wheels keep whole rows and clear the remainder (below). Block view
        // PixelDelta returned at the fractional branch above; alt screen and
        // mouse-protocol gestures returned even earlier (滚轮转发键盘,
        // plan 边界 — the accumulator never runs for them).
        if !block_view {
            if let winit::event::MouseScrollDelta::PixelDelta(pos) = delta {
                // rust-reviewer P3: mirror the block branch's touch
                // cancellation guard — a cancelled gesture must not scroll.
                if phase == winit::event::TouchPhase::Cancelled {
                    return;
                }
                self.scroll_grid_view_pixel(pos.y as f32);
                return;
            }
            // LineDelta (notched wheel): whole rows only — clear any px
            // remainder so the next trackpad gesture starts clean.
            if let Some(tab) = self.sessions.active_mut() {
                tab.active_mut().grid_scroll_px = 0.0;
            }
        }
        self.scroll_local_view(rows);
    }

    /// v1.14.0 (PLAN_v1140 T6): grid (纯终端模式) PixelDelta path — fold the
    /// gesture into whole rows + a sub-row px remainder, with the SAME
    /// scroll-up preamble `scroll_local_view` runs before its grid branch
    /// (rust-reviewer P1-2): the selection migration / primary-history entry
    /// decide whether the rows belong to the BLOCK path (a primary-screen
    /// TUI or a running command enters the history view — applying grid rows
    /// there would silently drop the user's scroll, the same class of bug
    /// the A1 live-max-scroll fix addressed) and keep the wheel and trackpad
    /// paths behaviorally identical. The accumulator itself stays pure.
    pub(crate) fn scroll_grid_view_pixel(&mut self, delta_px: f32) {
        let cell_h = self
            .renderer
            .as_ref()
            .map_or(40.0, |renderer| renderer.cell_height() as f32)
            .max(1.0);
        // 1) Read + accumulate (short pane borrow; no grid mutation yet).
        let (rows, remain) = {
            let Some(tab) = self.sessions.active_mut() else {
                return;
            };
            let pane = tab.active_mut();
            let current = pane.grid_scroll_px;
            let (offset, len) = pane
                .with_terminal(|t| {
                    let grid = t.grid();
                    (grid.scroll_offset(), grid.scrollback.len())
                })
                .unwrap_or((0, 0));
            crate::tab::accumulate_grid_scroll_px(current, delta_px, cell_h, offset, len)
        };
        if rows == 0 {
            // Pure sub-row motion — no preamble, no line scroll.
            if let Some(tab) = self.sessions.active_mut() {
                tab.active_mut().grid_scroll_px = remain;
            }
            self.request_redraw();
            return;
        }
        // 2) rows != 0 → the scroll_local_view up-arm preamble, verbatim.
        let up = rows > 0;
        if up {
            let (selecting, has_grid_selection, tui_active) = {
                match self.sessions.active() {
                    Some(tab) => (
                        tab.selection_handler.selecting,
                        tab.selection_handler.selection.is_some(),
                        tab.with_terminal(|t| t.primary_screen_app_active())
                            .unwrap_or(false),
                    ),
                    None => (false, false, false),
                }
            };
            if selecting && has_grid_selection && tui_active {
                if !self.migrate_grid_selection_to_primary_history() {
                    self.request_redraw();
                    return;
                }
            } else {
                let entered = self
                    .sessions
                    .active_mut()
                    .map(|tab| tab.enter_primary_history_if_active())
                    .unwrap_or(false);
                tracing::debug!(
                    entered,
                    rows,
                    app_active = tui_active,
                    "SCROLL_DIAG: pixel scroll up into primary history"
                );
            }
        }
        // 3) Route by the post-preamble view — the history entry above may
        // have flipped the active pane into block view.
        let block_view = self
            .sessions
            .active()
            .and_then(|tab| tab.with_terminal(|t| t.show_block_view()))
            .unwrap_or(false);
        if block_view {
            // A1: compute max_scroll LIVE from the layout cache (the
            // per-frame cached value is None during grid-mode execution —
            // scroll_local_view's block-branch twin).
            let max_scroll = self
                .renderer
                .as_ref()
                .and_then(|renderer| {
                    let tab = self.sessions.active()?;
                    let terminal = tab.lock_terminal()?;
                    let (_total, _visible, max) =
                        renderer.block_scroll_metrics(&terminal, tab.pane_session_id);
                    Some(max)
                })
                .unwrap_or(0);
            if up {
                if let Some(tab) = self.sessions.active_mut() {
                    tab.scroll_up_by(rows.unsigned_abs() as usize);
                    tab.clamp_block_scroll(max_scroll);
                }
            } else if let Some(tab) = self.sessions.active_mut() {
                tab.scroll_down_by(rows.unsigned_abs() as usize);
            }
            // The block path owns sub-row motion via block_scroll_fraction —
            // the grid px remainder is meaningless while browsing history.
            if let Some(tab) = self.sessions.active_mut() {
                tab.active_mut().grid_scroll_px = 0.0;
            }
            self.request_redraw();
            return;
        }
        // 4) Grid view: keep the remainder, apply the whole rows (the grid
        // branch of scroll_local_view's drift guard, rows != 0 ⇒ the offset
        // moves).
        if let Some(tab) = self.sessions.active_mut() {
            let pane = tab.active_mut();
            pane.grid_scroll_px = remain;
            if pane.selection_handler.selection.is_some() {
                tracing::debug!(
                    "cleared grid selection before pixel viewport scroll (drift guard)"
                );
                pane.selection_handler.clear();
            }
            if let Some(mut terminal) = pane.lock_terminal() {
                let grid = terminal.grid_mut();
                if up {
                    grid.scroll_up_history(rows as usize);
                } else {
                    grid.scroll_down_history((-rows) as usize);
                }
            }
        }
        self.request_redraw();
    }

    /// Apply signed rows to the normal terminal/block viewport. Positive rows
    /// move toward older content; negative rows move back toward the prompt.
    pub(crate) fn scroll_local_view(&mut self, rows: i32) {
        if rows == 0 {
            return;
        }
        let up = rows > 0;
        let lines = rows.unsigned_abs() as usize;
        if up {
            // v1.10.20: a wheel scroll while a grid selection is active must
            // go through the same migration entry point as the drag path —
            // entering the history view with a live grid selection would
            // orphan it (L3). Migration failure degrades: keep the grid
            // selection, do not scroll (would drift its anchor, L2).
            // v1.12.25 (audit 3-B, P1-01): empty-tabs transient reads as
            // "nothing selected" — the gesture degrades to a no-op below.
            let (selecting, has_grid_selection, tui_active) = {
                match self.sessions.active() {
                    Some(tab) => (
                        tab.selection_handler.selecting,
                        tab.selection_handler.selection.is_some(),
                        tab.with_terminal(|t| t.primary_screen_app_active())
                            .unwrap_or(false),
                    ),
                    None => (false, false, false),
                }
            };
            if selecting && has_grid_selection && tui_active {
                if !self.migrate_grid_selection_to_primary_history() {
                    self.request_redraw();
                    return;
                }
            } else {
                let entered = self
                    .sessions
                    .active_mut()
                    .map(|tab| tab.enter_primary_history_if_active())
                    .unwrap_or(false);
                tracing::debug!(
                    entered,
                    rows,
                    app_active = tui_active,
                    "SCROLL_DIAG: scroll up into primary history"
                );
            }
        }
        let block_view = self
            .sessions
            .active()
            .and_then(|tab| tab.with_terminal(|t| t.show_block_view()))
            .unwrap_or(false);
        if block_view {
            // A1: compute max_scroll LIVE from the layout cache. The per-frame
            // cached value is None during grid-mode execution (primary-screen TUI),
            // which would clamp to 0 and immediately exit the primary_history_view
            // just entered above — making scroll-up during a running command a
            // no-op. Live compute uses the retained cache (history is stable while
            // a command runs), so the wheel can actually detach into history.
            let max_scroll = self
                .renderer
                .as_ref()
                .and_then(|renderer| {
                    let tab = self.sessions.active()?;
                    let terminal = tab.lock_terminal()?;
                    let (_total, _visible, max) =
                        renderer.block_scroll_metrics(&terminal, tab.pane_session_id);
                    Some(max)
                })
                .unwrap_or(0);
            tracing::debug!(
                up,
                lines,
                max_scroll,
                current = self.sessions.active().map_or(0, |tab| tab.block_scroll()),
                "SCROLL_DIAG: block-view scroll"
            );
            if up {
                if let Some(tab) = self.sessions.active_mut() {
                    tab.scroll_up_by(lines);
                    tab.clamp_block_scroll(max_scroll);
                }
            } else if let Some(tab) = self.sessions.active_mut() {
                tab.scroll_down_by(lines);
            }
        } else {
            // Grid view scroll — needs the terminal lock.
            // v1.10.20 改动 4 (drift guard): a grid selection is
            // viewport-relative; scrolling would silently drift its anchor
            // (L2). No migration path exists here (non-TUI grid), so clear
            // the selection instead of corrupting the copy range. Only when
            // the offset actually moves — offset 0 + scroll-down is a no-op.
            //
            // [P2 TOCTOU 登记·评审裁定本轮不修] probe→clear→scroll is a
            // three-step sequence over two locks; the parse worker can grow
            // the scrollback between the probe lock and the scroll lock, so
            // the "offset will change" verdict can be one batch stale. The
            // failure mode (selection cleared without an actual scroll, or
            // vice versa one batch later) is the same single-batch
            // staleness the pre-worker pump produced; accepted for P2.
            let (had_selection, offset_will_change) = {
                let terminal = self.sessions.active().and_then(|tab| tab.lock_terminal());
                match terminal {
                    Some(t) => {
                        let grid = t.grid();
                        let offset = grid.scroll_offset();
                        let max = grid.scrollback.len();
                        let new = if up {
                            (offset + lines).min(max)
                        } else {
                            offset.saturating_sub(lines)
                        };
                        (
                            self.sessions
                                .active()
                                .is_some_and(|tab| tab.selection_handler.selection.is_some()),
                            new != offset,
                        )
                    }
                    None => (false, false),
                }
            };
            if had_selection && offset_will_change {
                tracing::debug!("cleared grid selection before viewport scroll (drift guard)");
                if let Some(tab) = self.sessions.active_mut() {
                    tab.selection_handler.clear();
                }
            }
            if let Some(mut terminal) = self
                .sessions
                .active_mut()
                .and_then(|tab| tab.lock_terminal())
            {
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

    /// Edge-band overshoot for the drag-selection autoscroll. Returns which
    /// direction (relative to the content band) the pointer overshoots and by
    /// how many pixels — measured from the band threshold (one line outside
    /// the content edge in block view; the top content edge in grid mode).
    /// Shared by the move handler (which only ARMS the 40ms timer) and the
    /// timer pump (which scrolls + extends). `None` inside the band.
    pub(super) fn selection_autoscroll_overshoot(&self, y: f64) -> Option<(AutoscrollDir, f32)> {
        if !self
            .sessions
            .active()
            .is_some_and(|tab| tab.selection_handler.selecting)
        {
            return None;
        }
        if self.block_view_active() {
            let (top, bottom) = self.block_content_vbounds()?;
            let ch = self
                .renderer
                .as_ref()
                .map(|r| r.cell_height() as f32)
                .unwrap_or(20.0);
            // v1.10.26 Batch D (D-4): the Down-band inner shift applies ONLY
            // in a snapshot/history view (primary_history_view or an active
            // alt peek), where no prompt chrome sits below the content. A
            // regular block view keeps the `bottom + ch` band — the prompt
            // chrome below makes it physically reachable even when the
            // geometry reads as full-bleed (a wide border/table fills the
            // pane), which otherwise mis-triggered a Down scroll from the
            // last visible line (v1.10.25 ML2).
            let snapshot_context = self
                .sessions
                .active()
                .and_then(|tab| {
                    tab.with_terminal(|terminal| {
                        terminal.primary_history_view() || terminal.is_alt_screen_history_peek()
                    })
                })
                .unwrap_or(false);
            let down_threshold = down_autoscroll_threshold(bottom, ch, snapshot_context);
            if (y as f32) < top - ch {
                Some((AutoscrollDir::Up, (top - ch) - y as f32))
            } else if (y as f32) > down_threshold {
                Some((AutoscrollDir::Down, y as f32 - down_threshold))
            } else {
                None
            }
        } else {
            // Grid mode (primary-screen TUI): only the top edge can trigger —
            // a drag held past it migrates into the primary history view.
            let tui_active = self
                .sessions
                .active()
                .and_then(|tab| tab.with_terminal(|t| t.primary_screen_app_active()))
                .unwrap_or(false);
            if !tui_active {
                return None;
            }
            let (top, _bottom) = self.grid_content_vbounds()?;
            let ch = self
                .renderer
                .as_ref()
                .map(|r| r.cell_height() as f32)
                .unwrap_or(20.0);
            if (y as f32) < top - ch {
                Some((AutoscrollDir::Up, (top - ch) - y as f32))
            } else {
                None
            }
        }
    }

    /// v1.10.26: arm/disarm the 40ms autoscroll timer from a mouse move. This
    /// is the ONLY thing a move does about autoscroll — the scroll itself
    /// happens on the timer (see `pump_selection_autoscroll`). Resets the
    /// fractional row carry when the pointer leaves the band.
    pub(super) fn arm_selection_autoscroll(&mut self, y: f64) {
        if self.selection_autoscroll_overshoot(y).is_some() {
            self.window_runtime
                .selection_autoscroll_active
                .store(true, Ordering::Relaxed);
        } else {
            self.interaction.selection_autoscroll_carry = 0.0;
            self.window_runtime
                .selection_autoscroll_active
                .store(false, Ordering::Relaxed);
        }
    }
}
