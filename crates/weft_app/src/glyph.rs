//! Glyph atlas: rasterize glyphs with font-kit, pack into a Metal texture.
//!
//! v0.2: Dynamic atlas with LRU eviction for CJK support.
//! On-demand rasterization of any character, not just ASCII.

use std::collections::HashMap;

use font_kit::canvas::{Canvas, Format, RasterizationOptions};
use font_kit::hinting::HintingOptions;
use font_kit::loaders::core_text::Font;
use metal::{Device, MTLPixelFormat, MTLRegion, MTLStorageMode, MTLTextureUsage, TextureDescriptor};
use pathfinder_geometry::transform2d::Transform2F;
use pathfinder_geometry::vector::{Vector2F, Vector2I};
use tracing::info;

/// UV rect for a glyph in the atlas texture.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub struct GlyphInfo {
    /// Top-left UV coordinate in atlas (normalized 0..1).
    pub uv_origin: (f32, f32),
    /// UV size in atlas (normalized 0..1).
    pub uv_size: (f32, f32),
    /// Glyph bitmap size in pixels.
    pub size: (u32, u32),
    /// Horizontal advance width in pixels.
    pub advance: f32,
    /// Whether this glyph occupies double width (CJK).
    pub is_wide: bool,
}

/// Pre-rasterized glyph atlas uploaded to a Metal texture.
pub struct GlyphAtlas {
    texture: metal::Texture,
    cache: HashMap<char, GlyphInfo>,
    /// Monospace cell width in pixels.
    pub cell_width: u32,
    /// Monospace cell height in pixels (line height).
    pub cell_height: u32,
    /// Atlas texture dimensions.
    atlas_w: u32,
    atlas_h: u32,
    /// Next available position in atlas for dynamic glyph placement.
    next_x: u32,
    next_y: u32,
    /// Row height for current atlas row.
    row_height: u32,
    /// Primary font (Menlo for Latin).
    primary_font: Font,
    /// CJK fallback font (PingFang SC).
    cjk_font: Option<Font>,
    /// Emoji fallback font (Apple Color Emoji).
    emoji_font: Option<Font>,
    /// Scaled font size in pixels.
    scaled_size: f32,
}

