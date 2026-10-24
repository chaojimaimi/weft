//! v1.14.0 (PLAN_v1140 T4): hover fade synchronization.
//!
//! One chokepoint re-syncs BOTH fade registries against the live hover
//! signals every frame (called from `run_redraw` before the draw-state
//! projection), instead of scattering trigger points over every hover
//! write site. `HoverFades::sync_target` is per-frame-idempotent: an entry
//! already heading to the desired target is left alone, so a mid-flight
//! tween keeps running from its drawn value (the plan's no-jump rule).

use std::time::Instant;

use crate::app::motion::HoverKey;
use crate::block_component::BlockHeaderAction;

impl crate::App {
    pub(crate) fn sync_hover_fades(&mut self, now: Instant) {
        let animated = crate::app::motion::gated(self.window_runtime.reduce_motion);
        let active = self.sessions.active_idx();

        // rust-reviewer P3: keys for tab indices that no longer exist are
        // dead state — a later new_tab reusing the index would inherit the
        // stale progress-1.0 highlight. Removed outright (not faded).
        let tab_count = self.sessions.len();
        self.tab_bar.hover_fades.retain(|key| match key {
            crate::app::motion::HoverKey::TabRow(i) | crate::app::motion::HoverKey::TabClose(i) => {
                *i < tab_count
            }
            _ => true,
        });

        // ── Tab bar registry: row / "+" / ‹ › / × ──
        // The index loop covers every live tab, so rows left stale by a tab
        // close (indices ≥ len are never queried) and formerly-hovered rows
        // inside range both get falsed here.
        let hovered = self.tab_bar.hovered_tab;
        for i in 0..self.sessions.len() {
            let on = hovered == Some(i);
            self.tab_bar
                .hover_fades
                .sync_target(HoverKey::TabRow(i), on, now, animated);
            // The "×" fades in on a hovered INACTIVE tab (the active tab's
            // × is steady — its paint branch ignores the progress).
            self.tab_bar.hover_fades.sync_target(
                HoverKey::TabClose(i),
                on && i != active,
                now,
                animated,
            );
        }
        self.tab_bar.hover_fades.sync_target(
            HoverKey::TabPlus,
            self.tab_bar.plus_hovered,
            now,
            animated,
        );
        self.tab_bar.hover_fades.sync_target(
            HoverKey::TabArrowLeft,
            self.tab_bar.arrow_left_hovered,
            now,
            animated,
        );
        self.tab_bar.hover_fades.sync_target(
            HoverKey::TabArrowRight,
            self.tab_bar.arrow_right_hovered,
            now,
            animated,
        );
        self.tab_bar.hover_fades.maintain(now);

        // ── Interaction registry: block header + per-button + scrollbar ──
        // The "on" set is at most four keys; every other tracked key must be
        // actively falsed or a block that LOST hover would stay lit (its
        // entry never leaves the registry on its own).
        let block_hover = self.interaction.block_hovered;
        let action_hover = self.interaction.block_action_hovered;
        let thumb_on =
            self.interaction.scrollbar_hovered || self.interaction.scrollbar_drag.is_some();

        let mut on_keys: Vec<HoverKey> = Vec::with_capacity(4);
        if let Some(block) = block_hover {
            on_keys.push(HoverKey::BlockHover(block));
        }
        match action_hover {
            Some(BlockHeaderAction::Copy(block)) => {
                on_keys.push(HoverKey::BlockActionCopy(block));
            }
            Some(BlockHeaderAction::ToggleFold(block)) => {
                on_keys.push(HoverKey::BlockActionFold(block));
            }
            Some(BlockHeaderAction::Diagnose(block)) => {
                on_keys.push(HoverKey::BlockActionDiagnose(block));
            }
            // CloseDiagnose has no dedicated hover surface (its paint branch
            // is selection-colored) — no fade key.
            _ => {}
        }

        for key in self.interaction.hover_fades.keys() {
            let on = match key {
                HoverKey::ScrollbarThumb => thumb_on,
                tracked => on_keys.contains(&tracked),
            };
            self.interaction
                .hover_fades
                .sync_target(key, on, now, animated);
        }
        if let Some(block) = block_hover {
            self.interaction.hover_fades.sync_target(
                HoverKey::BlockHover(block),
                true,
                now,
                animated,
            );
        }
        match action_hover {
            Some(BlockHeaderAction::Copy(block)) => self.interaction.hover_fades.sync_target(
                HoverKey::BlockActionCopy(block),
                true,
                now,
                animated,
            ),
            Some(BlockHeaderAction::ToggleFold(block)) => self.interaction.hover_fades.sync_target(
                HoverKey::BlockActionFold(block),
                true,
                now,
                animated,
            ),
            Some(BlockHeaderAction::Diagnose(block)) => self.interaction.hover_fades.sync_target(
                HoverKey::BlockActionDiagnose(block),
                true,
                now,
                animated,
            ),
            _ => {}
        }
        self.interaction
            .hover_fades
            .sync_target(HoverKey::ScrollbarThumb, thumb_on, now, animated);
        self.interaction.hover_fades.maintain(now);
    }
}
