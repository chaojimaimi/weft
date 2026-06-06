//! Glyph atlas: rasterize ASCII glyphs with font-kit, pack into a Metal texture.

use std::collections::HashMap;

use font_kit::canvas::{Canvas, Format, RasterizationOptions};
use font_kit::hinting::HintingOptions;
use font_kit::loaders::core_text::Font;
use metal::{Device, MTLPixelFormat, MTLRegion, MTLStorageMode, MTLTextureUsage, TextureDescriptor};
use pathfinder_geometry::transform2d::Transform2F;
use pathfinder_geometry::vector::Vector2I;
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
}

/// Pre-rasterized glyph atlas uploaded to a Metal texture.
pub struct GlyphAtlas {
    texture: metal::Texture,
    cache: HashMap<char, GlyphInfo>,
    /// Monospace cell width in pixels.
    pub cell_width: u32,
    /// Monospace cell height in pixels (line height).
    pub cell_height: u32,
}

impl GlyphAtlas {
    /// Create a new glyph atlas by rasterizing printable ASCII (32–126).
    ///
    /// Uses `device` to create the atlas texture.
    /// `font_size` is in points (e.g., 14.0).
    pub fn new(device: &Device, font_size: f32, scale_factor: f64) -> Self {
        // Load Menlo (macOS system monospace font)
        let font = Font::from_path("/System/Library/Fonts/Menlo.ttc", 0)
            .expect("Failed to load Menlo font");

        let scaled_size = font_size * scale_factor as f32;

        // Measure cell dimensions using a reference character
        let cell_w = Self::measure_advance(&font, 'M', scaled_size);
        let cell_h = (scaled_size * 1.2).ceil() as u32; // ~1.2x line height

        info!(
            "Glyph atlas: cell {}x{}, font {}pt @ {}x scale",
            cell_w, cell_h, font_size, scale_factor
        );

        // Layout: 16 columns, enough rows for 95 printable ASCII chars
        let cols: u32 = 16;
        let rows: u32 = 95_u32.div_ceil(cols);
        let atlas_w = cols * cell_w;
        let atlas_h = rows * cell_h;

        // Create atlas pixel buffer (R8 format, zero-initialized)
        let mut atlas_pixels = vec![0u8; (atlas_w * atlas_h) as usize];

        let mut cache = HashMap::new();

        for (i, ch) in (32u8..=126).enumerate() {
            let ch = ch as char;
            let col = i as u32 % cols;
            let row = i as u32 / cols;

            // Rasterize glyph
            if let Some(glyph_id) = font.glyph_for_char(ch) {
                let glyph_size = Vector2I::new(cell_w as i32, cell_h as i32);
                let mut canvas = Canvas::new(glyph_size, Format::A8);

                let result = font.rasterize_glyph(
                    &mut canvas,
                    glyph_id,
                    scaled_size,
                    Transform2F::default(),
                    HintingOptions::None,
                    RasterizationOptions::GrayscaleAa,
                );

                if result.is_ok() {
                    // Blit glyph pixels into atlas buffer
                    let dst_x = col * cell_w;
                    let dst_y = row * cell_h;
                    for y in 0..cell_h {
                        for x in 0..cell_w {
                            let src_idx = (y as usize * canvas.stride) + x as usize;
                            let dst_idx = ((dst_y + y) as usize * atlas_w as usize)
                                + (dst_x + x) as usize;
                            if src_idx < canvas.pixels.len() && dst_idx < atlas_pixels.len() {
                                atlas_pixels[dst_idx] = canvas.pixels[src_idx];
                            }
                        }
                    }
                }
            }

            // Store glyph info with normalized UV coordinates
            let uv_origin = (
                (col * cell_w) as f32 / atlas_w as f32,
                (row * cell_h) as f32 / atlas_h as f32,
            );
            let uv_size = (
                cell_w as f32 / atlas_w as f32,
                cell_h as f32 / atlas_h as f32,
            );

            cache.insert(
                ch,
                GlyphInfo {
                    uv_origin,
                    uv_size,
                    size: (cell_w, cell_h),
                    advance: cell_w as f32,
                },
            );
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
        }
    }

    /// Look up a cached glyph by character.
    pub fn get(&self, ch: char) -> Option<&GlyphInfo> {
        self.cache.get(&ch)
    }

    /// Get the Metal atlas texture.
    pub fn texture(&self) -> &metal::Texture {
        &self.texture
    }

    /// Measure the advance width of a character in pixels.
    fn measure_advance(font: &Font, ch: char, point_size: f32) -> u32 {
        let glyph_id = match font.glyph_for_char(ch) {
            Some(id) => id,
            None => return (point_size * 0.6) as u32, // fallback estimate
        };

        let advance = match font.advance(glyph_id) {
            Ok(a) => a,
            Err(_) => return (point_size * 0.6) as u32,
        };

        // font-kit returns advances in font units; scale to pixels
        let scale = point_size / font.metrics().units_per_em as f32;
        (advance.x() * scale).ceil() as u32
    }

    fn create_texture(device: &Device, width: u32, height: u32) -> metal::Texture {
        let desc = TextureDescriptor::new();
        desc.set_texture_type(metal::MTLTextureType::D2);
        desc.set_pixel_format(MTLPixelFormat::R8Unorm);
        desc.set_width(width as u64);
        desc.set_height(height as u64);
        desc.set_storage_mode(MTLStorageMode::Managed);
        desc.set_usage(MTLTextureUsage::ShaderRead);

        device.new_texture(&desc)
    }

    fn upload_pixels(texture: &metal::Texture, pixels: &[u8], width: u32, height: u32) {
        let region = MTLRegion {
            origin: metal::MTLOrigin { x: 0, y: 0, z: 0 },
            size: metal::MTLSize {
                width: width as u64,
                height: height as u64,
                depth: 1,
            },
        };

        // replace_region is safe in metal 0.29 (not marked unsafe)
        texture.replace_region(
            region,
            0,
            pixels.as_ptr() as *const std::ffi::c_void,
            width as u64,
        );
    }
}
