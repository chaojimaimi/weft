//! Device-free rasterization and transform probes.
//!
//! Split from `glyph/mod.rs` (Batch 6 Step 4) to keep the main file under
//! the 800-line limit. `use super::*` pulls in the glyph module's public
//! surface plus the re-exported `font` helpers (`nonzero_cell_dimension`,
//! `is_emoji_char`, etc.).

#![cfg(test)]

use super::font::{is_emoji_char, rasterize_emoji_rgba};
use super::*;
use font_kit::canvas::{Canvas, Format, RasterizationOptions};
use font_kit::hinting::HintingOptions;
use pathfinder_geometry::transform2d::Transform2F;
use pathfinder_geometry::vector::{Vector2F, Vector2I};

/// Production baseline anchor in pixels.
fn descent_px(font: &Font, scaled_size: f32) -> f32 {
    let m = font.metrics();
    m.descent.abs() * (scaled_size / m.units_per_em as f32)
}

#[test]
fn atlas_cell_dimensions_never_reach_zero() {
    assert_eq!(nonzero_cell_dimension(0), 1);
    assert_eq!(nonzero_cell_dimension(17), 17);
}

#[test]
fn menlo_variants_resolve_via_properties_match() {
    use font_kit::properties::{Style as FontStyle, Weight};
    // Bold and italic faces of Menlo must be resolvable through the same
    // family lookup the atlas uses — otherwise styled cells silently
    // render regular.
    let bold = super::font::resolve_font_variant(
        "Menlo",
        &["/System/Library/Fonts/Menlo.ttc"],
        Weight::BOLD,
        FontStyle::Normal,
    );
    let italic = super::font::resolve_font_variant(
        "Menlo",
        &["/System/Library/Fonts/Menlo.ttc"],
        Weight::NORMAL,
        FontStyle::Italic,
    );
    let bold_italic = super::font::resolve_font_variant(
        "Menlo",
        &["/System/Library/Fonts/Menlo.ttc"],
        Weight::BOLD,
        FontStyle::Italic,
    );
    assert!(bold.is_some(), "Menlo Bold must resolve");
    assert!(italic.is_some(), "Menlo Italic must resolve");
    assert!(bold_italic.is_some(), "Menlo Bold Italic must resolve");
    // The variant faces must actually be different glyphs from regular —
    // an 'i' in italic Menlo produces visibly different pixels.
    if let (Some(italic), Some(regular)) = (
        italic.as_ref(),
        Font::from_path("/System/Library/Fonts/Menlo.ttc", 0)
            .ok()
            .as_ref(),
    ) {
        let (w, h) = (16, 24);
        let it = GlyphAtlas::rasterize_glyph(
            italic,
            'i',
            20.0,
            w,
            h,
            false,
            descent_px(italic, 20.0),
            false,
        );
        let re = GlyphAtlas::rasterize_glyph(
            regular,
            'i',
            20.0,
            w,
            h,
            false,
            descent_px(regular, 20.0),
            false,
        );
        assert_ne!(it, re, "italic 'i' must rasterize differently from regular");
    }
}

// v1.10.12-fix regression: a family WITHOUT a true italic face (Fira Code
// ships only Regular/Bold/Light/…) must NOT resolve an Italic variant —
// Core Text approximates the request with the Regular face, which would
// render "italic" as plain. The atlas then synthesizes the oblique.
#[test]
fn fira_code_without_italic_variant_is_rejected() {
    use font_kit::properties::{Style as FontStyle, Weight};
    // v1.11.16: environment guard. This test asserts Fira Code's own behavior
    // (no Italic face), so it can only run where the family is installed.
    // CI installs it via scripts/install_tui_test_deps.sh; anywhere else the
    // test skips instead of failing on a missing font (same shape as
    // tui_integration's require_command guard).
    if super::font::resolve_font("Fira Code", &[]).is_none() {
        eprintln!("skipping fira_code_without_italic_variant_is_rejected: Fira Code not installed");
        return;
    }
    let bold = super::font::resolve_font_variant(
        "Fira Code",
        &["/System/Library/Fonts/Menlo.ttc"],
        Weight::BOLD,
        FontStyle::Normal,
    );
    let italic = super::font::resolve_font_variant(
        "Fira Code",
        &["/System/Library/Fonts/Menlo.ttc"],
        Weight::NORMAL,
        FontStyle::Italic,
    );
    let bold_italic = super::font::resolve_font_variant(
        "Fira Code",
        &["/System/Library/Fonts/Menlo.ttc"],
        Weight::BOLD,
        FontStyle::Italic,
    );
    assert!(bold.is_some(), "Fira Code Bold must resolve");
    assert!(
        italic.is_none(),
        "Fira Code has no italic face — the approximated regular must be rejected"
    );
    assert!(
        bold_italic.is_none(),
        "Fira Code has no bold-italic face — must be rejected"
    );
}

// v1.10.12-fix: synthetic italic must lean FORWARD on screen (top-right /
// bottom-left), matching a true italic face. Regression for the sign error
// where m12 = +0.2 rendered backward.
//
// IMPORTANT: the rasterized glyph bitmap is stored BOTTOM-UP (row 0 = visual
// bottom); the paint path V-flips it for display. So in the bitmap the VISUAL
// TOP is the high row indices (2h/3..h) and the VISUAL BOTTOM is the low row
// indices (0..h/3). Forward lean ⇒ the visual top shifts right relative to
// the visual bottom when the skew is applied.
#[test]
fn synthesized_italic_skews_forward() {
    let font = Font::from_path("/System/Library/Fonts/Menlo.ttc", 0).unwrap();
    let (w, h): (u32, u32) = (24, 32);
    let (wu, hu) = (w as usize, h as usize);
    let descent = descent_px(&font, 24.0);
    let plain = GlyphAtlas::rasterize_glyph(&font, 'l', 24.0, w, h, false, descent, false);
    let skewed = GlyphAtlas::rasterize_glyph(&font, 'l', 24.0, w, h, false, descent, true);
    assert_ne!(plain, skewed, "synthetic italic must differ from regular");
    let ink_x_mean = |px: &[u8], y0: usize, y1: usize| -> f32 {
        let (mut sum, mut n) = (0usize, 0usize);
        for y in y0..y1 {
            for x in 0..wu {
                if px[y * wu + x] > 0 {
                    sum += x;
                    n += 1;
                }
            }
        }
        if n == 0 {
            f32::NAN
        } else {
            sum as f32 / n as f32
        }
    };
    // Bitmap is bottom-up: rows 0..h/3 = visual BOTTOM, rows 2h/3..h = visual TOP.
    let bottom_shift = ink_x_mean(&skewed, 0, hu / 3) - ink_x_mean(&plain, 0, hu / 3);
    let top_shift = ink_x_mean(&skewed, 2 * hu / 3, hu) - ink_x_mean(&plain, 2 * hu / 3, hu);
    assert!(
        top_shift > bottom_shift + 0.5,
        "forward italic: visual top must shift right more than visual bottom \
         (top Δ={top_shift:.2}, bottom Δ={bottom_shift:.2})"
    );
}

