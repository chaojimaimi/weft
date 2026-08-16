use super::{build_row_instances, GlyphInstance, UNDERLINE_HEIGHT};
use weft_core::grid::{Cell, CellColor, CellFlags, Color, Cursor, CursorStyle, Grid};
use weft_core::selection::SelectionHandler;

// ── v1.10.4: SGR attribute rendering parity with block-view path ──────
//
// The grid (alt-screen) path historically ignored DIM (SGR 2), UNDERLINE,
// DOUBLE_UNDER and STRIKETHROUGH — they were parsed and stored on the
// cell but only honored by the block-view (shell output) path. TUI apps
// like opencode/vim emit SGR 2 to express visual hierarchy ("·" / model
// labels dimmer than the primary label); without rendering DIM they
// appeared at full brightness, flattening the hierarchy and making dim
// borders look like bright "white bars". These tests pin the grid path
// to the same behavior as block_view/style.rs:286-360.

const FG_FULL: [f32; 4] = [0.8, 0.6, 0.4, 1.0];
const BG_DARK: [f32; 4] = [0.1, 0.1, 0.2, 1.0];

/// Build a 1-cell grid row with a single cell for SGR attribute tests.
fn build_attr_cell(cell: &Cell, cursor: &Cursor, show: bool) -> super::GridRowInstances {
    let mut grid = Grid::new(1, 1);
    grid.viewport[0].cells[0] = cell.clone();
    build_row_instances(
        &grid,
        &Color::standard_palette(),
        0,
        FG_FULL,
        BG_DARK,
        [1.0; 4], // cursor color (white) — distinct from fg so cursor tests are unambiguous
        [0.3, 0.5, 0.7, 0.6],
        [0.22, 0.34, 0.50, 1.0],
        cursor,
        CursorStyle::Block,
        show,
        &SelectionHandler::new(),
        1.0,
        1.0, // minimum_contrast = 1.0 → never adjusts (we want to observe raw fg)
        10.0,
        20.0,
        0.0,
        0.0,
    )
}

#[test]
fn terminal_grid_applies_minimum_contrast_without_touching_background() {
    const BACKGROUND: [f32; 4] = [0.1, 0.1, 0.2, 1.0];
    let mut grid = Grid::new(1, 1);
    grid.viewport[0].cells[0] = Cell::with_char('A');
    let low_contrast_default = [80.0 / 255.0, 45.0 / 255.0, 20.0 / 255.0, 1.0];
    let result = build_row_instances(
        &grid,
        &Color::standard_palette(),
        0,
        low_contrast_default,
        BACKGROUND,
        [1.0; 4],
        [0.3, 0.5, 0.7, 0.6],
        [0.22, 0.34, 0.50, 1.0],
        &Cursor::default(),
        CursorStyle::Block,
        false,
        &SelectionHandler::new(),
        1.0,
        7.0,
        10.0,
        20.0,
        0.0,
        0.0,
    );
    assert_eq!(result.bg_instances[0].bg, BACKGROUND);
    let GlyphInstance::Text { fg, .. } = &result.glyph_instances[0] else {
        panic!("expected text glyph");
    };
    assert!(crate::paint::primitives::text_contrast_ratio(*fg, BACKGROUND) >= 6.99);
    assert!(fg[0] > fg[1] && fg[1] > fg[2]);
}

// ── v1.10.4: DIM / UNDERLINE / STRIKETHROUGH in grid (alt-screen) path ──

#[test]
fn grid_dim_attr_halves_non_cursor_fg_channels() {
    // DIM (SGR 2) must multiply fg RGB by 0.5, matching block-view path
    // (block_view/style.rs:288-297). Alpha preserved, channel order kept.
    let mut cell = Cell::with_char('·');
    cell.fg = CellColor::Rgb(Color::rgb(200, 150, 100));
    cell.flags = CellFlags::DIM;
    let result = build_attr_cell(&cell, &Cursor::default(), false);
    let GlyphInstance::Text { fg, .. } = &result.glyph_instances[0] else {
        panic!("expected text glyph");
    };
    let expected = [
        (200.0 / 255.0) * 0.5,
        (150.0 / 255.0) * 0.5,
        (100.0 / 255.0) * 0.5,
        1.0,
    ];
    for (actual, exp) in fg.iter().zip(expected.iter()) {
        assert!((actual - exp).abs() < 1e-6, "DIM fg mismatch");
    }
    assert!(fg[0] > fg[1] && fg[1] > fg[2], "channel order preserved");
}

#[test]
fn grid_explicit_terminal_color_preserves_application_hierarchy() {
    let mut grid = Grid::new(1, 1);
    grid.viewport[0].cells[0].character = 'A';
    grid.viewport[0].cells[0].fg = CellColor::Rgb(Color::rgb(128, 128, 128));
    let instances = build_row_instances(
        &grid,
        &Color::standard_palette(),
        0,
        FG_FULL,
        BG_DARK,
        [1.0; 4],
        [0.3, 0.5, 0.7, 0.6],
        [0.22, 0.34, 0.50, 1.0],
        &Cursor::default(),
        CursorStyle::Block,
        false,
        &SelectionHandler::new(),
        1.0,
        7.0,
        10.0,
        20.0,
        0.0,
        0.0,
    );
    let GlyphInstance::Text { fg, .. } = &instances.glyph_instances[0] else {
        panic!("expected text glyph");
    };
    let expected = 128.0 / 255.0;
    assert!(fg[..3]
        .iter()
        .all(|channel| (*channel - expected).abs() < 1e-6));
}

