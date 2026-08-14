//! Glyph lookup + on-demand rasterization (v1.10.12 split to keep
//! `glyph/mod.rs` under the line budget).

use std::sync::Arc;

use super::cluster;
use super::font::{is_emoji_char, rasterize_emoji_rgba, resolve_font};
use super::{ClusterEntry, GlyphAtlas, GlyphInfo, GlyphStyle, MAX_CLUSTER_CACHE_ENTRIES};

/// v1.10.4: a cluster renders via the RGBA color atlas only when it is
/// classified emoji **and** the emoji font contains the base scalar.
/// Clusters like `"e\u{fe0f}"` pass the classifier, but the emoji font has
/// no 'e' — CoreText cascades and draws a white text glyph that can't be
/// fg-tinted. Those stay on the A8 mask path. Real emoji keep the color path.
pub(super) fn cluster_wants_color_atlas(is_emoji: bool, emoji_font_has_base: bool) -> bool {
    is_emoji && emoji_font_has_base
}

impl GlyphAtlas {
    pub fn get(&self, ch: char) -> Option<&GlyphInfo> {
        self.get_style(ch, GlyphStyle::REGULAR)
    }

    /// Look up a cached glyph by character and SGR style variant.
    ///
    /// v1.0 P1.5-B3: ASCII fast path — direct array index beats HashMap
    /// hash + lookup. Terminal output is overwhelmingly ASCII, so this
    /// branch is the hot path. ASCII entries are mirrored in both
    /// `ascii[]` and `cache` (kept in sync in `new()` and
    /// `get_or_rasterize()`), so returning from either is equivalent.
    pub fn get_style(&self, ch: char, style: GlyphStyle) -> Option<&GlyphInfo> {
        let key = style.key();
        if (ch as u32) < 128 {
            return self.ascii[key as usize][ch as usize].as_ref();
        }
        self.cache.get(&(ch, key))
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
    /// atlas entry. Rasterizes exactly once per new character (per style)
    /// and uploads the pixels to the Metal texture.
    ///
    /// Style variants only apply to the primary (Latin) font: CJK / emoji /
    /// symbol fallbacks render regular, so styled CJK gracefully degrades.
    ///
    /// Returns None only if the atlas is full or the character can't be rasterized.
    pub fn get_or_rasterize(&mut self, ch: char, style: GlyphStyle) -> Option<&GlyphInfo> {
        // v1.0 P1.5-B3: ASCII fast path — direct array index check instead
        // of HashMap contains_key. ASCII entries are mirrored in both
        // `ascii[]` and `cache`, so a hit here means a hit in `cache` too.
        let key = style.key();
        if (ch as u32) < 128 {
            if self.ascii[key as usize][ch as usize].is_some() {
                return self.ascii[key as usize][ch as usize].as_ref();
            }
        } else if self.cache.contains_key(&(ch, key)) {
            return self.cache.get(&(ch, key));
        }

        // Determine which font to use
        let is_wide = weft_core::grid::terminal_char_width(ch) > 1;
        let is_emoji = is_emoji_char(ch);
        self.ensure_script_font(is_emoji, is_wide);

        // v1.10.12: styled glyphs use the primary font's variant face when
        // available; fallback fonts (CJK/emoji/symbol) and missing variants
        // degrade to the regular ladder. The `filter` keeps the borrow of a
        // single field alive through the fallback — no `expect`, no
        // two-phase borrow that could drift.
        let font = if style != GlyphStyle::REGULAR && !is_emoji && !is_wide {
            match style.key() {
                1 => self.bold_font.as_ref(),
                2 => self.italic_font.as_ref(),
                // v1.10.12-fix: no bold-italic face → bold + synthesized
                // skew, matching the prewarmed ASCII path (previously this
                // fell back to the regular face → italic-but-not-bold).
                3 => self.bold_italic_font.as_ref().or(self.bold_font.as_ref()),
                _ => None,
            }
            .filter(|f| f.glyph_for_char(ch).is_some())
            .unwrap_or_else(|| {
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
            })
        } else if is_emoji {
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
            crate::glyph::font::rasterize_emoji_rgba(
                font,
                ch,
                self.scaled_size,
                glyph_w,
                self.cell_height,
                self.primary_descent_px,
            )
            .unwrap_or_else(|| vec![0u8; (glyph_w * self.cell_height * 4) as usize])
        } else {
            // v1.10.12-fix: synthesize italic by skewing the base face when
            // the family has no true italic variant (e.g. Fira Code).
            let synthesize_italic = match style.key() {
                2 => self.synthesize_italic,
                3 => self.synthesize_bold_italic,
                _ => false,
            };
            Self::rasterize_glyph(
                font,
                ch,
                self.scaled_size,
                glyph_w,
                self.cell_height,
                is_wide,
                self.primary_descent_px,
                synthesize_italic,
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
            self.ascii[key as usize][ch as usize] = Some(info);
        }
        self.cache.insert((ch, key), info);

        // Return from the same source the corresponding `get()` will use, so
        // the caller's reference stays valid regardless of which path served
        // it: ASCII from `ascii[]`, otherwise from `cache`.
        if is_ascii {
            self.ascii[key as usize][ch as usize].as_ref()
        } else {
            self.cache.get(&(ch, key))
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
                        false,
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
}