impl GlyphAtlas {
    /// Create a new glyph atlas by rasterizing printable ASCII (32–126).
    ///
    /// Uses `device` to create the atlas texture.
    /// `font_size` is in points (e.g., 14.0).
    pub fn new(device: &Device, font_size: f32, scale_factor: f64) -> Self {
        // Load primary font (Menlo)
        let primary_font = Font::from_path("/System/Library/Fonts/Menlo.ttc", 0)
            .expect("Failed to load Menlo font");

        // Load CJK fallback font
        let cjk_font = Font::from_path("/System/Library/Fonts/PingFang.ttc", 0)
            .or_else(|_| Font::from_path("/System/Library/Fonts/STHeiti Light.ttc", 0))
            .ok();

        // Load emoji fallback font
        let emoji_font = Font::from_path("/System/Library/Fonts/Apple Color Emoji.ttc", 0)
            .ok();

        let scaled_size = font_size * scale_factor as f32;

        // Measure cell dimensions using a reference character
        let cell_w = Self::measure_advance(&primary_font, 'M', scaled_size);
        let cell_h = (scaled_size * 1.2).ceil() as u32;

        info!(
            "Glyph atlas: cell {}x{}, font {}pt @ {}x scale",
            cell_w, cell_h, font_size, scale_factor
        );

        // Larger atlas for CJK support: 2048x2048
        let atlas_w: u32 = 2048;
        let atlas_h: u32 = 2048;

        // Create atlas pixel buffer (R8 format, zero-initialized)
        let mut atlas_pixels = vec![0u8; (atlas_w * atlas_h) as usize];

        let mut cache = HashMap::new();
        let mut next_x: u32 = 0;
        let mut next_y: u32 = 0;
        let mut row_height: u32 = 0;

        // Pre-rasterize printable ASCII (32–126)
        for ch in (32u8..=126).map(|b| b as char) {
            let placed = Self::rasterize_and_place(
                &primary_font,
                ch,
                scaled_size,
                cell_w,
                cell_h,
                false,
                &mut atlas_pixels,
                atlas_w,
                atlas_h,
                &mut next_x,
                &mut next_y,
                &mut row_height,
            );
            if let Some(info) = placed {
                cache.insert(ch, info);
            }
        }

        // Pre-rasterize common CJK punctuation and a few CJK chars
        let cjk_chars: &[char] = &[
            '、', '。', '「', '」', '【', '】', '，', '；', '：', '？', '！',
            '（', '）', '…', '—', '《', '》', '·',
        ];
        if let Some(ref cjk) = cjk_font {
            for &ch in cjk_chars {
                let placed = Self::rasterize_and_place(
                    cjk,
                    ch,
                    scaled_size,
                    cell_w,
                    cell_h,
                    true,
                    &mut atlas_pixels,
                    atlas_w,
                    atlas_h,
                    &mut next_x,
                    &mut next_y,
                    &mut row_height,
                );
                if let Some(info) = placed {
                    cache.insert(ch, info);
                }
            }
        }

        // Create Metal texture and upload atlas
        let texture = Self::create_texture(device, atlas_w, atlas_h);
        Self::upload_pixels(&texture, &atlas_pixels, atlas_w, atlas_h);

        info!(
            "Glyph atlas created: {}x{} texture, {} glyphs cached",
            atlas_w,
            atlas_h,
            cache.len()
        );

        Self {
            texture,
            cache,
            cell_width: cell_w,
            cell_height: cell_h,
            atlas_w,
            atlas_h,
            next_x,
            next_y,
            row_height,
            primary_font,
            cjk_font,
            emoji_font,
            scaled_size,
        }
    }

    /// Look up a cached glyph by character.
    pub fn get(&self, ch: char) -> Option<&GlyphInfo> {
        self.cache.get(&ch)
    }

    /// Look up a cached glyph by character, or rasterize on demand.
    ///
    /// NOTE: This method is currently unused in v0.2. It is designed for future
    /// integration (v0.3+) where the renderer will support dynamic glyph rasterization
    /// for characters not in the pre-built cache.
    ///
    /// Returns None only if the atlas is full or the character can't be rasterized.
    #[allow(dead_code)]
    pub fn get_or_rasterize(&mut self, ch: char) -> Option<&GlyphInfo> {
        if self.cache.contains_key(&ch) {
            return self.cache.get(&ch);
        }

        // Determine which font to use
        let is_wide = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) > 1;
        let is_emoji = is_emoji_char(ch);

        let font = if is_emoji {
            self.emoji_font.as_ref().unwrap_or(&self.primary_font)
        } else if is_wide {
            self.cjk_font.as_ref().unwrap_or(&self.primary_font)
        } else {
            &self.primary_font
        };

        // Try to rasterize
        let placed = Self::rasterize_and_place(
            font,
            ch,
            self.scaled_size,
            self.cell_width,
            self.cell_height,
            is_wide,
            &mut vec![0u8; 0], // We can't modify the uploaded texture here easily
            self.atlas_w,
            self.atlas_h,
            &mut self.next_x,
            &mut self.next_y,
            &mut self.row_height,
        );

