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
    let right_padding = input.padding_x * 0.5;
    let traffic_offset = if input.chrome_left > 0.0 {
        0.0
    } else {
        input.traffic_lights_width
    };
    let tabs_start = input.chrome_left + traffic_offset + input.padding_x;
    let right_reserve = plus_width + right_padding;
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
        list_top,
        row_height,
        footer_rect,
    }
}
