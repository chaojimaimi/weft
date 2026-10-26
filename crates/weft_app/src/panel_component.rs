//! Scene model and hit testing for the history sidebar (panel).
//!
//! Replaces the hardcoded magic numbers (`ch * 1.6`, `ch * 1.4`, `ch * 0.4`,
//! `ch * 1.1`) that were duplicated between `build_panel_vertices` in the
//! renderer and the click handler in `mouse_press_controller`. Both sides now
//! share `PanelLayout` from `layout::layout_panel`, so geometry stays in sync
//! by construction.

use crate::layout::Rect;
use crate::scene::{HitRegion, Scene, SemanticNode, SemanticRole};

/// v1.14.4 (批A): single source for the history-footer "load older" label.
/// `push_text` is lookup-only — glyphs missing from the atlas are silently
/// skipped — so the paint site (paint/panel.rs), the AX label here, and the
/// atlas warm-up (renderer/atlas_warmup.rs) must all render the SAME string.
/// Three independent literals were the root cause of the lone-「更」footer
/// artifact (footer-label-glyph-warmup-forensics).
pub(crate) const FOOTER_LABEL: &str = "加载更早";

/// v1.15.0 (T3, PLAN_v1150): the footer's one-shot feedback messages.
/// `block_persist.rs` writes them into `Tab::footer_feedback`; paint/panel.rs
/// shows them in place of [`FOOTER_LABEL`] for 3s. The success form is
/// prefix + count + suffix ("已加载 12 条"). The atlas warm-up must
/// rasterize ALL of these glyphs plus [`FOOTER_FEEDBACK_DIGITS`] — 读/取/失/败
/// appear in no other warm list, and an unwarmed glyph is silently skipped
/// by push_text (the lone-「更」footer artifact,
/// footer-label-glyph-warmup-forensics).
pub(crate) const FOOTER_FEEDBACK_LOADED_PREFIX: &str = "已加载 ";
pub(crate) const FOOTER_FEEDBACK_LOADED_SUFFIX: &str = " 条";
pub(crate) const FOOTER_FEEDBACK_EXHAUSTED: &str = "没有更早的历史";
pub(crate) const FOOTER_FEEDBACK_ERROR: &str = "读取历史失败";
/// Digit coverage for the "已加载 N 条" count (warm-up list, not paint).
pub(crate) const FOOTER_FEEDBACK_DIGITS: &str = "0123456789";

/// v1.15.0 (T3): footer label + dim-state policy, pure so the truth table is
/// unit-testable. Fresh feedback replaces the visible label; the exhausted
/// ruling dims the label one notch (permanent until an eviction resets it) —
/// the button keeps rendering either way. The a11y button name stays
/// [`FOOTER_LABEL`]: the visible text is transient state feedback, the
/// button semantic never changes.
pub(crate) fn footer_label_and_dim(feedback: Option<&str>, exhausted: bool) -> (&str, bool) {
    (feedback.unwrap_or(FOOTER_LABEL), exhausted)
}

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
/// only generated for visible rows (`row_count` is already capped by the
/// shared `layout::panel_visible_row_budget`). `footer_rect` is `Some` only
/// when the active tab has at least one block — no blocks means no button
/// (PLAN_v1112 §8).
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
            label: FOOTER_LABEL.into(),
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
    use super::{
        build_panel_scene, footer_label_and_dim, panel_target_at, PanelTarget, FOOTER_LABEL,
    };

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

    // ── v1.15.0 (T3): footer label + dim policy truth table ──────────

    /// Plain state (no feedback, not exhausted): static label, no dim.
    #[test]
    fn footer_label_plain_state() {
        let (label, dim) = footer_label_and_dim(None, false);
        assert_eq!(label, FOOTER_LABEL);
        assert!(!dim);
    }

    /// Fresh feedback swaps the visible label; dim unaffected by feedback.
    #[test]
    fn footer_label_feedback_swaps_visible_text() {
        let (label, dim) = footer_label_and_dim(Some("已加载 12 条"), false);
        assert_eq!(label, "已加载 12 条");
        assert!(!dim);
        // The exhausted ruling dims whatever is on the label (feedback and
        // exhausted can co-occur: the empty-store click sets both).
        let (label, dim) = footer_label_and_dim(Some("没有更早的历史"), true);
        assert_eq!(label, "没有更早的历史");
        assert!(dim);
    }

    /// Exhausted with expired feedback: static label but persistent dim.
    #[test]
    fn footer_label_exhausted_dims_after_feedback_expires() {
        let (label, dim) = footer_label_and_dim(None, true);
        assert_eq!(label, FOOTER_LABEL, "expired feedback falls back");
        assert!(dim, "the dim ruling outlives the 3s feedback");
    }
}