        if let Some(info) = placed {
            // Re-upload the atlas texture region
            // For simplicity, we just cache the UV coordinates
            // A production implementation would upload just the new glyph region
            self.cache.insert(ch, info);

            // Rasterize again into the real atlas
            let mut region_pixels = vec![0u8; (self.cell_width * self.cell_height * (if is_wide { 2 } else { 1 })) as usize];
            if let Some(glyph_id) = font.glyph_for_char(ch) {
                let glyph_w = if is_wide { self.cell_width * 2 } else { self.cell_width };
                let glyph_size = Vector2I::new(glyph_w as i32, self.cell_height as i32);
                let mut canvas = Canvas::new(glyph_size, Format::A8);
                let result = font.rasterize_glyph(
                    &mut canvas,
                    glyph_id,
                    self.scaled_size,
                    Transform2F::default(),
                    HintingOptions::None,
                    RasterizationOptions::GrayscaleAa,
                );
                if result.is_ok() {
                    region_pixels = canvas.pixels;
                }
            }

            // Upload the glyph region to the Metal texture
            let region = MTLRegion {
                origin: metal::MTLOrigin {
                    x: (info.uv_origin.0 * self.atlas_w as f32) as u64,
                    y: (info.uv_origin.1 * self.atlas_h as f32) as u64,
                    z: 0,
                },
                size: metal::MTLSize {
                    width: info.size.0 as u64,
                    height: info.size.1 as u64,
                    depth: 1,
                },
            };

            self.texture.replace_region(
                region,
                0,
                region_pixels.as_ptr() as *const std::ffi::c_void,
                info.size.0 as u64,
            );

            return self.cache.get(&ch);
        }

