//! Tab-strip and history-sidebar chrome geometry.

use super::Rect;

// ── Tab strip ─────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub struct TabStripInput {
    pub viewport_width: f32,
    pub bar_height: f32,
    pub cell_width: f32,
    pub padding_x: f32,
    pub chrome_left: f32,
    pub traffic_lights_width: f32,
    pub tab_count: usize,
    pub requested_scroll_offset: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct TabStripLayout {
    pub bar_rect: Rect,
    pub tabs_start: f32,
    pub tab_width: f32,
    pub overflowing: bool,
    pub scroll_offset: f32,
    pub max_scroll: f32,
    pub visible_left: f32,
    pub visible_right: f32,
    pub arrow_width: f32,
    pub plus_width: f32,
    pub plus_rect: Rect,
    pub left_arrow_rect: Option<Rect>,
    pub right_arrow_rect: Option<Rect>,
}

/// Keep global tabs out of the right pane's chrome area. For a vertical split,
/// the strip stops at the nearest right edge among panes touching the leftmost
/// content edge. Horizontal splits retain the full viewport width.
pub fn tab_strip_right_edge(viewport_width: f32, pane_rects: &[Rect]) -> f32 {
    if pane_rects.len() < 2 {
        return viewport_width;
    }
    let left = pane_rects
        .iter()
        .map(|rect| rect[0])
        .fold(f32::INFINITY, f32::min);
    pane_rects
        .iter()
        .filter(|rect| (rect[0] - left).abs() < 0.5)
        .map(|rect| rect[2])
        .fold(viewport_width, f32::min)
        .clamp(0.0, viewport_width)
}

impl TabStripLayout {
    pub fn tab_rect(self, index: usize) -> Rect {
        let x0 = self.tabs_start + index as f32 * self.tab_width - self.scroll_offset;
        [
            x0.clamp(self.visible_left, self.visible_right),
            self.bar_rect[1],
            (x0 + self.tab_width).clamp(self.visible_left, self.visible_right),
            self.bar_rect[3],
        ]
    }

    pub fn scroll_offset_for_tab(self, index: usize) -> f32 {
        if !self.overflowing {
            return 0.0;
        }
        let unscrolled_x0 = self.tabs_start + index as f32 * self.tab_width;
        let x0 = unscrolled_x0 - self.scroll_offset;
        let x1 = x0 + self.tab_width;
        let target = if x0 < self.visible_left {
            unscrolled_x0 - self.visible_left
        } else if x1 > self.visible_right {
            unscrolled_x0 + self.tab_width - self.visible_right
        } else {
            self.scroll_offset
        };
        target.clamp(0.0, self.max_scroll)
    }
}

pub fn layout_tab_strip(input: TabStripInput) -> TabStripLayout {
    let max_tab_width = input.cell_width * 20.0;
    let min_tab_width = input.cell_width * 15.0;
    let arrow_width = input.cell_width * 2.5;
    let plus_width = input.cell_width * 3.0;
    // v1.13.17 S1: 保底可拖拽空白 — "+" 右侧永远保留至少这么宽的 bar 背景
    // （`tab_bar_target_at` 的 None 命中区，即现有拖拽/双击路径），即使 tab
    // 溢出填满整条 strip 也不失效（Chrome 式"永不失效的拖拽区"）。
    // cell 派生（cw*4.0）：与本函数上方 arrow=cw*2.5 / plus=cw*3.0 同一约定，
    // 随 DPI 缩放；默认字号下约合 36 逻辑 pt。
    let drag_reserve = input.cell_width * 4.0;
    let right_padding = input.padding_x * 0.5;
    let traffic_offset = if input.chrome_left > 0.0 {
        0.0
    } else {
        input.traffic_lights_width
    };
    let tabs_start = input.chrome_left + traffic_offset + input.padding_x;
    let right_reserve = plus_width + right_padding.max(drag_reserve);
    let available = (input.viewport_width - tabs_start - right_reserve).max(0.0);
    let count = input.tab_count as f32;
    let total_at_max = count * max_tab_width;
    let total_at_min = count * min_tab_width;
    let (tab_width, overflowing) = if input.tab_count == 0 || total_at_max <= available {
        (max_tab_width, false)
    } else if total_at_min <= available {
        (
            (available / count).clamp(min_tab_width, max_tab_width),
            false,
        )
    } else {
        (min_tab_width, true)
    };
    let total_tab_width = count * tab_width;
    let visible_left = if overflowing {
        tabs_start + arrow_width
    } else {
        tabs_start
    };
    let visible_right = if overflowing {
        input.viewport_width - right_reserve - arrow_width
    } else {
        input.viewport_width - right_reserve
    }
    .max(visible_left);
    let max_scroll = if overflowing {
        (total_tab_width - (visible_right - visible_left)).max(0.0)
    } else {
        0.0
    };
    let scroll_offset = input.requested_scroll_offset.clamp(0.0, max_scroll);
    let plus_x0 = if overflowing {
        visible_right + arrow_width
    } else {
        tabs_start + total_tab_width
    };
    let bar_rect = [0.0, 0.0, input.viewport_width, input.bar_height];
    TabStripLayout {
        bar_rect,
        tabs_start,
        tab_width,
        overflowing,
        scroll_offset,
        max_scroll,
        visible_left,
        visible_right,
        arrow_width,
        plus_width,
        plus_rect: [plus_x0, 0.0, plus_x0 + plus_width, input.bar_height],
        left_arrow_rect: overflowing.then_some([
            tabs_start,
            0.0,
            tabs_start + arrow_width,
            input.bar_height,
        ]),
        right_arrow_rect: overflowing.then_some([
            visible_right,
            0.0,
            visible_right + arrow_width,
            input.bar_height,
        ]),
    }
}

