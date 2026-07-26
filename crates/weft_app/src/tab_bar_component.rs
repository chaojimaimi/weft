//! Scene model and hit testing for the tab bar.
//!
//! Replaces the legacy `renderer.tab_hits: Vec<TabHit>` (with `usize::MAX`
// sentinel values for scroll arrows) with a typed `Scene<TabBarTarget>`.
//! Layout comes from `layout::layout_tab_strip`; close-button visibility
//! mirrors the renderer's `cx_in_view` rule so the hit region only covers
//! close buttons that are actually drawn.

use crate::layout::{Rect, TabStripLayout};
use crate::scene::{HitRegion, Scene, SemanticNode, SemanticRole};

/// A clickable region of the tab bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TabBarTarget {
    /// The tab label area (switches to this tab on click).
    Tab(usize),
    /// The close "×" button of tab `index`.
    Close(usize),
    /// The "+" button (opens a new tab).
    NewTab,
    /// Left scroll arrow (shown when tabs overflow).
    ArrowLeft,
    /// Right scroll arrow (shown when tabs overflow).
    ArrowRight,
}

/// Close-button geometry derived from cell metrics. Matches the renderer's
/// `close_w = cw * 2.0`, `label_gap = cw * 0.5` layout so hit regions align
/// exactly with the drawn × icons.
struct CloseMetrics {
    /// `close_w = cw * 2.0` — full width of the close hit zone.
    width: f32,
    /// `label_w = tab_w - close_w - label_gap` — x-offset of the close zone
    /// from the tab's x0.
    offset: f32,
    /// Half-extent of the drawn × icon. The renderer gates the × on
    /// `close_cx ± close_r` being inside the visible region; we mirror that
    /// exactly so hit regions never extend past the drawn icon.
    radius: f32,
}

impl CloseMetrics {
    /// `cell_height` drives the × icon radius (mirrors `renderer.rs`:
    /// `close_r = ch * 0.16` for active, `ch * 0.13` for inactive — we use
    /// the larger value so the gate is tight for both states).
    fn new(cell_width: f32, tab_width: f32, cell_height: f32) -> Self {
        let width = cell_width * 2.0;
        let label_gap = cell_width * 0.5;
        let offset = tab_width - width - label_gap;
        let radius = cell_height * 0.16;
        Self {
            width,
            offset,
            radius,
        }
    }

    /// Absolute close-button rect for tab `index`, or `None` if the × icon
    /// would overlap the arrow / "+" region. Mirrors the renderer's
    /// `cx_in_view` gate: `close_cx ± close_r` must be fully inside
    /// `[visible_left, visible_right]`.
    fn rect_for(&self, strip: TabStripLayout, index: usize) -> Option<Rect> {
        let x0 = strip.tabs_start + index as f32 * strip.tab_width - strip.scroll_offset;
        let close_x0 = x0 + self.offset;
        let close_x1 = close_x0 + self.width;
        let close_cx = (close_x0 + close_x1) * 0.5;
        if close_cx - self.radius < strip.visible_left
            || close_cx + self.radius > strip.visible_right
        {
            return None;
        }
        Some([close_x0, strip.bar_rect[1], close_x1, strip.bar_rect[3]])
    }
}

/// Build the tab-bar Scene from a pure layout product. Arrow hit regions are
/// pushed first (front of the list) so they win over tab rects that overlap
/// the arrow zone; close-button regions are pushed after their owning tab so
/// `tab_bar_target_at` finds the more specific close target first.
pub(crate) fn build_tab_bar_scene(
    strip: TabStripLayout,
    tab_count: usize,
    cell_width: f32,
    cell_height: f32,
) -> Scene<TabBarTarget> {
    let mut scene = Scene::default();
    let close = CloseMetrics::new(cell_width, strip.tab_width, cell_height);

    // Arrows first (highest z-order, front of the hit list).
    if let Some(left) = strip.left_arrow_rect {
        scene
            .hits
            .push(HitRegion::from_rect(left, TabBarTarget::ArrowLeft));
        scene.semantics.push(SemanticNode {
            role: SemanticRole::Button,
            label: "Scroll tabs left".into(),
            bounds: left,
            focus: None,
            state: String::new(),
        });
    }
    if let Some(right) = strip.right_arrow_rect {
        scene
            .hits
            .push(HitRegion::from_rect(right, TabBarTarget::ArrowRight));
        scene.semantics.push(SemanticNode {
            role: SemanticRole::Button,
            label: "Scroll tabs right".into(),
            bounds: right,
            focus: None,
            state: String::new(),
        });
    }

    // Per-tab: close button (if visible) then the tab label rect. Close must
    // precede its tab so the target lookup — which returns the first hit —
    // prefers the close button when the click is inside both rects.
    for index in 0..tab_count {
        if let Some(close_rect) = close.rect_for(strip, index) {
            scene
                .hits
                .push(HitRegion::from_rect(close_rect, TabBarTarget::Close(index)));
        }
        let tab_rect = strip.tab_rect(index);
        scene
            .hits
            .push(HitRegion::from_rect(tab_rect, TabBarTarget::Tab(index)));
        scene.semantics.push(SemanticNode {
            role: SemanticRole::Tab,
            label: format!("Tab {}", index + 1),
            bounds: tab_rect,
            focus: None,
            state: String::new(),
        });
    }

    // "+" button.
    scene
        .hits
        .push(HitRegion::from_rect(strip.plus_rect, TabBarTarget::NewTab));
    scene.semantics.push(SemanticNode {
        role: SemanticRole::Button,
        label: "New tab".into(),
        bounds: strip.plus_rect,
        focus: None,
        state: String::new(),
    });

    scene
}

