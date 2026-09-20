//! Unit tests for `paint/grid.rs` (B3-2 background-pane incremental row
//! cache + the pre-existing helper probes). Extracted from `grid.rs` to keep
//! the production file under the 800-line architecture gate — same pattern as
//! `paint/grid_instances/{contrast,style,startup_replay}_tests.rs`.

use super::{
    alt_screen_cursor_color, grid_content_origin_x, primary_screen_mask_changed,
    primary_screen_row_hidden,
};
use crate::layout::LayoutCtx;
use weft_core::grid::CursorStyle;

#[test]
fn primary_screen_mask_hides_unowned_rows_without_mutating_the_grid() {
    let owned = [false, true, false, true];
    assert!(primary_screen_row_hidden(0, Some(1), Some(&owned)));
    assert!(!primary_screen_row_hidden(1, Some(1), Some(&owned)));
    assert!(primary_screen_row_hidden(2, Some(1), Some(&owned)));
    assert!(!primary_screen_row_hidden(3, Some(1), Some(&owned)));
    assert!(!primary_screen_row_hidden(3, None, None));
}

#[test]
fn moving_or_removing_the_primary_screen_mask_invalidates_cached_rows() {
    assert!(primary_screen_mask_changed(Some(5), Some(2)));
    assert!(primary_screen_mask_changed(Some(5), None));
    assert!(primary_screen_mask_changed(None, Some(5)));
    assert!(!primary_screen_mask_changed(Some(5), Some(5)));
    assert!(!primary_screen_mask_changed(None, None));
}

// ── v1.10.4: alt-screen cursor color softening ──────────────────────

#[test]
fn alt_screen_blends_underline_cursor_toward_foreground() {
    let cursor = [0.94, 0.83, 0.66, 1.0]; // #f0d4a8 amber-white
    let fg = [0.88, 0.83, 0.77, 1.0]; // #e0d4c4 warm cream
    let softened = alt_screen_cursor_color(cursor, fg, true, CursorStyle::Underline);
    // 50% blend: each channel = (cursor + fg) / 2
    for i in 0..3 {
        assert!((softened[i] - (cursor[i] + fg[i]) * 0.5).abs() < 1e-6);
    }
    assert_eq!(softened[3], cursor[3], "alpha unchanged");
}

#[test]
fn alt_screen_keeps_block_cursor_at_full_intensity() {
    let cursor = [0.94, 0.83, 0.66, 1.0];
    let fg = [0.88, 0.83, 0.77, 1.0];
    let result = alt_screen_cursor_color(cursor, fg, true, CursorStyle::Block);
    assert_eq!(
        result, cursor,
        "Block cursor keeps full intensity in alt-screen"
    );
}

#[test]
fn primary_screen_keeps_cursor_at_full_intensity() {
    let cursor = [0.94, 0.83, 0.66, 1.0];
    let fg = [0.5, 0.5, 0.5, 1.0];
    let result = alt_screen_cursor_color(cursor, fg, false, CursorStyle::Underline);
    assert_eq!(
        result, cursor,
        "non-alt-screen keeps cursor at full intensity"
    );
}

#[test]
fn alt_screen_bar_cursor_also_blended() {
    let cursor = [0.94, 0.83, 0.66, 1.0];
    let fg = [0.2, 0.2, 0.2, 1.0];
    let result = alt_screen_cursor_color(cursor, fg, true, CursorStyle::Bar);
    assert_ne!(result, cursor, "Bar cursor is softened in alt-screen");
    assert!(result[0] < cursor[0], "blended toward darker fg");
}

// ── v1.10.19: grid ↔ BlockView content x alignment (Fix A) ──────────

/// v1.10.19: at the same geometry, the inset grid origin and the
/// BlockView content left edge must coincide — scrolling a primary-screen
/// TUI up into `primary_history_view` switches grid → BlockView and
/// every column must stay at the same physical x (no 1.5-col shift).
#[test]
fn grid_origin_x_matches_block_view_content_left_at_same_geometry() {
    let ctx = LayoutCtx {
        viewport: (1080.0, 720.0),
        cell_w: 10.0,
        cell_h: 20.0,
        padding_x: 8.0,
        padding_y: 8.0,
        chrome_top: 28.0,
        chrome_left: 60.0,
        pane_origin: (0.0, 0.0),
        clip: None,
    };
    let (block_left, _) = crate::layout::block_content_x_bounds(&ctx);
    assert_eq!(
        grid_content_origin_x(&ctx, true),
        block_left,
        "inset grid origin == BlockView content left edge"
    );
    // The inset is exactly the BlockView gutter (1.5 cells at this size).
    assert!((grid_content_origin_x(&ctx, true) - ctx.left() - ctx.cell_w * 1.5).abs() < 1e-6);
    // Alt-screen TUIs keep the pane edge (edge-to-edge, no gutter).
    assert_eq!(grid_content_origin_x(&ctx, false), ctx.left());
}

