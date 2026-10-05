//! PTY session pumping, message processing, and redraw requests — extracted
//! from `main.rs` (v1.12.25 3-B-2 P2-01).
//!
//! `spawn_pty` / `pump_pty` / `process_messages` / `request_redraw` are the
//! per-frame session I/O core. Their call sites span the top-level controller
//! family (`app_runtime`, `redraw_controller`, mouse / transfer /
//! accessibility controllers, …), so the visibility is `pub(crate)` —
//! `pub(super)` would only reach the `app` module tree and break every
//! top-level caller. Method bodies are moved verbatim (zero behavior
//! change); `main.rs` keeps the `App` struct definition, `new()` and
//! `tab()`.

use crate::effect;
use crate::first_run_welcome;
use crate::tab::{Tab, TuiScrollResolution};
use crate::App;
use tracing::{info, warn};

impl App {
    pub(crate) fn spawn_pty(&mut self, rows: usize, cols: usize) {
        let mut tab = Tab::new(
            rows,
            cols,
            self.config_state.config.scrollback.lines,
            &self.proxy,
            None,
        );
        // v1.11.2 X4: propagate the block retention cap to the fresh pane.
        crate::config_controller::apply_blocks_retained_limit(
            &mut tab,
            self.config_state.config.blocks.retained_limit,
        );
        // PLAN_v11217 §3.5 (T4): propagate the configured output cap too.
        crate::config_controller::apply_blocks_output_cap(
            &mut tab,
            self.config_state.config.blocks.output_cap_mib,
        );
        // v1.11.7 (P2-3): inject the user's TUI render tier — the core default
        // is Classic; the factory default here is `noninteractive`.
        crate::config_controller::apply_tui_render_mode(
            &mut tab,
            self.config_state.config.experimental.tui_render_mode,
        );
        // v1.0 V13: On first launch, inject a welcome banner via PTY.
        // The printf is prefixed with a space (HIST_IGNORE_SPACE keeps it
        // out of zsh history). The marker file is created in
        // first_run_welcome() so this only fires once ever.
        if let Some(cmd) = first_run_welcome() {
            if let Some(p) = tab.pty.as_ref() {
                let _ = p.write_sync(cmd.as_bytes());
            }
        }
        self.sessions.push_tab(tab);
    }

    /// Non-blocking drain of PTY events into channel. Drains ALL tabs per
    /// frame (v0.9 H1 decision: background tabs keep their PTY buffers
    /// flushed so switching to them is instant; only the active tab is
    /// rendered).
    pub(crate) fn pump_pty(&mut self) {
        for tab in self.sessions.tabs_mut() {
            tab.pump_pty();
        }
    }