/// Resolve a physical-pixel point to the topmost tab-bar target. Hit regions
/// are checked in insertion order (arrows → per-tab close/tab → "+"), so
/// arrows and close buttons take priority over the tab label they overlap.
pub(crate) fn tab_bar_target_at(
    scene: &Scene<TabBarTarget>,
    x: f32,
    y: f32,
) -> Option<TabBarTarget> {
    scene
        .hits
        .iter()
        .find(|hit| hit.contains_half_open(x, y))
        .map(|hit| hit.target)
}

impl crate::App {
    /// Right edge available to global tab items. In a vertical split, tabs
    /// stay above the left pane instead of straddling the pane divider.
    pub(super) fn tab_bar_layout_right(&self) -> f32 {
        let Some(renderer) = self.renderer.as_ref() else {
            return 0.0;
        };
        let full = renderer.viewport_width();
        let (Some(layout), Some(tab)) = (
            self.terminal_layout(),
            self.sessions.tab(self.sessions.active_idx()),
        ) else {
            return full;
        };
        let content = [
            layout.content.left as f32,
            layout.content.top as f32,
            layout.content.right as f32,
            layout.content.bottom as f32,
        ];
        let rects: Vec<_> = tab
            .split_tree()
            .layout(content)
            .into_iter()
            .map(|(_, rect)| rect)
            .collect();
        crate::layout::tab_strip_right_edge(full, &rects)
    }
}

#[cfg(test)]
mod tests {
    use super::{build_tab_bar_scene, tab_bar_target_at, TabBarTarget};
    use crate::layout::{layout_tab_strip, TabStripInput};

    /// Build a strip that fits `count` tabs without overflow.
    fn strip_fitting(count: usize) -> crate::layout::TabStripLayout {
        layout_tab_strip(TabStripInput {
            viewport_width: 1000.0,
            bar_height: 28.0,
            cell_width: 9.0,
            padding_x: 8.0,
            chrome_left: 0.0,
            traffic_lights_width: 72.0,
            tab_count: count,
            requested_scroll_offset: 0.0,
        })
    }

    #[test]
    fn close_button_takes_priority_over_tab_label() {
        let strip = strip_fitting(3);
        let cell_width = 9.0;
        let scene = build_tab_bar_scene(strip, 3, cell_width, 20.0);
        // Compute the close rect for tab 1 explicitly (matches CloseMetrics).
        let close_w = cell_width * 2.0;
        let label_gap = cell_width * 0.5;
        let label_w = strip.tab_width - close_w - label_gap;
        let tab1_x0 = strip.tabs_start + 1.0 * strip.tab_width - strip.scroll_offset;
        let close_cx = tab1_x0 + label_w + close_w * 0.5;
        let y = (strip.bar_rect[1] + strip.bar_rect[3]) * 0.5;
        assert_eq!(
            tab_bar_target_at(&scene, close_cx, y),
            Some(TabBarTarget::Close(1)),
            "close button should win over tab label at the same point"
        );
    }

    #[test]
    fn tab_label_hit_when_outside_close_zone() {
        let strip = strip_fitting(2);
        let scene = build_tab_bar_scene(strip, 2, 9.0, 20.0);
        let tab0_rect = strip.tab_rect(0);
        // Left edge of tab 0 (label area, before the close zone).
        let x = tab0_rect[0] + 9.0;
        let y = (tab0_rect[1] + tab0_rect[3]) * 0.5;
        assert_eq!(tab_bar_target_at(&scene, x, y), Some(TabBarTarget::Tab(0)),);
    }

    #[test]
    fn plus_button_is_clickable() {
        let strip = strip_fitting(1);
        let scene = build_tab_bar_scene(strip, 1, 9.0, 20.0);
        let cx = (strip.plus_rect[0] + strip.plus_rect[2]) * 0.5;
        let cy = (strip.plus_rect[1] + strip.plus_rect[3]) * 0.5;
        assert_eq!(
            tab_bar_target_at(&scene, cx, cy),
            Some(TabBarTarget::NewTab),
        );
    }

    #[test]
    fn arrows_present_only_on_overflow() {
        let fitting = strip_fitting(2);
        let scene = build_tab_bar_scene(fitting, 2, 9.0, 20.0);
        assert!(
            !scene
                .hits
                .iter()
                .any(|h| h.target == TabBarTarget::ArrowLeft),
            "no arrows when tabs fit"
        );

        // Overflow: many tabs in a narrow viewport.
        let overflowing = layout_tab_strip(TabStripInput {
            viewport_width: 200.0,
            bar_height: 28.0,
            cell_width: 9.0,
            padding_x: 8.0,
            chrome_left: 0.0,
            traffic_lights_width: 72.0,
            tab_count: 30,
            requested_scroll_offset: 0.0,
        });
        assert!(overflowing.overflowing);
        let scene = build_tab_bar_scene(overflowing, 30, 9.0, 20.0);
        // Arrows are pushed first, so they sit at the front and win over
        // overlapping tab rects.
        let left = overflowing.left_arrow_rect.unwrap();
        let x = (left[0] + left[2]) * 0.5;
        let y = (left[1] + left[3]) * 0.5;
        assert_eq!(
            tab_bar_target_at(&scene, x, y),
            Some(TabBarTarget::ArrowLeft),
        );
    }

    #[test]
    fn miss_outside_bar_is_none() {
        let strip = strip_fitting(2);
        let scene = build_tab_bar_scene(strip, 2, 9.0, 20.0);
        // Far below the bar.
        assert_eq!(tab_bar_target_at(&scene, 500.0, 100.0), None);
    }
}
