//! Glyph atlas: rasterize glyphs with font-kit, pack into a Metal texture.
//! v0.2: Dynamic atlas with LRU eviction for CJK support.
//! On-demand rasterization of any character, not just ASCII.
//!
//! Module structure (Batch 6 Step 4 split):
//! - `font` — font resolution + emoji rasterization helpers
//! - `rasterize` — `GlyphAtlas` rasterization/upload methods
//! - `atlas` — UV coordinate helpers
//! - `special` — procedural terminal glyphs (box drawing, block elements)
//! - `tests` — device-free rasterization and transform probes

use std::collections::HashMap;

use font_kit::loaders::core_text::Font;
use metal::Device;
use tracing::info;
use weft_core::config::FontConfig;

mod atlas;
mod font;
mod rasterize;
mod special;

use font::{is_emoji_char, nonzero_cell_dimension, resolve_font};

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
    /// v1.0 P1.5-B3: Direct-indexed lookup for ASCII chars (0..=127).
    /// Terminal output is overwhelmingly ASCII (digits, letters, punctuation,
    /// space) — a direct array index eliminates HashMap hashing for the
    /// common case. `ascii[0..32]` are control chars (unused, always None);
    /// 32..=127 covers printable ASCII. Falls back to `cache` for non-ASCII.
    ascii: [Option<GlyphInfo>; 128],
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
    /// Symbol fallback font (Apple Symbols) — last-resort glyphs the primary
    /// font lacks (e.g. ❯ U+276F in the Dingbats block, box-drawing, arrows).
    symbol_font: Option<Font>,
    /// Scaled font size in pixels.
    scaled_size: f32,
    /// Primary font's |descent| in pixels — the single vertical-baseline
    /// anchor shared by every glyph (Latin, CJK, emoji, symbol) so mixed-script
    /// rows share one baseline regardless of which fallback font rasterizes.
    /// See `glyph_transform` for the v0.8 baseline-alignment rationale.
    primary_descent_px: f32,
}

impl GlyphAtlas {
    /// Create a new glyph atlas by rasterizing printable ASCII (32–126).
    ///
    /// Uses `device` to create the atlas texture. `font_config` supplies the
    /// primary/CJK/emoji family names, point size, and line-height factor.
    pub fn new(device: &Device, font_config: &FontConfig, scale_factor: f64) -> Self {
        // Primary font — resolved by family name, falling back to the bundled
        // Menlo path (guaranteed on macOS). A missing primary is fatal.
        let primary_font = resolve_font(&font_config.family, &["/System/Library/Fonts/Menlo.ttc"])
            .expect("failed to load primary font (no family match and Menlo missing)");

        // CJK fallback
        let cjk_font = resolve_font(
            &font_config.cjk_family,
            &[
                "/System/Library/Fonts/PingFang.ttc",
                "/System/Library/Fonts/STHeiti Light.ttc",
            ],
        );

        // Emoji fallback
        let emoji_font = resolve_font(
            &font_config.emoji_family,
            &["/System/Library/Fonts/Apple Color Emoji.ttc"],
        );

        // Symbol fallback (Apple Symbols) — covers glyphs like ❯ (U+276F) that
        // Menlo lacks, so the prompt marker renders instead of blanking.
        let symbol_font = resolve_font(
            "Apple Symbols",
            &["/System/Library/Fonts/Apple Symbols.ttf"],
        );

        let font_size = font_config.size;
        let scaled_size = font_size * scale_factor as f32;

        // Metal uploads require non-zero row bytes even for a bad transient metric.
        let cell_w = nonzero_cell_dimension(Self::measure_advance(&primary_font, 'M', scaled_size));
        let cell_h = nonzero_cell_dimension((scaled_size * font_config.line_height).ceil() as u32);

        // Vertical baseline anchor: the primary font's |descent| in pixels.
        // Computed once here and threaded into every rasterize call so all
        // scripts (Latin/CJK/emoji/symbol) share one baseline. See
        // `glyph_transform` for the mixed-script alignment rationale.
        let primary_upem = primary_font.metrics().units_per_em as f32;
        let primary_descent_px =
            primary_font.metrics().descent.abs() * (scaled_size / primary_upem);

        info!(
            family = %font_config.family,
            "Glyph atlas: cell {}x{}, font {}pt @ {}x scale",
            cell_w,
            cell_h,
            font_size,
            scale_factor
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

        // Direct-indexed ASCII cache avoids hashing the common path.
        let mut ascii: [Option<GlyphInfo>; 128] = [None; 128];

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
                primary_descent_px,
            );
            if let Some(info) = placed {
                cache.insert(ch, info);
                // v1.0 P1.5-B3: mirror into the direct-index array for O(1)
                // ASCII lookups. GlyphInfo is Copy, so this is cheap.
                ascii[ch as usize] = Some(info);
            }
        }