#[test]
fn modern_pictographs_use_the_color_emoji_path() {
    for ch in ['🦞', '🧑', '🫠', '🪄'] {
        assert!(is_emoji_char(ch), "{ch} must use Apple Color Emoji");
    }
    assert!(!is_emoji_char('中'));
    assert!(!is_emoji_char('\u{1f650}'));
    assert!(!is_emoji_char('\u{1f800}'));
}

#[test]
fn lobster_rasterizes_with_visible_ink() {
    let font = Font::from_path("/System/Library/Fonts/Apple Color Emoji.ttc", 0).unwrap();
    // v1.10.4: color emoji rasterize via the CoreText RGBA path (the A8
    // `rasterize_glyph` no longer handles them).
    let pixels = rasterize_emoji_rgba(&font, '🦞', 28.0, 28, 32, 6.0)
        .expect("lobster must rasterize via CoreText RGBA");
    assert_eq!(pixels.len(), 28 * 32 * 4, "RGBA = 4 bytes/pixel");
    let (top, bottom) = ink_y_bbox_rgba(&pixels, 28, 32).expect("lobster emoji must have ink");
    let ink_height = bottom - top + 1;
    assert!(ink_height >= 20, "lobster ink is too small: {ink_height}px");
}

#[test]
fn astral_emoji_discards_the_surrogate_placeholder_glyph() {
    let font = Font::from_path("/System/Library/Fonts/Apple Color Emoji.ttc", 0).unwrap();
    let ct_font = font.native_font().clone_with_font_size(28.0);
    let glyphs = super::font::drawable_glyphs_for_char(&ct_font, '🦞');
    assert_eq!(glyphs.len(), 1);
    assert_ne!(glyphs[0], 0);
}

#[test]
fn color_emoji_rasterization_tracks_terminal_zoom() {
    let font = Font::from_path("/System/Library/Fonts/Apple Color Emoji.ttc", 0).unwrap();
    let small =
        rasterize_emoji_rgba(&font, '🦞', 16.0, 20, 22, 4.0).expect("small lobster must rasterize");
    let large =
        rasterize_emoji_rgba(&font, '🦞', 28.0, 32, 36, 7.0).expect("large lobster must rasterize");
    let (small_top, small_bottom) = ink_y_bbox_rgba(&small, 20, 22).expect("small emoji ink");
    let (large_top, large_bottom) = ink_y_bbox_rgba(&large, 32, 36).expect("large emoji ink");
    assert!(
        large_bottom - large_top > small_bottom - small_top,
        "color emoji must grow when terminal zoom grows"
    );
}

#[test]
fn prewarm_path_uses_edge_to_edge_procedural_box_line() {
    let font = Font::from_path("/System/Library/Fonts/Menlo.ttc", 0).unwrap();
    let (width, height) = (8, 16);
    let mut atlas = vec![0; (width * height) as usize];
    let (mut x, mut y, mut row_height) = (0, 0, 0);
    GlyphAtlas::rasterize_and_place(
        &font,
        '─',
        14.0,
        width,
        height,
        false,
        false,
        &mut atlas,
        width,
        height,
        &mut x,
        &mut y,
        &mut row_height,
        descent_px(&font, 14.0),
    )
    .unwrap();
    assert!(atlas
        .chunks_exact(width as usize)
        .any(|row| row[0] == u8::MAX && row[width as usize - 1] == u8::MAX));
}

fn ink_bbox(c: &Canvas) -> (i32, i32, usize) {
    let mut min_r = i32::MAX;
    let mut max_r = i32::MIN;
    let mut n = 0usize;
    for y in 0..c.size.y() {
        for x in 0..c.size.x() {
            if c.pixels[(y as usize * c.stride) + x as usize] > 0 {
                n += 1;
                min_r = min_r.min(y);
                max_r = max_r.max(y);
            }
        }
    }
    if n == 0 {
        (0, 0, 0)
    } else {
        (min_r, max_r, n)
    }
}

fn raster(font: &Font, ch: char, scaled: f32, cw: i32, ch_h: i32, t: Transform2F) -> Canvas {
    let gid = font.glyph_for_char(ch).unwrap();
    let mut canvas = Canvas::new(Vector2I::new(cw, ch_h), Format::A8);
    let _ = font.rasterize_glyph(
        &mut canvas,
        gid,
        scaled,
        t,
        HintingOptions::None,
        RasterizationOptions::GrayscaleAa,
    );
    canvas
}

#[test]
fn menlo_renders_angle_quote_with_ink() {
    // real ink from Menlo. (Apple Symbols does NOT have U+276F, so the
    // prompt color/visibility depends on Menlo rendering it directly.)
    let font = Font::from_path("/System/Library/Fonts/Menlo.ttc", 0).unwrap();
    assert!(font.glyph_for_char('❯').is_some(), "Menlo must have ❯");
    let px = GlyphAtlas::rasterize_glyph(
        &font,
        '❯',
        28.0,
        14,
        28,
        false,
        descent_px(&font, 28.0),
        false,
    );
    let ink = px.iter().filter(|p| **p > 0).count();
    assert!(ink > 50, "❯ must rasterize with ink, got {ink}");
}

#[test]
fn unsupported_grapheme_fallbacks_rasterize_with_matching_cell_width() {
    let cases = [
        ("/System/Library/Fonts/Menlo.ttc", '\u{fffd}', false, 14),
        (
            "/System/Library/Fonts/STHeiti Light.ttc",
            '\u{ff1f}',
            true,
            28,
        ),
    ];
    for (path, ch, is_wide, glyph_w) in cases {
        let font = Font::from_path(path, 0).unwrap();
        assert!(
            font.glyph_for_char(ch).is_some(),
            "{path} must contain {ch}"
        );
        let px = GlyphAtlas::rasterize_glyph(
            &font,
            ch,
            28.0,
            glyph_w,
            28,
            is_wide,
            descent_px(&font, 28.0),
            false,
        );
        assert!(px.iter().any(|value| *value > 0), "{ch} must have ink");
        assert_eq!(
            weft_core::grid::terminal_char_width(ch),
            if is_wide { 2 } else { 1 }
        );
    }
}

#[test]
fn angle_quote_is_not_classified_wide() {
    // If ❯ were wide (unicode-width > 1), get_or_rasterize would route it
    // to the CJK font (which lacks it) → a blank prompt marker. It must be
    // width 1 so the primary (Menlo) path renders it.
    let w = unicode_width::UnicodeWidthChar::width('❯').unwrap_or(0);
    assert_eq!(w, 1, "❯ must be width 1, got {w}");
}