/// v1.10.19: the same alignment must hold for a split-pane context
/// (pane_origin + clip): grid and BlockView are both pane-local.
#[test]
fn grid_origin_alignment_holds_for_pane_local_context() {
    let ctx = LayoutCtx {
        viewport: (1080.0, 720.0),
        cell_w: 10.0,
        cell_h: 20.0,
        padding_x: 8.0,
        padding_y: 8.0,
        chrome_top: 28.0,
        chrome_left: 0.0,
        pane_origin: (0.0, 0.0),
        clip: None,
    };
    let pane_ctx = ctx.for_pane([100.0, 100.0, 600.0, 500.0]);
    let (block_left, _) = crate::layout::block_content_x_bounds(&pane_ctx);
    assert_eq!(grid_content_origin_x(&pane_ctx, true), block_left);
    assert_eq!(grid_content_origin_x(&pane_ctx, false), pane_ctx.left());
}

// ── v1.12.2 B3-2: background-pane incremental row cache ─────────────

/// Mirror the golden skip precedent: no Metal device (CI without GPU)
/// skips instead of failing.
fn headless_renderer_or_skip() -> Option<crate::renderer::MetalRenderer> {
    metal::Device::system_default()?;
    Some(crate::renderer::MetalRenderer::new_headless_paint(
        weft_core::config::Theme::weft_warm(),
    ))
}

/// B3-2 core scenario: first build is full, an idle pane rebuilds
/// nothing, and one written PTY line rebuilds exactly one row.
#[test]
fn background_pane_rebuilds_only_dirty_rows() {
    let Some(renderer) = headless_renderer_or_skip() else {
        eprintln!("skipping background-pane cache test: no Metal device available");
        return;
    };
    let mut terminal = weft_core::vt::Terminal::new(6, 30);
    let (_, rebuilt) = renderer.build_grid_instances_for_background_pane(&terminal, 4242);
    assert_eq!(rebuilt, 6, "first build for a fresh pane is a full rebuild");

    let (_, rebuilt) = renderer.build_grid_instances_for_background_pane(&terminal, 4242);
    assert_eq!(rebuilt, 0, "idle background pane must not rebuild any row");

    terminal.process(b"hello");
    let (batch, rebuilt) = renderer.build_grid_instances_for_background_pane(&terminal, 4242);
    assert_eq!(rebuilt, 1, "only the written row rebuilds");
    assert!(
        !batch.glyph_stream.is_empty(),
        "batch still flattens the full cache"
    );
}

