//! v1.18.9 (PLAN_v1189 §2 T8/T9) rendering-level tests for
//! `MetalRenderer::block_view_tui_cursor`'s capture-cursor anchor: the
//! nested-shell readline fix (bash's CUB moves the caret) plus the B1'
//! wrap-pending clamp and the streaming equivalence table against the
//! v1.18.8 tail-end formula. Split out of b_path_tests.rs per the
//! gate-exempt sibling-module convention (panes/freeze_tests.rs precedent)
//! — these target the renderer seam itself, not the layout pass.

use crate::paint::region_cache::{region_source_for, RegionLiveCache};
use crate::renderer::MetalRenderer;
use weft_core::vt::Terminal;

/// Mirror the golden skip precedent: no Metal device (CI without GPU)
/// skips instead of failing (panes/freeze_tests.rs twin).
fn headless_renderer_or_skip() -> Option<MetalRenderer> {
    metal::Device::system_default()?;
    Some(crate::renderer::MetalRenderer::new_headless_paint(
        weft_core::config::Theme::weft_warm(),
    ))
}

/// Bootstrap a block-view session in CommandExecuting (OSC 133;A/B markers,
/// no screen ownership — `bootstrap_ready` flips on 133;A) and print `text`
/// verbatim into the live capture.
fn block_view_terminal(text: &str) -> Terminal {
    let mut t = Terminal::new(24, 200);
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07");
    t.process(text.as_bytes());
    t
}

/// T8 (修复目标): bash ← ×8 mirrors into the capture; the caret column
/// follows the edit head instead of the pinned tail end. With no cursor
/// traffic the new anchor equals the old formula value (24) — the pure
/// streaming shapes stay byte-identical to v1.18.8.
#[test]
fn tui_caret_col_follows_cub_in_nested_shell_edit() {
    let Some(renderer) = headless_renderer_or_skip() else {
        eprintln!("skipping T8 CUB test: no Metal device available");
        return;
    };
    let mut terminal = block_view_terminal("sh-3.2$ ls -al | grep \"\"");
    assert!(terminal.show_block_view());
    // Pre-CUB: anchor at the tail end, same value the formula produced.
    assert_eq!(
        renderer.block_view_tui_cursor(&terminal, terminal.grid(), None),
        Some((0, 24))
    );
    for _ in 0..8 {
        terminal.process(b"\x1b[D"); // ← — CUB 1, mirrored on_set_cursor_column
    }
    assert_eq!(
        renderer.block_view_tui_cursor(&terminal, terminal.grid(), None),
        Some((0, 16)),
        "caret must follow the CUB-mirrored edit head (v1.18.9 fix)"
    );
}

/// T8 (regression guard): a CSI A progress repaint moves the mirrored
/// cursor OFF the tail line → anchor None → the tail-end formula anchor
/// stays (brew/ollama equivalence, renderer seam).
#[test]
fn tui_caret_keeps_formula_anchor_after_csi_a_repaint() {
    let Some(renderer) = headless_renderer_or_skip() else {
        eprintln!("skipping T8 CSI-A test: no Metal device available");
        return;
    };
    let mut terminal = block_view_terminal("frame a\nframe b");
    terminal.process(b"\x1b[1A"); // CUU — the progress-frame repaint move
                                  // Cursor parked on the early row → None → formula anchor = "frame b" end.
    assert_eq!(
        renderer.block_view_tui_cursor(&terminal, terminal.grid(), None),
        Some((1, 7))
    );
}

/// T8 (B1' 渲染断言): a tail line whose width is an exact multiple of cols
/// (k ≥ 2) parks the caret on the last visual cell (wrap-pending), not on
/// a nonexistent next chunk — the bare anchor value would vanish the quad.
#[test]
fn tui_caret_clamps_wrap_pending_exact_multiple_row_end() {
    let Some(renderer) = headless_renderer_or_skip() else {
        eprintln!("skipping T8 B1' test: no Metal device available");
        return;
    };
    // 220 x's at 110 cols → tail width 220 == 2×cols → wrap-pending end.
    let mut terminal = Terminal::new(24, 110);
    terminal.process(b"\x1b]133;A\x07\x1b]133;B\x07");
    terminal.process(b"x".repeat(220).as_slice());
    assert_eq!(
        renderer.block_view_tui_cursor(&terminal, terminal.grid(), None),
        Some((0, 219)),
        "wrap-pending exact-multiple end parks one cell back (B1')"
    );
}