#[test]
fn probe_transforms() {
    let font = Font::from_path("/System/Library/Fonts/Menlo.ttc", 0).unwrap();
    let scaled = 28.0f32;
    let (cw, ch_h) = (17i32, 34i32);
    let upem = font.metrics().units_per_em as f32;
    let ascent = font.metrics().ascent;
    let descent = font.metrics().descent;
    let scale_px = scaled / upem;
    let ascent_px = ascent * scale_px;
    let descent_px = descent * scale_px;
    let glyph_h = ascent_px - descent_px;
    let top_pad = (ch_h as f32 - glyph_h).max(0.0) / 2.0;
    eprintln!(
        "metrics upem={} ascent={} descent={} scale_px={:.4} ascent_px={:.1} descent_px={:.1} glyph_h={:.1} top_pad={:.1}",
        upem, ascent, descent, scale_px, ascent_px, descent_px, glyph_h, top_pad
    );

    // Production transform: Y-flip only (no translation). Regression guard for
    // the bug where adding translate(0, ascent_px) clipped every glyph's top,
    // leaving only a bottom fragment (ink rows ~[25,33] instead of the full glyph).
    let prod_transform = Transform2F::from_scale(Vector2F::new(1.0, -1.0));

    for &probe_ch in &['M', 'a', 'g'] {
        let c = raster(&font, probe_ch, scaled, cw, ch_h, prod_transform);
        let (min_row, max_row, count) = ink_bbox(&c);
        eprintln!(
            "  '{}' under scale(1,-1): ink rows [{},{}] count={}",
            probe_ch, min_row, max_row, count
        );
        // The clipped-bug produced ink only in the bottom ~8 rows (min_row >= 25).
        // A full upright glyph must start near the top of the cell.
        assert!(
            count > 50,
            "'{}' produced almost no ink (count={}); rasterization is broken",
            probe_ch,
            count
        );
        assert!(
            min_row <= 8,
            "'{}' ink starts at row {} — top is clipped (the ascent-translation bug). \
             Expected the glyph to start near the top of the cell.",
            probe_ch,
            min_row
        );
    }
}

/// Horizontal ink extents (left_col, right_col) — non-zero pixel columns.
fn ink_x_extents(c: &Canvas) -> Option<(i32, i32)> {
    let mut min_x = i32::MAX;
    let mut max_x = i32::MIN;
    let mut any = false;
    for y in 0..c.size.y() {
        for x in 0..c.size.x() {
            if c.pixels[(y as usize * c.stride) + x as usize] > 0 {
                any = true;
                min_x = min_x.min(x);
                max_x = max_x.max(x);
            }
        }
    }
    if any {
        Some((min_x, max_x))
    } else {
        None
    }
}

#[test]
fn cjk_glyph_not_stretched_and_centered() {
    // CJK glyph '中' rasterized into a double-wide slot must NOT be
    // horizontally stretched (old behaviour filled the full 2·cell_width,
    // distorting every CJK char ~1.2×). It should sit roughly centered
    // with side bearings on both edges.
    let cjk = match Font::from_path("/System/Library/Fonts/PingFang.ttc", 0) {
        Ok(f) => f,
        Err(_) => {
            eprintln!("PingFang.ttc not available on this system — skipping");
            return;
        }
    };
    // Match production cell geometry: Menlo cell_width=17 → CJK slot=34.
    let (cw, ch_h) = (34i32, 34i32);
    let scaled = 28.0f32;
    let pixels = GlyphAtlas::rasterize_glyph(
        &cjk,
        '中',
        scaled,
        cw as u32,
        ch_h as u32,
        true,
        descent_px(&cjk, scaled),
        false,
    );
    // rasterize_glyph returns a Vec<u8>; build a Canvas-like view.
    assert_eq!(pixels.len(), (cw * ch_h) as usize);
    let mut probe_canvas = Canvas::new(Vector2I::new(cw, ch_h), Format::A8);
    probe_canvas.pixels.copy_from_slice(&pixels);
    let (left, right) =
        ink_x_extents(&probe_canvas).expect("'中' must rasterize with ink in the CJK font");

    // With no stretch, the glyph advance is ~1em ≈ 28px in a 34px slot.
    // So ink should span well under 34px wide, and both side bearings
    // (left > 0 AND right < cw-1) must be present — centering.
    let ink_w = right - left + 1;
    eprintln!("'中' ink x=[{left},{right}] width={ink_w} slot={cw}");
    assert!(
        ink_w < cw,
        "'中' ink width {ink_w} >= slot {cw}; glyph is stretched (the bug)"
    );
    assert!(
        left > 0,
        "'中' has no left bearing (left={left}); not centered"
    );
    assert!(
        right < cw - 1,
        "'中' has no right bearing (right={right}); not centered"
    );
    // Symmetry: left and right bearings within a few px of each other.
    let left_bearing = left;
    let right_bearing = cw - 1 - right;
    let asymmetry = (left_bearing - right_bearing).abs();
    assert!(
        asymmetry <= 4,
        "'中' is off-center: left_bearing={left_bearing} right_bearing={right_bearing} (Δ={asymmetry})"
    );
}

/// Ink bounding box (top_row, bottom_row) over an A8 pixel slice for a w×h
/// glyph (1 byte/pixel) — the font-kit mask path.
fn ink_y_bbox(pixels: &[u8], w: u32, h: u32) -> Option<(i32, i32)> {
    let mut min_r = i32::MAX;
    let mut max_r = i32::MIN;
    let mut any = false;
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            if pixels[(y as usize * w as usize) + x as usize] > 0 {
                any = true;
                min_r = min_r.min(y);
                max_r = max_r.max(y);
            }
        }
    }
    if any {
        Some((min_r, max_r))
    } else {
        None
    }
}

/// Ink bounding box (top_row, bottom_row) over a **premultiplied RGBA**
/// pixel slice for a w×h glyph (4 bytes/pixel). Ink = alpha > 0.
fn ink_y_bbox_rgba(pixels: &[u8], w: u32, h: u32) -> Option<(i32, i32)> {
    let mut min_r = i32::MAX;
    let mut max_r = i32::MIN;
    let mut any = false;
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let idx = (y as usize * w as usize + x as usize) * 4;
            if pixels[idx + 3] > 0 {
                any = true;
                min_r = min_r.min(y);
                max_r = max_r.max(y);
            }
        }
    }
    if any {
        Some((min_r, max_r))
    } else {
        None
    }
}

/// v1.10.4: color-emoji pixels must keep their RGB (the R8 alpha-only
/// extraction of the pre-fix pipeline destroyed color). Every nonzero-alpha
/// pixel must have at least one nonzero RGB channel, and the RGB channels
/// must not be all identical to the alpha channel (which would indicate a
/// grayscale reduction).
#[test]
fn emoji_rgba_preserves_color_channels() {
    let font = Font::from_path("/System/Library/Fonts/Apple Color Emoji.ttc", 0).unwrap();
    let pixels =
        rasterize_emoji_rgba(&font, '🦞', 28.0, 28, 32, 6.0).expect("lobster must rasterize");
    let colored = pixels
        .chunks_exact(4)
        .filter(|px| px[3] > 0 && (px[0] > 0 || px[1] > 0 || px[2] > 0))
        .count();
    assert!(
        colored > 0,
        "lobster must have colored (non-grayscale) pixels"
    );
}

