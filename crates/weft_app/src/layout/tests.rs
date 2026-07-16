use super::*;

/// A typical 800×600 window at 2× scale with 14pt Menlo (~7.2×16.8 px
/// cells) and 8px logical padding (16px physical).
fn sample_ctx() -> LayoutCtx {
    LayoutCtx::new((1600.0, 1200.0), 7.2, 16.8, 16.0, 16.0)
}

// ── LayoutCtx edges ──────────────────────────────────────────────────

#[test]
fn edges_use_padding() {
    let ctx = sample_ctx();
    assert_eq!(ctx.left(), 16.0);
    assert_eq!(ctx.right(), 1600.0 - 16.0);
    assert_eq!(ctx.top(), 16.0);
    assert_eq!(ctx.bottom(), 1200.0 - 16.0);
}

#[test]
fn width_and_height_exclude_padding() {
    let ctx = sample_ctx();
    assert_eq!(ctx.width(), 1600.0 - 32.0);
    assert_eq!(ctx.height(), 1200.0 - 32.0);
}

#[test]
fn col_x_and_row_y_step_by_cell_size() {
    let ctx = sample_ctx();
    assert_eq!(ctx.col_x(0), 16.0);
    assert_eq!(ctx.col_x(1), 16.0 + 7.2);
    assert_eq!(ctx.col_x(10), 16.0 + 72.0);
    assert_eq!(ctx.row_y(0), 16.0);
    assert_eq!(ctx.row_y(2), 16.0 + 33.6);
}

// ── Clip / child / visibility ───────────────────────────────────────

#[test]
fn child_intersect_parent_clip() {
    let parent = sample_ctx().child([100.0, 100.0, 1000.0, 1000.0]);
    let clip = parent.clip.expect("parent has clip");
    assert_eq!(clip, [100.0, 100.0, 1000.0, 1000.0]);

    // Child inside parent → intersection = child rect.
    let child = parent.child([200.0, 200.0, 800.0, 800.0]);
    let cclip = child.clip.expect("child has clip");
    assert_eq!(cclip, [200.0, 200.0, 800.0, 800.0]);

    // Child partially outside parent → clamped to parent.
    let overflowing = parent.child([50.0, 50.0, 1200.0, 1200.0]);
    assert_eq!(overflowing.clip.unwrap(), [100.0, 100.0, 1000.0, 1000.0]);
}

#[test]
fn is_visible_respects_clip() {
    let ctx = sample_ctx().child([100.0, 100.0, 500.0, 500.0]);
    assert!(ctx.is_visible([200.0, 200.0, 300.0, 300.0])); // inside
    assert!(!ctx.is_visible([600.0, 200.0, 700.0, 300.0])); // outside X
    assert!(ctx.is_visible([400.0, 400.0, 600.0, 600.0])); // overlapping
}

#[test]
fn is_visible_without_clip_is_true() {
    let ctx = sample_ctx();
    assert!(ctx.is_visible([0.0, 0.0, 10.0, 10.0]));
}

#[test]
fn is_visible_rejects_degenerate_rect() {
    let ctx = sample_ctx();
    assert!(!ctx.is_visible([10.0, 10.0, 10.0, 20.0])); // zero width
    assert!(!ctx.is_visible([10.0, 10.0, 20.0, 10.0])); // zero height
}

// ── Spacing tokens ──────────────────────────────────────────────────

#[test]
fn horizontal_spacing_scales_with_cell_w() {
    let ctx = sample_ctx(); // cell_w = 7.2
    assert_eq!(Spacing::xs(&ctx), 7.2 * 0.25);
    assert_eq!(Spacing::sm(&ctx), 7.2 * 0.5);
    assert_eq!(Spacing::md(&ctx), 7.2);
    assert_eq!(Spacing::lg(&ctx), 7.2 * 1.5);
    assert_eq!(Spacing::xl(&ctx), 7.2 * 2.0);
}

#[test]
fn vertical_spacing_scales_with_cell_h() {
    let ctx = sample_ctx(); // cell_h = 16.8
    assert_eq!(Spacing::row_xs(&ctx), 16.8 * 0.25);
    assert_eq!(Spacing::row_sm(&ctx), 16.8 * 0.5);
    assert_eq!(Spacing::row_md(&ctx), 16.8);
}