#[test]
fn grid_dim_attr_does_not_darken_cursor_block_text() {
    // When DIM and cursor-block coincide, cursor wins: black text on cursor
    // color. Applying DIM on top would push text toward invisible.
    let mut cell = Cell::with_char('A');
    cell.flags = CellFlags::DIM;
    let cursor = Cursor {
        row: 0,
        col: 0,
        visible: true,
        wrap_pending: false,
    };
    let result = build_attr_cell(&cell, &cursor, true);
    let GlyphInstance::Text { fg, .. } = &result.glyph_instances[0] else {
        panic!("expected text glyph");
    };
    // cursor block → black text, NOT dimmed black
    assert_eq!(*fg, [0.0, 0.0, 0.0, 1.0]);
}

#[test]
fn grid_underline_attr_emits_fg_colored_decoration() {
    let mut cell = Cell::with_char('A');
    cell.flags = CellFlags::UNDERLINE;
    let result = build_attr_cell(&cell, &Cursor::default(), false);
    let text_fg = result
        .glyph_instances
        .iter()
        .find_map(|g| match g {
            GlyphInstance::Text { fg, .. } => Some(*fg),
            _ => None,
        })
        .expect("text glyph present");
    let deco = result
        .glyph_instances
        .iter()
        .find_map(|g| match g {
            GlyphInstance::Decoration { dst, color } if *color == text_fg => Some(*dst),
            _ => None,
        })
        .expect("fg-colored underline decoration");
    // Underline sits at the bottom edge, UNDERLINE_HEIGHT tall.
    assert!((deco[1] - (20.0 - UNDERLINE_HEIGHT)).abs() < 0.01);
    assert!((deco[3] - 20.0).abs() < 0.01);
}

#[test]
fn grid_double_under_emits_two_stacked_decorations() {
    let mut cell = Cell::with_char('A');
    cell.flags = CellFlags::DOUBLE_UNDER;
    let result = build_attr_cell(&cell, &Cursor::default(), false);
    let decos: Vec<[f32; 4]> = result
        .glyph_instances
        .iter()
        .filter_map(|g| match g {
            GlyphInstance::Decoration { dst, .. } => Some(*dst),
            _ => None,
        })
        .collect();
    assert_eq!(decos.len(), 2, "DOUBLE_UNDER emits exactly 2 lines");
    // Lower line at y1 - UNDERLINE_HEIGHT, upper line 3px above.
    let lower = decos
        .iter()
        .copied()
        .max_by(|a, b| a[1].partial_cmp(&b[1]).unwrap())
        .unwrap();
    let upper = decos
        .iter()
        .copied()
        .min_by(|a, b| a[1].partial_cmp(&b[1]).unwrap())
        .unwrap();
    assert!((lower[1] - (20.0 - UNDERLINE_HEIGHT)).abs() < 0.01);
    assert!((upper[1] - (20.0 - UNDERLINE_HEIGHT - 3.0)).abs() < 0.01);
}

#[test]
fn grid_strikethrough_emits_mid_cell_decoration() {
    let mut cell = Cell::with_char('A');
    cell.flags = CellFlags::STRIKETHROUGH;
    let result = build_attr_cell(&cell, &Cursor::default(), false);
    let expected_y = 0.0 + 20.0 * 0.5 - 1.0; // y0 + ch*0.5 - 1.0
    let has_strike = result.glyph_instances.iter().any(|g| match g {
        GlyphInstance::Decoration { dst, .. } => (dst[1] - expected_y).abs() < 0.01,
        _ => false,
    });
    assert!(
        has_strike,
        "STRIKETHROUGH decoration at mid-cell y={expected_y}"
    );
}

#[test]
fn grid_dim_applies_to_reverse_swapped_fg() {
    // REVERSE swaps fg/bg at line 267-269, then DIM multiplies the swapped
    // fg. Pinning this ordering guards against a refactor that accidentally
    // applies DIM before the swap (which would dim the original fg, not the
    // swapped one).
    let mut cell = Cell::with_char('A');
    cell.fg = CellColor::Rgb(Color::rgb(220, 220, 220)); // near-white fg
    cell.bg = CellColor::Rgb(Color::rgb(40, 40, 40)); // dark bg
    cell.flags = CellFlags::REVERSE | CellFlags::DIM;
    let result = build_attr_cell(&cell, &Cursor::default(), false);
    let GlyphInstance::Text { fg, .. } = &result.glyph_instances[0] else {
        panic!("expected text glyph");
    };
    // After REVERSE: fg = dark bg color (40/255). After DIM: ×0.5 = 20/255.
    let expected_swapped_dim = (40.0 / 255.0) * 0.5;
    assert!(
        (fg[0] - expected_swapped_dim).abs() < 1e-6,
        "REVERSE+DIM fg should be swapped-then-dimmed, got fg={fg:?}"
    );
}