/// v1.10.4: `allocate_slot` flags color-atlas slots via `is_color`, so the
/// caller can route uploads and the fg.a=2.0 sentinel consistently. This is
/// pure layout logic — no Metal device needed.
#[test]
fn allocate_slot_marks_color_slots() {
    let (atlas_w, atlas_h) = (256u32, 256u32);
    let (mut nx, mut ny, mut rh) = (0u32, 0u32, 0u32);
    let color = GlyphAtlas::allocate_slot(
        false, 14, 28, atlas_w, atlas_h, &mut nx, &mut ny, &mut rh, true,
    )
    .expect("color slot must allocate");
    assert!(color.is_color, "emoji slot must be flagged is_color");

    let (mut nx, mut ny, mut rh) = (0u32, 0u32, 0u32);
    let mask = GlyphAtlas::allocate_slot(
        false, 14, 28, atlas_w, atlas_h, &mut nx, &mut ny, &mut rh, false,
    )
    .expect("mask slot must allocate");
    assert!(!mask.is_color, "mask slot must not be flagged is_color");

    // Independent allocator cursors: each starts at the origin.
    assert_eq!(color.uv_origin, mask.uv_origin);
}

/// v1.10.4: a cluster only uses the RGBA color atlas when it is emoji AND
/// the emoji font has the base scalar. `"e\u{fe0f}"` passes the classifier
/// but would render as an untintable white 'e' from CoreText's cascade.
#[test]
fn cluster_color_atlas_requires_emoji_classification_and_font_glyph() {
    assert!(super::query::cluster_wants_color_atlas(true, true));
    assert!(!super::query::cluster_wants_color_atlas(true, false));
    assert!(!super::query::cluster_wants_color_atlas(false, true));
    assert!(!super::query::cluster_wants_color_atlas(false, false));
}

/// v1.10.4: lock the font behavior the `cluster_wants_color_atlas` gate
/// depends on — real emoji base scalars exist in Apple Color Emoji, plain
/// text scalars don't. If a font-kit/CoreText change breaks this, color
/// emoji silently degrade (gate closes) or `"e\u{fe0f}"` turns white
/// (gate opens too wide).
#[test]
fn emoji_font_glyph_presence_matches_gate_expectations() {
    let font = Font::from_path("/System/Library/Fonts/Apple Color Emoji.ttc", 0).unwrap();
    for ch in ['🦞', '👩', '🇨', '❤', '✅', '☕', '⚽'] {
        assert!(
            font.glyph_for_char(ch).is_some(),
            "{ch} must exist in Apple Color Emoji for the color gate"
        );
    }
    assert!(
        font.glyph_for_char('e').is_none(),
        "'e' must NOT exist in Apple Color Emoji (text cluster stays on A8)"
    );
}

/// Regression: a CJK glyph (PingFang '中') and a Latin glyph (Menlo 'M')
/// must share the same baseline when rasterized into the same cell_h.
///
/// Pre-fix, `glyph_transform` shifted each glyph by *its own font's*
/// |descent|. Menlo and PingFang have different descent ratios, so the
/// CJK glyph's baseline landed ~3-5px below the Latin baseline — visible
/// in `ls -l` output as Chinese characters "sinking" below the Latin row.
///
/// The fix anchors the vertical offset to the **primary (Latin) font's**
/// metrics regardless of which font rasterizes the glyph, so both scripts
/// share one baseline. This test quantifies the gap by comparing the
/// bottom ink row of each glyph: if they share a baseline, their bottom
/// ink rows (descender depth) should be within ~2px.
#[test]
fn cjk_and_latin_share_baseline() {
    let menlo = Font::from_path("/System/Library/Fonts/Menlo.ttc", 0).unwrap();

    // Diagnostic: print primary vs CJK font vertical metrics so the
    // baseline gap is visible in test output across machines.
    {
        let m = menlo.metrics();
        let scaled = 28.0f32;
        let upem = m.units_per_em as f32;
        eprintln!(
            "Menlo: ascent={:.2}px descent={:.2}px (|d|={:.2})",
            m.ascent * (scaled / upem),
            m.descent * (scaled / upem),
            m.descent.abs() * (scaled / upem)
        );
    }
    // Match the production CJK fallback chain (PingFang → STHeiti → Hiragino).
    let cjk_font_paths = [
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
    ];
    let pingfang = match cjk_font_paths
        .iter()
        .find_map(|p| Font::from_path(p, 0).ok())
    {
        Some(f) => f,
        None => {
            eprintln!("no CJK font available — skipping");
            return;
        }
    };
    {
        let m = pingfang.metrics();
        let scaled = 28.0f32;
        let upem = m.units_per_em as f32;
        eprintln!(
            "CJK:   ascent={:.2}px descent={:.2}px (|d|={:.2})",
            m.ascent * (scaled / upem),
            m.descent * (scaled / upem),
            m.descent.abs() * (scaled / upem)
        );
    }

    let scaled = 28.0f32;
    let cell_w = 17u32;
    let cell_h = 34u32;

    // Production cell geometry
    let cjk_w = cell_w * 2;

    // Rasterize Latin 'M' and CJK '中' at the SAME cell_h.
    // CRITICAL: both pass Menlo's descent as the baseline anchor — the
    // production invariant. Pre-fix, CJK used its own (smaller) descent.
    let primary_descent = descent_px(&menlo, scaled);
    let m_px = GlyphAtlas::rasterize_glyph(
        &menlo,
        'M',
        scaled,
        cell_w,
        cell_h,
        false,
        primary_descent,
        false,
    );
    let cjk_px = GlyphAtlas::rasterize_glyph(
        &pingfang,
        '中',
        scaled,
        cjk_w,
        cell_h,
        true,
        primary_descent,
        false,
    );

    let (m_top, m_bot) = ink_y_bbox(&m_px, cell_w, cell_h).expect("'M' must have ink");
    let (cjk_top, cjk_bot) = ink_y_bbox(&cjk_px, cjk_w, cell_h).expect("'中' must have ink");

    eprintln!(
        "Latin 'M' ink rows [{m_top},{m_bot}] center={:.1}; \
         CJK '中' ink rows [{cjk_top},{cjk_bot}] center={:.1}",
        (m_top + m_bot) as f32 / 2.0,
        (cjk_top + cjk_bot) as f32 / 2.0
    );

    // Perception of "sinking": the visual center of the CJK glyph should
    // not sit noticeably below the Latin glyph's center. CJK ideographs
    // fill their em box, so when both share a baseline, '中' naturally
    // extends further down — but its vertical center must still sit
    // within ~2px of 'M's center for the row to read as one line.
    let m_center = (m_top + m_bot) as f32 / 2.0;
    let cjk_center = (cjk_top + cjk_bot) as f32 / 2.0;
    let center_gap = (m_center - cjk_center).abs();
    assert!(
        center_gap <= 2.0,
        "CJK vertical center {cjk_center:.1} is >2px below Latin center {m_center:.1} \
         (Δ={center_gap:.1}px); mixed-script rows will look like CJK sinks. \
         ink: M=[{m_top},{m_bot}] 中=[{cjk_top},{cjk_bot}]"
    );
}