#[test]
fn spacing_scales_when_font_grows() {
    // Cmd+/- font zoom: cell dimensions change, spacing follows.
    let small = LayoutCtx::new((1600.0, 1200.0), 7.2, 16.8, 16.0, 16.0);
    let big = LayoutCtx::new((1600.0, 1200.0), 10.8, 25.2, 16.0, 16.0); // 1.5×
    assert!(Spacing::md(&big) > Spacing::md(&small));
    assert!(Spacing::row_md(&big) > Spacing::row_md(&small));
    // Ratio is preserved (1.5×).
    assert!((Spacing::md(&big) / Spacing::md(&small) - 1.5).abs() < 1e-5);
}

#[test]
fn zero_padding_context() {
    // A borderless context (e.g. fullscreen alt-screen) is valid.
    let ctx = LayoutCtx::new((800.0, 600.0), 8.0, 16.0, 0.0, 0.0);
    assert_eq!(ctx.left(), 0.0);
    assert_eq!(ctx.right(), 800.0);
    assert_eq!(ctx.width(), 800.0);
}

// ── Completion layout (stage 4 — U2) ──────────────────────────────
//
// Coordinate-only assertions: each case recomputes the popup rect and
// column anchors from `LayoutCtx + inputs` and checks the math, not the
// GPU output. Mirrors the geometry that `build_completion_vertices`
// (renderer.rs:1606) used to compute inline.

/// Build a layout for the common case: 3 short matches, all visible.
/// Verifies: popup_rect, column anchors, no scroll, popup fits above
/// the anchor.
#[test]
fn completion_3_items_short_text() {
    let ctx = sample_ctx(); // 1600×1200, cell 7.2×16.8, pad 16
                            // Anchor near the bottom; plenty of room above.
    let anchor_y = 1100.0;
    let box_x0 = 16.0;
    // 3 short labels, ~5 cols each. max_label_cols is computed by the
    // renderer in practice; pass it directly here.
    let max_label_cols = 5;
    let popup_max_rows = 8;

    let (start, end, shown) = completion_window(anchor_y, ctx.cell_h, popup_max_rows, 0, 3);
    assert_eq!((start, end, shown), (0, 3, 3));

    let layout = layout_completion(
        &ctx,
        start,
        end,
        max_label_cols,
        anchor_y,
        box_x0,
        0.6, // popup_width_scale
    );

    // popup_cols = 1 + 2 + max(5,10) + 2 + 10 + 1 = 26 (clamped to ≥25)
    // popup_w = 26 * 7.2 = 187.2; popup_x0=16, popup_x1 = 16+187.2=203.2
    // (well under vp_w - pad = 1584)
    assert_eq!(layout.popup_rect[0], 16.0);
    assert!((layout.popup_rect[2] - 203.2).abs() < 1e-3);
    // popup_h = 3 * 16.8 + 0.5*16.8 = 58.8; popup_top = 1100 - 58.8
    assert!((layout.popup_rect[1] - (1100.0 - 58.8)).abs() < 1e-3);
    assert_eq!(layout.popup_rect[3], 1100.0);
    // Popup top must stay inside the viewport (no overflow above).
    assert!(layout.popup_rect[1] >= ctx.top());

    // Column anchors: pad = 0.5*cw = 3.6 (Spacing::sm), icon_w = 2*cw = 14.4
    // icon_x = popup_x0 + pad = 16 + 3.6 = 19.6
    // label_x = popup_x0 + pad + icon_w = 16 + 3.6 + 14.4 = 34.0
    assert!((layout.icon_x - 19.6).abs() < 1e-3);
    assert!((layout.label_x - 34.0).abs() < 1e-3);
    // suffix_x = label_x + (min(max_label_cols, label_cols) + gap) * cw
    // max_label_cols is floored to 10 inside layout_completion; label_cols
    // = 26 - 16 = 10; min(10, 10) = 10; suffix_x = label_x + 12*cw
    assert_eq!(layout.label_cols, 10);
    assert_eq!(layout.suffix_cols, 10);
    let expected_suffix_x = layout.label_x + 12.0 * ctx.cell_w;
    assert!((layout.suffix_x - expected_suffix_x).abs() < 1e-3);
}

