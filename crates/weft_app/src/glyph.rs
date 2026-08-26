//! Glyph atlas: rasterize glyphs with font-kit, pack into a Metal texture.
//! v0.2: Dynamic atlas for CJK support. Linear fill with NO eviction — when
//! the texture is exhausted, rasterization fails and those glyphs render
//! blank until the next atlas rebuild. (AUDIT_v1.10.39: this header used to
//! claim "LRU eviction", which the implementation never had.)
//! On-demand rasterization of any character, not just ASCII.
//!
//! Module structure (Batch 6 Step 4 split):
//! - `font` — font resolution + emoji rasterization helpers
//! - `rasterize` — `GlyphAtlas` rasterization/upload methods
//! - `atlas` — UV coordinate helpers
//! - `special` — procedural terminal glyphs (box drawing, block elements)
//! - `tests` — device-free rasterization and transform probes

use std::collections::HashMap;
use std::sync::Arc;

use font_kit::loaders::core_text::Font;
use metal::Device;
use tracing::info;
use weft_core::config::FontConfig;

mod atlas;
mod cluster;
mod font;
mod query;
mod rasterize;
mod special;
mod style;
mod tests;

use font::{nonzero_cell_dimension, resolve_font, resolve_font_variant};

pub use atlas::GlyphInfo;
pub use style::GlyphStyle;

/// v1.6.0 step 6: LRU-tracked entry in the cluster cache. Each entry carries
/// its `GlyphInfo` plus a `last_used` generation counter for LRU eviction.
/// The counter is compared against `GlyphAtlas::cluster_access_counter` to
/// determine which entry was least recently used when the cache is full.
#[derive(Clone, Copy, Debug)]
struct ClusterEntry {
    info: GlyphInfo,
    /// Generation counter at the time of last access. Updated on every
    /// `get_cluster` hit and on insert via `get_or_rasterize_cluster`.
    last_used: u64,
}

/// v1.6.0 step 6: Maximum number of cluster entries the atlas will cache.
/// Multi-scalar graphemes are rare in typical terminal output (most chars
/// are single-scalar ASCII), so 512 slots cover even CJK-heavy workloads
/// with combining marks. When exceeded, the LRU entry is evicted — its
/// atlas texture slot is leaked (not reclaimed), but 512 × ~20px × ~20px
/// ≈ 200Kpx is negligible against a 4096² atlas.
const MAX_CLUSTER_CACHE_ENTRIES: usize = 512;

/// v1.6.0 step 6: Performance counters for the cluster cache. Exposed via
/// [`GlyphAtlas::cluster_stats`] for diagnostics and the perf gate.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ClusterCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub entries: usize,
    pub capacity: usize,
}