/// 2026-10-16 forensics (openclaw update symbol complaint): run the openclaw
/// update symbol set through the ACTUAL glyph pipeline functions and report
/// pixel yields. Run with:
///   cargo test -p weft_app probe_openclaw -- --nocapture
#[test]
fn probe_openclaw_symbol_rasterization() {
    use font_kit::source::SystemSource;

    let scaled_size = 26.0f32; // 13pt @2x, the common case
    let primary = super::font::resolve_font("Menlo", &["/System/Library/Fonts/Menlo.ttc"])
        .expect("Menlo must load");
    let symbols = super::font::resolve_font(
        "Apple Symbols",
        &["/System/Library/Fonts/Apple Symbols.ttf"],
    );
    let emoji = super::font::resolve_font(
        "Apple Color Emoji",
        &["/System/Library/Fonts/Apple Color Emoji.ttc"],
    );

    // CJK chain as configured by default.
    let cjk = super::font::resolve_font(
        "PingFang SC",
        &[
            "/System/Library/Fonts/PingFang.ttc",
            "/System/Library/Fonts/STHeiti Light.ttc",
        ],
    );
    let pingfang_family = SystemSource::new()
        .select_family_by_name("PingFang SC")
        .is_ok();
    println!(
        "CJK: resolve_font(PingFang SC, [PingFang.ttc, STHeiti Light.ttc]) = {}",
        cjk.is_some()
    );
    println!("CJK: CoreText family lookup 'PingFang SC' = {pingfang_family}");

    // Loader-equivalent metrics (glyph.rs init).
    let cell_w = super::GlyphAtlas::measure_advance(&primary, 'M', scaled_size).max(1);
    let cell_h = (scaled_size * 1.2f32).ceil() as u32;
    let m = primary.metrics();
    let descent = m.descent.abs() * (scaled_size / m.units_per_em as f32);
    println!("metrics: cell {cell_w}x{cell_h}, descent {descent:.1}px");

    // --- narrow ladder: primary -> Apple Symbols -> Menlo -> blank ---
    // v1.13.14: reuse the production ladder instead of an inline copy. This
    // probe's primary IS Menlo, so the third hop receives the same face
    // cloned (font-kit `Font` is a cheap #[derive(Clone)] handle). Bound to a
    // local first — `&Some(primary.clone())` as a temporary would be E0716.
    println!("--- narrow chars (ladder: primary -> Apple Symbols -> Menlo -> blank) ---");
    let menlo_hop = Some(primary.clone());
    for ch in ['✓', '✔', '✗', '❯', '→', '◇', '⠋', '⚠', '│'] {
        let in_primary = primary.glyph_for_char(ch).is_some();
        let in_symbols = symbols
            .as_ref()
            .is_some_and(|f| f.glyph_for_char(ch).is_some());
        let chosen = if in_primary {
            "Menlo"
        } else if in_symbols {
            "AppleSymbols"
        } else {
            "BLANK(menlo)"
        };
        let font_used = super::narrow_font_ladder(&primary, &symbols, &menlo_hop, ch);
        let pixels = super::GlyphAtlas::rasterize_glyph(
            font_used,
            ch,
            scaled_size,
            cell_w,
            cell_h,
            false,
            descent,
            false,
        );
        let nonzero = pixels.iter().filter(|&&p| p > 0).count();
        println!(
            "U+{:04X} {:>2}: primary={} symbols={} -> route={} {} px nonzero /{} (A8)",
            ch as u32,
            ch,
            in_primary,
            in_symbols,
            chosen,
            nonzero,
            cell_w * cell_h
        );
    }

    // --- emoji color path: rasterize_emoji_rgba ---
    println!("--- color emoji (Apple Color Emoji, CTFontDrawGlyphs) ---");
    if let Some(emoji_font) = emoji.as_ref() {
        for ch in ['✅', '❌', 'ℹ'] {
            let rgba = super::font::rasterize_emoji_rgba(
                emoji_font,
                ch,
                scaled_size,
                cell_w * 2,
                cell_h,
                descent,
            );
            match rgba {
                Some(buf) => {
                    let opaque = buf.chunks_exact(4).filter(|px| px[3] > 0).count();
                    println!(
                        "U+{:04X} {:>2}: RGBA Some, alpha>0 px = {}/{}",
                        ch as u32,
                        ch,
                        opaque,
                        cell_w * 2 * cell_h
                    );
                }
                None => println!("U+{:04X} {:>2}: RGBA None (BLANK)", ch as u32, ch),
            }
        }
    } else {
        println!("emoji font DID NOT LOAD -> all color emoji render blank");
    }

    // --- cluster path gate: ◇+FE0F, ⚠️ ---
    println!("--- VS16 clusters route via cluster query (color gate) ---");
    let wants_color = super::query::cluster_wants_color_atlas(true, false);
    println!("cluster_wants_color_atlas(is_emoji=true) = {wants_color} (gate for FE0F clusters)");

    // Hard assertions tied to the actual bug question:
    assert!(
        emoji.is_some(),
        "Apple Color Emoji must load (color emoji fallback)"
    );
    assert!(cjk.is_some(), "CJK fallback must resolve on this system");
    let em_rgba = super::font::rasterize_emoji_rgba(
        emoji.as_ref().unwrap(),
        '✅',
        scaled_size,
        cell_w * 2,
        cell_h,
        descent,
    );
    assert!(
        em_rgba.is_some_and(|b| b.chunks_exact(4).any(|px| px[3] > 0)),
        "✅ must rasterize to a non-blank RGBA bitmap (else it renders blank)"
    );
}

// ── v1.13.14: narrow ladder third hop (Menlo) ───────────────────────────
// 取证: docs/FIX_hack-checkmark-fallback.md — Apple Symbols 缺 ✓/✔/✗，
// 主字体也缺时（用户 font=Hack）两跳阶梯盲走 primary → 全零位图永久缓存 → 空白。

/// Fact pin: Apple Symbols does NOT carry the check/cross family (✓ U+2713 /
/// ✔ U+2714 / ✗ U+2717) while Menlo has all three (2026-10-19, verified via
/// both CTFontGetGlyphsForCharacters and CTFontCopyCharacterSet). This pins
/// the system facts that justify the ladder's third hop — deleting the hop
/// would re-break ✓/✔/✗ for primaries that lack the family.
#[test]
fn fact_pin_apple_symbols_lacks_check_family_menlo_has_it() {
    let symbols = super::font::resolve_font(
        "Apple Symbols",
        &["/System/Library/Fonts/Apple Symbols.ttf"],
    );
    let menlo = super::font::resolve_font("Menlo", &["/System/Library/Fonts/Menlo.ttc"]);
    let symbols = symbols.expect("Apple Symbols must load on macOS");
    let menlo = menlo.expect("Menlo must load on macOS");
    for ch in ['✓', '✔', '✗'] {
        assert!(
            symbols.glyph_for_char(ch).is_none(),
            "fact pin: Apple Symbols must NOT have U+{:04X} (this gap is why the ladder needs the Menlo hop)",
            ch as u32
        );
        assert!(
            menlo.glyph_for_char(ch).is_some(),
            "fact pin: Menlo must have U+{:04X} (the third hop must reach ink)",
            ch as u32
        );
    }
}

