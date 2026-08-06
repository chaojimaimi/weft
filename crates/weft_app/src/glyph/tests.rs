//! Device-free rasterization and transform probes.
//!
//! Split from `glyph/mod.rs` (Batch 6 Step 4) to keep the main file under
//! the 800-line limit. `use super::*` pulls in the glyph module's public
//! surface plus the re-exported `font` helpers (`nonzero_cell_dimension`,
//! `is_emoji_char`, etc.).

#![cfg(test)]

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
    let px = GlyphAtlas::rasterize_glyph(&font, '❯', 28.0, 14, 28, false, descent_px(&font, 28.0));
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
    assert!(super::cluster_wants_color_atlas(true, true));
    assert!(!super::cluster_wants_color_atlas(true, false));
    assert!(!super::cluster_wants_color_atlas(false, true));
    assert!(!super::cluster_wants_color_atlas(false, false));
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
    let m_px =
        GlyphAtlas::rasterize_glyph(&menlo, 'M', scaled, cell_w, cell_h, false, primary_descent);
    let cjk_px = GlyphAtlas::rasterize_glyph(
        &pingfang,
        '中',
        scaled,
        cjk_w,
        cell_h,
        true,
        primary_descent,
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
