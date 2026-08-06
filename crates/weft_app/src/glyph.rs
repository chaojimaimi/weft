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
use std::sync::Arc;

use font_kit::loaders::core_text::Font;
use metal::Device;
use tracing::info;
use weft_core::config::FontConfig;

mod atlas;
mod cluster;
mod font;
mod rasterize;
mod special;
mod tests;

use font::{is_emoji_char, nonzero_cell_dimension, rasterize_emoji_rgba, resolve_font};

/// v1.10.4: a cluster renders via the RGBA color atlas only when it is
/// classified emoji **and** the emoji font contains the base scalar.
///
/// Clusters like `"e\u{fe0f}"` pass the `is_emoji_char || contains(fe0f)`
/// classifier, but the emoji font has no 'e' — CoreText cascades and draws
/// a white text glyph that can't be fg-tinted. Those stay on the A8 mask
/// path so normal foreground coloring applies. Real emoji (🦞, 👩🏽, 🇨🇳,
/// ❤️) all have base scalars in Apple Color Emoji (probe-verified) and
/// keep the color path.
fn cluster_wants_color_atlas(is_emoji: bool, emoji_font_has_base: bool) -> bool {
    is_emoji && emoji_font_has_base
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
    /// v1.10.4: whether this glyph's pixels live in the RGBA color atlas
    /// (color emoji — Apple Color Emoji sbix bitmaps) instead of the R8
    /// alpha-mask atlas. Color glyphs carry their own RGB; the instance's
    /// `fg` is ignored (a sentinel alpha encodes this to the shader).
    pub is_color: bool,
}

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
    cache: HashMap<char, GlyphInfo>,
    /// v1.0 P1.5-B3: Direct-indexed lookup for ASCII chars (0..=127).
    /// Terminal output is overwhelmingly ASCII (digits, letters, punctuation,
    /// space) — a direct array index eliminates HashMap hashing for the
    /// common case. `ascii[0..32]` are control chars (unused, always None);
    /// 32..=127 covers printable ASCII. Falls back to `cache` for non-ASCII.
    ascii: [Option<GlyphInfo>; 128],
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
            cjk_font: None,
            cjk_family: font_config.cjk_family.clone(),
            emoji_font: None,
            emoji_family: font_config.emoji_family.clone(),
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

    /// Load large TTC fallbacks only when output actually needs them. On the
    /// current macOS font set, eagerly opening PingFang + Apple Color Emoji
    /// retained more than 500 MiB of `MALLOC_LARGE` allocations even in an
    /// ASCII-only idle terminal.
    fn ensure_script_font(&mut self, is_emoji: bool, is_wide: bool) {
        if is_emoji && self.emoji_font.is_none() {
            self.emoji_font = resolve_font(
                &self.emoji_family,
                &["/System/Library/Fonts/Apple Color Emoji.ttc"],
            );
        } else if is_wide && self.cjk_font.is_none() {
            self.cjk_font = resolve_font(
                &self.cjk_family,
                &[
                    "/System/Library/Fonts/PingFang.ttc",
                    "/System/Library/Fonts/STHeiti Light.ttc",
                ],
            );
        }
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
        self.ensure_script_font(is_emoji, is_wide);

        let font = if is_emoji {
            self.emoji_font.as_ref().unwrap_or(&self.primary_font)
        } else if is_wide {
            self.cjk_font.as_ref().unwrap_or(&self.primary_font)
        } else {
            // Narrow text: prefer the monospace primary, then the lightweight
            // symbol fallback. CJK and emoji fonts are loaded by script class
            // above instead of probing both large TTCs for every missing glyph.
            if self.primary_font.glyph_for_char(ch).is_some() {
                &self.primary_font
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

        // Step 1: Allocate atlas slot (layout only, no pixel work). Color
        // emoji get a slot in the RGBA color atlas (independent allocator);
        // everything else uses the R8 mask atlas.
        let info = if is_emoji {
            Self::allocate_slot(
                is_wide,
                self.cell_width,
                self.cell_height,
                self.atlas_w,
                self.atlas_h,
                &mut self.color_next_x,
                &mut self.color_next_y,
                &mut self.color_row_height,
                true,
            )?
        } else {
            Self::allocate_slot(
                is_wide,
                self.cell_width,
                self.cell_height,
                self.atlas_w,
                self.atlas_h,
                &mut self.next_x,
                &mut self.next_y,
                &mut self.row_height,
                false,
            )?
        };

        // Step 2: Rasterize the glyph once. Color emoji go through the
        // CoreText RGBA path (their own RGB is stored in the color atlas);
        // everything else through the font-kit A8 mask path.
        let glyph_w = if is_wide {
            self.cell_width * 2
        } else {
            self.cell_width
        };
        let pixels = if is_emoji {
            // Fall back to a fully transparent slot if CoreText can't draw
            // the emoji — a blank is safer than a garbage atlas sample.
            font::rasterize_emoji_rgba(
                font,
                ch,
                self.scaled_size,
                glyph_w,
                self.cell_height,
                self.primary_descent_px,
            )
            .unwrap_or_else(|| vec![0u8; (glyph_w * self.cell_height * 4) as usize])
        } else {
            Self::rasterize_glyph(
                font,
                ch,
                self.scaled_size,
                glyph_w,
                self.cell_height,
                is_wide,
                self.primary_descent_px,
            )
        };

        // Step 3: Upload to Metal texture — color emoji into the RGBA atlas
        // (4 B/px), everything else into the R8 mask atlas (1 B/px).
        if is_emoji {
            Self::upload_region(
                &self.color_texture,
                &pixels,
                info.size.0,
                info.uv_origin,
                self.atlas_w,
                self.atlas_h,
                4,
            );
        } else {
            Self::upload_region(
                &self.texture,
                &pixels,
                info.size.0,
                info.uv_origin,
                self.atlas_w,
                self.atlas_h,
                1,
            );
        }

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

    /// v1.6.0 step 4: Look up a cached cluster glyph by its full string.
    ///
    /// Returns `None` for clusters that haven't been rasterized yet. Call
    /// [`get_or_rasterize_cluster`](Self::get_or_rasterize_cluster) to
    /// rasterize on demand. The caller is responsible for only consulting
    /// this path when `CellFlags::EXTRA` is set on the source cell — single
    /// scalar cells must continue to use [`get`](Self::get) / the ASCII
    /// direct-index array.
    ///
    /// v1.6.0 step 5: the grid paint path now calls this from the UV
    /// resolver closure in `build_grid_instances` / `build_grid_instances_for_background_pane`.
    ///
    /// v1.6.0 step 6: tracks hits/misses for perf diagnostics. Hits update
    /// the entry's `last_used` counter for LRU eviction. Uses interior
    /// mutability via `Cell` for the counter to keep the `&self` signature
    /// (the paint path holds an immutable borrow during `push_row`).
    pub fn get_cluster(&self, cluster: &str) -> Option<&GlyphInfo> {
        // v1.6.0 step 6: bump the access counter and update last_used on hit.
        // We can't mutate `cluster_cache` through `&self`, so we track hits
        // via Cell. The LRU `last_used` update is deferred to the next
        // `get_or_rasterize_cluster` call — a read-only lookup doesn't
        // reorder the LRU. This is a deliberate simplification: the paint
        // path calls `get_cluster` many times per frame for the same clusters,
        // and updating `last_used` on every paint would require `&mut self`
        // which conflicts with the immutable atlas borrow in `draw()`.
        if self.cluster_cache.contains_key(cluster) {
            self.cluster_hits.set(self.cluster_hits.get() + 1);
            self.cluster_cache.get(cluster).map(|e| &e.info)
        } else {
            self.cluster_misses.set(self.cluster_misses.get() + 1);
            None
        }
    }

    /// v1.6.0 step 4: Look up a cached cluster glyph, or rasterize on demand.
    ///
    /// `cluster` is the full grapheme cluster string (e.g. `"e\u{0301}"`,
    /// `"👩\u{200d}🔬"`). `base_char` is the lead scalar (the cell's
    /// `character` field) — used to pick the right font (emoji vs CJK vs
    /// primary) when the cluster contains characters from multiple scripts.
    /// `is_wide` indicates whether the cluster occupies a double-width slot.
    ///
    /// Returns `None` if the atlas is full or the cluster cannot be shaped
    /// by CoreText. Callers should fall back to the lead scalar's atlas
    /// entry (via [`get`](Self::get)) in that case — the cell will render
    /// with the lead scalar only, which is the v1.5 behavior.
    ///
    /// [plan]: ../../docs/V16_IMPLEMENTATION_PLAN.md
    pub fn get_or_rasterize_cluster(
        &mut self,
        cluster: &str,
        base_char: char,
        is_wide: bool,
    ) -> Option<&GlyphInfo> {
        // Fast path: already rasterized. Use contains_key + get to keep the
        // immutable borrow scoped to this block — a direct `if let Some(info)
        // = self.cluster_cache.get(cluster)` would extend the borrow to the
        // end of the function and block the insert below.
        if self.cluster_cache.contains_key(cluster) {
            // v1.6.0 step 6: update last_used for LRU on re-rasterization hits.
            let now = self.cluster_access_counter.get().saturating_add(1);
            self.cluster_access_counter.set(now);
            if let Some(entry) = self.cluster_cache.get_mut(cluster) {
                entry.last_used = now;
            }
            return self.cluster_cache.get(cluster).map(|e| &e.info);
        }

        // v1.6.0 step 6: evict the LRU entry if the cluster cache is at capacity.
        // Linear scan is O(n) but n ≤ 512, and eviction is rare (only when
        // the working set exceeds 512 distinct clusters — uncommon for
        // terminal output). The evicted entry's atlas texture slot is leaked
        // (not reclaimed) — see [`MAX_CLUSTER_CACHE_ENTRIES`] doc comment.
        if self.cluster_cache.len() >= MAX_CLUSTER_CACHE_ENTRIES {
            let lru_key = self
                .cluster_cache
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(k, _)| Arc::clone(k));
            if let Some(key) = lru_key {
                self.cluster_cache.remove(&key);
                let evictions = self.cluster_evictions.get().saturating_add(1);
                self.cluster_evictions.set(evictions);
            }
        }

        // Pick the font for the cluster's base scalar. Emoji clusters (ZWJ
        // sequences, flag pairs, skin tone modifiers) need Apple Color Emoji;
        // wide CJK clusters use the CJK fallback; everything else uses the
        // primary font with the same fallback ladder as `get_or_rasterize`.
        let is_emoji = is_emoji_char(base_char)
            || cluster.contains('\u{fe0f}')
            || cluster.contains('\u{200d}');
        self.ensure_script_font(is_emoji, is_wide);
        let font = if is_emoji {
            self.emoji_font.as_ref().unwrap_or(&self.primary_font)
        } else if is_wide {
            self.cjk_font.as_ref().unwrap_or(&self.primary_font)
        } else if self.primary_font.glyph_for_char(base_char).is_some() {
            &self.primary_font
        } else if self
            .symbol_font
            .as_ref()
            .is_some_and(|f| f.glyph_for_char(base_char).is_some())
        {
            self.symbol_font.as_ref().unwrap()
        } else {
            &self.primary_font
        };

        // v1.10.4: only emoji clusters whose base scalar the emoji font
        // actually contains go to the RGBA color atlas (see
        // `cluster_wants_color_atlas`). Everything else stays on the R8
        // mask atlas so fg tinting still applies.
        let color_emoji = cluster_wants_color_atlas(
            is_emoji,
            self.emoji_font
                .as_ref()
                .is_some_and(|f| f.glyph_for_char(base_char).is_some()),
        );

        // Allocate atlas slot (layout only). Emoji clusters go into the RGBA
        // color atlas; other clusters (CJK+combining marks) use the R8 atlas.
        let info = if color_emoji {
            Self::allocate_slot(
                is_wide,
                self.cell_width,
                self.cell_height,
                self.atlas_w,
                self.atlas_h,
                &mut self.color_next_x,
                &mut self.color_next_y,
                &mut self.color_row_height,
                true,
            )?
        } else {
            Self::allocate_slot(
                is_wide,
                self.cell_width,
                self.cell_height,
                self.atlas_w,
                self.atlas_h,
                &mut self.next_x,
                &mut self.next_y,
                &mut self.row_height,
                false,
            )?
        };

        let glyph_w = if is_wide {
            self.cell_width * 2
        } else {
            self.cell_width
        };

        // Shape + rasterize the full cluster via CoreText CTLine. Color emoji
        // clusters keep their RGBA pixels for the color atlas; text clusters
        // (combining marks) reduce to an alpha mask for the R8 atlas.
        let pixels = cluster::rasterize_cluster_alpha(
            font,
            cluster,
            self.scaled_size,
            glyph_w,
            self.cell_height,
            self.primary_descent_px,
            color_emoji,
        );

        let pixels = match pixels {
            Some(p) => p,
            None => {
                // Cluster couldn't be shaped — fall back to rasterizing the
                // lead scalar alone so the cell renders something visible
                // (v1.5 behavior) instead of a blank slot. The fallback must
                // produce the same pixel format the upload below expects.
                if color_emoji {
                    rasterize_emoji_rgba(
                        font,
                        base_char,
                        self.scaled_size,
                        glyph_w,
                        self.cell_height,
                        self.primary_descent_px,
                    )
                    .unwrap_or_else(|| vec![0u8; (glyph_w * self.cell_height * 4) as usize])
                } else {
                    Self::rasterize_glyph(
                        font,
                        base_char,
                        self.scaled_size,
                        glyph_w,
                        self.cell_height,
                        is_wide,
                        self.primary_descent_px,
                    )
                }
            }
        };

        if color_emoji {
            Self::upload_region(
                &self.color_texture,
                &pixels,
                info.size.0,
                info.uv_origin,
                self.atlas_w,
                self.atlas_h,
                4,
            );
        } else {
            Self::upload_region(
                &self.texture,
                &pixels,
                info.size.0,
                info.uv_origin,
                self.atlas_w,
                self.atlas_h,
                1,
            );
        }

        let key = Arc::from(cluster);
        // v1.6.0 step 6: store with current access counter as last_used.
        let now = self.cluster_access_counter.get().saturating_add(1);
        self.cluster_access_counter.set(now);
        self.cluster_cache.insert(
            key,
            ClusterEntry {
                info,
                last_used: now,
            },
        );

        // Return from cluster_cache so the caller's reference matches what
        // future get_cluster() calls will return.
        // SAFETY: we just inserted `info` under `key`; the lookup below
        // borrows cluster_cache immutably and returns a reference into it.
        // The mutable borrow of self ended with the insert above; the
        // immutable borrow for the lookup is the only outstanding borrow.
        self.cluster_cache.get(cluster).map(|e| &e.info)
    }

    /// Get the Metal atlas texture.
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