/// v1.13.14: with a primary that lacks ✓ — Apple Symbols itself, so hop 1 and
/// hop 2 are the same font and both miss — the ladder's third hop must route
/// ✓ to Menlo, `get_or_rasterize` must succeed through the A8 mask path (not
/// the color atlas), and Menlo must produce a non-blank bitmap.
#[test]
fn narrow_ladder_third_hop_reaches_menlo() {
    let Some(device) = metal::Device::system_default() else {
        eprintln!("skipping: no Metal device");
        return;
    };
    let config = weft_core::config::FontConfig {
        family: "Apple Symbols".into(),
        ..Default::default()
    };
    let mut atlas = crate::glyph::GlyphAtlas::new(&device, &config, 2.0);

    // Ladder: primary (Apple Symbols) misses ✓, the symbol hop is the same
    // font and misses too → the third hop must hand back Menlo.
    let font = super::narrow_font_ladder(
        &atlas.primary_font,
        &atlas.symbol_font,
        &atlas.menlo_font,
        '✓',
    );
    assert_eq!(
        font.postscript_name().as_deref(),
        Some("Menlo-Regular"),
        "third hop must route ✓ to Menlo when primary and Apple Symbols both miss"
    );

    // End-to-end: the scalar path rasterizes ✓ via the mask (non-color) atlas.
    let info = atlas
        .get_or_rasterize('✓', crate::glyph::GlyphStyle::REGULAR)
        .expect("✓ must rasterize via the Menlo third hop");
    assert!(
        !info.is_color,
        "✓ must stay on the A8 mask path, not the RGBA color atlas"
    );

    // Pixel-level: Menlo actually rasterizes ✓ into ink (probe-style call).
    let menlo = atlas.menlo_font.as_ref().expect("Menlo must load on macOS");
    let scaled_size = 26.0f32; // 13pt @2x, matches the probe above
    let cell_w = super::GlyphAtlas::measure_advance(&atlas.primary_font, 'M', scaled_size).max(1);
    let cell_h = (scaled_size * 1.2f32).ceil() as u32;
    let pixels = super::GlyphAtlas::rasterize_glyph(
        menlo,
        '✓',
        scaled_size,
        cell_w,
        cell_h,
        false,
        descent_px(menlo, scaled_size),
        false,
    );
    assert!(
        pixels.iter().filter(|&&p| p > 0).count() > 0,
        "Menlo must rasterize ✓ to a non-blank A8 bitmap (else the third hop still renders blank)"
    );
}

/// v1.13.14 real-world repro: 用户 config font.family=Hack 场景 — Hack lacks
/// ✓/✔/✗ AND Apple Symbols lacks them, so the ladder's third hop must route
/// ✓ to Menlo. Skips gracefully where Hack isn't installed.
#[test]
fn hack_primary_routes_check_via_menlo() {
    use font_kit::source::SystemSource;
    if SystemSource::new().select_family_by_name("Hack").is_err() {
        eprintln!("skipping hack_primary_routes_check_via_menlo: Hack not installed");
        return;
    }
    let Some(device) = metal::Device::system_default() else {
        eprintln!("skipping: no Metal device");
        return;
    };
    let config = weft_core::config::FontConfig {
        family: "Hack".into(),
        ..Default::default()
    };
    let atlas = crate::glyph::GlyphAtlas::new(&device, &config, 2.0);
    let font = super::narrow_font_ladder(
        &atlas.primary_font,
        &atlas.symbol_font,
        &atlas.menlo_font,
        '✓',
    );
    assert_eq!(
        font.postscript_name().as_deref(),
        Some("Menlo-Regular"),
        "用户 config font.family=Hack 场景: Hack 与 Apple Symbols 都无 ✓, 阶梯第三跳必须路由到 Menlo"
    );
}

/// 2026-10-16 forensics: block view draws glyphs via `get_style` (pure cache
/// lookup, paint/text.rs:103) — prove that (a) a fresh atlas MISSES ✅ so the
/// block view would draw a blank gap, (b) the live-grid warm path fixes it,
/// (c) ℹ/⚠ are NOT classified emoji so the block view draws them narrow from
/// Menlo (monochrome) while the live grid draws the FE0F cluster wide/color.
#[test]
fn probe_block_view_glyph_lookup_misses_unwarmed_emoji() {
    let Some(device) = metal::Device::system_default() else {
        eprintln!("skipping: no Metal device");
        return;
    };
    let mut atlas =
        crate::glyph::GlyphAtlas::new(&device, &weft_core::config::FontConfig::default(), 2.0);

    // (a) fresh atlas: block view's ONLY lookup path misses ✅ → blank gap.
    assert!(
        atlas
            .get_style('✅', crate::glyph::GlyphStyle::REGULAR)
            .is_none(),
        "fresh atlas must miss ✅ (block view draws blank until warmed)"
    );

    // (b) live-grid warm path (get_or_rasterize) succeeds via color atlas.
    let warmed = atlas.get_or_rasterize('✅', crate::glyph::GlyphStyle::REGULAR);
    assert!(warmed.is_some(), "✅ must warm through get_or_rasterize");
    assert!(warmed.unwrap().is_color, "✅ must take the RGBA color path");

    // (c) after warm, block view lookup hits the shared cache.
    assert!(atlas
        .get_style('✅', crate::glyph::GlyphStyle::REGULAR)
        .is_some());

    // (d) ℹ/⚠/◇ are not in is_emoji_char → block view scalar path is
    // narrow/monochrome, unlike the live grid's wide FE0F cluster.
    assert!(
        !crate::glyph::font::is_emoji_char('\u{2139}'),
        "ℹ must NOT be classified emoji"
    );
    assert!(
        !crate::glyph::font::is_emoji_char('\u{26a0}'),
        "⚠ must NOT be classified emoji"
    );
    assert!(
        !crate::glyph::font::is_emoji_char('\u{25c7}'),
        "◇ must NOT be classified emoji"
    );
    assert!(crate::glyph::font::is_emoji_char('✅'));
    assert!(crate::glyph::font::is_emoji_char('❌'));

    // (e) block view cluster splitting: push_block_output_text iterates
    // chars, so "ℹ️" contributes width-1 'ℹ' + width-0 FE0F = 1 column,
    // while the live grid stores the cluster at width 2.
    assert_eq!(weft_core::grid::terminal_char_width('ℹ'), 1);
    assert_eq!(weft_core::grid::terminal_text_width("ℹ\u{fe0f}"), 2);
}

// ── v1.13.13 FIX-B: cluster cache has NO eviction ───────────────────────

