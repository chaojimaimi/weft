//! UI scene builders and hit-test target resolvers, extracted from
//! `geometry_controller.rs` (T10 P1a, PLAN_v1136 §3 — zero-behavior
//! line-budget extraction ahead of the terminal-guard insertion; the
//! palette / find / panel / tab-bar scene family is one cohesive
//! hit-test domain). Visibility was `pub(super)` at the crate-root module
//! (= crate-visible); the same surface is spelled `pub(crate)` here.

use super::FindButtonAction;
use crate::palette_state::{PaletteEntry, PaletteSubMode};
use crate::App;

impl App {
    pub(crate) fn palette_scene(
        &self,
    ) -> Option<crate::scene::Scene<crate::palette_component::PaletteTarget>> {
        // v1.14.0 (T1): interactive — Closing 期 targets 视作已关
        // (PLAN_v1140 §2.1: geometry targets 归 interactive)。
        if !self.palette.interactive() {
            return None;
        }
        let ctx = self.renderer.as_ref()?.layout_ctx?;
        let labels: Vec<String> = match &self.palette.submode {
            PaletteSubMode::SelectTheme { buffer, themes } => {
                let query = buffer.to_lowercase();
                themes
                    .iter()
                    .filter(|name| query.is_empty() || name.to_lowercase().contains(&query))
                    .cloned()
                    .collect()
            }
            _ => self
                .palette
                .results
                .iter()
                .map(|entry| match entry {
                    PaletteEntry::Workflow(workflow) => workflow.name.clone(),
                    PaletteEntry::Builtin(command) => command.label().to_string(),
                    // v1.5.1: Profile entries render as "Switch Profile: <name>".
                    PaletteEntry::Profile { name, .. } => {
                        format!("Switch Profile: {name}")
                    }
                    // v1.7.1: Search hits use the document title as label.
                    PaletteEntry::SearchHit(hit) => hit.doc.title.clone(),
                    PaletteEntry::Runbook(entry) => entry.command.clone(),
                    // v1.8.1: AI suggestions use the generated command as label.
                    PaletteEntry::AiSuggestion { command, .. } => command.clone(),
                })
                .collect(),
        };
        let layout = crate::palette_component::derive_palette_layout(
            &ctx,
            labels.len(),
            self.palette.selection,
            self.palette.form.as_ref().map(|form| form.var_names.len()),
            self.interaction.popup_max_rows,
            self.interaction.popup_width_scale,
        );
        Some(crate::palette_component::build_palette_scene(
            layout, &labels, ctx.cell_h,
        ))
    }

    pub(crate) fn completion_scene(
        &self,
    ) -> Option<crate::scene::Scene<crate::completion_component::CompletionTarget>> {
        let renderer = self.renderer.as_ref()?;
        let ctx = renderer.layout_ctx?;
        let terminal = self.sessions.active().and_then(|tab| tab.lock_terminal())?;
        if terminal.effective_input_mode() != weft_core::input::InputMode::Editor
            || terminal.editor().search_view().is_some()
        {
            return None;
        }
        let editor = terminal.editor();
        let (matches, selected) = editor.completion_view()?;
        let (visual, _, _) = crate::paint::prompt::prompt_layout_for_buffer(
            &ctx,
            &editor.buffer.lines,
            editor.buffer.cursor,
        );
        let layout = crate::completion_component::derive_completion_layout(
            &ctx,
            matches,
            selected,
            visual.rows.len(),
            (visual.cursor_row, visual.cursor_display_col),
            self.interaction.popup_max_rows,
            self.interaction.popup_width_scale,
        )?;
        Some(crate::completion_component::build_completion_scene(
            layout, matches, ctx.cell_h,
        ))
    }

    /// F3-3: Check whether a physical-pixel point `(x, y)` lands on the
    /// sidebar's right-edge resize handle. Returns `true` only when:
    ///   - the history panel is open,
    ///   - the window is NOT Compact (sidebar is push mode, not overlay),
    ///   - the point is within `tolerance` px of the sidebar's right edge,
    ///   - the point is within the viewport's vertical extent.
    ///
    /// `tolerance` is in physical pixels (≈4 px each side of the edge). The
    /// pure geometry lives in `ui_tokens::sidebar_edge_hit` and the gate
    /// chain in `sidebar_resize_hit_decision` (both unit-tested
    /// independently of the renderer/App state).
    ///
    /// v1.14.1 (F3) 抓取即接管: a running width tween no longer dead-zones
    /// the hit — the press path's `cancel_sidebar_width_anim()` (which runs
    /// before the drag state is recorded) cancels the tween, freezes the
    /// width at the current value and hands the drag the width channel.
    pub(crate) fn sidebar_resize_hit(&self, x: f32, y: f32, tolerance: f32) -> bool {
        let Some(renderer) = &self.renderer else {
            return false;
        };
        sidebar_resize_hit_decision(
            self.panel.open,
            self.panel.sidebar_width_anim.is_some(),
            renderer.sidebar_push_width(),
            renderer.sidebar_width(),
            renderer.viewport().1,
            x,
            y,
            tolerance,
        )
    }

