//! Responsive Settings panel geometry.

use super::Rect;

// ── Settings panel layout ───────────────────────────────────────────

/// Footer button hit rects for the settings panel.
#[derive(Clone, Copy, Debug, Default)]
pub struct FooterButtonRects {
    pub apply: Option<Rect>,
    pub close: Option<Rect>,
    pub save: Option<Rect>,
}

/// Layout product for the settings panel. Shared between
/// `build_settings_vertices` (renderer) and `mouse_press_controller` so the
/// sidebar/content/footer geometry never drifts apart.
#[derive(Clone, Debug)]
pub struct SettingsLayout {
    /// Full panel bounding box `[x0, y0, x1, y1]`.
    pub box_rect: Rect,
    /// F5: Sidebar area `[x0, y0, x1, y1]` where category list is rendered.
    /// In narrow drill-down content mode this is zero-sized (sidebar hidden).
    pub sidebar_rect: Rect,
    /// F5: Width of the sidebar column (0 when sidebar hidden).
    #[allow(dead_code)]
    pub sidebar_width: f32,
    /// F5: Top of the first sidebar category row.
    pub sidebar_top: f32,
    /// F5: True when the sidebar is visible (wide mode, or narrow sidebar view).
    pub show_sidebar: bool,
    /// F5: True when the content form is visible (wide mode, or narrow content view).
    pub show_content: bool,
    /// Legacy: tab bar Y baseline — kept for footer backward compat. F5
    /// repurposes this as the top of the sidebar/content area.
    #[allow(dead_code)]
    pub tab_bar_y: f32,
    /// Legacy: width of each tab slot — kept for backward compat. F5 doesn't
    /// use a horizontal tab bar; this is now the sidebar row pitch.
    #[allow(dead_code)]
    pub tab_width: f32,
    /// Content area X bounds (excludes sidebar when split).
    pub content_x0: f32,
    pub content_x1: f32,
    /// Top of the content area (after error banner if present).
    pub content_top: f32,
    /// Max visible rows in the content area.
    pub max_rows: usize,
    /// Footer Y baseline.
    pub footer_y: f32,
    /// Footer button rects (only clickable buttons; None if culled).
    pub footer_buttons: FooterButtonRects,
    /// v1.5.1: Profile toolbar rect `[x0, y0, x1, y1]`. Zero-sized when
    /// the content area is hidden (narrow sidebar-only view). The toolbar
    /// sits at the top of the content area, above `content_top`, and
    /// pushes the form rows down by one row height.
    pub profile_toolbar_rect: Rect,
    /// v1.5.1: Hit rect for the "New" (+) button in the profile toolbar.
    /// `None` when the toolbar is hidden or the button is culled.
    pub profile_create_button: Option<Rect>,
    /// v1.5.1: Hit rect for the "Delete" (−) button in the profile toolbar.
    /// `None` when the toolbar is hidden, no active profile to delete, or
    /// the button is culled.
    pub profile_delete_button: Option<Rect>,
}

/// F5: Narrow-window breakpoint (logical points). Below this, the settings
/// panel switches to single-column drill-down mode.
pub const SETTINGS_NARROW_THRESHOLD: f32 = 640.0;