/// FIX-B ①: >512 unique clusters (the old MAX_CLUSTER_CACHE_ENTRIES) must
/// ALL stay cached — no LRU eviction — and re-access must NOT re-rasterize
/// (same GlyphInfo, i.e. no atlas slot re-burned). The old eviction path
/// leaked the evicted entry's texture slot AND re-rasterized re-accesses
/// into fresh slots; removal makes the cluster cache match the scalar
/// cache's "rasterize exactly once" behavior.
#[test]
fn cluster_cache_holds_every_unique_cluster_without_eviction() {
    let Some(device) = metal::Device::system_default() else {
        eprintln!("skipping: no Metal device");
        return;
    };
    let mut atlas =
        crate::glyph::GlyphAtlas::new(&device, &weft_core::config::FontConfig::default(), 2.0);

    // 600 unique two-scalar clusters (base letters + combining acute) —
    // beyond the old 512-entry cap. Latin Extended-A/B bases keep the
    // slots narrow (is_wide = false), so 600 entries fit the 2048² atlas
    // with room to spare.
    let cluster = |i: u32| {
        let base = char::from_u32(0x0100 + i).expect("in-range scalar");
        let full = format!("{base}\u{0301}");
        (full, base)
    };

    // First insert: record the GlyphInfo so re-access can be compared.
    let (first_cluster, first_base) = cluster(0);
    let first_info = *atlas
        .get_or_rasterize_cluster(&first_cluster, first_base, false)
        .expect("the first cluster must rasterize");

    for i in 1..600 {
        let (c, base) = cluster(i);
        atlas
            .get_or_rasterize_cluster(&c, base, false)
            .expect("every cluster must rasterize (no eviction pressure)");
    }

    // No eviction: the OLDEST entry (the first-inserted cluster, which the
    // old LRU would have evicted at insert #513) still hits.
    assert!(
        atlas.get_cluster(&first_cluster).is_some(),
        ">512 unique clusters must NOT evict earlier entries"
    );
    // No re-rasterization: re-inserting the first cluster returns the SAME
    // GlyphInfo (same atlas slot). Under the old path the re-access after
    // eviction would have allocated a fresh slot.
    let reaccessed = atlas
        .get_or_rasterize_cluster(&first_cluster, first_base, false)
        .expect("re-access must hit the cache");
    assert_eq!(
        reaccessed.uv_origin, first_info.uv_origin,
        "re-access must reuse the original atlas slot (no re-rasterization)"
    );
    assert_eq!(reaccessed.uv_size, first_info.uv_size);
    assert_eq!(reaccessed.size, first_info.size);
}

/// FIX-B ②: the hit/miss counters (kept as diagnostics after the LRU
/// removal) still count both paths — a miss on a fresh atlas, a hit after
/// insert, and no counter movement on the insertion call itself.
#[test]
fn cluster_cache_counts_hits_and_misses() {
    let Some(device) = metal::Device::system_default() else {
        eprintln!("skipping: no Metal device");
        return;
    };
    let mut atlas =
        crate::glyph::GlyphAtlas::new(&device, &weft_core::config::FontConfig::default(), 2.0);

    let cluster = "e\u{0301}";
    let hits_before = atlas.cluster_hits.get();
    let misses_before = atlas.cluster_misses.get();

    // Miss: fresh atlas has never seen the cluster.
    assert!(atlas.get_cluster(cluster).is_none());
    assert_eq!(atlas.cluster_misses.get(), misses_before + 1);
    assert_eq!(atlas.cluster_hits.get(), hits_before);

    // Insert (no counter movement on the insert path itself).
    atlas.get_or_rasterize_cluster(cluster, 'e', false);
    assert_eq!(atlas.cluster_hits.get(), hits_before);
    assert_eq!(atlas.cluster_misses.get(), misses_before + 1);

    // Hit: the inserted cluster resolves through the cache.
    assert!(atlas.get_cluster(cluster).is_some());
    assert_eq!(atlas.cluster_hits.get(), hits_before + 1);
}

// ── v1.13.15: wide ladder + Apple SD Gothic Neo fallback ────────────────
// 取证: docs/PLAN_v1.13.15.md §〇 — 旧 is_wide 分支
// `cjk_font.unwrap_or(primary)` 加载后无条件选用,PingFang 缺的宽字符渲染
// 其全零位图 = 空白。
//
// 本机实测(2026-10-19, macOS 27.0.1;三法一致:font-kit `glyph_for_char`
// 运行时探测 + CoreText 双 API 复核 CTFontGetGlyphsForCharacters/
// CTFontCopyCharacterSet + AssetsV2 PingFang.ttc cmap 直读):
//  - PingFang SC(家族解析实际落在
//    /System/Library/AssetsV2/com_apple_MobileAsset_Font8/.../PingFang.ttc,
//    老 /System/Library/Fonts/PingFang.ttc 已不存在)自带假名/・/𠮷,
//    缺韩文全族(한/가/㈜/㉠/𛀀)—— 与 PLAN §〇 取证表的假名/ExtB 行不符,
//    断言以本机实测为准。
//  - Apple SD Gothic Neo 补韩文全族 + 假名;但 𛀀(1B000)/ExtB 亦缺
//    (PLAN §〇 的"SDG ExtB-H 全 Y/1B000 Y"在本机不成立,留 A-4)。

/// Fact pin: PingFang SC does NOT carry the hangul family (syllables 한/가,
/// parenthesized ㈜ U+321C, circled ㉠ U+3260, kana-supplement 𛀀 U+1B000)
/// — while Apple SD Gothic Neo has the full hangul family AND kana. This
/// pins the system facts that justify the wide ladder's second rung —
/// deleting the rung would re-blank hangul for wide routing.
#[test]
fn fact_pin_pingfang_lacks_hangul_sdgothic_covers_hangul_and_kana() {
    let pingfang =
        super::font::resolve_font("PingFang SC", &["/System/Library/Fonts/PingFang.ttc"])
            .expect("PingFang SC must load on macOS (fact pin target)");
    let sdg = super::font::resolve_font(
        "Apple SD Gothic Neo",
        &["/System/Library/Fonts/AppleSDGothicNeo.ttc"],
    )
    .expect("Apple SD Gothic Neo must load on macOS (wide fallback)");
    // The gap that justifies the wide rung: PingFang misses every hangul
    // class (the old unwrap_or branch rendered them as its all-zero bitmap).
    for ch in ['한', '가', '㈜', '㉠', '𛀀'] {
        assert!(
            pingfang.glyph_for_char(ch).is_none(),
            "fact pin: PingFang SC must NOT have U+{:04X} (this gap is why the wide ladder needs the SDG rung)",
            ch as u32
        );
    }
    // The rung must reach ink: SDG carries the whole hangul family plus
    // kana (the ladder's cjk-first keeps kana on PingFang where present,
    // but SDG must still be able to supply both classes).
    for ch in ['한', '가', '㈜', '㉠', 'あ', 'ア', '・'] {
        assert!(
            sdg.glyph_for_char(ch).is_some(),
            "fact pin: Apple SD Gothic Neo must have U+{:04X} (the wide rung must reach ink)",
            ch as u32
        );
    }
    // Pin the flip side: on this system PingFang itself covers kana —
    // cjk-first serves them without ever loading SDG. A regression here
    // means the kana range gate would start loading SDG for characters
    // PingFang already carries.
    for ch in ['あ', 'ア', '・', '中'] {
        assert!(
            pingfang.glyph_for_char(ch).is_some(),
            "fact pin: PingFang SC must have U+{:04X} on this system (kana territory served cjk-first, zero SDG cost)",
            ch as u32
        );
    }
    // ExtB 覆盖版本相关(本机 AssetsV2 PingFang 有 𠮷;CI macos-14/Sonoma 的
    // 老 PingFang 无在库证据),不作为跨版本硬事实:有→pin 自持;无→记录
    // 版本差异跳过(仿 PingFang.ttc 路径探测先例)。
    if pingfang.glyph_for_char('𠮷').is_none() {
        eprintln!(
            "note: PingFang SC lacks U+20BB7 on this macOS build — ExtB coverage is version-dependent, optional pin skipped"
        );
    }
}

