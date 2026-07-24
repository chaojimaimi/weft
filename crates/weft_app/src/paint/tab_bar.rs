//! Tab bar vertex builder extracted from renderer.rs (A5).

use crate::paint::primitives::{color_to_normalized, push_line, push_quad};
use crate::paint::ui_helpers::truncate_to_columns;
use crate::renderer::MetalRenderer;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TabTitle {
    pub(crate) compact: String,
    pub(crate) tooltip: String,
}

/// Build a dense tab title once, leaving final pixel truncation to the shared
/// tab-strip layout. Symmetric one-column spaces keep the separator visually
/// balanced after ambiguous-width punctuation was standardized to one cell.
pub(crate) fn tab_title(index: usize, cwd: Option<&str>, command: Option<&str>) -> TabTitle {
    let fallback = format!("Tab {}", index + 1);
    let full_cwd = cwd.unwrap_or(&fallback);
    let compact_cwd = if full_cwd.chars().count() <= 20 {
        full_cwd
    } else {
        full_cwd
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .filter(|part| !part.is_empty())
            .unwrap_or("/")
    };
    let command = command.map(str::trim).filter(|command| !command.is_empty());
    match command {
        Some(command) => TabTitle {
            compact: format!("{compact_cwd} • {command}"),
            tooltip: format!("{full_cwd} • {command}"),
        },
        None => TabTitle {
            compact: compact_cwd.to_string(),
            tooltip: full_cwd.to_string(),
        },
    }
}

/// v0.9 H1: Tab bar state passed to the renderer each frame.
#[derive(Clone, Default)]
pub struct TabBarDrawState {
    /// Number of open tabs.
    pub tab_count: usize,
    /// Index of the active tab (0-based).
    pub active_tab: usize,
    /// Tab labels (e.g., shell cwd basename or "Tab N").
    pub labels: Vec<String>,
    /// Full cwd + running command shown below a hovered tab.
    pub tooltips: Vec<String>,
    /// v0.9 W1+: index of the tab currently hovered by the mouse (0-based),
    /// or `None` when the cursor isn't over any tab. Used to show the close
    /// "×" button on hover (Warp-style) — active tab always shows "×".
    pub hovered_tab: Option<usize>,
    /// v1.2: horizontal scroll offset in physical pixels. 0 = scrolled all
    /// the way left (showing the first tab). Applied as `x0 -= scroll_offset`
    /// in `build_tab_bar_vertices`. The app clamps this to the valid range
    /// (0 .. total_tab_width - visible_width) on each frame.
    pub scroll_offset: f32,
    /// v1.2: true when the mouse is over the "+" (new tab) button. Drives a
    /// hover highlight effect on the button.
    pub plus_hovered: bool,
    /// v1.2: true when the mouse is over the left scroll arrow.
    pub arrow_left_hovered: bool,
    /// v1.2: true when the mouse is over the right scroll arrow.
    pub arrow_right_hovered: bool,
    /// Physical left edge reserved for a visible history panel. This differs
    /// from terminal `chrome_left` in Compact mode, where the panel overlays
    /// the PTY instead of resizing it.
    pub chrome_left: f32,
}

