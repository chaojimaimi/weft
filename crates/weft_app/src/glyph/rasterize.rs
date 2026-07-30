//! Rasterization and Metal upload methods for `GlyphAtlas`.
//!
//! Split from `glyph/mod.rs` (Batch 6 Step 4) to keep the main file under
//! the 800-line limit. All methods are `pub(super)` — visible only within
//! the `glyph` module tree, callable from the parent module's `impl` block.

use font_kit::canvas::{Canvas, Format, RasterizationOptions};
use font_kit::hinting::HintingOptions;
use font_kit::loaders::core_text::Font;
use metal::{
    Device, MTLPixelFormat, MTLRegion, MTLStorageMode, MTLTextureUsage, TextureDescriptor,
};
use pathfinder_geometry::transform2d::Transform2F;
use pathfinder_geometry::vector::{Vector2F, Vector2I};

use super::atlas;
use super::font::{is_emoji_char, rasterize_emoji_alpha};
use super::special;
use super::{GlyphAtlas, GlyphInfo};

impl GlyphAtlas {
    /// Allocate a slot in the atlas (layout only, no pixel work).
    /// Advances the atlas cursor and returns UV/layout info for the slot.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn allocate_slot(
        is_wide: bool,
        cell_w: u32,
        cell_h: u32,
        atlas_w: u32,
        atlas_h: u32,
        next_x: &mut u32,
        next_y: &mut u32,
        row_height: &mut u32,
    ) -> Option<GlyphInfo> {
        let glyph_w = if is_wide { cell_w * 2 } else { cell_w };

        // Check if we need a new row
        if *next_x + glyph_w > atlas_w {
            *next_x = 0;
            *next_y += *row_height;
            *row_height = 0;
        }

        // Check if atlas is full
        if *next_y + cell_h > atlas_h {
            return None;
        }

        let dst_x = *next_x;
        let dst_y = *next_y;

        let (uv_origin, uv_size) =
            atlas::pixel_center_uv(dst_x, dst_y, glyph_w, cell_h, atlas_w, atlas_h);

        // Advance position
        *next_x += glyph_w;
        *row_height = (*row_height).max(cell_h);

        Some(GlyphInfo {
            uv_origin,
            uv_size,
            size: (glyph_w, cell_h),
            advance: glyph_w as f32,
            is_wide,
        })
    }

    /// Compute the rasterization transform for a glyph.
    ///
    /// **Vertical baseline anchoring (v0.8 bug fix):** the descent offset is
    /// passed in as `primary_descent_px` — the *primary* (Latin) font's
    /// |descent| scaled to pixels — rather than read from `font.metrics()`.
    /// Why: a CJK font (PingFang, STHeiti) has a smaller |descent| ratio than
    /// a monospace Latin font (Menlo). When each glyph was shifted by its own
    /// font's descent, the CJK baseline landed a few px above the Latin one,
    /// so in mixed-script rows (e.g. `ls -l` output with Chinese filenames)
    /// the CJK glyphs visually "sank" below the Latin baseline. Anchoring
    /// every glyph to the primary font's descent makes both scripts share one
    /// baseline regardless of which fallback font rasterizes them.
    ///
    /// **CJK (is_wide=true):** the glyph is NOT horizontally stretched. A CJK
    /// glyph's natural advance is ~1em but its slot is `2·cell_width` (≈1.2em);
    /// stretching to fill the slot (the old behaviour) distorted every
    /// character ~1.2× wider than the typeface intends. Instead we keep
    /// `scale_x = 1.0` and horizontally center the glyph inside the slot — the
    /// ~0.1em of side bearing on each side matches how PingFang / Apple's own
    /// terminal stack render CJK.
    ///
    /// **Half-width (is_wide=false):** scale_x ≈ 1 (the monospace primary
    /// advances `cell_width == glyph_w`), unchanged.
    pub(super) fn glyph_transform(
        font: &Font,
        glyph_id: u32,
        scaled_size: f32,
        glyph_w: u32,
        is_wide: bool,
        primary_descent_px: f32,
    ) -> Transform2F {
        let upem = font.metrics().units_per_em as f32;
        let descent_px = primary_descent_px;

        if is_wide {
            // Center the glyph's natural advance inside the double-wide slot.
            let advance_px = font
                .advance(glyph_id)
                .map(|a| a.x() * (scaled_size / upem))
                .unwrap_or(0.0)
                .max(0.0);
            let x_offset = ((glyph_w as f32 - advance_px) / 2.0).max(0.0);
            Transform2F::from_translation(Vector2F::new(x_offset, descent_px))
                * Transform2F::from_scale(Vector2F::new(1.0, -1.0))
        } else {
            Transform2F::from_translation(Vector2F::new(0.0, descent_px))
                * Transform2F::from_scale(Vector2F::new(1.0, -1.0))
        }
    }

    /// Rasterize a single glyph into a pixel buffer.
    /// Returns the pixel data ready for upload to the atlas texture.
    pub(super) fn rasterize_glyph(
        font: &Font,
        ch: char,
        scaled_size: f32,
        glyph_w: u32,
        cell_h: u32,
        is_wide: bool,
        primary_descent_px: f32,
    ) -> Vec<u8> {
        let glyph_size = Vector2I::new(glyph_w as i32, cell_h as i32);

        if let Some(pixels) = special::rasterize(ch, glyph_w, cell_h) {
            return pixels;
        }

        // Emoji (color bitmap glyphs like 📁📄) cannot be rasterized by font-kit
        // (it produces 0 pixels on A8). Use the CoreText/CG color path instead.
        if is_emoji_char(ch) {
            if let Some(alpha) =
                rasterize_emoji_alpha(font, ch, scaled_size, glyph_w, cell_h, primary_descent_px)
            {
                return alpha;
            }
            // Fall through to font-kit if CoreText path fails.
        }

        let mut canvas = Canvas::new(glyph_size, Format::A8);

        if let Some(glyph_id) = font.glyph_for_char(ch) {
            // See `glyph_transform`: CJK glyphs are centered (not stretched)
            // inside the double-wide slot; half-width uses scale_x ≈ 1.
            // primary_descent_px anchors both scripts to one baseline.
            let transform = Self::glyph_transform(
                font,
                glyph_id,
                scaled_size,
                glyph_w,
                is_wide,
                primary_descent_px,
            );

            let result = font.rasterize_glyph(
                &mut canvas,
                glyph_id,
                scaled_size,
                transform,
                HintingOptions::None,
                RasterizationOptions::GrayscaleAa,
            );

            if result.is_err() {
                tracing::warn!("Failed to rasterize '{}': {:?}", ch, result);
            }
        }

        canvas.pixels
    }

    /// Upload a glyph's pixel data to the Metal texture at the given UV origin.
    pub(super) fn upload_region(
        texture: &metal::Texture,
        pixels: &[u8],
        width: u32,
        uv_origin: (f32, f32),
        atlas_w: u32,
        atlas_h: u32,
    ) {
        let x = (uv_origin.0 * atlas_w as f32) as u64;
        let y = (uv_origin.1 * atlas_h as f32) as u64;
        let height = (pixels.len() / width as usize) as u64;

        let region = MTLRegion {
            origin: metal::MTLOrigin { x, y, z: 0 },
            size: metal::MTLSize {
                width: width as u64,
                height,
                depth: 1,
            },
        };

        texture.replace_region(
            region,
            0,
            pixels.as_ptr() as *const std::ffi::c_void,
            width as u64,
        );
    }

    /// Rasterize a glyph and place it in the atlas buffer (used during init).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn rasterize_and_place(
        font: &Font,
        ch: char,
        scaled_size: f32,
        cell_w: u32,
        cell_h: u32,
        is_wide: bool,
        atlas_pixels: &mut [u8],
        atlas_w: u32,
        atlas_h: u32,
        next_x: &mut u32,
        next_y: &mut u32,
        row_height: &mut u32,
        primary_descent_px: f32,
    ) -> Option<GlyphInfo> {
        let glyph_w = if is_wide { cell_w * 2 } else { cell_w };

        // Check if we need a new row
        if *next_x + glyph_w > atlas_w {
            *next_x = 0;
            *next_y += *row_height;
            *row_height = 0;
        }

        // Check if atlas is full
        if *next_y + cell_h > atlas_h {
            return None;
        }

        let dst_x = *next_x;
        let dst_y = *next_y;

        // Use the same rasterizer as draw-time cache misses. In particular,
        // this keeps prewarmed box/block glyphs on the exact-cell path.
        let pixels = Self::rasterize_glyph(
            font,
            ch,
            scaled_size,
            glyph_w,
            cell_h,
            is_wide,
            primary_descent_px,
        );
        for y in 0..cell_h {
            for x in 0..glyph_w {
                let src_idx = (y * glyph_w + x) as usize;
                let dst_idx = ((dst_y + y) * atlas_w + dst_x + x) as usize;
                if let (Some(src), Some(dst)) = (pixels.get(src_idx), atlas_pixels.get_mut(dst_idx))
                {
                    *dst = *src;
                }
            }
        }

        // Store glyph info with normalized UV coordinates
        let (uv_origin, uv_size) =
            atlas::pixel_center_uv(dst_x, dst_y, glyph_w, cell_h, atlas_w, atlas_h);

        // Advance position
        *next_x += glyph_w;
        *row_height = (*row_height).max(cell_h);

        Some(GlyphInfo {
            uv_origin,
            uv_size,
            size: (glyph_w, cell_h),
            advance: glyph_w as f32,
            is_wide,
        })
    }

    /// Measure the advance width of a character in pixels.
    pub(super) fn measure_advance(font: &Font, ch: char, point_size: f32) -> u32 {
        let glyph_id = match font.glyph_for_char(ch) {
            Some(id) => id,
            None => return (point_size * 0.6) as u32,
        };

        let advance = match font.advance(glyph_id) {
            Ok(a) => a,
            Err(_) => return (point_size * 0.6) as u32,
        };

        let scale = point_size / font.metrics().units_per_em as f32;
        (advance.x() * scale).ceil() as u32
    }

    pub(super) fn create_texture(device: &Device, width: u32, height: u32) -> metal::Texture {
        let desc = TextureDescriptor::new();
        desc.set_texture_type(metal::MTLTextureType::D2);
        desc.set_pixel_format(MTLPixelFormat::R8Unorm);
        desc.set_width(width as u64);
        desc.set_height(height as u64);
        desc.set_storage_mode(MTLStorageMode::Shared);
        desc.set_usage(MTLTextureUsage::ShaderRead);

        device.new_texture(&desc)
    }

    pub(super) fn upload_pixels(texture: &metal::Texture, pixels: &[u8], width: u32, height: u32) {
        let non_zero = pixels.iter().filter(|&&p| p > 0).count();
        tracing::info!(
            "Uploading texture: {}x{}, non-zero pixels: {}/{} ({:.1}%)",
            width,
            height,
            non_zero,
            pixels.len(),
            non_zero as f32 / pixels.len() as f32 * 100.0
        );

        let region = MTLRegion {
            origin: metal::MTLOrigin { x: 0, y: 0, z: 0 },
            size: metal::MTLSize {
                width: width as u64,
                height: height as u64,
                depth: 1,
            },
        };

        texture.replace_region(
            region,
            0,
            pixels.as_ptr() as *const std::ffi::c_void,
            width as u64,
        );
    }
}