/// 10 items, popup_max_rows=8: window scrolls to keep `selected` in
/// view when it exceeds the visible range. Also verifies that the
/// popup height equals `shown` rows (not the full item count).
#[test]
fn completion_10_items_scrolls_to_keep_selected_visible() {
    let ctx = sample_ctx();
    let anchor_y = 1100.0;
    let popup_max_rows = 8;

    // selected = 9 (last item) → window should snap to show rows 2..10.
    let (start, end, shown) = completion_window(anchor_y, ctx.cell_h, popup_max_rows, 9, 10);
    assert_eq!(start, 2);
    assert_eq!(end, 10);
    assert_eq!(shown, 8);

    let layout = layout_completion(&ctx, start, end, 10, anchor_y, 16.0, 0.6);
    // popup_h = 8 * 16.8 + 8.4 = 142.8
    assert!((layout.popup_rect[1] - (1100.0 - 142.8)).abs() < 1e-3);
    assert_eq!(layout.popup_rect[3], 1100.0);
    // Still fits inside the viewport (no overflow).
    assert!(layout.popup_rect[1] >= ctx.top());
}

/// An extremely long label drives popup_cols above the viewport cap,
/// forcing a clamp. Verifies popup_x1 never exceeds `vp_w - padding_x`.
#[test]
fn completion_long_label_clamps_to_viewport_width() {
    let ctx = sample_ctx(); // vp_w=1600, padding_x=16
    let anchor_y = 1100.0;

    // max_label_cols = 200 → popup_cols would be 1+2+200+2+10+1 = 216
    // popup_max_cols = (1600 * 0.6) / 7.2 = 133
    // → popup_cols clamps to 133
    let layout = layout_completion(&ctx, 0, 3, 200, anchor_y, 16.0, 0.6);
    // popup_x1 must not exceed vp_w - padding_x = 1584
    assert!(layout.popup_rect[2] <= 1600.0 - 16.0 + 1e-3);
    // And popup_cols >= 25 (the floor).
    // Width = 133 * 7.2 = 957.6
    assert!((layout.popup_rect[2] - (16.0 + 957.6)).abs() < 1e-3);
}

/// Anchor very close to the top: avail_rows becomes 1, so even with
/// many matches only 1 row shows. Verifies the window's `max_rows`
/// clamp doesn't panic on small `anchor_y` and produces a valid
/// (degenerate) popup_rect.
#[test]
fn completion_near_top_edge_shows_one_row() {
    let ctx = sample_ctx();
    // anchor_y = 1 ch + a tiny bit → avail_rows = ceil(20.0/16.8) - 1 = 1
    let anchor_y = 20.0;
    let popup_max_rows = 8;

    let (start, end, shown) = completion_window(anchor_y, ctx.cell_h, popup_max_rows, 0, 5);
    assert_eq!(shown, 1);
    assert_eq!((start, end), (0, 1));

    let layout = layout_completion(&ctx, start, end, 8, anchor_y, 16.0, 0.6);
    // popup_h = 1 * 16.8 + 8.4 = 25.2; popup_top = 20 - 25.2 = -5.2
    // (slightly above viewport — the renderer's top-pad accounts for
    // this; what matters for the test is that the math is stable and
    // the row sits flush with the anchor.)
    assert!((layout.popup_rect[1] - -5.2).abs() < 1e-3);
    assert_eq!(layout.popup_rect[3], 20.0);
    assert_eq!(layout.end - layout.start, 1);
}

// ── Command Palette layout (stage 4 — U2) ──────────────────────────

/// Empty query + few entries: popup centers horizontally, results
/// window covers all entries, no scroll.
#[test]
fn palette_search_basic_centered_popup() {
    let ctx = sample_ctx(); // vp 1600×1200, cw 7.2, ch 16.8
                            // 5 entries, selected 0, max 8 rows
    let layout = layout_palette_search(&ctx, 5, 0, 8, 0.6);

    // popup_w = 1600 * 0.6 = 960; x0 = (1600-960)/2 = 320; x1 = 1280
    assert!((layout.popup_rect[0] - 320.0).abs() < 1e-3);
    assert!((layout.popup_rect[2] - 1280.0).abs() < 1e-3);
    // popup_top = 1200 * 0.15 = 180
    assert!((layout.popup_rect[1] - 180.0).abs() < 1e-3);
    // popup_h = (5 + 2) * 16.8 + 8.4 = 126; bottom = 180 + 126 = 306
    assert!((layout.popup_rect[3] - 306.0).abs() < 1e-3);
    // query_y = popup_top + 0.5*ch = 180 + 8.4 = 188.4
    assert!((layout.query_y - 188.4).abs() < 1e-3);
    // sep_y = query_y + ch = 188.4 + 16.8 = 205.2
    assert!((layout.sep_y - 205.2).abs() < 1e-3);
    // results_y = sep_y + ch = 222.0
    assert!((layout.results_y - 222.0).abs() < 1e-3);
    // query_x = popup_x0 + 0.5*cw = 320 + 3.6 = 323.6
    assert!((layout.query_x - 323.6).abs() < 1e-3);
    // suffix_x = popup_x1 - 0.5*cw - 10*cw = 1280 - 3.6 - 72 = 1204.4
    assert!((layout.suffix_x - 1204.4).abs() < 1e-3);
    // Window: all 5 visible.
    assert_eq!((layout.start, layout.end), (0, 5));
}