/// Pre-rasterized glyph atlas uploaded to a Metal texture.
pub struct GlyphAtlas {
    texture: metal::Texture,
    /// v1.10.4: RGBA8Unorm atlas for color emoji (Apple Color Emoji sbix
    /// bitmaps carry their own RGB; the R8 mask atlas cannot store color).
    /// Same dimensions / row-packing as `texture` but with an independent
    /// cursor (color glyphs are rare, so their own allocator keeps the R8
    /// layout dense). UVs live in [0,1] of whichever texture `GlyphInfo`
    /// points into, selected by `GlyphInfo::is_color`.
    color_texture: metal::Texture,
    /// v1.10.12: glyph cache keyed by `(char, style key)` — each SGR style
    /// variant is a distinct atlas entry (regular / bold / italic /
    /// bold-italic) so styled text samples the correct face.
    cache: HashMap<(char, u8), GlyphInfo>,
    /// v1.0 P1.5-B3: Direct-indexed lookup for ASCII chars (0..=127),
    /// flattened per style key (`ascii[style_key][char]`). Terminal output
    /// is overwhelmingly ASCII (digits, letters, punctuation, space) — a
    /// direct array index eliminates HashMap hashing for the common case.
    /// `ascii[0][0..32]` are control chars (unused, always None);
    /// `ascii[style_key][32..=127]` covers printable ASCII. Falls back to
    /// `cache` for non-ASCII.
    ascii: [[Option<GlyphInfo>; 128]; 4],
    /// v1.6.0 step 4: Cluster atlas for multi-scalar graphemes (e + combining
    /// acute, ZWJ emoji, regional flag pairs, skin tone modifiers, VS16
    /// promotions). Keyed by the full cluster string (e.g. `"e\u{0301}"`,
    /// `"👩\u{200d}🔬"`). Empty in the common case — most terminal output is
    /// single-scalar and goes through `cache`/`ascii` instead. The cluster
    /// path costs one `Arc<str>` hash per lookup, which is intentionally
    /// avoided for the ASCII fast path.
    ///
    /// v1.6.0 step 6: entries are wrapped in [`ClusterEntry`] for LRU tracking.
    /// When [`MAX_CLUSTER_CACHE_ENTRIES`] is reached, the least-recently-used
    /// entry is evicted. `cluster_access_counter` is bumped on every access
    /// so `last_used` ordering stays correct.
    cluster_cache: HashMap<Arc<str>, ClusterEntry>,
    /// v1.6.0 step 6: Monotonic counter bumped on every cluster access
    /// (hit or miss). Used as the `last_used` timestamp for LRU eviction.
    /// Stored as `Cell` because `get_cluster` takes `&self` (the paint path
    /// holds an immutable atlas borrow during `push_row`).
    cluster_access_counter: std::cell::Cell<u64>,
    /// v1.6.0 step 6: Performance counters for diagnostics / perf gate.
    /// All `Cell` so they can be updated from `&self` in `get_cluster`.
    cluster_hits: std::cell::Cell<u64>,
    cluster_misses: std::cell::Cell<u64>,
    cluster_evictions: std::cell::Cell<u64>,
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
    /// v1.10.4: independent allocator cursor for the RGBA color atlas.
    color_next_x: u32,
    color_next_y: u32,
    color_row_height: u32,
    /// Primary font (Menlo for Latin).
    primary_font: Font,
    /// v1.10.12: Style-variant primary fonts (bold / italic / bold-italic).
    /// `None` when the family has no variant — the regular face is used and
    /// styled cells silently render regular.
    bold_font: Option<Font>,
    italic_font: Option<Font>,
    bold_italic_font: Option<Font>,
    /// v1.10.12-fix: the family has no true italic face (e.g. Fira Code) —
    /// italic glyphs are synthesized by skewing the regular/bold face during
    /// rasterization instead of rendering plain.
    synthesize_italic: bool,
    synthesize_bold_italic: bool,
    /// CJK fallback font (PingFang SC).
    cjk_font: Option<Font>,
    cjk_family: String,
    /// Emoji fallback font (Apple Color Emoji).
    emoji_font: Option<Font>,
    emoji_family: String,
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

        // v1.10.12: style variants of the primary font. Best-effort — a
        // family without bold/italic faces degrades to regular rendering.
        use font_kit::properties::{Style as FontStyle, Weight};
        let family = font_config.family.as_str();
        let menlo = ["/System/Library/Fonts/Menlo.ttc"];
        let bold_font = resolve_font_variant(family, &menlo, Weight::BOLD, FontStyle::Normal);
        let italic_font = resolve_font_variant(family, &menlo, Weight::NORMAL, FontStyle::Italic);
        let bold_italic_font =
            resolve_font_variant(family, &menlo, Weight::BOLD, FontStyle::Italic);
        // v1.10.12-fix: no true italic face → synthesize by skewing the
        // regular/bold face (Fira Code ships no italic; Core Text would have
        // approximated the request with the regular face).
        let synthesize_italic = italic_font.is_none();
        let synthesize_bold_italic = bold_italic_font.is_none();
        if bold_font.is_some() || italic_font.is_some() || synthesize_italic {
            info!(
                family,
                bold = bold_font.is_some(),
                italic = italic_font.is_some(),
                synthesize_italic,
                "Glyph atlas: loaded bold/italic style variants"
            );
        }

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
        // v1.10.12: one plane per style key (regular/bold/italic/bold-italic).
        let mut ascii: [[Option<GlyphInfo>; 128]; 4] = [[None; 128]; 4];