/// Fingerprint invalidations: layout-origin move and grid resize (the
/// live-resize dimension change) both force a full rebuild, and separate
/// pane session ids hold separate caches.
#[test]
fn background_pane_cache_invalidates_on_origin_dims_and_namespace() {
    let Some(mut renderer) = headless_renderer_or_skip() else {
        eprintln!("skipping background-pane cache test: no Metal device available");
        return;
    };
    let mut terminal = weft_core::vt::Terminal::new(4, 20);
    let (_, rebuilt) = renderer.build_grid_instances_for_background_pane(&terminal, 7);
    assert_eq!(rebuilt, 4, "fresh cache");

    // Same pane, no change → incremental (0 rebuilt).
    let (_, rebuilt) = renderer.build_grid_instances_for_background_pane(&terminal, 7);
    assert_eq!(rebuilt, 0);

    // Layout origin moved (split drag / sidebar) → full rebuild even
    // though the grid itself has no dirty rows. The headless constructor
    // injects a real layout_ctx; its padding feeds ctx.left(), the
    // grid's origin_x source.
    if let Some(ctx) = renderer.layout_ctx.as_mut() {
        ctx.padding_x += 5.0;
    }
    let (_, rebuilt) = renderer.build_grid_instances_for_background_pane(&terminal, 7);
    assert_eq!(rebuilt, 4, "origin move invalidates baked row coordinates");

    // Dimension change (the live-resize case) → full rebuild. resize
    // marks all rows dirty on the shared grid — both panes legitimately
    // need those rows.
    terminal.resize(8, 20);
    let (_, rebuilt) = renderer.build_grid_instances_for_background_pane(&terminal, 7);
    assert_eq!(rebuilt, 8, "dims change forces a full rebuild");

    // Steady state: the redraw_controller's post-frame clear (active
    // pane path) and Tab::clear_background_grid_dirty (B3-2 companion)
    // reset the shared dirty flags once consumed.
    terminal.grid_mut().clear_all_dirty();
    let (_, rebuilt) = renderer.build_grid_instances_for_background_pane(&terminal, 7);
    assert_eq!(rebuilt, 0);

    // A different session id (another pane) has its own cache: its
    // first build is full and it does not disturb pane 7's cache.
    let (_, rebuilt_other) = renderer.build_grid_instances_for_background_pane(&terminal, 8);
    assert_eq!(rebuilt_other, 8, "new namespace builds fully");
    let (_, rebuilt_7) = renderer.build_grid_instances_for_background_pane(&terminal, 7);
    assert_eq!(rebuilt_7, 0, "pane 7's cache survived pane 8's build");
}
/// P0 regression (rust-reviewer): a full-viewport scroll records ONLY a
/// pending_scroll delta — it marks no rows dirty. The background cache
/// must force one full rebuild on that delta, or cached rows lag streamed
/// content by a row forever (the pre-fix bug: `tail -f` output stale
/// until dims/scroll/origin changed).
#[test]
fn background_pane_full_viewport_scroll_rebuilds_to_match_grid() {
    let Some(renderer) = headless_renderer_or_skip() else {
        eprintln!("skipping background-pane scroll test: no Metal device available");
        return;
    };
    let mut terminal = weft_core::vt::Terminal::new(3, 20);
    let session = 99u64;
    let build = |renderer: &crate::renderer::MetalRenderer, terminal: &weft_core::vt::Terminal| {
        renderer.build_grid_instances_for_background_pane(terminal, session)
    };

    terminal.process(b"A1\nB2\nC3");
    let (_, rebuilt) = build(&renderer, &terminal);
    assert_eq!(rebuilt, 3, "fresh cache");

    // Stream one more line → full-viewport scroll: A1 leaves the screen,
    // pending_scroll = 1, rows 0-1 rotate WITHOUT being marked dirty.
    terminal.process(b"\nD4");
    assert!(
        terminal.grid().pending_scroll() != 0,
        "fixture: the scroll must record a pending delta"
    );

    let (_, rebuilt) = build(&renderer, &terminal);
    assert_eq!(
        rebuilt, 3,
        "pending scroll delta must force a full rebuild (P0)"
    );

    // Cached rows must match the grid exactly now (post-rotation rows are
    // B2 / C3 / D4).
    let caches = renderer.background_grid_row_caches.borrow();
    let entry = caches.get(&session).expect("cache entry exists");
    assert_eq!(entry.rows.len(), 3);
    for (row, row_inst) in entry.rows.iter().enumerate() {
        let cached_chars: Vec<char> = row_inst
            .glyph_instances
            .iter()
            .map(|gi| match gi {
                crate::paint::grid_instances::GlyphInstance::Text { ch, .. } => *ch,
                _ => ' ',
            })
            .collect();
        for col in 0..terminal.grid().num_cols {
            let grid_char = terminal.grid().cell(row, col).character;
            if grid_char == ' ' || grid_char == '\0' {
                continue;
            }
            assert!(
                cached_chars.contains(&grid_char),
                "row {row} col {col}: grid char {grid_char:?} missing from cached row {cached_chars:?}"
            );
        }
    }
    drop(caches);

    // Frame-tail clear (Tab::clear_background_grid_dirty in the real
    // flow also zeroes pending_scroll) → back to incremental.
    terminal.grid_mut().clear_all_dirty();
    let (_, rebuilt) = build(&renderer, &terminal);
    assert_eq!(rebuilt, 0, "post-scroll steady state must be incremental");
}