/// Compute settings panel geometry. Returns `None` when the viewport/cell
/// dimensions are degenerate (matching the renderer's guard clause).
///
/// F5: `is_narrow` switches to single-column drill-down (sidebar OR content).
/// `drill_down` is only meaningful when `is_narrow` is true: false = show
/// sidebar, true = show content. In wide mode both are always visible.
///
/// `footer_pair_widths` provides the measured widths of the 6 footer pairs
/// (key+desc) in physical pixels — the caller computes these via
/// `text_col_width`. Pairs 1 (apply), 4 (close), 5 (save) carry hit rects.
#[allow(clippy::too_many_arguments)]
pub fn layout_settings(
    vp_w: f32,
    vp_h: f32,
    cw: f32,
    ch: f32,
    _tab_count: usize,
    has_error: bool,
    footer_pair_widths: &[f32; 6],
    is_narrow: bool,
    drill_down: bool,
) -> Option<SettingsLayout> {
    if cw <= 0.0 || ch <= 0.0 || vp_w <= 0.0 || vp_h <= 0.0 {
        return None;
    }
    let box_w = vp_w * 0.72;
    let box_h = vp_h * 0.78;
    let box_x0 = (vp_w - box_w) / 2.0;
    let box_x1 = box_x0 + box_w;
    let box_y0 = (vp_h - box_h) / 2.0;
    let box_y1 = box_y0 + box_h;

    // F5: sidebar width — wide enough for "Keybindings" (11 chars) + padding.
    // Capped at 40% of box width so the content area stays usable.
    let sidebar_w = (cw * 14.0).max(box_w * 0.28).min(box_w * 0.40);

    // F5: visibility flags. Wide mode = both visible. Narrow mode = one at a time.
    let (show_sidebar, show_content) = if is_narrow {
        (!drill_down, drill_down)
    } else {
        (true, true)
    };

    let area_top = box_y0 + ch * 2.0;
    let footer_y = box_y1 - ch * 1.5;
    let content_bottom = footer_y - ch * 0.5;
    let content_base = area_top + ch * 0.5;
    let content_top_initial = if has_error {
        content_base + ch
    } else {
        content_base
    };

    // F5: sidebar rect. In narrow mode the sidebar spans the full box width.
    let sidebar_rect = if show_sidebar {
        let sx1 = if is_narrow {
            box_x1
        } else {
            box_x0 + sidebar_w
        };
        [box_x0, area_top, sx1, content_bottom]
    } else {
        [0.0; 4]
    };

    // F5: content X bounds. In split mode content starts after sidebar.
    // In narrow sidebar-only mode, content_x0 == content_x1 (no content drawn).
    let pad_x = cw * 1.5;
    let (content_x0, content_x1) = if show_content {
        if is_narrow {
            // Narrow content mode: full box width.
            (box_x0 + pad_x, box_x1 - pad_x)
        } else {
            // Split mode: content starts after sidebar + separator.
            (box_x0 + sidebar_w + cw * 0.5, box_x1 - pad_x)
        }
    } else {
        // Content hidden — degenerate bounds.
        (box_x1, box_x1)
    };

    // The profile toolbar is slightly taller than one text row so the active
    // indicator has its own breathing room below the label.
    // The toolbar is only drawn when the content area is visible — in
    // narrow sidebar-only mode the toolbar is zero-sized.
    let (profile_toolbar_rect, profile_create_button, profile_delete_button, content_top) =
        if show_content {
            let toolbar_y0 = content_top_initial;
            let toolbar_y1 = content_top_initial + ch * 1.2;
            let toolbar = [content_x0, toolbar_y0, content_x1, toolbar_y1];
            // "+" and "-" buttons sit at the right edge of the toolbar.
            let btn_w = ch * 1.5;
            let gap = ch * 0.5;
            let create_btn = Some([
                content_x1 - btn_w * 2.0 - gap,
                toolbar_y0,
                content_x1 - btn_w - gap,
                toolbar_y1,
            ]);
            let delete_btn = Some([content_x1 - btn_w, toolbar_y0, content_x1, toolbar_y1]);
            (toolbar, create_btn, delete_btn, toolbar_y1)
        } else {
            ([0.0; 4], None, None, content_top_initial)
        };
    let content_h = (content_bottom - content_top).max(0.0);
    let max_rows = (content_h / ch).max(1.0) as usize;

    // Footer pair layout: left-to-right from content_x0, gap between pairs.
    // `footer_pair_widths` already includes the per-pair `inner` gap (key↔desc)
    // measured by the caller via text_col_width — we only add the inter-pair
    // `gap` here.
    let gap = cw * crate::settings_component::FOOTER_HINT_GAP_CELLS;
    let footer_start = if is_narrow {
        box_x0 + pad_x
    } else {
        content_x0
    };
    let footer_end = box_x1 - pad_x;
    let mut fx = footer_start;
    let mut apply = None;
    let mut close = None;
    let mut save = None;
    for (i, &pair_w) in footer_pair_widths.iter().enumerate() {
        if fx + pair_w > footer_end {
            break;
        }
        // Pairs: 0=navigate, 1=apply, 2=switch, 3=adjust, 4=close, 5=save
        match i {
            1 => apply = Some([fx, footer_y, fx + pair_w, footer_y + ch]),
            4 => close = Some([fx, footer_y, fx + pair_w, footer_y + ch]),
            5 => save = Some([fx, footer_y, fx + pair_w, footer_y + ch]),
            _ => {}
        }
        fx += pair_w + gap;
    }

    Some(SettingsLayout {
        box_rect: [box_x0, box_y0, box_x1, box_y1],
        sidebar_rect,
        sidebar_width: if show_sidebar {
            sidebar_rect[2] - sidebar_rect[0]
        } else {
            0.0
        },
        sidebar_top: area_top + ch * 0.5,
        show_sidebar,
        show_content,
        tab_bar_y: area_top,
        tab_width: ch, // repurposed as sidebar row pitch
        content_x0,
        content_x1,
        content_top,
        max_rows,
        footer_y,
        footer_buttons: FooterButtonRects { apply, close, save },
        profile_toolbar_rect,
        profile_create_button,
        profile_delete_button,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_toolbar_reserves_space_below_one_text_row() {
        let ch = 20.0;
        let layout =
            layout_settings(1200.0, 900.0, 10.0, ch, 2, false, &[80.0; 6], false, false).unwrap();
        let toolbar = layout.profile_toolbar_rect;
        assert_eq!(toolbar[3] - toolbar[1], ch * 1.2);
        assert_eq!(layout.content_top, toolbar[3]);
    }
}