/// Long query drives results to scroll. selected=20 with 25 entries,
/// max_rows=8 → window snaps to keep selected at the bottom.
#[test]
fn palette_search_scrolls_long_results() {
    let ctx = sample_ctx();
    // 25 entries, selected 20, max 8 rows.
    let layout = layout_palette_search(&ctx, 25, 20, 8, 0.6);
    // start = 20.saturating_sub(7) = 13; end = min(13+8, 25) = 21
    assert_eq!(layout.start, 13);
    assert_eq!(layout.end, 21);
    assert_eq!(layout.end - layout.start, 8);
    // Popup height grows with shown=8, not entries=25.
    // popup_h = (8 + 2) * 16.8 + 8.4 = 176.4; bottom = 180 + 176.4 = 356.4
    assert!((layout.popup_rect[3] - 356.4).abs() < 1e-3);
}

/// Workflow form mode: popup rect grows with field count, shares the
/// same horizontal centering as search mode.
#[test]
fn palette_form_rect_grows_with_fields() {
    let ctx = sample_ctx();
    // 2 fields: popup_h = (2 + 3) * 16.8 + 8.4 = 92.4; bottom = 272.4
    let r2 = layout_palette_form_rect(&ctx, 2, 0.6);
    assert!((r2[0] - 320.0).abs() < 1e-3); // same x as search
    assert!((r2[2] - 1280.0).abs() < 1e-3);
    assert!((r2[1] - 180.0).abs() < 1e-3); // popup_top = vp_h * 0.15
    assert!((r2[3] - 272.4).abs() < 1e-3);

    // 5 fields: popup_h = (5 + 3) * 16.8 + 8.4 = 142.8; bottom = 322.8
    let r5 = layout_palette_form_rect(&ctx, 5, 0.6);
    assert!((r5[3] - 322.8).abs() < 1e-3);
    // X range unchanged — form shares search's horizontal centering.
    assert_eq!(r5[0], r2[0]);
    assert_eq!(r5[2], r2[2]);
}

// ── Tab strip layout ────────────────────────────────────────────────

fn tab_input(tab_count: usize, requested_scroll_offset: f32) -> TabStripInput {
    TabStripInput {
        viewport_width: 1200.0,
        bar_height: 56.0,
        cell_width: 8.0,
        padding_x: 10.0,
        chrome_left: 0.0,
        traffic_lights_width: 72.0,
        tab_count,
        requested_scroll_offset,
    }
}

#[test]
fn tab_strip_uses_three_tier_width_and_clamps_scroll() {
    let three = layout_tab_strip(tab_input(3, 100.0));
    assert!(!three.overflowing);
    assert_eq!(three.tab_width, 160.0);
    assert_eq!(three.scroll_offset, 0.0);

    let ten = layout_tab_strip(tab_input(10, 10_000.0));
    assert!(ten.overflowing);
    assert_eq!(ten.tab_width, 120.0);
    assert_eq!(ten.scroll_offset, ten.max_scroll);
    assert!(ten.left_arrow_rect.is_some());
    assert!(ten.right_arrow_rect.is_some());
    assert_eq!(ten.plus_rect[0], ten.visible_right + ten.arrow_width);
}

#[test]
fn tab_strip_reveals_active_tab_using_rendered_bounds() {
    let layout = layout_tab_strip(tab_input(10, 0.0));
    let offset = layout.scroll_offset_for_tab(9);
    assert!(offset > 0.0);
    let revealed = layout_tab_strip(tab_input(10, offset));
    let rect = revealed.tab_rect(9);
    assert!(rect[0] >= revealed.visible_left);
    assert!(rect[2] <= revealed.visible_right);
}

#[test]
fn sidebar_replaces_traffic_light_offset_for_tabs() {
    let mut input = tab_input(2, 0.0);
    input.chrome_left = 240.0;
    let layout = layout_tab_strip(input);
    assert_eq!(layout.tabs_start, 250.0);
}

