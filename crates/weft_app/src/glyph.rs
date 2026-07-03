//! Glyph atlas: rasterize glyphs with font-kit, pack into a Metal texture.
//!
//! v0.2: Dynamic atlas with LRU eviction for CJK support.
//! On-demand rasterization of any character, not just ASCII.

use std::collections::HashMap;

use font_kit::canvas::{Canvas, Format, RasterizationOptions};
use font_kit::family_name::FamilyName;
use font_kit::hinting::HintingOptions;
use font_kit::loaders::core_text::Font;
use font_kit::properties::Properties;
use font_kit::source::SystemSource;
use metal::{
    Device, MTLPixelFormat, MTLRegion, MTLStorageMode, MTLTextureUsage, TextureDescriptor,
};
use pathfinder_geometry::transform2d::Transform2F;
use pathfinder_geometry::vector::{Vector2F, Vector2I};
use tracing::{debug, info, warn};
use weft_core::config::FontConfig;

/// Rasterize a color emoji (sbix bitmap) glyph to an alpha mask via CoreText +
/// CoreGraphics. font-kit's `rasterize_glyph` cannot handle color bitmap fonts
/// (Apple Color Emoji produces 0 pixels on A8/RGBA32 canvases). This bypasses
/// font-kit by creating a color-supporting CGContext and using
/// `CTFontDrawGlyphs` directly, which renders the sbix bitmap in full color.
/// The RGBA result is reduced to a single alpha channel for the R8 atlas.
///
/// Returns `None` if the glyph cannot be found or rasterized.
fn rasterize_emoji_alpha(font: &Font, ch: char, glyph_w: u32, cell_h: u32) -> Option<Vec<u8>> {
    use core_foundation::string::UniChar;
    use core_graphics::color_space::CGColorSpace;
    use core_graphics::context::CGContext;
    use core_text::font::CTFont;

    let ct_font: CTFont = font.native_font();

    // Map character → CGGlyph via UTF-16 (astral-plane chars need surrogate pair).
    let mut utf16 = [0u16; 3];
    let encoded = ch.encode_utf16(&mut utf16);
    let chars: Vec<UniChar> = encoded.iter().map(|&u| u as UniChar).collect();
    let mut glyphs = vec![0u16; chars.len()];
    let got = unsafe {
        ct_font.get_glyphs_for_characters(
            chars.as_ptr(),
            glyphs.as_mut_ptr(),
            chars.len() as core_foundation::base::CFIndex,
        )
    };
    if !got || glyphs.iter().all(|&g| g == 0) {
        return None;
    }

    // Create a color-supporting RGBA bitmap context. sbix bitmaps only render
    // in RGB color spaces — device-gray yields 0 pixels.
    let cs = CGColorSpace::create_device_rgb();
    let w = glyph_w as usize;
    let h = cell_h as usize;
    let mut ctx = CGContext::create_bitmap_context(
        None,
        w,
        h,
        8,     // bitsPerComponent
        w * 4, // bytesPerRow
        &cs,
        core_graphics::base::kCGImageAlphaPremultipliedLast,
    );

    // Draw glyph at bottom-left with descent offset (CG is Y-up).
    // Provide one position per glyph (astral-plane chars may produce 2 glyphs
    // from a surrogate pair — draw_glyphs asserts glyphs.len() == positions.len()).
    let descent = ct_font.descent();
    let positions: Vec<core_graphics_types::geometry::CGPoint> = glyphs
        .iter()
        .map(|_| core_graphics_types::geometry::CGPoint::new(0.0, descent.abs()))
        .collect();
    ct_font.draw_glyphs(&glyphs, &positions, ctx.clone());

    // Read RGBA → alpha mask with Y-flip (atlas is top-down).
    let bpr = ctx.bytes_per_row();
    let raw = ctx.data();
    let mut alpha = vec![0u8; w * h];
    for y in 0..h {
        for x in 0..w {
            let src_idx = y * bpr + x * 4;
            if src_idx + 3 < raw.len() {
                let dst_y = h - 1 - y; // flip Y for top-down atlas
                alpha[dst_y * w + x] = raw[src_idx + 3];
            }
        }
    }

    let nonzero = alpha.iter().filter(|&&p| p > 0).count();
    if nonzero == 0 {
        tracing::warn!("emoji '{ch}' rasterized to 0 pixels via CoreText");
        return None;
    }
    tracing::debug!("emoji '{ch}' rasterized: {nonzero} non-zero pixels");
    Some(alpha)
}

/// Resolve a font by family name via the system source, falling back to a list
/// of absolute `.ttc` paths (the bundled macOS defaults). Returns the first
/// loadable font.
///
/// Logging policy: a family-name miss is common and harmless on macOS —
/// font-kit's `FamilyName::Title` lookup doesn't always index built-in fonts
/// (Menlo, PingFang, Apple Color Emoji, Apple Symbols) on every locale / OS
/// version, so the path fallback is the de-facto primary path in practice.
/// Therefore:
///   - family miss + path hit  → `debug!` (noise-free in normal runs)
///   - family miss + path miss → `warn!` (genuine load failure worth surfacing)
fn resolve_font(family: &str, fallback_paths: &[&str]) -> Option<Font> {
    if !family.is_empty() {
        if let Some(f) = load_by_family(family) {
            return Some(f);
        }
    }
    // Family lookup missed (or was empty) — try the bundled paths. We log at
    // debug here because the path fallback is expected to succeed; if every
    // path also fails we escalate to warn below.
    for path in fallback_paths {
        match Font::from_path(path, 0) {
            Ok(f) => {
                debug!(
                    family,
                    path, "font family not found; loaded via path fallback"
                );
                return Some(f);
            }
            Err(e) => {
                debug!(family, path, error = %e, "path fallback failed");
            }
        }
    }
    warn!(
        family,
        fallback_count = fallback_paths.len(),
        "font load failed: family not found and no path fallback succeeded"
    );
    None
}