/// Range gate matrix (`may_need_wide_fallback`): true exactly for the
/// v1.13.15 取证定表 blocks (kana / hangul jamo + syllables / compat jamo /
/// 3200-321F / 3260-327F / 1B000-1B16F / ExtB-H); false for PingFang
/// territory (URO, CJK punctuation, fullwidth forms) and the documented
/// exclusion blocks (31F0 Ainu kana — SDG 双法 N; 32A4 circled ideographs —
/// PingFang territory).
#[test]
fn may_need_wide_fallback_ranges() {
    for ch in ['あ', 'ア', '・', '한', 'ᄀ', '가', '㈜', '㉠', '𛀀', '𠮷'] {
        assert!(
            super::query::may_need_wide_fallback(ch),
            "U+{:04X} must pass the wide-fallback range gate",
            ch as u32
        );
    }
    for ch in ['中', '！', '、', '→', 'A', '㊤', 'ㇰ'] {
        assert!(
            !super::query::may_need_wide_fallback(ch),
            "U+{:04X} must NOT pass the wide-fallback range gate (PingFang territory or documented exclusion)",
            ch as u32
        );
    }
}

/// v1.13.15: with the default config (primary Menlo), '한' is wide and
/// PingFang misses it — `ensure_script_font` must lazily load Apple SD
/// Gothic Neo (FIX-B), the wide ladder must route '한' to it, and
/// `get_or_rasterize` must succeed through the A8 mask path (not the color
/// atlas) for both hangul and kana.
#[test]
fn wide_ladder_routes_kana_hangul_to_sdgothic() {
    let Some(device) = metal::Device::system_default() else {
        eprintln!("skipping: no Metal device");
        return;
    };
    let mut atlas =
        crate::glyph::GlyphAtlas::new(&device, &weft_core::config::FontConfig::default(), 2.0);

    // Warm path: '한' (wide, non-emoji) triggers the cjk load + the FIX-B
    // SDG load, then rasterizes via the mask atlas.
    let info = atlas
        .get_or_rasterize('한', crate::glyph::GlyphStyle::REGULAR)
        .expect("한 must rasterize via the wide ladder");
    assert!(
        !info.is_color,
        "한 must stay on the A8 mask path, not the RGBA color atlas"
    );

    // Ladder routing: cjk (PingFang) misses '한' → the wide rung must hand
    // back the SDG face.
    let font = super::wide_font_ladder(
        &atlas.cjk_font,
        &atlas.wide_fallback_font,
        &atlas.primary_font,
        &atlas.symbol_font,
        &atlas.menlo_font,
        '한',
    );
    assert_eq!(
        font.postscript_name().as_deref(),
        Some("AppleSDGothicNeo-Regular"),
        "wide ladder must route '한' to Apple SD Gothic Neo when PingFang misses it"
    );

    // Kana rides the same rung end-to-end.
    let kana = atlas
        .get_or_rasterize('あ', crate::glyph::GlyphStyle::REGULAR)
        .expect("あ must rasterize via the wide ladder");
    assert!(
        !kana.is_color,
        "あ must stay on the A8 mask path, not the RGBA color atlas"
    );

    // Pixel-level: SDG actually rasterizes '한' into ink (probe-style call,
    // parameters follow the narrow_ladder_third_hop precedent; wide cells
    // are cell_w * 2 + is_wide=true, matching the production call).
    let sdg = atlas
        .wide_fallback_font
        .as_ref()
        .expect("FIX-B must have loaded Apple SD Gothic Neo");
    let scaled_size = 26.0f32; // 13pt @2x, matches the probe above
    let cell_w = super::GlyphAtlas::measure_advance(&atlas.primary_font, 'M', scaled_size).max(1);
    let cell_h = (scaled_size * 1.2f32).ceil() as u32;
    let pixels = super::GlyphAtlas::rasterize_glyph(
        sdg,
        '한',
        scaled_size,
        cell_w * 2,
        cell_h,
        true,
        descent_px(sdg, scaled_size),
        false,
    );
    assert!(
        pixels.iter().filter(|&&p| p > 0).count() > 0,
        "Apple SD Gothic Neo must rasterize '한' to a non-blank A8 bitmap (else the wide rung still renders blank)"
    );
}

/// v1.13.15 金像不变式: cjk-first 保序 — '中' is in PingFang's territory
/// (URO), so the wide ladder must hand it back to PingFang EVEN when SDG is
/// already loaded (mixed Chinese+Korean session — SDG also carries 中, so a
/// swapped probe order would steal it and drift every pre-existing wide
/// glyph). The hangul warm-up puts the atlas in that worst-case world; the
/// trailing assert pins the range gate keeping SDG unloaded for pure
/// Chinese sessions (zero cost).
#[test]
fn wide_ladder_keeps_chinese_on_pingfang() {
    let Some(device) = metal::Device::system_default() else {
        eprintln!("skipping: no Metal device");
        return;
    };
    let mut atlas =
        crate::glyph::GlyphAtlas::new(&device, &weft_core::config::FontConfig::default(), 2.0);

    // Range gate: '中' alone must NOT trigger the SDG load (pure-Chinese
    // session zero cost — the wide fallback stays None).
    atlas
        .get_or_rasterize('中', crate::glyph::GlyphStyle::REGULAR)
        .expect("中 must rasterize via the CJK rung");
    assert!(
        atlas.wide_fallback_font.is_none(),
        "范围门: '中' 是 PingFang 领地,SDG 不得加载(纯中文会话零成本)"
    );

    // Warm the wide fallback: after hangul, wide_fallback_font is Some
    // — the only world where cjk-first ordering is actually under threat.
    atlas
        .get_or_rasterize('한', crate::glyph::GlyphStyle::REGULAR)
        .expect("한 must rasterize via the wide rung");
    assert!(
        atlas.wide_fallback_font.is_some(),
        "precondition: hangul must have loaded the SDG fallback"
    );

    let font = super::wide_font_ladder(
        &atlas.cjk_font,
        &atlas.wide_fallback_font,
        &atlas.primary_font,
        &atlas.symbol_font,
        &atlas.menlo_font,
        '中',
    );
    assert_eq!(
        font.postscript_name().as_deref(),
        Some("PingFangSC-Regular"),
        "cjk-first 保序(金像不变式): 即便 SDG 已加载,'中' 必须仍来自 PingFang,不得被 SDG 抢走"
    );
}