#[test]
fn zero_tabs_and_extremely_narrow_viewport_remain_finite() {
    let zero = layout_tab_strip(TabStripInput {
        tab_count: 0,
        viewport_width: 80.0,
        ..tab_input(0, f32::INFINITY)
    });
    assert!(!zero.overflowing);
    assert_eq!(zero.scroll_offset, 0.0);
    assert!(zero.plus_rect.iter().all(|value| value.is_finite()));

    let narrow = layout_tab_strip(TabStripInput {
        viewport_width: 80.0,
        ..tab_input(4, 500.0)
    });
    assert!(narrow.overflowing);
    assert!(narrow.visible_right >= narrow.visible_left);
    assert!(narrow.max_scroll.is_finite());
    assert!(narrow.scroll_offset.is_finite());
    assert!(narrow.tab_rect(0).iter().all(|value| value.is_finite()));
}

#[test]
fn supported_minimum_window_keeps_tab_controls_inside_viewport() {
    let layout = layout_tab_strip(TabStripInput {
        viewport_width: crate::ui_tokens::MIN_WINDOW_WIDTH as f32,
        ..tab_input(4, 500.0)
    });
    assert!(layout.overflowing);
    assert!(layout.plus_rect[0] >= 0.0);
    assert!(layout.plus_rect[2] <= layout.bar_rect[2]);
    for rect in [layout.left_arrow_rect, layout.right_arrow_rect]
        .into_iter()
        .flatten()
    {
        assert!(rect[0] >= 0.0);
        assert!(rect[2] <= layout.bar_rect[2]);
    }
}

// ── Context menu layout (stage 4 — U2) ──────────────────────────────

/// Click in the middle of the viewport: menu anchored at click, items
/// stacked downward, separators between items (not after last).
#[test]
fn context_menu_basic_click() {
    let ctx = sample_ctx(); // vp 1600×1200, cw 7.2, ch 16.8, scale 2
    let x = 800.0;
    let y = 600.0;
    let scale = 2.0;
    let layout = layout_context_menu(&ctx, x, y, scale);

    // menu_w = 180 * 2 = 360; menu_h = 4 * (16.8*1.2) + 16.8*0.4
    //                  = 4 * 20.16 + 6.72 = 80.64 + 6.72 = 87.36
    // 800 + 360 = 1160 ≤ 1600 - 4 → no clamp
    assert!((layout.menu_rect[0] - 800.0).abs() < 1e-3);
    assert!((layout.menu_rect[1] - 600.0).abs() < 1e-3);
    assert!((layout.menu_rect[2] - 1160.0).abs() < 1e-3);
    assert!((layout.menu_rect[3] - 687.36).abs() < 1e-3);
    // item_y[0] = 600 + 0.2*16.8 = 603.36
    // item_y[1] = 603.36 + 20.16 = 623.52
    // item_y[2] = 603.36 + 40.32 = 643.68
    // item_y[3] = 603.36 + 60.48 = 663.84
    assert!((layout.item_y[0] - 603.36).abs() < 1e-3);
    assert!((layout.item_y[1] - 623.52).abs() < 1e-3);
    assert!((layout.item_y[2] - 643.68).abs() < 1e-3);
    assert!((layout.item_y[3] - 663.84).abs() < 1e-3);
    // separator_ys = [item_y[0]+20.16, item_y[1]+20.16, item_y[2]+20.16]
    assert!((layout.separator_ys[0] - 623.52).abs() < 1e-3);
    assert!((layout.separator_ys[1] - 643.68).abs() < 1e-3);
    assert!((layout.separator_ys[2] - 663.84).abs() < 1e-3);
    // text_x = menu_x0 + 0.4*cw = 800 + 2.88 = 802.88
    assert!((layout.text_x - 802.88).abs() < 1e-3);
    assert_eq!(layout.item_at(900.0, 610.0), Some(0));
    assert_eq!(layout.item_at(900.0, 630.0), Some(1));
    assert_eq!(layout.item_at(900.0, 670.0), Some(3));
    assert_eq!(layout.item_at(700.0, 610.0), None);
    assert_eq!(layout.item_at(900.0, 690.0), None);
}