        // Pre-rasterize the prompt marker and common UI glyphs at init.
        for &ch in &['❯', '❮', '›', '→', '•', '·', '…', '─', '▸', '▾', '●', '○']
        {
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
                primary_descent_px,
            );
            if let Some(info) = placed {
                cache.insert(ch, info);
            }
        }

        // Pre-rasterize common CJK punctuation and a few CJK chars
        let cjk_chars: &[char] = &[
            '、', '。', '「', '」', '【', '】', '，', '；', '：', '？', '！', '（', '）', '…', '—',
            '《', '》', '·',
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
                    primary_descent_px,
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

        // Anchor every glyph's vertical baseline to the primary (Latin) font's
        // |descent| (computed above, near cell_h). CJK/emoji fallback fonts
        // have smaller descent ratios; if each used its own, CJK glyphs would
        // land on a different baseline and appear to "sink" in mixed-script
        // rows (visible in `ls -l` output with Chinese filenames).
        Self {
            texture,
            cache,
            ascii,
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
            symbol_font,
            scaled_size,
            primary_descent_px,
        }
    }

    /// Look up a cached glyph by character.
    pub fn get(&self, ch: char) -> Option<&GlyphInfo> {
        // v1.0 P1.5-B3: ASCII fast path — direct array index beats HashMap
        // hash + lookup. Terminal output is overwhelmingly ASCII, so this
        // branch is the hot path. ASCII entries are mirrored in both
        // `ascii[]` and `cache` (kept in sync in `new()` and
        // `get_or_rasterize()`), so returning from either is equivalent.
        if (ch as u32) < 128 {
            return self.ascii[ch as usize].as_ref();
        }
        self.cache.get(&ch)
    }

    /// Look up a cached glyph by character, or rasterize on demand.
    ///
    /// Called from the render path so the grid's characters always have an
    /// atlas entry. Rasterizes exactly once per new character and uploads
    /// the pixels to the Metal texture.
    ///
    /// Returns None only if the atlas is full or the character can't be rasterized.
    pub fn get_or_rasterize(&mut self, ch: char) -> Option<&GlyphInfo> {
        // v1.0 P1.5-B3: ASCII fast path — direct array index check instead
        // of HashMap contains_key. ASCII entries are mirrored in both
        // `ascii[]` and `cache`, so a hit here means a hit in `cache` too.
        if (ch as u32) < 128 {
            if self.ascii[ch as usize].is_some() {
                return self.ascii[ch as usize].as_ref();
            }
        } else if self.cache.contains_key(&ch) {
            return self.cache.get(&ch);
        }

        // Determine which font to use
        let is_wide = weft_core::grid::terminal_char_width(ch) > 1;
        let is_emoji = is_emoji_char(ch);

        let font = if is_emoji {
            self.emoji_font.as_ref().unwrap_or(&self.primary_font)
        } else if is_wide {
            self.cjk_font.as_ref().unwrap_or(&self.primary_font)
        } else {
            // Narrow text: prefer the monospace primary, but if it lacks the
            // glyph (e.g. ❯ U+276F, some box-drawing/punctuation) fall back to
            // the CJK then emoji font before giving up. Without this, a missing
            // glyph rasterizes as a blank cell — the input-box prompt marker ❯
            // disappeared entirely.
            if self.primary_font.glyph_for_char(ch).is_some() {
                &self.primary_font
            } else if self
                .cjk_font
                .as_ref()
                .is_some_and(|f| f.glyph_for_char(ch).is_some())
            {
                self.cjk_font.as_ref().unwrap()
            } else if self
                .emoji_font
                .as_ref()
                .is_some_and(|f| f.glyph_for_char(ch).is_some())
            {
                self.emoji_font.as_ref().unwrap()
            } else if self
                .symbol_font
                .as_ref()
                .is_some_and(|f| f.glyph_for_char(ch).is_some())
            {
                self.symbol_font.as_ref().unwrap()
            } else {
                &self.primary_font
            }
        };

        // Step 1: Allocate atlas slot (layout only, no pixel work)
        let info = Self::allocate_slot(
            is_wide,
            self.cell_width,
            self.cell_height,
            self.atlas_w,
            self.atlas_h,
            &mut self.next_x,
            &mut self.next_y,
            &mut self.row_height,
        )?;

        // Step 2: Rasterize the glyph once
        let glyph_w = if is_wide {
            self.cell_width * 2
        } else {
            self.cell_width
        };
        let pixels = Self::rasterize_glyph(
            font,
            ch,
            self.scaled_size,
            glyph_w,
            self.cell_height,
            is_wide,
            self.primary_descent_px,
        );

        // Step 3: Upload to Metal texture
        Self::upload_region(
            &self.texture,
            &pixels,
            info.size.0,
            info.uv_origin,
            self.atlas_w,
            self.atlas_h,
        );

        // v1.0 P1.5-B3: ASCII chars are mirrored into the direct-index array
        // (alongside `cache`) so future `get()` calls hit the O(1) fast path
        // instead of hashing into `cache`. Non-ASCII chars go into `cache`
        // only — the array covers just 0..=127.
        let is_ascii = (ch as u32) < 128;
        if is_ascii {
            self.ascii[ch as usize] = Some(info);
        }
        self.cache.insert(ch, info);

        // Return from the same source the corresponding `get()` will use, so
        // the caller's reference stays valid regardless of which path served
        // it: ASCII from `ascii[]`, otherwise from `cache`.
        if is_ascii {
            self.ascii[ch as usize].as_ref()
        } else {
            self.cache.get(&ch)
        }
    }

    /// Get the Metal atlas texture.
    pub fn texture(&self) -> &metal::Texture {
        &self.texture
    }
}

#[cfg(test)]
mod tests;