/// P1-1 regression (rust-reviewer): theme changes bake new colors into
/// the background row caches via the generation counter. The active
/// path's `force_full_grid` cannot be read here — multi-pane frames set
/// it unconditionally, which would degenerate the cache to per-frame
/// full rebuilds.
#[test]
fn theme_change_invalidates_background_pane_cache() {
    let Some(mut renderer) = headless_renderer_or_skip() else {
        eprintln!("skipping background-pane theme test: no Metal device available");
        return;
    };
    let terminal = weft_core::vt::Terminal::new(4, 20);
    let build = |renderer: &crate::renderer::MetalRenderer, terminal: &weft_core::vt::Terminal| {
        renderer.build_grid_instances_for_background_pane(terminal, 5150)
    };

    let (_, rebuilt) = build(&renderer, &terminal);
    assert_eq!(rebuilt, 4, "fresh cache");
    let (_, rebuilt) = build(&renderer, &terminal);
    assert_eq!(rebuilt, 0, "steady state incremental");

    renderer.set_theme(weft_core::config::Theme::weft_light());
    let (_, rebuilt) = build(&renderer, &terminal);
    assert_eq!(
        rebuilt, 4,
        "theme change must force one full rebuild of cached colors"
    );
    let (_, rebuilt) = build(&renderer, &terminal);
    assert_eq!(rebuilt, 0, "and then return to incremental");
}

/// P1 regression (rust-reviewer, 2nd round): exiting alt screen (DEC 1049)
/// restores the primary screen WITHOUT marking rows dirty and with
/// identical dims/scroll/origin — the alt_active fingerprint is the only
/// thing that can catch the flip. Without it the cache kept showing vim /
/// less / htop remnants until a focus change or resize.
#[test]
fn background_pane_alt_exit_rebuilds_stale_alt_content() {
    let Some(renderer) = headless_renderer_or_skip() else {
        eprintln!("skipping background-pane alt test: no Metal device available");
        return;
    };
    let mut terminal = weft_core::vt::Terminal::new(4, 20);
    let session = 606u64;
    let build = |renderer: &crate::renderer::MetalRenderer, terminal: &weft_core::vt::Terminal| {
        renderer.build_grid_instances_for_background_pane(terminal, session)
    };
    let cached_chars = |renderer: &crate::renderer::MetalRenderer| {
        let caches = renderer.background_grid_row_caches.borrow();
        let entry = caches.get(&session).expect("cache entry exists");
        entry
            .rows
            .iter()
            .flat_map(|r| r.glyph_instances.iter())
            .filter_map(|gi| match gi {
                crate::paint::grid_instances::GlyphInstance::Text { ch, .. } => Some(*ch),
                _ => None,
            })
            .collect::<Vec<char>>()
    };

    // Primary screen: shell content, cache warm, steady incremental.
    terminal.process(b"MAIN");
    let (_, rebuilt) = build(&renderer, &terminal);
    assert_eq!(rebuilt, 4, "fresh cache");
    // Frame-tail mirror (Tab::clear_background_grid_dirty): background
    // builds consume-but-cannot-clear dirty flags, so every build in this
    // test is followed by the clear the real caller performs.
    terminal.grid_mut().clear_all_dirty();
    let (_, rebuilt) = build(&renderer, &terminal);
    assert_eq!(rebuilt, 0, "steady incremental");

    // Enter alt (DEC 1049): alt_active flips → full rebuild of alt content.
    // (Alt entry marks every row dirty via mark_all_dirty; the real frame
    // tail — Tab::clear_background_grid_dirty — clears the flags after the
    // build consumed them, mirrored here.)
    terminal.process(b"\x1b[?1049h");
    let (_, rebuilt) = build(&renderer, &terminal);
    assert_eq!(rebuilt, 4, "alt entry flips the alt_active fingerprint");
    terminal.grid_mut().clear_all_dirty();
    let (_, rebuilt) = build(&renderer, &terminal);
    assert_eq!(rebuilt, 0, "steady on the alt screen");
    let alt_view = cached_chars(&renderer);
    assert!(
        !alt_view.contains(&'M'),
        "fixture: alt grid must not show primary text"
    );

    // Exit alt (DEC 1049): the primary screen is restored with no dirty
    // rows and identical dims/scroll/origin — fixture asserts the exact
    // trap the fingerprint guards against.
    terminal.process(b"\x1b[?1049l");
    assert_eq!(
        terminal.grid().dirty_rows().count(),
        0,
        "fixture: alt exit must not mark rows dirty (the pre-fix trap)"
    );
    let (_, rebuilt) = build(&renderer, &terminal);
    assert_eq!(
        rebuilt, 4,
        "alt exit must force a full rebuild (alt_active true→false flip)"
    );
    terminal.grid_mut().clear_all_dirty();
    let view = cached_chars(&renderer);
    assert!(
        view.contains(&'M') && view.contains(&'A') && view.contains(&'I') && view.contains(&'N'),
        "cache must show the restored primary content, got {view:?}"
    );

    // Steady again on the restored primary screen.
    let (_, rebuilt) = build(&renderer, &terminal);
    assert_eq!(rebuilt, 0);
}