#[test]
fn find_layout_preserves_popup_and_button_geometry() {
    let ctx = LayoutCtx::new((1000.0, 700.0), 9.0, 20.0, 8.0, 8.0);
    let layout = layout_find(&ctx, 3);
    assert_eq!(layout.popup_rect, [472.0, 18.0, 972.0, 54.0]);
    assert_eq!(layout.line_y, 26.0);
    assert_eq!(layout.regex_rect, [943.0, 24.0, 965.0, 48.0]);
    assert_eq!(layout.case_rect, [916.0, 24.0, 938.0, 48.0]);
    assert_eq!(layout.down_rect, Some([889.0, 24.0, 911.0, 48.0]));
    assert_eq!(layout.up_rect, Some([862.0, 24.0, 884.0, 48.0]));
}

/// Click near right edge: menu clamps left so its right edge stays
/// inside the viewport with a 4px gutter.
#[test]
fn context_menu_clamps_to_right_edge() {
    let ctx = sample_ctx(); // vp_w = 1600
    let scale = 2.0; // menu_w = 360
                     // Click at x=1500; without clamp menu_x1 would be 1860 > 1596.
    let layout = layout_context_menu(&ctx, 1500.0, 100.0, scale);
    // menu_x0 = min(1500, 1600 - 360 - 4) = min(1500, 1236) = 1236
    assert!((layout.menu_rect[0] - 1236.0).abs() < 1e-3);
    assert!((layout.menu_rect[2] - 1596.0).abs() < 1e-3); // 1236 + 360
                                                          // Right edge must stay inside viewport.
    assert!(layout.menu_rect[2] <= 1600.0 - 4.0 + 1e-3);
}

/// Click at negative X (rare, but possible during drag): clamped to 0.
#[test]
fn context_menu_clamps_negative_x_to_zero() {
    let ctx = sample_ctx();
    let layout = layout_context_menu(&ctx, -50.0, 100.0, 2.0);
    assert!(layout.menu_rect[0] >= 0.0);
    assert!((layout.menu_rect[0] - 0.0).abs() < 1e-3);
}

/// v0.9 fix: when the click is near the bottom of the viewport, the menu
/// flips upward so it doesn't get clipped.
#[test]
fn context_menu_flips_upward_near_bottom() {
    let ctx = sample_ctx(); // vp_h = 1200
    let scale = 2.0; // menu_h = 4 * (16.8*1.2) + 16.8*0.4 = 87.36
                     // Click at y=1180 (near bottom): 1180 + 87.36 = 1267.36 > 1196 → flip.
    let layout = layout_context_menu(&ctx, 100.0, 1180.0, scale);
    // menu_y0 = 1180 - 87.36 = 1092.64
    assert!((layout.menu_rect[1] - 1092.64).abs() < 1e-3);
    assert!(layout.menu_rect[3] <= 1200.0 - 4.0 + 1e-3);
}

/// When there's enough space below, the menu opens downward (no flip).
#[test]
fn context_menu_opens_downward_with_space() {
    let ctx = sample_ctx();
    let scale = 2.0;
    // Click at y=500: 500 + 87.36 = 587.36 < 1196 → no flip.
    let layout = layout_context_menu(&ctx, 100.0, 500.0, scale);
    assert!((layout.menu_rect[1] - 500.0).abs() < 1e-3);
}

// ── Prompt layout (stage 4 — U2) ────────────────────────────────────

/// Single-line input: box hugs the bottom of the viewport, the prompt
/// glyph sits at `left`, and the caret sits right after "❯ " when the
/// cursor is at col 0 line 0.
#[test]
fn prompt_single_line_caret_after_prompt_glyph() {
    let ctx = sample_ctx(); // vp 1600×1200, cw 7.2, ch 16.8, pad 16
                            // 1 line, cursor at (0, 0) — caret right after "❯ ".
    let layout = layout_prompt(&ctx, 1, 0, 0, 0);

    // box_h = ch * (1 + 2) = 50.4; box_y1 = 1200 - 16 = 1184;
    // box_y0 = 1184 - 50.4 = 1133.6
    assert!((layout.box_rect[1] - 1133.6).abs() < 1e-3);
    assert!((layout.box_rect[3] - 1184.0).abs() < 1e-3);
    assert_eq!(layout.box_rect[0], 16.0);
    assert_eq!(layout.box_rect[2], 1600.0 - 16.0);
    // text_y0 = box_y0 + ch = 1133.6 + 16.8 = 1150.4
    assert!((layout.text_y0 - 1150.4).abs() < 1e-3);
    // left = padding_x = 16
    assert_eq!(layout.left, 16.0);
    // box_cols = (1584 - 16) / 7.2 = 1568 / 7.2 = 217.77... → 217
    assert_eq!(layout.box_cols, 217);
    // first_line_text_x = left + 2*cw = 16 + 14.4 = 30.4
    assert!((layout.first_line_text_x - 30.4).abs() < 1e-3);
    // cursor at (0, 0) → cursor_offset_cols = 0; cx = first_line_text_x
    assert!((layout.cursor_x - 30.4).abs() < 1e-3);
    // cursor_y = text_y0 + 0*ch = 1150.4
    assert!((layout.cursor_y - 1150.4).abs() < 1e-3);
    // bar_w = max(7.2 * 0.12, 2.0) = max(0.864, 2.0) = 2.0
    assert!((layout.bar_w - 2.0).abs() < 1e-3);
}