/// Position a one-line tab tooltip below the strip, centered on its tab and
/// clamped inside the viewport. All inputs and output use physical pixels.
pub fn layout_tab_tooltip(
    tab_rect: Rect,
    viewport_width: f32,
    bar_height: f32,
    cell_width: f32,
    cell_height: f32,
    text_cols: usize,
) -> Rect {
    let margin = cell_width.max(1.0);
    let max_width = (viewport_width - 2.0 * margin).max(cell_width);
    let width = ((text_cols as f32 + 1.5) * cell_width).min(max_width);
    let center = (tab_rect[0] + tab_rect[2]) * 0.5;
    let max_x0 = (viewport_width - margin - width).max(margin);
    let x0 = (center - width * 0.5).clamp(margin, max_x0);
    let y0 = bar_height + cell_height * 0.2;
    [x0, y0, x0 + width, y0 + cell_height * 1.4]
}

// ── Panel (history sidebar) layout ──────────────────────────────────

/// Layout product for the history sidebar. Shared between
/// `build_panel_vertices` (renderer) and `mouse_press_controller` so the
/// search-field and row geometry never drift apart.
///
/// All values are in physical pixels.
#[derive(Clone, Copy, Debug)]
pub struct PanelLayout {
    /// Full sidebar rect `[x0, y0, x1, y1]` (starts at chrome_top).
    pub panel_rect: Rect,
    /// Search input field rect (clickable → focus search).
    pub search_field_rect: Rect,
    /// v1.12.26 (P1-03): X of the query text inside the search field — the
    /// origin the query + its inline IME preedit draw from, and the panel
    /// IME candidate-window anchor column. Exposed (instead of recomputed
    /// inline) so `build_panel_vertices`, `panel_ime_area` and the pinning
    /// test read ONE formula.
    pub search_text_x: f32,
    /// v1.12.26 (P1-03): Y of the query text inside the search field
    /// (vertically centered within the field rect).
    pub search_text_y: f32,
    /// Y of the first history row's top edge.
    pub list_top: f32,
    /// Per-row height (pitch).
    pub row_height: f32,
    /// v1.11.2 X4 (PLAN_v1112 §1.3): bottom footer strip — the "load older"
    /// button target. Drawn/clickable only when the tab has blocks; the
    /// renderer and scene builder share this rect so they cannot drift.
    pub footer_rect: Rect,
}

/// Compute panel geometry from a layout context + sidebar width.
///
/// Mirrors the constants in `build_panel_vertices`:
/// - header at `chrome_top + ch*0.4`
/// - search field at `chrome_top + ch*1.6`, height `ch*1.4`
/// - list starts at `field_y1 + ch*0.4`
/// - row height (pitch) = `ch*1.1`
pub fn layout_panel(
    chrome_top: f32,
    cell_w: f32,
    cell_h: f32,
    sidebar_width: f32,
    viewport_h: f32,
) -> PanelLayout {
    let panel_x = 0.0;
    let panel_rect = [panel_x, chrome_top, panel_x + sidebar_width, viewport_h];
    let field_pad_x = cell_w * 0.5;
    let field_pad_y = cell_h * 1.6;
    let field_h = cell_h * 1.4;
    let field_y0 = chrome_top + field_pad_y;
    let field_y1 = field_y0 + field_h;
    let search_field_rect = [
        panel_x + field_pad_x,
        field_y0,
        panel_x + sidebar_width - field_pad_x,
        field_y1,
    ];
    let list_top = field_y1 + cell_h * 0.4;
    let row_height = cell_h * 1.1;
    // v1.12.26 (P1-03): query text origin — mirrors build_panel_vertices'
    // inline formula (0.4cw inset, vertically centered in the field).
    let search_text_x = search_field_rect[0] + cell_w * 0.4;
    let search_text_y = search_field_rect[1] + (field_h - cell_h) * 0.5;
    // v1.11.2 X4: footer strip anchored just above the viewport bottom.
    let footer_h = cell_h * 1.6;
    let footer_y1 = viewport_h - cell_h * 0.5;
    let footer_rect = [
        panel_x + field_pad_x,
        footer_y1 - footer_h,
        panel_x + sidebar_width - field_pad_x,
        footer_y1,
    ];
    PanelLayout {
        panel_rect,
        search_field_rect,
        search_text_x,
        search_text_y,
        list_top,
        row_height,
        footer_rect,
    }
}