        // Pre-rasterize printable ASCII (32–126) for every available style.
        // ~95 glyphs × 4 styles ≈ 380 slots ≈ 152Kpx — negligible against
        // the 2048² atlas, and it makes styled ASCII a pure cache hit.
        // v1.10.12-fix: styles without a true italic face rasterize the
        // base face with the skew transform (`synthesize_italic`).
        let styles: [(&Font, GlyphStyle, bool); 4] = [
            (&primary_font, GlyphStyle::REGULAR, false),
            (
                bold_font.as_ref().unwrap_or(&primary_font),
                GlyphStyle::BOLD,
                false,
            ),
            (
                italic_font.as_ref().unwrap_or(&primary_font),
                GlyphStyle::ITALIC,
                synthesize_italic,
            ),
            (
                bold_italic_font
                    .as_ref()
                    .or(bold_font.as_ref())
                    .unwrap_or(&primary_font),
                GlyphStyle::BOLD_ITALIC,
                synthesize_bold_italic,
            ),
        ];
        for (font, style, synthesize) in styles {
            for ch in (32u8..=126).map(|b| b as char) {
                let placed = Self::rasterize_and_place(
                    font,
                    ch,
                    scaled_size,
                    cell_w,
                    cell_h,
                    false,
                    synthesize,
                    &mut atlas_pixels,
                    atlas_w,
                    atlas_h,
                    &mut next_x,
                    &mut next_y,
                    &mut row_height,
                    primary_descent_px,
                );
                if let Some(info) = placed {
                    let key = style.key();
                    cache.insert((ch, key), info);
                    ascii[key as usize][ch as usize] = Some(info);
                }
            }
        }

        // Pre-rasterize the prompt marker and common UI glyphs at init.
        for &ch in &['❯', '❮', '›', '→', '•', '·', '…', '─', '▸', '▾', '●', '○']
        {
            let font = if primary_font.glyph_for_char(ch).is_some() {
                &primary_font
            } else {
                symbol_font.as_ref().unwrap_or(&primary_font)
            };
            let placed = Self::rasterize_and_place(
                font,
                ch,
                scaled_size,
                cell_w,
                cell_h,
                false,
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
                cache.insert((ch, GlyphStyle::REGULAR.key()), info);
            }
        }

        // Create Metal texture and upload atlas
        let texture = Self::create_texture(device, atlas_w, atlas_h);
        Self::upload_pixels(&texture, &atlas_pixels, atlas_w, atlas_h);
        // v1.10.4: RGBA color atlas for color emoji. Same size as the mask
        // atlas; its own allocator cursor keeps the R8 layout dense (color
        // glyphs are rare — a handful of emoji per session).
        let color_texture = Self::create_color_texture(device, atlas_w, atlas_h);

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
            color_texture,
            cache,
            ascii,
            cluster_cache: HashMap::new(),
            cluster_access_counter: std::cell::Cell::new(0),
            cluster_hits: std::cell::Cell::new(0),
            cluster_misses: std::cell::Cell::new(0),
            cluster_evictions: std::cell::Cell::new(0),
            cell_width: cell_w,
            cell_height: cell_h,
            atlas_w,
            atlas_h,
            next_x,
            next_y,
            row_height,
            color_next_x: 0,
            color_next_y: 0,
            color_row_height: 0,
            primary_font,
            bold_font,
            italic_font,
            bold_italic_font,
            synthesize_italic,
            synthesize_bold_italic,
            cjk_font: None,
            cjk_family: font_config.cjk_family.clone(),
            emoji_font: None,
            emoji_family: font_config.emoji_family.clone(),
            symbol_font,
            scaled_size,
            primary_descent_px,
        }
    }

    /// Look up a cached glyph by character (regular style — UI text path).
    pub fn texture(&self) -> &metal::Texture {
        &self.texture
    }

    /// v1.10.4: Get the RGBA color atlas texture (color emoji). The renderer
    /// binds it at fragment texture index 1 and the shader selects it when
    /// `GlyphInfo::is_color` is set (via the fg.a sentinel).
    pub fn color_texture(&self) -> &metal::Texture {
        &self.color_texture
    }

    /// v1.6.0 step 6: Snapshot of cluster cache performance counters for
    /// diagnostics and the perf gate. Returns hits, misses, evictions,
    /// current entry count, and capacity.
    #[allow(dead_code)]
    pub fn cluster_stats(&self) -> ClusterCacheStats {
        ClusterCacheStats {
            hits: self.cluster_hits.get(),
            misses: self.cluster_misses.get(),
            evictions: self.cluster_evictions.get(),
            entries: self.cluster_cache.len(),
            capacity: MAX_CLUSTER_CACHE_ENTRIES,
        }
    }
}
