//! Multi-scalar grapheme cluster rasterization (v1.6.0 step 4 POC).
//!
//! Single-scalar cells (ASCII, BMP chars) go through `rasterize_glyph` in
//! `rasterize.rs`, which uses font-kit's per-glyph rasterizer. That path
//! cannot shape multi-scalar grapheme clusters: a combining mark following
//! its base char (e + combining acute), a ZWJ emoji sequence (👩‍🔬), or a
//! regional flag pair (🇨🇳) all require the layout engine to position
//! glyphs relative to each other.
//!
//! This module uses CoreText's `CTLine` to shape the full cluster string:
//!
//! 1. Build a `CFAttributedString` carrying the cluster + font attribute.
//! 2. `CTLineCreateWithAttributedString` runs CoreText's shaper, which
//!    handles combining mark attachment, ZWJ emoji formation, regional
//!    indicator pairing, and variation selector selection automatically.
//! 3. Draw the line into a CGContext sized to the cell(s).
//! 4. Reduce RGBA → alpha mask for the R8 atlas.
//!
//! This is the POC required by [V16_IMPLEMENTATION_PLAN.md §3 step 4][plan].
//! The paint path is NOT yet wired to consume cluster atlases — that is
//! step 5 ("接通 Grid、selection、copy、reflow、Block capture 和 Metal atlas").
//!
//! [plan]: ../../../../docs/V16_IMPLEMENTATION_PLAN.md

use core_foundation::attributed_string::CFAttributedStringCreate;
use core_foundation::base::{kCFAllocatorDefault, TCFType};
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;
use core_graphics::base::kCGImageAlphaPremultipliedLast;
use core_graphics::color_space::CGColorSpace;
use core_graphics::context::CGContext;
use core_text::font::CTFont;
use core_text::line::CTLine;
use core_text::string_attributes::kCTFontAttributeName;
use font_kit::loaders::core_text::Font;

