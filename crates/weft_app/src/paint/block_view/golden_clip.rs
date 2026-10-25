//! v1.14.2 (PLAN_v1142 §2 A1): bottom-clip golden companion.
//!
//! Lives in its own `#[path]` file (kitty_golden_tests precedent) —
//! golden_tests.rs sits close to the module-size gate. Sees every fixture
//! and helper above through `use super::*`.
//!
//! Locks the hard bottom clip at the visible boundary: with
//! `cwd_header_active == false` (`content_bottom == region_bottom`, no legal
//! element below the line) and a FRACTIONAL `block_scroll` (trackpad
//! sub-row offset), the bottom-most glyph band straddles `content_bottom_y`
//! and — pre-A1 — bled into the input box below (缺陷一). The byte golden
//! pins the cropped stream; the quad scan proves every emitted vertex stops
//! at the clip line (贴边豁免: the canvas quad's y1 == content_bottom and
//! the surfaces are pre-clipped, both within the same bound).

use super::*;

/// S5 `scroll_clip_boundary` — the S4 fixture shape (no CWD header, no live
/// block → `cwd_header_active == false`) with a half-pitch `block_scroll`
/// so the bottom output row crosses `content_bottom_y`.
fn s5_model<'a>(
    blocks: &'a [Block],
    palette: &'a [Color; 256],
    diagnose_state: &'a HashMap<BlockId, BlockDiagnoseState>,
) -> BlockViewPaintModel<'a> {
    BlockViewPaintModel {
        blocks,
        live_head_lines: 0,
        region_bottom_y: 600.0,
        cwd: None,
        git_branch: None,
        live: None,
        block_scroll: 0.5,
        viewport_rows: 24,
        block_hovered: None,
        block_selected: None,
        block_action_hovered: None,
        spinner_phase: -1.0,
        find_block_highlight: None,
        palette,
        cache_namespace: 5,
        block_diagnose_state: diagnose_state,
        ai_configured: false,
        tui_cursor: None,
        tui_preedit: None,
        cursor_blink_on: false,
        is_alt: false,
        // Fixed clock: pairs with the fixtures' started_at (same constant).
        now: std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
    }
}

#[test]
fn golden_scroll_clip_boundary() {
    require_metal_or_skip();
    // Three output lines: with block_scroll 0.5 the LAST line's band is
    // [content_bottom - 0.5·pitch, content_bottom + 0.5·pitch) — the
    // crossing row the clip must shear.
    let blocks = vec![block_literal(
        1,
        "seq 1 200",
        Some("/tmp/weft"),
        "line one\nline two\nline three\n",
        Some(0),
    )];
    let palette = palette_literal();
    let renderer = renderer_headless();
    let empty = HashMap::new();
    let model_a = s5_model(&blocks, &palette, &empty);
    let model_b = s5_model(&blocks, &palette, &empty);
    let verts = run_golden(
        "scroll_clip_boundary",
        &renderer,
        model_a,
        model_b,
        "3|BlockActionCopy(BlockId(1)),BlockActionFold(BlockId(1)),BlockFold(BlockId(1))",
    );

    // Same geometry source as the paint: cwd_header_active == false →
    // content_bottom == region_bottom == 600.
    let ctx = renderer.layout_ctx.expect("headless layout ctx");
    let layout = crate::layout::layout_block_view(&ctx, 600.0, false);
    let clip_bottom = layout.clip_bottom;

    const VERTEX_FLOATS: usize = 12;
    const QUAD_FLOATS: usize = 6 * VERTEX_FLOATS;
    assert_eq!(
        verts.len() % QUAD_FLOATS,
        0,
        "vertex stream must be a whole number of quads"
    );
    let mut max_y1 = f32::MIN;
    let mut boundary_cropped = false;
    for quad in verts.chunks_exact(QUAD_FLOATS) {
        let mut y0 = f32::INFINITY;
        let mut y1 = f32::NEG_INFINITY;
        for v in 0..6 {
            let y = quad[v * VERTEX_FLOATS + 1];
            y0 = y0.min(y);
            y1 = y1.max(y);
        }
        max_y1 = max_y1.max(y1);
        // The sheared boundary row: starts above the line, ends ON it.
        if y0 < clip_bottom && (y1 - clip_bottom).abs() <= 0.5 {
            boundary_cropped = true;
        }
        assert!(
            y1 <= clip_bottom + 0.5,
            "row-paint quad [{y0}, {y1}) bleeds past content_bottom {clip_bottom} — \
             the A1 hard bottom clip regressed (缺陷一)"
        );
    }
    assert!(
        boundary_cropped,
        "the scene must actually exercise the boundary (a cropped row must exist)"
    );
    assert!(
        (max_y1 - clip_bottom).abs() <= 0.5,
        "no vertex may sit below content_bottom: max y1 = {max_y1}"
    );
}