/// v1.13.18 P4: bottom edge of the panel's list-row area. When the
///「加载更早」footer band is shown (criterion at call sites: the tab has
/// ANY finished block — `!blocks.is_empty()`, the same test
/// `paint/panel.rs` and the panel scene builders use; NOT the query-filtered
/// count, so paint and hit-test cannot fork) the rows must stop at the
/// band's hairline separator: its top edge sits `0.25ch` above
/// `footer_rect[1]`. Without a footer the rows may run to the viewport
/// bottom. Single source for every `rows_bottom` consumer.
pub fn panel_rows_bottom(
    layout: &PanelLayout,
    viewport_h: f32,
    footer_shown: bool,
    cell_h: f32,
) -> f32 {
    if footer_shown {
        layout.footer_rect[1] - cell_h * 0.25
    } else {
        viewport_h
    }
}

/// v1.13.18 P4: the ONE panel visible-row budget, derived from PanelLayout
/// geometry — replaces `visible_panel_rows`'s `(vp_h/ch) − 2` approximation,
/// which ignored `chrome_top` and the 1.1ch row pitch and could never
/// reconcile with the drawn geometry. Row `i` spans
/// `[list_top + i*row_height, +ch]`; the count is the largest `i + 1` whose
/// bottom edge clears `rows_bottom` (from [`panel_rows_bottom`]). Pure —
/// shared by the paint loop, the scroll clamp, the hit-test scene, the
/// accessibility scene and the atlas warm-up.
pub fn panel_visible_row_budget(
    list_top: f32,
    row_height: f32,
    ch: f32,
    rows_bottom: f32,
) -> usize {
    if row_height <= 0.0 || ch <= 0.0 {
        return 0;
    }
    let room = rows_bottom - list_top - ch;
    if room < 0.0 {
        return 0;
    }
    (room / row_height).floor() as usize + 1
}

#[cfg(test)]
mod tests {
    use super::{layout_panel, panel_rows_bottom, panel_visible_row_budget};

    // ── v1.13.18 (P4): panel_visible_row_budget ──────────────────────────

    /// A reference PanelLayout matching the snapshot geometry (ch=18).
    fn ref_layout(ch: f32, viewport_h: f32) -> super::PanelLayout {
        layout_panel(36.0, 9.0, ch, 240.0, viewport_h)
    }

    #[test]
    fn row_budget_reserves_footer_band_when_shown() {
        let ch = 18.0;
        let layout = ref_layout(ch, 600.0);
        let bottom = panel_rows_bottom(&layout, 600.0, true, ch);
        let with_footer = panel_visible_row_budget(layout.list_top, layout.row_height, ch, bottom);
        let free = panel_visible_row_budget(layout.list_top, layout.row_height, ch, 600.0);
        // The footer band (1.6ch + 0.5ch margin + 0.25ch separator inset)
        // must cost rows — a footer-bearing budget is strictly smaller.
        assert!(
            with_footer < free,
            "footer {with_footer} must free rows from {free}"
        );
        // And rows must clear the footer band's top edge: the last row's
        // bottom edge stays at or above (smaller y than) footer_rect[1].
        let last_bottom = layout.list_top + (with_footer - 1) as f32 * layout.row_height + ch;
        assert!(last_bottom <= bottom + 0.01);
        assert!(last_bottom + layout.row_height > bottom);
    }

    #[test]
    fn row_budget_runs_to_viewport_without_footer() {
        let ch = 18.0;
        let layout = ref_layout(ch, 600.0);
        let budget = panel_visible_row_budget(layout.list_top, layout.row_height, ch, 600.0);
        // First row fits (list_top + ch ≤ 600), the next one does not.
        assert!(layout.list_top + (budget - 1) as f32 * layout.row_height + ch <= 600.0);
        assert!(layout.list_top + budget as f32 * layout.row_height + ch > 600.0);
    }

    #[test]
    fn row_budget_is_zero_for_an_extremely_short_viewport() {
        let ch = 18.0;
        let layout = ref_layout(ch, 200.0);
        // rows_bottom below the first row's bottom edge → 0 rows.
        let budget = panel_visible_row_budget(layout.list_top, layout.row_height, ch, 100.0);
        assert_eq!(budget, 0);
        // Degenerate geometry (zero pitch / zero cell) never panics.
        assert_eq!(panel_visible_row_budget(10.0, 0.0, 18.0, 600.0), 0);
        assert_eq!(panel_visible_row_budget(10.0, 11.0, 0.0, 600.0), 0);
    }

    #[test]
    fn rows_bottom_tracks_the_separator_edge_only_when_shown() {
        let ch = 18.0;
        let layout = ref_layout(ch, 600.0);
        // Shown: the hairline separator's top edge above the footer band.
        let shown = panel_rows_bottom(&layout, 600.0, true, ch);
        assert!((shown - (layout.footer_rect[1] - ch * 0.25)).abs() < 0.001);
        // Hidden: the plain viewport bottom.
        assert_eq!(panel_rows_bottom(&layout, 600.0, false, ch), 600.0);
    }
}