impl MetalRenderer {
    /// v0.9 H1: Build the tab bar vertices (background + tab labels + close
    /// buttons). Returns `(vertices, tab_hits)` where `tab_hits` is the
    /// click hit-test data for the app's mouse handler.
    ///
    /// Layout:
    /// - Tab bar spans the full viewport width at the top.
    /// - Each tab is ~16 cells wide, with a 1px divider between tabs.
    /// - Active tab gets a brighter background + accent underline.
    /// - Close "×" button at the right of each tab.
    pub(crate) fn build_tab_bar_vertices(&self, tab_bar: &TabBarDrawState) -> Vec<f32> {
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let bar_h = self.tab_bar_height();
        let vp_w = self.viewport.0;
        let pad_x = self.padding_x;
        let chrome_left = tab_bar.chrome_left;

        let ui = crate::ui_tokens::UiColors::from_theme(&self.theme)
            .with_increase_contrast(self.increase_contrast);
        let bg = color_to_normalized(ui.canvas);
        let fg = color_to_normalized(ui.text_primary);
        let accent = color_to_normalized(ui.focus);
        let separator = color_to_normalized(self.theme.separator);

        let bar_bg = color_to_normalized(ui.chrome);

        let mut vertices = Vec::new();

        // v1.2 architecture: renderer and App scroll/hit behavior consume the
        // same pure tab-strip layout product.
        let strip = crate::layout::layout_tab_strip(crate::layout::TabStripInput {
            viewport_width: vp_w,
            bar_height: bar_h,
            cell_width: cw,
            padding_x: pad_x,
            chrome_left,
            traffic_lights_width: self.traffic_lights_width(),
            tab_count: tab_bar.tab_count,
            requested_scroll_offset: tab_bar.scroll_offset,
        });
        let tab_w = strip.tab_width;
        let overflowing = strip.overflowing;
        let arrow_w = strip.arrow_width;
        let plus_w = strip.plus_width;
        let tabs_start = strip.tabs_start;
        let scroll_offset = strip.scroll_offset;
        let vis_left = strip.visible_left;
        let vis_right = strip.visible_right;

        push_quad(&mut vertices, strip.bar_rect, [0.0; 4], [0.0; 4], bar_bg);

        let close_w = cw * 2.0;
        // v1.2-fix: reserve a small gap between label text and close button
        // so truncated "…" doesn't overlap the × icon.
        let label_gap = cw * 0.5;
        let label_w = tab_w - close_w - label_gap;
        let y0 = 0.0f32;
        let y1 = bar_h;

        for i in 0..tab_bar.tab_count {
            // Apply scroll offset to x position.
            let x0 = tabs_start + i as f32 * tab_w - scroll_offset;
            let x1 = x0 + tab_w;

            // CPU-side cull: skip tabs entirely outside the visible region.
            if x1 < vis_left || x0 > vis_right {
                continue;
            }

            let is_active = i == tab_bar.active_tab;
            let is_hovered = tab_bar.hovered_tab == Some(i);

            // Clamp rendering to [vis_left, vis_right] so tab backgrounds
            // don't bleed under the arrows or the "+" button.
            let draw_x0 = x0.max(vis_left);
            let draw_x1 = x1.min(vis_right);

            // Tab background: active tab gets the main bg; hovered tab gets
            // a subtle highlight (Warp-style hover feedback).
            if is_active {
                push_quad(
                    &mut vertices,
                    [draw_x0, y0, draw_x1, y1],
                    [0.0; 4],
                    [0.0; 4],
                    bg,
                );
                push_quad(
                    &mut vertices,
                    [draw_x0, y1 - 2.0, draw_x1, y1],
                    [0.0; 4],
                    [0.0; 4],
                    accent,
                );
            } else if is_hovered {
                // v1.2: hover highlight — a subtle light overlay on inactive
                // tabs when the mouse is over them (matches demo behavior).
                let hover_bg = [
                    fg[0] * 0.08 + bar_bg[0] * 0.92,
                    fg[1] * 0.08 + bar_bg[1] * 0.92,
                    fg[2] * 0.08 + bar_bg[2] * 0.92,
                    1.0,
                ];
                push_quad(
                    &mut vertices,
                    [draw_x0, y0, draw_x1, y1],
                    [0.0; 4],
                    [0.0; 4],
                    hover_bg,
                );
            }

            // Divider between tabs.
            if i > 0 && x0 >= vis_left {
                push_quad(
                    &mut vertices,
                    [draw_x0, y0, draw_x0 + 1.0, y1],
                    [0.0; 4],
                    [0.0; 4],
                    separator,
                );
            }

            // ── Tab label ──
            // Text starts at x0 + cw*0.5 (clamped to vis_left). Text must end
            // before the close button area: text_right = min(x1, vis_right)
            // - close_w. This ensures "…" truncation never overlaps ×.
            let label_x = (x0 + cw * 0.5).max(vis_left);
            let text_right = x1.min(vis_right) - close_w;
            let avail_text_w = (text_right - label_x).max(0.0);
            if avail_text_w >= cw {
                let max_cols = ((avail_text_w / cw) as usize).max(1);
                let label = tab_bar.labels.get(i).map(|s| s.as_str()).unwrap_or("");
                let display = truncate_to_columns(label, max_cols.saturating_sub(1));
                let label_color = if is_active {
                    fg
                } else if is_hovered {
                    [fg[0] * 0.85, fg[1] * 0.85, fg[2] * 0.85, 1.0]
                } else {
                    [fg[0] * 0.6, fg[1] * 0.6, fg[2] * 0.6, 1.0]
                };
                self.push_text(
                    &mut vertices,
                    label_x,
                    y0 + (bar_h - ch) * 0.5,
                    &display,
                    label_color,
                    max_cols,
                );
            }

            // ── Close button ──
            // The × center cx = x0 + label_w + close_w*0.5. The × is shown
            // only when cx is inside the visible region [vis_left, vis_right]
            // — this applies to BOTH active and inactive tabs, so the × never
            // overlaps the scroll arrows, "+" button, or tab label text.
            // Inactive tabs additionally require hover.
            let close_cx = x0 + label_w + close_w * 0.5;
            let close_cy = y0 + bar_h * 0.5;
            let close_r = if is_active { ch * 0.16 } else { ch * 0.13 };
            let cx_in_view = close_cx - close_r >= vis_left && close_cx + close_r <= vis_right;
            let show_close = cx_in_view && (is_active || is_hovered);
            if show_close {
                let close_color = if is_active {
                    fg
                } else {
                    [fg[0] * 0.5, fg[1] * 0.5, fg[2] * 0.5, 1.0]
                };
                let line_w = crate::ui_tokens::UiMetrics::for_scale(self.scale).stroke;
                push_line(
                    &mut vertices,
                    close_cx - close_r,
                    close_cy - close_r,
                    close_cx + close_r,
                    close_cy + close_r,
                    line_w,
                    close_color,
                );
                push_line(
                    &mut vertices,
                    close_cx - close_r,
                    close_cy + close_r,
                    close_cx + close_r,
                    close_cy - close_r,
                    line_w,
                    close_color,
                );
            }

            // Hit rect: close_rect registered only when cx is in view.
            // Hit regions now live in the TabBar Scene (tab_bar_component.rs).
        }

        // Full cwd + command tooltip. The tab itself stays dense; hovering
        // exposes the unabridged context even when the label is ellipsized.
        if let Some(index) = tab_bar
            .hovered_tab
            .filter(|index| *index < tab_bar.tab_count)
        {
            if let Some(tooltip) = tab_bar.tooltips.get(index).filter(|text| !text.is_empty()) {
                let viewport_cols = ((vp_w - 2.0 * cw).max(cw) / cw) as usize;
                let display = truncate_to_columns(tooltip, viewport_cols.saturating_sub(2).max(1));
                let text_cols = Self::text_col_width(&display).max(1);
                let rect = crate::layout::layout_tab_tooltip(
                    strip.tab_rect(index),
                    vp_w,
                    bar_h,
                    cw,
                    ch,
                    text_cols,
                );
                let tooltip_bg = color_to_normalized(ui.raised);
                let tooltip_border = color_to_normalized(ui.border_subtle);
                let stroke = crate::ui_tokens::UiMetrics::for_scale(self.scale).stroke;
                push_quad(&mut vertices, rect, [0.0; 4], [0.0; 4], tooltip_bg);
                push_quad(
                    &mut vertices,
                    [rect[0], rect[1], rect[2], rect[1] + stroke],
                    [0.0; 4],
                    [0.0; 4],
                    tooltip_border,
                );
                push_quad(
                    &mut vertices,
                    [rect[0], rect[3] - stroke, rect[2], rect[3]],
                    [0.0; 4],
                    [0.0; 4],
                    tooltip_border,
                );
                self.push_text(
                    &mut vertices,
                    rect[0] + cw * 0.75,
                    rect[1] + ch * 0.2,
                    &display,
                    fg,
                    text_cols,
                );
            }
        }

        // v1.2: Scroll arrows — drawn when tabs overflow.
        // Opaque background quads under the arrows prevent partially-visible
        // tabs from showing through behind the arrow icons.
        if overflowing {
            let max_scroll = strip.max_scroll;
            let left_arrow_rect = strip.left_arrow_rect.expect("overflow has left arrow");
            let right_arrow_rect = strip.right_arrow_rect.expect("overflow has right arrow");

            // ── Left arrow (‹) ──
            let la_cx = tabs_start + arrow_w * 0.5;
            let la_cy = bar_h * 0.5;
            let la_r = ch * 0.14;
            let la_w = 1.5 * self.scale as f32;
            // Background: bar_bg + hover highlight if the mouse is over it.
            let la_bg = if tab_bar.arrow_left_hovered && scroll_offset > 0.0 {
                [
                    fg[0] * 0.12 + bar_bg[0] * 0.88,
                    fg[1] * 0.12 + bar_bg[1] * 0.88,
                    fg[2] * 0.12 + bar_bg[2] * 0.88,
                    1.0,
                ]
            } else {
                bar_bg
            };
            push_quad(&mut vertices, left_arrow_rect, [0.0; 4], [0.0; 4], la_bg);
            let la_color = if scroll_offset > 0.0 {
                if tab_bar.arrow_left_hovered {
                    fg
                } else {
                    [fg[0] * 0.7, fg[1] * 0.7, fg[2] * 0.7, 1.0]
                }
            } else {
                [fg[0] * 0.25, fg[1] * 0.25, fg[2] * 0.25, 1.0]
            };
            push_line(
                &mut vertices,
                la_cx + la_r,
                la_cy - la_r,
                la_cx - la_r,
                la_cy,
                la_w,
                la_color,
            );
            push_line(
                &mut vertices,
                la_cx - la_r,
                la_cy,
                la_cx + la_r,
                la_cy + la_r,
                la_w,
                la_color,
            );

            // ── Right arrow (›) ──
            let ra_cx = vis_right + arrow_w * 0.5;
            let ra_cy = bar_h * 0.5;
            let ra_r = ch * 0.14;
            let ra_w = 1.5 * self.scale as f32;
            let ra_bg = if tab_bar.arrow_right_hovered && scroll_offset < max_scroll {
                [
                    fg[0] * 0.12 + bar_bg[0] * 0.88,
                    fg[1] * 0.12 + bar_bg[1] * 0.88,
                    fg[2] * 0.12 + bar_bg[2] * 0.88,
                    1.0,
                ]
            } else {
                bar_bg
            };
            push_quad(&mut vertices, right_arrow_rect, [0.0; 4], [0.0; 4], ra_bg);
            let ra_color = if scroll_offset < max_scroll {
                if tab_bar.arrow_right_hovered {
                    fg
                } else {
                    [fg[0] * 0.7, fg[1] * 0.7, fg[2] * 0.7, 1.0]
                }
            } else {
                [fg[0] * 0.25, fg[1] * 0.25, fg[2] * 0.25, 1.0]
            };
            push_line(
                &mut vertices,
                ra_cx - ra_r,
                ra_cy - ra_r,
                ra_cx + ra_r,
                ra_cy,
                ra_w,
                ra_color,
            );
            push_line(
                &mut vertices,
                ra_cx + ra_r,
                ra_cy,
                ra_cx - ra_r,
                ra_cy + ra_r,
                ra_w,
                ra_color,
            );
        }

        // v1.2: "+" button position:
        //   - Non-overflowing: right after the last tab (natural flow).
        //   - Overflowing: after the right scroll arrow (fixed position).
        let plus_x0 = strip.plus_rect[0];
        let plus_cx = plus_x0 + plus_w * 0.5;
        let plus_cy = bar_h * 0.5;
        let plus_r = ch * 0.22;
        let plus_line_w = 1.5 * self.scale as f32;
        // v1.2: hover highlight.
        if tab_bar.plus_hovered {
            let hover_bg = [
                fg[0] * 0.12 + bar_bg[0] * 0.88,
                fg[1] * 0.12 + bar_bg[1] * 0.88,
                fg[2] * 0.12 + bar_bg[2] * 0.88,
                1.0,
            ];
            push_quad(
                &mut vertices,
                [plus_x0, y0, plus_x0 + plus_w, y1],
                [0.0; 4],
                [0.0; 4],
                hover_bg,
            );
        }
        let plus_color = if tab_bar.plus_hovered {
            fg
        } else {
            [fg[0] * 0.7, fg[1] * 0.7, fg[2] * 0.7, 1.0]
        };
        push_line(
            &mut vertices,
            plus_cx - plus_r,
            plus_cy,
            plus_cx + plus_r,
            plus_cy,
            plus_line_w,
            plus_color,
        );
        push_line(
            &mut vertices,
            plus_cx,
            plus_cy - plus_r,
            plus_cx,
            plus_cy + plus_r,
            plus_line_w,
            plus_color,
        );
        vertices
    }
}