/// T9 (PLAN_v1189 N1): streaming shapes — with no cursor traffic the new
/// capture-cursor anchor must equal the old tail-end formula, case by case
/// (including the CJK fold and the trailing-newline phantom row). Wrapping
/// expectations ride on the painter's own chunk math via the formula entry
/// itself, so any drift between the two paths fails here.
#[test]
fn tui_caret_streaming_shapes_stay_equivalent_to_formula() {
    let Some(renderer) = headless_renderer_or_skip() else {
        eprintln!("skipping T9 equivalence test: no Metal device available");
        return;
    };
    let cases: Vec<(String, usize)> = vec![
        ("short line".to_string(), 80),        // non-wrapped tail → bare width
        ("x".repeat(180), 110),                // k=2 fold 110/70 → bare 180
        ("x".repeat(80), 80),                  // k=1 exact full row → bare cols (T16c)
        ("中".repeat(60), 110),                // CJK fold 110/10 → bare 120
        ("abc\n".to_string(), 80),             // trailing '\n' phantom → formula pins last line
        ("frame a\nframe b".to_string(), 200), // multi-row tail, no wrap
    ];
    for (text, cols) in cases {
        let mut terminal = Terminal::new(24, cols);
        terminal.process(b"\x1b]133;A\x07\x1b]133;B\x07");
        terminal.process(text.as_bytes());
        let live = terminal.block_tracker().in_flight().unwrap();
        let old_col = crate::paint::tui_caret::tui_cursor_tail_display_col(
            live.output,
            terminal.grid().num_cols,
        );
        let line = terminal.block_tracker().live_cursor_tail_line();
        assert_eq!(
            renderer.block_view_tui_cursor(&terminal, terminal.grid(), None),
            Some((line, old_col)),
            "streaming equivalence for {text:?} @ {cols} cols"
        );
    }
}

/// v1.20.7 (PLAN_v1207 §4 D5-6): pseudo-block frames (live=None between
/// submit and 133;B) reach the region-track D6 anchor — the branch no
/// longer early-returns on `in_flight` (D3b). State comes from the real
/// sync path (`region_source_for` + `RegionLiveCache::sync`).
#[test]
fn tui_caret_anchors_in_pseudo_block_region_without_in_flight() {
    let Some(renderer) = headless_renderer_or_skip() else {
        eprintln!("skipping D5-6 pseudo-block caret test: no Metal device available");
        return;
    };
    let mut terminal = Terminal::new(6, 40);
    terminal.set_live_block_grid_render(true);
    terminal.process(b"\x1b]133;A\x07$ \x1b[Hecho hi\r\n");
    for ch in "sudo su".chars() {
        terminal.editor_mut().buffer.insert_char(ch);
    }
    terminal.submit_command(); // primes begin; no 133;B → no in-flight block
    assert!(
        terminal.block_tracker().in_flight().is_none(),
        "pre-133;B: the pseudo-block window"
    );
    terminal.process(b"sudo su\r\n> "); // PTY echo + PS2 continuation
    let grid = terminal.grid();
    let region = region_source_for(&terminal, grid).expect("the primed anchor yields a source");
    let cache = RegionLiveCache::default();
    cache.sync(1, &region, 40);
    let state = cache.state(1).expect("sync inserts the pane");
    let anchor = renderer.block_view_tui_cursor(&terminal, terminal.grid(), Some(&*state));
    let grid = terminal.grid();
    let cursor_doc = (grid.scrollback.len() + grid.cursor.row) as u64;
    let expected_row = state.display_logical_row_of_cursor(cursor_doc);
    assert!(
        expected_row.is_some(),
        "the cursor must land inside the snapshot slots (PS2 row)"
    );
    assert_eq!(
        anchor,
        expected_row.map(|row| (row, grid.cursor.col)),
        "D6 anchor in the pseudo-block window: region row + cursor col"
    );
}