/// Multi-line buffer (5 lines), cursor at the last line: caret Y steps
/// down by `ch` per line; X starts at `left` (not `first_line_text_x`)
/// because the prompt glyph only occupies line 0.
#[test]
fn prompt_multi_line_cursor_on_last_line() {
    let ctx = sample_ctx();
    // 5 lines, cursor at (4, 0)
    let layout = layout_prompt(&ctx, 5, 4, 0, 0);

    // box_h = ch * (5 + 2) = 117.6; box_y0 = 1184 - 117.6 = 1066.4
    assert!((layout.box_rect[1] - 1066.4).abs() < 1e-3);
    // text_y0 = 1066.4 + 16.8 = 1083.2
    assert!((layout.text_y0 - 1083.2).abs() < 1e-3);
    // cursor_y = text_y0 + 4*ch = 1083.2 + 67.2 = 1150.4
    assert!((layout.cursor_y - 1150.4).abs() < 1e-3);
    // cursor_line != 0 → text_start_x = left; cursor_offset_cols = 0
    // → cx = left = 16
    assert!((layout.cursor_x - 16.0).abs() < 1e-3);
}

/// CJK input: cursor after "Weft项目" (5 chars, but 项目 = 4 cols) must
/// land at `first_line_text_x + (5+4) * cw`. Verifies that the
/// pre-computed `cursor_offset_cols` (not raw char count) drives the
/// caret X — the original v0.8 fix for "Weft项目设计.md".
#[test]
fn prompt_cjk_input_uses_display_col_width() {
    let ctx = sample_ctx();
    // "Weft项目" = 4 ASCII (4 cols) + 2 CJK (4 cols) = 8 display cols.
    let cursor_offset_cols = 8;
    let layout = layout_prompt(&ctx, 1, 0, cursor_offset_cols, 0);
    // cx = first_line_text_x + 8 * cw = 30.4 + 57.6 = 88.0
    assert!((layout.cursor_x - 88.0).abs() < 1e-3);
}

/// M1: `box_rect` must be invariant to cursor position — the prompt
/// input box bounds depend only on `n_lines` + `LayoutCtx`, never on
/// where the caret is. This validates that the geometry_controller's
/// `prompt_box_rect()` (which passes cursor=(0,0)) produces the same
/// box_rect as the draw path (which passes the actual cursor).
#[test]
fn prompt_box_rect_invariant_to_cursor() {
    let ctx = sample_ctx();
    for n_lines in [1, 3, 10] {
        let a = layout_prompt(&ctx, n_lines, 0, 0, 0);
        let b = layout_prompt(&ctx, n_lines, n_lines - 1, 42, 0);
        assert_eq!(a.box_rect, b.box_rect, "n_lines={n_lines}");
    }
}

/// F2 P0-1: when n_lines exceeds the 30% viewport clamp, the box height
/// is capped and `visible_rows` < n_lines. The caret Y is offset by
/// `scroll_offset` so it stays inside the visible window.
#[test]
fn prompt_clamps_box_height_and_offsets_cursor() {
    let ctx = sample_ctx(); // vp_h=1200, ch=16.8 → max_box_h=360
                            // raw_box_h = 16.8 * (30 + 2) = 537.6 > 360 → clamped.
                            // visible_rows = floor(360 / 16.8) - 2 = 21 - 2 = 19.
    let n_lines = 30;
    let layout = layout_prompt(&ctx, n_lines, 25, 0, 20);
    assert_eq!(layout.visible_rows, 19);
    // cursor_y = text_y0 + (25 - 20) * ch = text_y0 + 5 * 16.8
    // (caret on the 5th visible row, inside the window).
    let expected_cy = layout.text_y0 + 5.0 * 16.8;
    assert!((layout.cursor_y - expected_cy).abs() < 1e-3);
}