#[cfg(test)]
mod tests {
    use super::tab_title;

    #[test]
    fn running_title_uses_balanced_separator_without_early_command_truncation() {
        let title = tab_title(
            0,
            Some("/Users/andylee/McDull/OpenCode"),
            Some("opencode upgrade"),
        );
        assert_eq!(title.compact, "OpenCode • opencode upgrade");
        assert_eq!(weft_core::grid::terminal_char_width('•'), 1);
        assert_eq!(
            title.tooltip,
            "/Users/andylee/McDull/OpenCode • opencode upgrade"
        );
    }

    #[test]
    fn long_cwd_uses_basename_in_tab_and_full_path_in_tooltip() {
        let title = tab_title(
            1,
            Some("/Users/andylee/McDull/Claude/projects/weft"),
            Some("cargo test --workspace"),
        );
        assert_eq!(title.compact, "weft • cargo test --workspace");
        assert!(title
            .tooltip
            .starts_with("/Users/andylee/McDull/Claude/projects/weft • "));
    }

    #[test]
    fn cjk_tooltip_is_truncated_by_display_columns() {
        let title = tab_title(0, Some("/用户/项目目录"), Some("运行升级命令"));
        let display = crate::paint::ui_helpers::truncate_to_columns(&title.tooltip, 12);
        let width = weft_core::grid::terminal_text_width(&display);
        assert!(width <= 12);
        assert!(display.ends_with('…'));
    }
}
