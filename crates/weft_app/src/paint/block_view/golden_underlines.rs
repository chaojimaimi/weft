//! v1.11.13 (PLAN_v11113 §M4): the styled_underlines golden scene.
//!
//! Nested via `#[path]` from golden_tests.rs (module-size gate — the
//! kitty_tests → kitty_golden_tests precedent); every helper (fixtures,
//! run_golden, headless renderer, Metal skip) comes from the parent
//! through `use super::*`.

use super::*;

use weft_core::blocks::{AttributeSpan, ColorSpan};
use weft_core::grid::CellFlags;

/// v1.11.13 (PLAN_v11113 §M4): styled_underlines golden — the .3 shared
/// geometry kernel's four SGR shapes (4:2 double / 4:3 wavy / 4:4 dotted /
/// 4:5 dashed) plus an SGR 58;2 colored single, captured through the same
/// block_styled_literal → run_golden pipeline as S4.
///
/// Two locks per the plan:
/// 1. the combined scene's .bin vertex bytes + hits digest (regression);
/// 2. behavioral assert_ne!s — the four styles' serialized vertex streams
///    are MUTUALLY different and each differs from the no-underline
///    baseline (at vertex-golden granularity, distinct underline geometry
///    ⇒ distinct bytes), and the 58;2 color changes the bytes of the same
///    Single shape.
#[test]
fn golden_styled_underlines() {
    require_metal_or_skip();
    let palette = palette_literal();
    let renderer = renderer_headless();
    let empty = HashMap::new();

    // One attribute span with the UNDERLINE flag + the SGR-numbered shape.
    // Mirrors what the VT parser stores for `CSI 4:<n>m` (UNDERLINE bit +
    // style u8); double also carries the DOUBLE_UNDER bit per the dual-track
    // priority (style.rs §1.2).
    let span = |start: u32, end: u32, extra: CellFlags, style: u8| AttributeSpan {
        start,
        end,
        flags: CellFlags::UNDERLINE.union(extra),
        underline_style: style,
    };

    // Combined scene: 6 groups of 3 chars — double/wavy/dotted/dashed/
    // colored-single/plain. Block id 6 (fresh namespace slot, no collision
    // with the styled-cache entries of the other golden scenes).
    let combined = StyledOutput {
        lines: vec![StyledLine {
            line: 0,
            foregrounds: Vec::new(),
            backgrounds: Vec::new(),
            links: Vec::new(),
            attributes: vec![
                span(0, 3, CellFlags::DOUBLE_UNDER, 2), // `4;4`(plan)/4:2 double
                span(3, 6, CellFlags::empty(), 3),      // 4:3 wavy
                span(6, 9, CellFlags::empty(), 4),      // 4:4 dotted
                span(9, 12, CellFlags::empty(), 5),     // 4:5 dashed
                span(12, 15, CellFlags::empty(), 1),    // 4 + 58;2 colored
            ],
            underline_colors: vec![ColorSpan {
                start: 12,
                end: 15,
                color: CellColor::Rgb(Color::rgb(255, 128, 0)),
            }],
        }],
    };
    let blocks = vec![block_styled_literal(
        6,
        "printf underline-demo",
        Some("/tmp/weft"),
        "AAABBBCCCDDDEEEFFF\n",
        Some(0),
        combined,
    )];
    let model_a = s4_model(&blocks, &palette, &empty);
    let model_b = s4_model(&blocks, &palette, &empty);
    run_golden(
        "styled_underlines",
        &renderer,
        model_a,
        model_b,
        "3|BlockActionCopy(BlockId(6)),BlockActionFold(BlockId(6)),BlockFold(BlockId(6))",
    );

    // ── behavioral lock: per-style scenes, byte-level mutual difference ──
    //
    // All scenes share ONE block id and get a FRESH headless renderer each
    // (the styled-cache is renderer-owned, so nothing cross-hits). This is
    // load-bearing for the assertion purity: block_surface_color stripes
    // the block background by id parity (surfaces.rs:78), so different ids
    // would make ANY two scenes differ regardless of underline geometry.
    // With id + renderer held fixed, the ONLY possible byte difference
    // between two scenes is the decoration itself.
    let scene_bytes = |attributes: Vec<AttributeSpan>, colors: Vec<ColorSpan>| {
        let styled = StyledOutput {
            lines: vec![StyledLine {
                line: 0,
                foregrounds: Vec::new(),
                backgrounds: Vec::new(),
                links: Vec::new(),
                attributes,
                underline_colors: colors,
            }],
        };
        let blocks = vec![block_styled_literal(
            60,
            "printf underline-demo",
            Some("/tmp/weft"),
            "AAAA\n",
            Some(0),
            styled,
        )];
        let renderer = renderer_headless();
        let model = s4_model(&blocks, &palette, &empty);
        let mut sel = SelectionHandler::new();
        let (verts, _, _) = renderer.build_block_view_vertices(model, &mut sel);
        verts
            .iter()
            .flat_map(|f| f32::to_le_bytes(*f))
            .collect::<Vec<u8>>()
    };
    let plain_attrs: Vec<AttributeSpan> = Vec::new();
    let underline = |style: u8, extra: CellFlags| span(0, 4, extra, style);

    let baseline = scene_bytes(plain_attrs.clone(), Vec::new());
    let double_b = scene_bytes(vec![underline(2, CellFlags::empty())], Vec::new());
    let double_flag_b = scene_bytes(vec![underline(1, CellFlags::DOUBLE_UNDER)], Vec::new());
    let wavy_b = scene_bytes(vec![underline(3, CellFlags::empty())], Vec::new());
    let dotted_b = scene_bytes(vec![underline(4, CellFlags::empty())], Vec::new());
    let dashed_b = scene_bytes(vec![underline(5, CellFlags::empty())], Vec::new());
    let single_b = scene_bytes(vec![underline(1, CellFlags::empty())], Vec::new());
    let colored_b = scene_bytes(
        vec![underline(1, CellFlags::empty())],
        vec![ColorSpan {
            start: 0,
            end: 4,
            color: CellColor::Rgb(Color::rgb(255, 128, 0)),
        }],
    );

    // Four styles mutually different (the shapes emit different quad
    // counts/coordinates: double 2 bars, wavy 8 steps, dotted dots,
    // dashed dashes).
    assert_ne!(double_b, wavy_b, "double vs wavy");
    assert_ne!(double_b, dotted_b, "double vs dotted");
    assert_ne!(double_b, dashed_b, "double vs dashed");
    assert_ne!(wavy_b, dotted_b, "wavy vs dotted");
    assert_ne!(wavy_b, dashed_b, "wavy vs dashed");
    assert_ne!(dotted_b, dashed_b, "dotted vs dashed");
    // …and each differs from the no-underline baseline.
    for (name, bytes) in [
        ("double", &double_b),
        ("wavy", &wavy_b),
        ("dotted", &dotted_b),
        ("dashed", &dashed_b),
    ] {
        assert_ne!(bytes, &baseline, "{name} vs no-underline baseline");
    }
    // Dual-track compat (style.rs §1.2): the legacy DOUBLE_UNDER flag bit
    // and the modern style u8=2 must render IDENTICAL vertices.
    assert_eq!(
        double_flag_b, double_b,
        "flag-double vs u8-double must be byte-equal"
    );
    // The SGR 58;2 color changes the same Single shape's bytes.
    assert_ne!(colored_b, single_b, "colored vs plain single");
    assert_ne!(colored_b, baseline, "colored vs baseline");
}