#[test]
fn prompt_hit_row_is_clamped_to_visible_scrolled_window() {
    let ctx = sample_ctx();
    let layout = layout_prompt(&ctx, 30, 25, 0, 20);
    assert_eq!(prompt_line_at_y(&layout, layout.box_rect[1], 20, 30), 20);
    assert_eq!(
        prompt_line_at_y(&layout, layout.box_rect[3], 20, 30),
        29.min(20 + layout.visible_rows - 1)
    );
}

#[test]
fn block_visible_rows_use_clamped_prompt_height() {
    let ctx = sample_ctx();
    let thirty = block_visible_rows(&ctx, 30, true);
    let hundred = block_visible_rows(&ctx, 100, true);
    assert_eq!(thirty, hundred);
    assert!(thirty > 0);
}

#[test]
fn cwd_header_remains_reserved_while_a_command_runs() {
    assert!(block_cwd_header_active(true, true));
    assert!(block_cwd_header_active(false, true));
    assert!(!block_cwd_header_active(true, false));
    assert!(!block_cwd_header_active(false, false));
}

// ── Block view layout (stage 4 — U2) ────────────────────────────────

/// Editor mode (cwd_header_active=true): clip_bottom retreats by 2
/// pitches (one for CWD text, one for the divider above it), and
/// fixed_cwd_y sits one pitch above region_bottom_y.
#[test]
fn block_view_editor_mode_reserves_cwd_band() {
    let ctx = sample_ctx(); // ch=16.8, pitch=16.8*1.1=18.48
    let region_bottom_y = 1100.0;
    let layout = layout_block_view(&ctx, region_bottom_y, true);

    // pitch = 16.8 * 1.1 = 18.48
    assert!((layout.pitch - 18.48).abs() < 1e-3);
    // left = padding_x = 16; right = vp_w - 16 = 1584
    assert_eq!(layout.left, 16.0);
    assert_eq!(layout.right, 1584.0);
    // cols = (1584 - 16) / 7.2 = 217
    assert_eq!(layout.cols, 217);
    // clip_top = padding_y = 16
    assert_eq!(layout.clip_top, 16.0);
    // content_bottom_y = 1100 - 2*18.48 = 1063.04
    assert!((layout.clip_bottom - 1063.04).abs() < 1e-3);
    // fixed_cwd_y = 1100 - 18.48 = 1081.52
    assert!((layout.fixed_cwd_y - 1081.52).abs() < 1e-3);
}

/// CommandExecuting mode (cwd_header_active=false): clip_bottom sits
/// flush with region_bottom_y, fixed_cwd_y is unused (0.0).
#[test]
fn block_view_command_mode_no_cwd_band() {
    let ctx = sample_ctx();
    let region_bottom_y = 1100.0;
    let layout = layout_block_view(&ctx, region_bottom_y, false);
    // No CWD reservation: content goes all the way to region_bottom_y.
    assert_eq!(layout.clip_bottom, 1100.0);
    // fixed_cwd_y is meaningless in this mode — renderers must check
    // the input `cwd_header_active` flag, not this field.
    assert_eq!(layout.fixed_cwd_y, 0.0);
}

/// Sticky header Y equals clip_top (the renderer uses clip_top to
/// anchor the sticky block header at the top of the viewport).
#[test]
fn block_view_sticky_y_anchors_to_clip_top() {
    let ctx = sample_ctx();
    let layout = layout_block_view(&ctx, 1100.0, true);
    // The renderer's sticky header draws at y = layout.clip_top (= pad_y).
    assert_eq!(layout.clip_top, 16.0);
}

#[test]
fn tab_tooltip_clamps_at_both_viewport_edges() {
    let left = layout_tab_tooltip([0.0, 0.0, 120.0, 32.0], 600.0, 32.0, 8.0, 16.0, 40);
    assert_eq!(left[0], 8.0);
    assert!(left[2] <= 592.0);

    let right = layout_tab_tooltip([520.0, 0.0, 600.0, 32.0], 600.0, 32.0, 8.0, 16.0, 40);
    assert!(right[0] >= 8.0);
    assert_eq!(right[2], 592.0);
    assert!(right[1] >= 32.0);
}
