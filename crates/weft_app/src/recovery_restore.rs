//! Recovery-session tab snapshot restore (T4 follow-up split from
//! lifecycle_controller.rs so both files stay under the 800-line gate;
//! pure move — methods keep their `pub(super)` App surface).

use super::*;

impl App {
    /// v1.0 H4: restore saved tab snapshots (cwd + editor drafts) so the
    /// session layout survives restarts. The PTY itself is NOT revived — each
    /// restored tab gets a fresh shell, with the editor draft rehydrated.
    ///
    /// v1.0 fix: cwd is restored via `chdir` in the child process before
    /// exec (Pty::spawn_with_args `cwd` param), NOT by sending a `cd`
    /// command. Sending `cd` polluted the terminal, shell history, and
    /// block tracker with a spurious `cd <cwd>` block.
    ///
    /// v1.10.36 fix: the chdir-target filter is now
    /// `crate::pane::restore_spawn_cwd` (an absent/empty saved cwd yields
    /// `None`; everything else is honored verbatim). The old filter also
    /// dropped a saved cwd equal to `$HOME` and equal to the Weft process's
    /// cwd, so a Finder-launched Weft (process cwd `/`) restored
    /// home-directory tabs at the filesystem root.
    pub(super) fn restore_tab_snapshots(&mut self) {
        let Some(store) = self.sessions.block_store() else {
            return;
        };
        let snaps_result = store.load_tabs();
        match snaps_result {
            Ok(snaps) if !snaps.is_empty() => {
                info!(count = snaps.len(), "restoring saved tab snapshots");
                let (rows, cols) = self.current_size();
                let total = snaps.len();
                for (i, snap) in snaps.iter().enumerate() {
                    let saved_cwd = snap.cwd.clone();
                    let cwd_to_apply = crate::pane::restore_spawn_cwd(saved_cwd.as_deref());
                    let blocks_limit = self.config_state.config.blocks.retained_limit;
                    let render_mode = self.config_state.config.experimental.tui_render_mode;
                    if i == 0 {
                        if cwd_to_apply.is_some() {
                            let mut tab = Tab::new(
                                rows,
                                cols,
                                self.config_state.config.scrollback.lines,
                                &self.proxy,
                                cwd_to_apply.as_deref(),
                            );
                            if let Some(t) = &mut tab.terminal {
                                t.set_blocks_retained_limit(blocks_limit);
                                t.set_block_output_cap(crate::config_controller::output_cap_bytes(
                                    self.config_state.config.blocks.output_cap_mib,
                                ));
                                // v1.11.7 (P2-3): inject the user's TUI tier.
                                t.set_tui_render_mode(render_mode);
                                if let Some(r) = &self.renderer {
                                    t.set_palette(r.theme().palette);
                                    t.set_background_color(r.theme().background);
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
                            cwd_to_apply.as_deref(),
                        );
                        tab.restore_from_snapshot(snap);
                        if let Some(t) = &mut tab.terminal {
                            t.set_blocks_retained_limit(blocks_limit);
                            t.set_block_output_cap(crate::config_controller::output_cap_bytes(
                                self.config_state.config.blocks.output_cap_mib,
                            ));
                            // v1.11.7 (P2-3): inject the user's TUI tier.
                            t.set_tui_render_mode(render_mode);
                            if let Some(r) = &self.renderer {
                                t.set_palette(r.theme().palette);
                                t.set_background_color(r.theme().background);
                            }
                        }
                        self.sessions.push_tab(tab);
                    }
                }
                let active = weft_core::persistence::TabSnapshot::restored_active_index(&snaps);
                self.sessions.set_active(active);
                info!(restored = total, "tab snapshots restored");
            }
            Ok(_) => {}
            Err(e) => {
                warn!(error = %e, "failed to load tab snapshots; starting fresh");
            }
        }
    }

    /// v1.8.9 fix: After recovery restore, attach persisted TabSnapshots to
    /// the rebuilt tabs so the history-hydration loop can read `block_ids`.
    ///
    /// `restore_workspace` rebuilds tabs + panes + cwds from the recovery
    /// snapshot but leaves `restored_snapshot = None`. The normal
    /// `restore_tab_snapshots` path sets it via `restore_from_snapshot`. Since
    /// both stores (recovery YAML + tabs SQLite) are written in the same
    /// `TabsAutoSave` cycle, we match by position and only carry over the
    /// `block_ids` (the editor draft is already set by `restore_workspace`).
    ///
    /// v1.10.24 (FIX_RECOVERY_DESIGN_ALIGNMENT Fix 2): attachment now also
    /// applies the persisted `block_scroll_offset` via
    /// [`Tab::attach_recovery_snapshot`] (which reuses `set_block_scroll`),
    /// so the recovery Restore path restores the block-view scroll position
    /// exactly like the SQLite path does.
    ///
    /// v1.10.24 B1: this now actually works — `restore_workspace` only sets
    /// the `restored_cwd` fallback (no stub snapshot), so the `is_some()`
    /// guard inside `attach_recovery_snapshot` cannot skip the real attach.
    /// Before the fix `set_restored_cwd_fallback` wrote a stub
    /// `restored_snapshot`, making this attach a no-op since v1.8.9.
    pub(super) fn attach_recovery_tab_snapshots(&mut self) {
        let Some(store) = self.sessions.block_store() else {
            return;
        };
        let snaps = match store.load_tabs() {
            Ok(s) if !s.is_empty() => s,
            Ok(_) => {
                info!("no saved tab snapshots to attach after recovery restore");
                return;
            }
            Err(e) => {
                warn!(error = %e, "failed to load tab snapshots after recovery");
                return;
            }
        };
        let tabs = self.sessions.tabs_mut();
        let attached = snaps
            .iter()
            .zip(tabs.iter_mut())
            .filter_map(|(snap, tab)| tab.attach_recovery_snapshot(snap).then_some(()))
            .count();
        info!(
            attached,
            total_tabs = self.sessions.tabs().len(),
            "attached block_ids from TabSnapshots after recovery restore"
        );
    }
}