    /// Hit-test the find popup's clickable buttons. Returns the action the
    /// click should trigger, or `None` when the click landed outside any
    /// button (or the find popup isn't open). Layout, hit testing and semantic
    /// bounds are produced by the same Find Scene component.
    pub(crate) fn find_button_at(&self, x: f32, y: f32) -> Option<FindButtonAction> {
        let renderer = self.renderer.as_ref()?;
        let ctx = renderer.layout_ctx?;
        let total = if self.block_view_active() {
            self.find.block_matches.len()
        } else {
            self.find.matches.len()
        };
        let scene =
            crate::find_component::build_find_scene(crate::layout::layout_find(&ctx, total));
        crate::find_component::find_target_at(&scene, x, y).map(|target| match target {
            crate::find_component::FindTarget::Previous => FindButtonAction::Prev,
            crate::find_component::FindTarget::Next => FindButtonAction::Next,
            crate::find_component::FindTarget::ToggleCase => FindButtonAction::ToggleCase,
            crate::find_component::FindTarget::ToggleRegex => FindButtonAction::ToggleRegex,
        })
    }

    /// v0.9: map a physical-pixel click to a palette results-list row
    /// index. Returns `Some(idx)` when the click lands inside a visible
    /// results row, `None` otherwise (outside the popup, on the query/banner
    /// row, in workflow form mode, or below the last visible row). Border-drag
    /// clicks are handled earlier by `check_popup_border_drag`, so they never
    /// reach here.
    ///
    /// Geometry and hit targets come from the shared Palette Scene.
    pub(crate) fn palette_row_at(&self, x: f64, y: f64) -> Option<usize> {
        let scene = self.palette_scene()?;
        match crate::palette_component::palette_target_at(&scene, x as f32, y as f32) {
            Some(crate::palette_component::PaletteTarget::Item(index)) => Some(index),
            _ => None,
        }
    }

    /// Resolve a physical-pixel point in the tab bar to the topmost target.
    /// Rebuilds the tab-strip layout from the current renderer geometry +
    /// `TabBarState.scroll_offset`, then queries the shared TabBar Scene.
    /// Returns `None` when the renderer is absent or the point is outside the
    /// bar.
    pub(crate) fn tab_bar_target_at(
        &self,
        x: f32,
        y: f32,
    ) -> Option<crate::tab_bar_component::TabBarTarget> {
        let renderer = self.renderer.as_ref()?;
        if y > renderer.tab_bar_height() {
            return None;
        }
        let chrome_left = self.tab_bar_chrome_left();
        let strip = crate::layout::layout_tab_strip(crate::layout::TabStripInput {
            viewport_width: self.tab_bar_layout_right(),
            bar_height: renderer.tab_bar_height(),
            cell_width: renderer.cell_width() as f32,
            padding_x: renderer.padding_x(),
            chrome_left,
            traffic_lights_width: renderer.traffic_lights_width(),
            tab_count: self.sessions.len(),
            requested_scroll_offset: self.tab_bar.scroll_offset,
        });
        let scene = crate::tab_bar_component::build_tab_bar_scene(
            strip,
            self.sessions.len(),
            renderer.cell_width() as f32,
            renderer.cell_height() as f32,
        );
        crate::tab_bar_component::tab_bar_target_at(&scene, x, y)
    }

    /// Resolve a physical-pixel point in the history panel to a target.
    /// Rebuilds the panel layout from renderer geometry (single source of
    /// truth — no duplicated magic numbers). Returns `None` when the panel
    /// is closed, the renderer is absent, or the point misses every target.
    pub(crate) fn panel_target_at(
        &self,
        x: f32,
        y: f32,
    ) -> Option<crate::panel_component::PanelTarget> {
        if !self.panel.open {
            return None;
        }
        let renderer = self.renderer.as_ref()?;
        let chrome_top = renderer.layout_ctx.map(|c| c.chrome_top).unwrap_or(0.0);
        let ch = renderer.cell_height() as f32;
        let layout = crate::layout::layout_panel(
            chrome_top,
            renderer.cell_width() as f32,
            ch,
            renderer.sidebar_width(),
            renderer.viewport().1,
        );
        // v1.13.18 P4: the hit-test row range reads the ONE row budget from
        // the PanelLayout geometry (same number as paint / scroll clamp /
        // a11y — no second approximation).
        let rows_bottom = crate::layout::panel_rows_bottom(
            &layout,
            renderer.viewport().1,
            self.panel_footer_visible(),
            ch,
        );
        let max_rows = crate::layout::panel_visible_row_budget(
            layout.list_top,
            layout.row_height,
            ch,
            rows_bottom,
        );
        // v1.11.2 X4: the footer button exists only when this tab has blocks
        // (panel_footer_visible — the shared `!blocks.is_empty()` criterion).
        let footer = self.panel_footer_visible().then_some(layout.footer_rect);
        let scene = crate::panel_component::build_panel_scene(
            layout.panel_rect,
            layout.search_field_rect,
            layout.list_top,
            layout.row_height,
            max_rows,
            footer,
        );
        crate::panel_component::panel_target_at(&scene, x, y)
    }