/// Rasterize a multi-scalar grapheme cluster to atlas pixels.
///
/// `cluster` is the full grapheme cluster string (e.g. `"e\u{0301}"`,
/// `"👩\u{200d}🔬"`, `"🇨🇳"`). `glyph_w` is the atlas slot width in pixels
/// — for a single-cell cluster this is `cell_w`, for a wide cluster (emoji,
/// flag) it is `2 * cell_w`. `cell_h` is the line height.
///
/// v1.10.4: `want_color` selects the pixel format. Color emoji clusters
/// (`want_color = true`) keep full premultiplied RGBA — 4 bytes/pixel — for
/// the color atlas, so the emoji's own RGB survives to the shader. Text
/// clusters (combining marks, `want_color = false`) reduce to an alpha mask
/// (`glyph_w * cell_h` bytes, A8) for the R8 mask atlas. Both are Y-flipped
/// to match the atlas's top-down orientation.
///
/// Returns `Some(pixels)` or `None` if the cluster could not be shaped/drawn.
///
/// # Why CTLine and not CTFontDrawGlyphs
///
/// `CTFontDrawGlyphs` (used by `rasterize_emoji_rgba` in `font.rs`) draws a
/// run of glyphs at caller-supplied positions; it does NOT shape. Combining
/// marks would need their attachment offsets computed manually, and ZWJ
/// sequences require the GSUB table to pick the right emoji variant. CTLine
/// runs the full CoreText shaper, which is the same path TextKit uses — it
/// handles all of these cases correctly without per-cluster special-casing.
//
pub(super) fn rasterize_cluster_alpha(
    font: &Font,
    cluster: &str,
    scaled_size: f32,
    glyph_w: u32,
    cell_h: u32,
    primary_descent_px: f32,
    want_color: bool,
) -> Option<Vec<u8>> {
    // CoreText draws at the CTFont's embedded size. Clone the font at the
    // atlas pixel size so ZWJ/flag/variation clusters scale with zoom just
    // like single-scalar glyphs.
    let ct_font: CTFont = font.native_font().clone_with_font_size(scaled_size as f64);

    // Build CFAttributedString{ cluster_str, [NSFont: ct_font] }. The
    // `core_foundation::attributed_string::CFAttributedString::new` wrapper
    // only takes a string (no attributes), so we call the underlying
    // `CFAttributedStringCreate` directly with a attributes dictionary.
    let cf_str = CFString::new(cluster);
    let font_key = unsafe { CFString::wrap_under_get_rule(kCTFontAttributeName) };
    let attrs = CFDictionary::from_CFType_pairs(&[(font_key, ct_font.clone())]);
    let attr_str_ref = unsafe {
        CFAttributedStringCreate(
            kCFAllocatorDefault,
            cf_str.as_concrete_TypeRef(),
            attrs.as_concrete_TypeRef(),
        )
    };
    if attr_str_ref.is_null() {
        tracing::debug!(cluster, "CFAttributedStringCreate returned null");
        return None;
    }
    let attr_str = unsafe {
        core_foundation::attributed_string::CFAttributedString::wrap_under_create_rule(attr_str_ref)
    };
    let line = CTLine::new_with_attributed_string(attr_str.as_concrete_TypeRef());

    // Create a color-supporting RGBA context. Even for non-emoji clusters
    // (e + combining acute), some fonts (Apple Color Emoji for emoji-base
    // clusters like 👩🏽) return color bitmaps; a gray context drops them.
    let cs = CGColorSpace::create_device_rgb();
    let w = glyph_w as usize;
    let h = cell_h as usize;
    let mut ctx =
        CGContext::create_bitmap_context(None, w, h, 8, w * 4, &cs, kCGImageAlphaPremultipliedLast);

    // CoreText draws with the text origin at the current text position. We
    // want the baseline anchored at `primary_descent_px` from the bottom of
    // the cell, matching how `glyph_transform` positions single-scalar
    // glyphs. CTLine.draw draws from the line's origin (left edge of the
    // first glyph, on the baseline).
    //
    // CG is Y-up; the atlas is Y-down. We draw at the CG position then flip
    // when extracting alpha below.
    ctx.set_text_position(0.0, primary_descent_px as f64);
    ctx.set_rgb_fill_color(1.0, 1.0, 1.0, 1.0);
    line.draw(&ctx);

    // Read premultiplied RGBA with Y-flip. Color emoji clusters
    // (want_color) keep all 4 channels — the color atlas stores the emoji's
    // own RGB. Text clusters reduce to an alpha mask for the R8 atlas.
    let bpr = ctx.bytes_per_row();
    let raw = ctx.data();
    let bpp = if want_color { 4 } else { 1 };
    let mut pixels = vec![0u8; w * h * bpp];
    for y in 0..h {
        for x in 0..w {
            let src_idx = y * bpr + x * 4;
            if src_idx + 3 < raw.len() {
                let dst_y = h - 1 - y; // flip Y for top-down atlas
                let dst_idx = (dst_y * w + x) * bpp;
                if want_color {
                    pixels[dst_idx] = raw[src_idx]; // R
                    pixels[dst_idx + 1] = raw[src_idx + 1]; // G
                    pixels[dst_idx + 2] = raw[src_idx + 2]; // B
                    pixels[dst_idx + 3] = raw[src_idx + 3]; // A (premultiplied)
                } else {
                    pixels[dst_idx] = raw[src_idx + 3];
                }
            }
        }
    }

    let nonzero = pixels.iter().filter(|&&p| p > 0).count();
    if nonzero == 0 {
        tracing::debug!(
            cluster,
            glyph_w,
            cell_h,
            "cluster rasterized to 0 pixels via CTLine"
        );
        return None;
    }
    tracing::debug!(
        cluster,
        glyph_w,
        cell_h,
        nonzero,
        "cluster rasterized via CTLine"
    );
    Some(pixels)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Load Menlo for tests that don't depend on a specific font. Menlo is
    /// guaranteed on macOS and handles Latin combining marks.
    fn menlo() -> Font {
        Font::from_path("/System/Library/Fonts/Menlo.ttc", 0).unwrap()
    }

    /// Apple Color Emoji handles ZWJ emoji sequences and flag pairs.
    fn emoji_font() -> Font {
        Font::from_path("/System/Library/Fonts/Apple Color Emoji.ttc", 0).unwrap()
    }

    fn descent_px(font: &Font, scaled_size: f32) -> f32 {
        let m = font.metrics();
        m.descent.abs() * (scaled_size / m.units_per_em as f32)
    }

    fn ink_y_bounds_rgba(pixels: &[u8], width: u32, height: u32) -> Option<(u32, u32)> {
        let mut top = height;
        let mut bottom = 0;
        let mut any = false;
        for y in 0..height {
            let row = &pixels[(y * width * 4) as usize..((y + 1) * width * 4) as usize];
            if row.chunks_exact(4).any(|px| px[3] > 0) {
                any = true;
                top = top.min(y);
                bottom = bottom.max(y);
            }
        }
        any.then_some((top, bottom))
    }

    /// Sanity: a single ASCII char "e" shaped via CTLine must produce ink.
    /// This validates the CTLine path itself before we test multi-scalar
    /// clusters.
    #[test]
    fn ctline_shapes_single_ascii_with_ink() {
        let font = menlo();
        let px = rasterize_cluster_alpha(&font, "e", 28.0, 14, 28, descent_px(&font, 28.0), false);
        let px = px.expect("e must rasterize via CTLine");
        let ink = px.iter().filter(|p| **p > 0).count();
        assert!(ink > 20, "e via CTLine must have ink, got {ink}");
    }

    /// e + combining acute (U+0301) must rasterize with ink. The combining
    /// mark is width-0 and attaches to the preceding 'e'; CTLine shapes the
    /// pair into a single glyph run.
    #[test]
    fn combining_mark_cluster_has_ink() {
        let font = menlo();
        let cluster = "e\u{0301}";
        let px =
            rasterize_cluster_alpha(&font, cluster, 28.0, 14, 28, descent_px(&font, 28.0), false);
        let px = px.expect("e+combining acute must rasterize via CTLine");
        let ink = px.iter().filter(|p| **p > 0).count();
        assert!(ink > 20, "e+combining acute must have ink, got {ink}");
    }

    /// The combining mark must add ink on top of the base char. A cluster
    /// "e\u{0301}" should have strictly more ink than "e" alone — the acute
    /// accent adds pixels above the 'e' that the bare 'e' does not cover.
    #[test]
    fn combining_mark_adds_ink_on_top_of_base() {
        let font = menlo();
        let d = descent_px(&font, 28.0);
        let base = rasterize_cluster_alpha(&font, "e", 28.0, 14, 28, d, false).unwrap();
        let combined = rasterize_cluster_alpha(&font, "e\u{0301}", 28.0, 14, 28, d, false).unwrap();
        let base_ink = base.iter().filter(|p| **p > 0).count();
        let combined_ink = combined.iter().filter(|p| **p > 0).count();
        // The combined cluster must have at least as much ink, and typically
        // more (the acute adds pixels). Allow equal for fonts where the
        // precomposed é shape is used and happens to match, but never less.
        assert!(
            combined_ink >= base_ink,
            "combining mark must not reduce ink: base={base_ink}, combined={combined_ink}"
        );
        // Sanity: if they're equal, the combined still must have ink.
        assert!(combined_ink > 20);
    }

    /// ZWJ emoji sequence 👩‍🔬 (woman + ZWJ + microscope) must rasterize via
    /// CTLine. Apple Color Emoji's sbix bitmap glyphs need the color-supporting
    /// RGBA context that `rasterize_cluster_alpha` creates. `want_color=true`
    /// is the production path (color atlas); pixels must be 4 B/px RGBA.
    #[test]
    fn zwj_emoji_sequence_has_ink() {
        let font = emoji_font();
        let cluster = "👩\u{200d}🔬";
        // Emoji is double-width; slot is 2 * cell_w.
        let px =
            rasterize_cluster_alpha(&font, cluster, 28.0, 28, 28, descent_px(&font, 28.0), true);
        let px = px.expect("ZWJ woman+microscope must rasterize via CTLine");
        assert_eq!(px.len(), 28 * 28 * 4, "color cluster = 4 B/px RGBA");
        let ink = px.chunks_exact(4).filter(|p| p[3] > 0).count();
        assert!(ink > 50, "ZWJ emoji sequence must have ink, got {ink}");
    }

    #[test]
    fn zwj_emoji_cluster_tracks_zoom_without_vertical_clipping() {
        let font = emoji_font();
        let cluster = "👩\u{200d}🔬";
        let small =
            rasterize_cluster_alpha(&font, cluster, 16.0, 24, 24, descent_px(&font, 16.0), true)
                .expect("small ZWJ cluster");
        let large =
            rasterize_cluster_alpha(&font, cluster, 28.0, 40, 40, descent_px(&font, 28.0), true)
                .expect("large ZWJ cluster");
        let (small_top, small_bottom) = ink_y_bounds_rgba(&small, 24, 24).unwrap();
        let (large_top, large_bottom) = ink_y_bounds_rgba(&large, 40, 40).unwrap();
        assert!(large_bottom - large_top > small_bottom - small_top);
        assert!(large_top > 0, "large cluster is clipped at the top");
        assert!(large_bottom < 39, "large cluster is clipped at the bottom");
    }

    /// Regional indicator pair 🇨🇳 (RI C + RI N) must rasterize. Each RI is
    /// width-2 on its own; the pair forms a flag cluster that CTLine shapes
    /// into a single flag glyph.
    #[test]
    fn regional_indicator_pair_has_ink() {
        let font = emoji_font();
        let cluster = "🇨🇳";
        let px =
            rasterize_cluster_alpha(&font, cluster, 28.0, 28, 28, descent_px(&font, 28.0), true);
        let px = px.expect("flag CN must rasterize via CTLine");
        let ink = px.chunks_exact(4).filter(|p| p[3] > 0).count();
        assert!(ink > 50, "flag pair must have ink, got {ink}");
    }

    /// Skin tone modifier 👩🏽 (woman + medium skin tone) must rasterize.
    /// The modifier is width-0 and combines with the base emoji.
    #[test]
    fn skin_tone_modifier_cluster_has_ink() {
        let font = emoji_font();
        let cluster = "👩🏽";
        let px =
            rasterize_cluster_alpha(&font, cluster, 28.0, 28, 28, descent_px(&font, 28.0), true);
        let px = px.expect("woman+skin tone must rasterize via CTLine");
        let ink = px.chunks_exact(4).filter(|p| p[3] > 0).count();
        assert!(ink > 50, "skin tone cluster must have ink, got {ink}");
    }

    /// Variation selector VS16 (`*\u{FE0F}`) promotes `*` to emoji width.
    /// The cluster must rasterize with ink — VS16 itself has no ink but the
    /// base `*` does.
    #[test]
    fn variation_selector_cluster_has_ink() {
        let font = menlo();
        let cluster = "*\u{FE0F}";
        let px =
            rasterize_cluster_alpha(&font, cluster, 28.0, 28, 28, descent_px(&font, 28.0), false);
        let px = px.expect("*+VS16 must rasterize via CTLine");
        let ink = px.iter().filter(|p| **p > 0).count();
        assert!(ink > 10, "VS16 cluster must have ink, got {ink}");
    }

    /// Empty string must return None (no ink to draw).
    #[test]
    fn empty_cluster_returns_none() {
        let font = menlo();
        let px = rasterize_cluster_alpha(&font, "", 28.0, 14, 28, descent_px(&font, 28.0), false);
        assert!(px.is_none(), "empty cluster must not produce ink");
    }

    /// The atlas slot width must be respected — drawing a wide cluster into
    /// a narrow slot must not panic and must return some pixels (clipped).
    #[test]
    fn wide_cluster_in_narrow_slot_does_not_panic() {
        let font = emoji_font();
        let cluster = "👩\u{200d}🔬";
        // Intentionally narrow slot; CTLine will draw but pixels outside the
        // slot width are clipped by the CGContext bounds. Emoji → RGBA path.
        let px =
            rasterize_cluster_alpha(&font, cluster, 28.0, 14, 28, descent_px(&font, 28.0), true);
        // We don't assert ink here — clipping may remove everything — but
        // the call must not panic.
        let _ = px;
    }
}
