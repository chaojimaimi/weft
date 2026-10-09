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
                    // v1.12.28 (P1-02 ③): a split-tree snapshot rebuilds the
                    // whole pane tree (root leaf spawns the tab, the shared
                    // `build_subtree` recursion splits the rest); every other
                    // snapshot follows the single-pane path below unchanged.
                    if snap.panes.is_some() {
                        self.restore_multi_pane_tab(snap, i == 0);
                        continue;
                    }
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
                            if let Some(mut t) = tab.lock_terminal() {
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
                        if let Some(mut t) = tab.lock_terminal() {
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

    /// v1.12.28 (P1-02 ③): rebuild ONE multi-pane tab from its persisted
    /// split-tree snapshot (`snap.panes = Some`). The root leaf spawns the
    /// initial tab (cwd via `restore_spawn_cwd`, same contract as the
    /// single-pane tab path); `pane_rebuild::build_subtree` splits the tree
    /// in the saved directions/ratios (ratio clamped to [0.1, 0.9], workspace
    /// validate parity) and drops each leaf's payload (cwd fallback, editor
    /// buffer, per-leaf `restored_snapshot` with `block_ids`) on its pane.
    /// Afterwards the saved active leaf is focused (DFS index) and the
    /// tab-level `block_scroll_offset` is applied via `set_block_scroll`
    /// (the single-pane `restore_from_snapshot` precedent, tab.rs).
    ///
    /// Headless verification is not possible here (App + PTY) — the manual
    /// split→quit→relaunch checklist is the final acceptance criterion.
    fn restore_multi_pane_tab(
        &mut self,
        snap: &weft_core::persistence::TabSnapshot,
        is_first: bool,
    ) {
        use crate::workspace_controller::{build_subtree, PaneTreeRebuild};
        let Some(panes) = snap.panes.as_ref() else {
            return;
        };
        let (rows, cols) = self.current_size();
        let scrollback = self.config_state.config.scrollback.lines;
        let blocks_limit = self.config_state.config.blocks.retained_limit;
        let output_cap_mib = self.config_state.config.blocks.output_cap_mib;
        let render_mode = self.config_state.config.experimental.tui_render_mode;

        // Root leaf spawns the initial tab. `root_leaf_spawn_cwd` returns ""
        // for an absent cwd → `restore_spawn_cwd` → None → inherit weft cwd.
        let root_cwd = panes.tree.root_leaf_spawn_cwd();
        let root_spawn_cwd = crate::pane::restore_spawn_cwd(Some(root_cwd.as_str()));
        let mut tab = Tab::new(
            rows,
            cols,
            scrollback,
            &self.proxy,
            root_spawn_cwd.as_deref(),
        );
        if let Some(mut t) = tab.lock_terminal() {
            t.set_blocks_retained_limit(blocks_limit);
            t.set_block_output_cap(crate::config_controller::output_cap_bytes(output_cap_mib));
            // v1.11.7 (P2-3): inject the user's TUI tier.
            t.set_tui_render_mode(render_mode);
            if let Some(r) = &self.renderer {
                t.set_palette(r.theme().palette);
                t.set_background_color(r.theme().background);
            }
        }
        // Shared block-id allocator (workspace restore_tab precedent) so the
        // restored panes never mint ids colliding with the persisted store.
        if let Some(allocator) = self
            .sessions
            .block_store()
            .map(weft_core::persistence::BlockStore::block_id_allocator)
        {
            tab.with_terminal(|t| {
                t.block_tracker_mut().use_shared_id_allocator(allocator);
            });
        }

        let initial_pane_id = tab.active_pane_id();
        let proxy = self.proxy.clone();
        let mut split_fn = |tab: &mut Tab,
                            leaf,
                            dir: weft_core::pane_layout::SplitDirection,
                            ratio: f32,
                            cwd: &str| {
            let spawn_cwd = crate::pane::restore_spawn_cwd(Some(cwd));
            let mut new_pane =
                crate::pane::Pane::spawn(rows, cols, scrollback, &proxy, spawn_cwd.as_deref());
            new_pane.set_blocks_retained_limit(blocks_limit);
            new_pane
                .set_blocks_output_cap(crate::config_controller::output_cap_bytes(output_cap_mib));
            new_pane.set_tui_render_mode(render_mode);
            tab.split_pane_with_pane(leaf, dir, ratio.clamp(0.1, 0.9), new_pane)
                .map_err(|e| {
                    warn!(?e, "tab snapshot restore: split failed, skipping subtree");
                    e
                })
                .ok()
        };
        build_subtree(&mut tab, &panes.tree, initial_pane_id, &mut split_fn);

        // Focus the saved active leaf (DFS index, pane_id_at_index order) and
        // apply the tab-level block scroll to the now-active pane.
        let panes_list = tab.split_tree().panes();
        if let Some(target) = panes_list.get(panes.active_leaf).copied() {
            if let Err(e) = tab.set_active_pane(target) {
                warn!(
                    ?e,
                    index = panes.active_leaf,
                    "tab snapshot restore: failed to set active pane"
                );
            }
        }
        tab.set_block_scroll(snap.block_scroll_offset);

        if is_first {
            self.sessions.replace_tab(0, tab);
        } else {
            self.sessions.push_tab(tab);
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