    pub(crate) fn active_panel_scrollbar_layout(
        &self,
    ) -> Option<crate::panel_scrollbar::PanelScrollbarLayout> {
        if !self.panel.open {
            return None;
        }
        let renderer = self.renderer.as_ref()?;
        // Batch 5 Step 2: read cached metrics from the last draw() instead of
        // re-running panel_filtered_count (O(n) over all blocks) on every
        // mouse move. The cache is written at the end of build_panel_vertices;
        // mouse events read the previous frame's metrics (1-frame lag is
        // imperceptible for scrollbar hit-testing).
        let (total, visible, _max_scroll) = renderer.cached_panel_scroll_metrics.get()?;
        let chrome_top = renderer.layout_ctx.map(|ctx| ctx.chrome_top).unwrap_or(0.0);
        let layout = crate::layout::layout_panel(
            chrome_top,
            renderer.cell_width() as f32,
            renderer.cell_height() as f32,
            renderer.sidebar_width(),
            renderer.viewport().1,
        );
        crate::panel_scrollbar::panel_scrollbar_layout(
            layout.panel_rect,
            layout.list_top,
            total,
            visible,
            self.panel.scroll_offset,
            renderer.cell_height() as f32 * 0.8,
        )
    }
}

/// v1.14.1 (F3): pure decision core of `App::sidebar_resize_hit` — the
/// `App`/renderer shell is not constructible in unit tests (main.rs owns the
/// struct), so the gate chain lives here (the `ui_tokens::sidebar_edge_hit`
/// extraction precedent).
///
/// 抓取即接管 (PLAN_v1141 §3.2.1): `width_anim_active` is deliberately NOT a
/// gate — the v1.14.0 mid-tween dead zone is gone; a press during the
/// animation is routed to the existing `cancel_sidebar_width_anim()` chain
/// (`mouse_press/panel.rs`), which freezes the animated width and hands the
/// drag the width channel. The flag is logged so grab-takeover frames stay
/// identifiable in forensics.
#[allow(clippy::too_many_arguments)]
pub(crate) fn sidebar_resize_hit_decision(
    panel_open: bool,
    width_anim_active: bool,
    push_width: f32,
    edge: f32,
    viewport_h: f32,
    x: f32,
    y: f32,
    tolerance: f32,
) -> bool {
    tracing::debug!(width_anim_active, "sidebar resize hit decision");
    if !panel_open {
        return false;
    }
    // Compact windows don't support sidebar resize (overlay drawer).
    if push_width == 0.0 {
        return false;
    }
    crate::ui_tokens::sidebar_edge_hit(x, edge, tolerance, viewport_h, y)
}

#[cfg(test)]
mod tests {
    use super::sidebar_resize_hit_decision;

    /// v1.14.1 (F3): a running width tween must NOT dead-zone the edge hit —
    /// the grab takes over via the press path's cancel chain. The verdict
    /// with the tween live equals the settled verdict, and a point on the
    /// edge hits in both frames.
    #[test]
    fn sidebar_resize_hit_during_animation_matches_settled_verdict() {
        let hit = |anim: bool| {
            sidebar_resize_hit_decision(true, anim, 300.0, 300.0, 600.0, 302.0, 100.0, 4.0)
        };
        assert!(hit(true), "hit during the width animation must stay live");
        assert_eq!(
            hit(true),
            hit(false),
            "the animation state must not change the verdict"
        );
    }

    #[test]
    fn sidebar_resize_hit_still_gated_by_open_drawer_and_edge() {
        // Closed panel: never a hit (animation state irrelevant).
        assert!(!sidebar_resize_hit_decision(
            false, true, 300.0, 300.0, 600.0, 302.0, 100.0, 4.0
        ));
        // Compact overlay drawer (push_width 0): never a hit.
        assert!(!sidebar_resize_hit_decision(
            true, false, 0.0, 260.0, 600.0, 261.0, 100.0, 4.0
        ));
        // Point off the edge: no hit.
        assert!(!sidebar_resize_hit_decision(
            true, false, 300.0, 300.0, 600.0, 320.0, 100.0, 4.0
        ));
    }
}