    pub(crate) fn process_messages(&mut self) -> bool {
        let mut any_redraw = false;
        let mut had_pty_output = false;
        let mut deferred_local_scroll = 0_i32;
        let mut drained_blocks: Vec<weft_core::blocks::Block> = Vec::new();
        // FIX_background_pane_pump §2.4: events are (source pane, event)
        // pairs — every pane is drained now, so the OSC 52 read path can
        // route its reply back to the pane that asked.
        let mut drained_ui_events: Vec<(weft_core::pane_layout::PaneId, weft_core::vt::UiEvent)> =
            Vec::new();
        let mut exit_requested = false;
        for i in 0..self.sessions.len() {
            let (alive, drained, need_redraw, ui_events) =
                self.sessions.tabs_mut()[i].process_messages();
            // Collect final blocks before handling a shell exit.
            drained_blocks.extend(drained);
            // v1.11.5 (PLAN_v1115 §M2): app-facing ui events (OSC 52 / 9 /
            // 777), source-pane tagged — dispatch after the loop, once tab
            // borrows are released.
            drained_ui_events.extend(ui_events);
            if !alive {
                // Exit the app only when the last shell exits; Cmd+W is separate.
                let dead_session_id = self.sessions.tab(i).map(|tab| tab.session_id);
                let menu_belongs_to_dead_session = dead_session_id.is_some_and(|session_id| {
                    self.interaction
                        .context_menu
                        .as_ref()
                        .is_some_and(|menu| menu.belongs_to_session(session_id))
                });
                if menu_belongs_to_dead_session {
                    self.take_context_menu("context menu owner shell exited");
                }
                // v1.12.24 (N-2): the dying tab's final snapshot must land in the tabs
                // DB BEFORE removal — after remove_dead no save ever runs on the
                // should_exit tail (saving empty tabs would wipe the table), so the
                // 1 Hz autosave's ≤1s lag was the last command's only loss window.
                self.persist_tabs_snapshot_now();
                let is_last = self.sessions.remove_dead(i);
                if is_last {
                    exit_requested = true;
                    break;
                }
                info!(
                    closed = i,
                    active = self.sessions.active_idx(),
                    "tab shell exited"
                );
                break;
            }
            if let Some(resolution) = self.sessions.tabs_mut()[i].resolve_pending_tui_scroll() {
                match resolution {
                    TuiScrollResolution::PtyBytes(bytes) => {
                        if let Some(tab) = self.sessions.tab_mut(i) {
                            if let Err(e) = tab.write_user_input(&bytes) {
                                warn!(error = %e, tab = i, "failed to replay queued TUI scroll");
                            }
                        }
                    }
                    TuiScrollResolution::LocalRows(rows) if i == self.sessions.active_idx() => {
                        deferred_local_scroll =
                            deferred_local_scroll.saturating_add(rows).clamp(-100, 100);
                    }
                    TuiScrollResolution::LocalRows(_) => {}
                }
                any_redraw = true;
            }
            if need_redraw {
                any_redraw = true;
                had_pty_output = true;
            }
        }
        // v1.11.15 (FIX B, PLAN_v11115_EXIT_RACE_MOUSE_LEAK §2): when the
        // dead tab was the LAST one, `remove_dead` has already emptied
        // `tabs` by this point — the UiEvents collected above came from a
        // session that no longer exists, and the dispatch arms deref the
        // active tab. Drop them at the source (#4 panic entry).
        // Review P3: this guard also precedes `scroll_local_view` (active
        // tab deref) as pure defense against future drain restructuring.
        if self.sessions.tabs().is_empty() {
            tracing::debug!(
                dropped = drained_ui_events.len(),
                "dropping stale ui events — last session exited"
            );
            drained_ui_events.clear();
        } else if deferred_local_scroll != 0 {
            self.scroll_local_view(deferred_local_scroll);
        }
        // v1.11.5 (PLAN_v1115 §M2): app-facing ui events (OSC 52 clipboard,
        // OSC 9/777 notify, OSC 9;4 Dock progress) — dispatched after all
        // tab borrows are released. Each arm gates against config + state;
        // the sinks land in later modules (M3 read prompt, M4 notify sink,
        // M7 Dock badge). FIX_background_pane_pump §2.4: the events carry
        // their source pane, so the OSC 52 read reply routes to the asking
        // pane instead of assuming the active one.
        if !drained_ui_events.is_empty() {
            self.dispatch_ui_events(drained_ui_events);
        }
        // v1.11.5 (PLAN_v1115 §M5): finished command blocks → completion
        // notifications (threshold + focus gate + rate limiter inside).
        if !drained_blocks.is_empty() {
            self.dispatch_block_completion_notifications(&drained_blocks);
        }
        // v1.11.5: land a debounced Dock badge whose window elapsed (a
        // stalled OSC 9;4 stream still reaches its final value ~200ms late).
        self.flush_dock_badge();
        let effects = effect::process_message_effects(exit_requested, drained_blocks, any_redraw);
        self.drain_effects(effects);
        had_pty_output
    }

    pub(crate) fn request_redraw(&self) {
        if let (Some(window), Some(_renderer)) = (&self.window, &self.renderer) {
            window.request_redraw();
        }
    }
}