/// Look up a font by family name using the Core Text system source.
fn load_by_family(family: &str) -> Option<Font> {
    let source = SystemSource::new();
    let handle = source
        .select_best_match(&[FamilyName::Title(family.to_string())], &Properties::new())
        .ok()?;
    match handle {
        font_kit::handle::Handle::Path { path, font_index } => {
            Font::from_path(path, font_index).ok()
        }
        // Memory handles are rare (in-process fonts); skip them.
        font_kit::handle::Handle::Memory { .. } => None,
    }
}

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

        // Measure cell dimensions using a reference character
        let cell_w = Self::measure_advance(&primary_font, 'M', scaled_size);
        let cell_h = (scaled_size * font_config.line_height).ceil() as u32;

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
            }
        }

        // Pre-rasterize the prompt marker and common UI glyphs with the primary
        // font, at init (atlas empty, no draw-time upload). The ❯ (U+276F)
        // prompt must be in the texture before the first draw — the warm-up
        // path was leaving it blank in the running app.
        for &ch in &['❯', '❮', '›', '→', '•', '·', '…', '─', '▸', '▾'] {
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

        self.cache.insert(ch, info);
        self.cache.get(&ch)
    }

    /// Get the Metal atlas texture.
    pub fn texture(&self) -> &metal::Texture {
        &self.texture
    }

    /// Allocate a slot in the atlas (layout only, no pixel work).
    /// Advances the atlas cursor and returns UV/layout info for the slot.
    #[allow(clippy::too_many_arguments)]
    fn allocate_slot(
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

        let uv_origin = (dst_x as f32 / atlas_w as f32, dst_y as f32 / atlas_h as f32);
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
    fn glyph_transform(
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
    fn rasterize_glyph(
        font: &Font,
        ch: char,
        scaled_size: f32,
        glyph_w: u32,
        cell_h: u32,
        is_wide: bool,
        primary_descent_px: f32,
    ) -> Vec<u8> {
        let glyph_size = Vector2I::new(glyph_w as i32, cell_h as i32);

        // Emoji (color bitmap glyphs like 📁📄) cannot be rasterized by font-kit
        // (it produces 0 pixels on A8). Use the CoreText/CG color path instead.
        if is_emoji_char(ch) {
            if let Some(alpha) = rasterize_emoji_alpha(font, ch, glyph_w, cell_h) {
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
    fn upload_region(
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
            // font-kit's core_text backend rasterizes glyphs in a Y-up space; the
            // canvas is Y-down, so a Y-flip is required to render them upright. The
            // backend positions the baseline itself — do NOT add a translation, or the
            // ascent gets double-counted and glyphs are pushed below the cell (clipping
            // their tops → unreadable fragments). Verified via the transform_probe test:
            // scale(1,-1) yields full-height upright glyphs (e.g. 'M' ink rows [0,20]).
            // Shift down by the descent depth so descenders sit inside the cell; the
            // CAMetalLayer flips vertically, so this lifts the glyph off the screen-cell
            // bottom and stops the next row's opaque background from clipping descenders.
            //
            // CJK glyphs: no horizontal stretch — centered in the slot instead
            // (see `glyph_transform`). Half-width: scale_x ≈ 1, unchanged.
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

            if result.is_ok() {
                let non_zero = canvas.pixels.iter().filter(|&&p| p > 0).count();
                tracing::debug!(
                    "Rasterized '{}': canvas {}x{}, stride={}, non-zero in canvas={}",
                    ch,
                    glyph_w,
                    cell_h,
                    canvas.stride,
                    non_zero
                );

                // Blit glyph pixels into atlas buffer
                let mut copied = 0;
                for y in 0..cell_h {
                    for x in 0..glyph_w {
                        let src_idx = (y as usize * canvas.stride) + x as usize;
                        let dst_idx =
                            ((dst_y + y) as usize * atlas_w as usize) + (dst_x + x) as usize;
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
        let uv_origin = (dst_x as f32 / atlas_w as f32, dst_y as f32 / atlas_h as f32);
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

#[cfg(test)]
mod transform_probe {
    //! Device-free diagnostic: rasterize glyphs under candidate transforms and
    //! print exact ink bounding boxes, to calibrate the rasterize transform.
    use super::*;
    use font_kit::canvas::{Format, RasterizationOptions};
    use font_kit::hinting::HintingOptions;

    /// |descent| of `font` at `scaled_size` px, in pixels — the production
    /// baseline-anchor value. Mirrors `GlyphAtlas::primary_descent_px`.
    fn descent_px(font: &Font, scaled_size: f32) -> f32 {
        let m = font.metrics();
        m.descent.abs() * (scaled_size / m.units_per_em as f32)
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
        let px =
            GlyphAtlas::rasterize_glyph(&font, '❯', 28.0, 14, 28, false, descent_px(&font, 28.0));
        let ink = px.iter().filter(|p| **p > 0).count();
        assert!(ink > 50, "❯ must rasterize with ink, got {ink}");
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

    /// Ink bounding box (top_row, bottom_row) over a pixel slice for a w×h glyph.
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
        );
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
}