#[test]
fn grid_underline_renders_even_when_text_is_hidden() {
    // HIDDEN suppresses the text glyph, but UNDERLINE must still render.
    // This guards against moving the decoration block inside has_visible_text.
    let mut cell = Cell::with_char('X');
    cell.flags = CellFlags::HIDDEN | CellFlags::UNDERLINE;
    let result = build_attr_cell(&cell, &Cursor::default(), false);
    // No text glyph...
    assert!(
        result
            .glyph_instances
            .iter()
            .all(|g| !matches!(g, GlyphInstance::Text { .. })),
        "HIDDEN suppresses text glyph"
    );
    // ...but underline decoration present.
    let has_underline = result.glyph_instances.iter().any(|g| {
        matches!(
            g,
            GlyphInstance::Decoration { dst, .. } if (dst[1] - (20.0 - UNDERLINE_HEIGHT)).abs() < 0.01
        )
    });
    assert!(has_underline, "UNDERLINE renders even when text is HIDDEN");
}

// ── v1.10.4: minimum-contrast skip for terminal graphic glyphs ────────

/// Build a 1-cell row with an aggressive minimum_contrast (7.0) so the
/// booster would brighten low-contrast colors — used to pin the graphic-
/// char exemption.
fn build_high_contrast_cell(cell: &Cell) -> super::GridRowInstances {
    let mut grid = Grid::new(1, 1);
    grid.viewport[0].cells[0] = cell.clone();
    build_row_instances(
        &grid,
        &Color::standard_palette(),
        0,
        FG_FULL,
        BG_DARK,
        [1.0; 4],
        [0.3, 0.5, 0.7, 0.6],
        [0.22, 0.34, 0.50, 1.0],
        &Cursor::default(),
        CursorStyle::Block,
        false,
        &SelectionHandler::new(),
        1.0,
        7.0, // aggressive threshold — would boost low-contrast colors
        10.0,
        20.0,
        0.0,
        0.0,
    )
}

#[test]
fn graphic_char_skips_minimum_contrast_boost() {
    // opencode draws its input-box edge with ▀ (U+2580) fg=rgb(21,20,27) on
    // bg=rgb(15,15,15) — contrast ratio 1.05, far below the 7.0 threshold.
    // Before the fix, ensure_minimum_text_contrast boosted the fg toward
    // white (→ rgb(156,156,159)), turning the intended subtle border into a
    // glaring "white bar". Graphic glyphs must keep their exact color.
    let mut cell = Cell::with_char('▀');
    cell.fg = CellColor::Rgb(Color::rgb(21, 20, 27));
    cell.bg = CellColor::Rgb(Color::rgb(15, 15, 15));
    let result = build_high_contrast_cell(&cell);
    let GlyphInstance::Text { fg, .. } = &result.glyph_instances[0] else {
        panic!("expected text glyph");
    };
    let expected = [21.0 / 255.0, 20.0 / 255.0, 27.0 / 255.0, 1.0];
    for (actual, exp) in fg.iter().zip(expected.iter()) {
        assert!(
            (actual - exp).abs() < 1e-4,
            "graphic glyph fg must stay as-designed (no boost), got {fg:?}"
        );
    }
}

#[test]
fn graphic_char_keeps_designed_dim_color_under_high_threshold() {
    // Same check with a mid-gray border (a common "dim" border color):
    // rgb(80,80,80) on rgb(15,15,15) has ratio ~4.1 — still below 7.0, so
    // without the exemption it would be brightened. It must stay exact.
    let mut cell = Cell::with_char('▄');
    cell.fg = CellColor::Rgb(Color::rgb(80, 80, 80));
    cell.bg = CellColor::Rgb(Color::rgb(15, 15, 15));
    let result = build_high_contrast_cell(&cell);
    let GlyphInstance::Text { fg, .. } = &result.glyph_instances[0] else {
        panic!("expected text glyph");
    };
    assert!(
        (fg[0] - 80.0 / 255.0).abs() < 1e-4,
        "graphic glyph must not be boosted, got fg={fg:?}"
    );
}

#[test]
fn graphic_char_classification_covers_box_drawing_and_block_elements() {
    // Sanity-pin the character range used by the exemption.
    assert!(super::is_terminal_graphic_char('▀')); // U+2580 upper half
    assert!(super::is_terminal_graphic_char('▄')); // U+2584 lower half
    assert!(super::is_terminal_graphic_char('█')); // U+2588 full block
    assert!(super::is_terminal_graphic_char('─')); // U+2500 box drawing
    assert!(super::is_terminal_graphic_char('│')); // U+2502
    assert!(!super::is_terminal_graphic_char('A'));
    assert!(!super::is_terminal_graphic_char('·'));
    assert!(!super::is_terminal_graphic_char('中'));
}