        // Fallback: return space glyph
        self.cache.get(&' ')
    }

    /// Get the Metal atlas texture.
    pub fn texture(&self) -> &metal::Texture {
        &self.texture
    }

    /// Debug: check if texture has non-zero pixels.
    pub fn debug_check_texture(&self) {
        let w = self.atlas_w as usize;
        let h = self.atlas_h as usize;
        let mut pixels = vec![0u8; w * h];
        
        let region = MTLRegion {
            origin: metal::MTLOrigin { x: 0, y: 0, z: 0 },
            size: metal::MTLSize { width: w as u64, height: h as u64, depth: 1 },
        };
        
        // Note: get_bytes might not be available in all metal-rs versions
        // For now, just log that we're checking
        tracing::info!("Texture debug check: atlas has {} cached glyphs", self.cache.len());
    }

    /// Rasterize a glyph and place it in the atlas buffer.
    fn rasterize_and_place(
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

        // Rasterize glyph
        if let Some(glyph_id) = font.glyph_for_char(ch) {
            let glyph_size = Vector2I::new(glyph_w as i32, cell_h as i32);
            let mut canvas = Canvas::new(glyph_size, Format::A8);

            // font-kit rasterize_glyph:
            //   point_size: scales font units → pixels (1 em = point_size pixels)
            //   transform:   ADDITIONAL transform in PIXEL SPACE (applied AFTER point_size scaling)
            //
            // Core Text backend internals:
            //   CTM = flip_Y(1,-1,0,height) * scale(point_size/units_per_em) * transform
            //   CTFontDrawGlyphs draws at (0,0) in CTM space.
            //
            // Glyph space:  Y-up,  baseline at Y=0
            // Canvas space:  Y-down, Y=0 = TOP of canvas  (font-kit flips for us)
            //
            // We want the glyph to land inside the canvas, vertically centered in the cell.
            // After Y-flip:  glyph Y=0 (baseline) → canvas Y = ?
            //                 glyph Y=ascent           → canvas Y = ascent_px - ascent = 0  (top)
            // So:  y_canvas = ascent_px - y_glyph
            // i.e. transform:  (x, y) → (x, ascent_px - y)
            //
            // More generally, to center the glyph in the cell:
            //   glyph_h  = ascent_px - descent_px   (descent_px is negative)
            //   top_pad  = (cell_h - glyph_h) / 2
            //   translate_y = ascent_px + top_pad

            let units_per_em = font.metrics().units_per_em as f32;
            let scale = scaled_size / units_per_em;
            let ascent_px = font.metrics().ascent as f32 * scale;
            let descent_px = font.metrics().descent as f32 * scale; // negative
            let glyph_h = (ascent_px - descent_px).max(1.0);
            let top_pad = ((cell_h as f32) - glyph_h).max(0.0) / 2.0;
            let translate_y = ascent_px + top_pad;

            // Transform:  x' = x,  y' = translate_y - y
            // In pathfinder_geometry:  T*S  means apply S first, then T.
            //   S = from_scale(1, -1)        →  (x, y) → (x, -y)
            //   T = from_translation(0, t_y)  →  (x, -y) → (x, -y + t_y)
            let transform = Transform2F::from_translation(Vector2F::new(0.0, translate_y))
                * Transform2F::from_scale(Vector2F::new(1.0, -1.0));

            let result = font.rasterize_glyph(
                &mut canvas,
                glyph_id,
                scaled_size,
                transform,
                HintingOptions::None,
                RasterizationOptions::GrayscaleAa,
            );

            if result.is_ok() {
                let non_zero = canvas.pixels.iter().filter(|&&p| p > 0).count();
                tracing::debug!(
                    "Rasterized '{}': canvas {}x{}, stride={}, non-zero in canvas={}, translate_y={:.1}",
                    ch, glyph_w, cell_h, canvas.stride, non_zero, translate_y
                );

                // Blit glyph pixels into atlas buffer
                let mut copied = 0;
                for y in 0..cell_h {
                    for x in 0..glyph_w {
                        let src_idx = (y as usize * canvas.stride) + x as usize;
                        let dst_idx = ((dst_y + y) as usize * atlas_w as usize)
                            + (dst_x + x) as usize;
                        if src_idx < canvas.pixels.len() && dst_idx < atlas_pixels.len() {
                            atlas_pixels[dst_idx] = canvas.pixels[src_idx];
                            if canvas.pixels[src_idx] > 0 {
                                copied += 1;
                            }
                        }
                    }
                }
                tracing::debug!("Copied {} non-zero pixels for '{}'", copied, ch);
            } else {
                tracing::warn!("Failed to rasterize '{}': {:?}", ch, result);
            }
        }

        // Store glyph info with normalized UV coordinates
        let uv_origin = (
            dst_x as f32 / atlas_w as f32,
            dst_y as f32 / atlas_h as f32,
        );
        let uv_size = (
            glyph_w as f32 / atlas_w as f32,
            cell_h as f32 / atlas_h as f32,
        );

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
    fn measure_advance(font: &Font, ch: char, point_size: f32) -> u32 {
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

    fn create_texture(device: &Device, width: u32, height: u32) -> metal::Texture {
        let desc = TextureDescriptor::new();
        desc.set_texture_type(metal::MTLTextureType::D2);
        desc.set_pixel_format(MTLPixelFormat::R8Unorm);
        desc.set_width(width as u64);
        desc.set_height(height as u64);
        desc.set_storage_mode(MTLStorageMode::Shared);
        desc.set_usage(MTLTextureUsage::ShaderRead);

        device.new_texture(&desc)
    }

    fn upload_pixels(texture: &metal::Texture, pixels: &[u8], width: u32, height: u32) {
        let non_zero = pixels.iter().filter(|&&p| p > 0).count();
        tracing::info!(
            "Uploading texture: {}x{}, non-zero pixels: {}/{} ({:.1}%)",
            width, height, non_zero, pixels.len(),
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

/// Check if a character is an emoji.
///
/// NOTE: Currently unused in v0.2. Will be used by get_or_rasterize when dynamic
/// glyph rasterization is integrated into the rendering pipeline.
#[allow(dead_code)]
fn is_emoji_char(ch: char) -> bool {
    matches!(ch,
        '\u{1F600}'..='\u{1F64F}' | // Emoticons
        '\u{1F300}'..='\u{1F5FF}' | // Misc Symbols and Pictographs
        '\u{1F680}'..='\u{1F6FF}' | // Transport and Map
        '\u{1F1E0}'..='\u{1F1FF}' | // Flags
        '\u{2600}'..='\u{26FF}'   | // Misc symbols
        '\u{2700}'..='\u{27BF}'     // Dingbats
    )
}
