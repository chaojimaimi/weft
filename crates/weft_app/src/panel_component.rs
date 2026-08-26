//! Scene model and hit testing for the history sidebar (panel).
//!
//! Replaces the hardcoded magic numbers (`ch * 1.6`, `ch * 1.4`, `ch * 0.4`,
//! `ch * 1.1`) that were duplicated between `build_panel_vertices` in the
//! renderer and the click handler in `mouse_press_controller`. Both sides now
//! share `PanelLayout` from `layout::layout_panel`, so geometry stays in sync
//! by construction.

use crate::layout::Rect;
use crate::scene::{HitRegion, Scene, SemanticNode, SemanticRole};

/// A clickable region of the history panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PanelTarget {
    /// The search input field at the top.
    SearchField,
    /// A history row at index `i` (0-based, topmost visible = 0).
    Row(usize),
    /// v1.11.2 X4 (PLAN_v1112 §1.3): the footer "load older" button. Present
    /// only when the tab has blocks (see [`build_panel_scene`]).
    LoadOlder,
}

/// Build the panel Scene from a shared layout product. Row hit regions are
/// only generated for visible rows (`row_count` is already capped by
/// `visible_panel_rows`). `footer_rect` is `Some` only when the active tab
/// has at least one block — no blocks means no button (PLAN_v1112 §8).
pub(crate) fn build_panel_scene(
    panel_rect: Rect,
    search_field_rect: Rect,
    list_top: f32,
    row_height: f32,
    row_count: usize,
    footer_rect: Option<Rect>,
) -> Scene<PanelTarget> {
    let mut scene = Scene::default();

    // Search field (focusable input).
    scene.semantics.push(SemanticNode {
        role: SemanticRole::TextField,
        label: "Search history".into(),
        bounds: search_field_rect,
        focus: Some(crate::scene::FocusId::SidebarSearch),
        state: String::new(),
    });
    scene.hits.push(HitRegion::from_rect(
        search_field_rect,
        PanelTarget::SearchField,
    ));

    // History rows.
    for i in 0..row_count {
        let y0 = list_top + i as f32 * row_height;
        let row_rect = [panel_rect[0], y0, panel_rect[2], y0 + row_height];
        scene
            .hits
            .push(HitRegion::from_rect(row_rect, PanelTarget::Row(i)));
        scene.semantics.push(SemanticNode {
            role: SemanticRole::ListItem,
            label: format!("History row {}", i + 1),
            bounds: row_rect,
            focus: None,
            state: String::new(),
        });
    }

    // v1.11.2 X4: footer button — appended AFTER the row nodes so the
    // accessibility pairing (`semantics[1..] ↔ display rows`) is unchanged.
    if let Some(footer) = footer_rect {
        scene
            .hits
            .push(HitRegion::from_rect(footer, PanelTarget::LoadOlder));
        scene.semantics.push(SemanticNode {
            role: SemanticRole::Button,
            label: "加载更早".into(),
            bounds: footer,
            focus: None,
            state: String::new(),
        });
    }

    scene
}

/// Resolve a physical-pixel point to the topmost panel target. Search field
/// is checked first, then rows in order.
pub(crate) fn panel_target_at(scene: &Scene<PanelTarget>, x: f32, y: f32) -> Option<PanelTarget> {
    scene
        .hits
        .iter()
        .find(|hit| hit.contains_half_open(x, y))
        .map(|hit| hit.target)
}

#[cfg(test)]
mod tests {
    use super::{build_panel_scene, panel_target_at, PanelTarget};

    const PANEL_RECT: [f32; 4] = [0.0, 28.0, 240.0, 700.0];
    const SEARCH_RECT: [f32; 4] = [4.0, 50.0, 236.0, 70.0];
    const LIST_TOP: f32 = 78.0;
    const ROW_H: f32 = 22.0;

    #[test]
    fn search_field_hit() {
        let scene = build_panel_scene(PANEL_RECT, SEARCH_RECT, LIST_TOP, ROW_H, 5, None);
        assert_eq!(
            panel_target_at(&scene, 100.0, 60.0),
            Some(PanelTarget::SearchField),
        );
    }

    #[test]
    fn row_hit_returns_index() {
        let scene = build_panel_scene(PANEL_RECT, SEARCH_RECT, LIST_TOP, ROW_H, 5, None);
        // Row 2 starts at LIST_TOP + 2*ROW_H = 78 + 44 = 122
        assert_eq!(
            panel_target_at(&scene, 100.0, 125.0),
            Some(PanelTarget::Row(2)),
        );
    }

    #[test]
    fn miss_below_last_row_is_none() {
        let scene = build_panel_scene(PANEL_RECT, SEARCH_RECT, LIST_TOP, ROW_H, 3, None);
        // Below row 2 (the last row with 3 rows): 78 + 3*22 = 144
        assert_eq!(panel_target_at(&scene, 100.0, 200.0), None);
    }

    /// v1.11.2 X4: the footer rect is a LoadOlder hit when present.
    #[test]
    fn footer_hit_when_present_and_absent_when_not() {
        const FOOTER: [f32; 4] = [4.0, 640.0, 236.0, 670.0];
        let scene = build_panel_scene(PANEL_RECT, SEARCH_RECT, LIST_TOP, ROW_H, 5, Some(FOOTER));
        assert_eq!(
            panel_target_at(&scene, 100.0, 650.0),
            Some(PanelTarget::LoadOlder),
        );
        // No blocks → no footer region → same point misses entirely.
        let scene = build_panel_scene(PANEL_RECT, SEARCH_RECT, LIST_TOP, ROW_H, 5, None);
        assert_eq!(panel_target_at(&scene, 100.0, 650.0), None);
    }
}
