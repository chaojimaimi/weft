use std::sync::Arc;

use crate::paint::primitives::{push_quad, resolve_cell_color};
use crate::paint::styled_line_cache::{
    palette_fingerprint, translate_and_append_shifts_only_xy, StyledLineCache, StyledLineCacheKey,
};
use crate::renderer::MetalRenderer;
use weft_core::blocks::StyledLine;
use weft_core::grid::Color;

pub(super) struct BlockOutputTextPaint<'a> {
    pub(super) x: f32,
    pub(super) y: f32,
    pub(super) text: &'a str,
    pub(super) style: Option<&'a StyledLine>,
    pub(super) char_offset: usize,
    pub(super) fallback: [f32; 4],
    pub(super) max_cols: usize,
    pub(super) palette: &'a [Color; 256],
    pub(super) row_pitch: f32,
}

/// v1.4.1: Inputs needed to build a `StyledLineCacheKey` and verify Arc
/// identity, without re-borrowing the paint struct. Passed alongside the
/// `BlockOutputTextPaint` to `push_block_output_text_cached`.
pub(super) struct CacheKeyInput {
    pub(super) pane_session_id: u64,
    pub(super) block_id: u64,
    pub(super) line_idx: usize,
    pub(super) chunk_idx: usize,
    pub(super) render_generation: u64,
    pub(super) palette_fingerprint: u64,
    /// Source text Arc identity guard. `None` for live/in-flight blocks —
    /// when `None`, the cache is bypassed entirely (caller convention).
    pub(super) source: Option<Arc<str>>,
    /// Styled output Arc identity guard. `None` matches both "block has no
    /// styled output" and "live block bypass".
    pub(super) styled: Option<Arc<weft_core::blocks::StyledOutput>>,
}

/// v1.4.1: Look up a completed block's `Arc<str>` source and
/// `Option<Arc<StyledOutput>>` by `block_id`. Returns `(None, None)` when
/// `block_id` is `None` (live/in-flight block rows) or the block isn't
/// found — both cases bypass the styled-line cache.
pub(super) fn block_arc_identity(
    blocks: &[weft_core::blocks::Block],
    block_id: Option<weft_core::blocks::BlockId>,
) -> (
    Option<Arc<str>>,
    Option<Arc<weft_core::blocks::StyledOutput>>,
) {
    let Some(id) = block_id else {
        return (None, None);
    };
    blocks
        .iter()
        .find(|b| b.id == id)
        .map(|b| (Some(Arc::clone(&b.output)), b.styled_output.clone()))
        .unwrap_or((None, None))
}

fn fills_terminal_cell_edges(ch: char) -> bool {
    matches!(ch, '\u{2500}'..='\u{259f}')
}

impl MetalRenderer {
    /// v1.4.1: Cached wrapper around `push_block_output_text`. On a cache
    /// hit, the cached vertices (built in local origin (0, 0)) are translated
    /// to (x, y) and appended. On a miss, vertices are built into a temp Vec
    /// at origin (0, 0), inserted into the cache, then translated+appended.
    ///
    /// `key_input.source` controls caching eligibility:
    /// - `Some(arc)`: cache is consulted/inserted (completed block path).
    /// - `None`: cache is bypassed; vertices are built directly at (x, y)
    ///   via the legacy `push_block_output_text` path (live block rows).
    pub(super) fn push_block_output_text_cached(
        &self,
        vertices: &mut Vec<f32>,
        paint: BlockOutputTextPaint<'_>,
        key_input: CacheKeyInput,
        cache: &std::cell::RefCell<StyledLineCache>,
    ) {
        // Live block bypass: no Arc source identity → can't cache safely.
        let Some(source) = key_input.source else {
            self.push_block_output_text(vertices, paint);
            return;
        };

        let key = StyledLineCacheKey {
            pane_session_id: key_input.pane_session_id,
            block_id: key_input.block_id,
            line_idx: key_input.line_idx as u32,
            chunk_idx: key_input.chunk_idx as u32,
            cols: paint.max_cols as u32,
            char_offset: paint.char_offset as u32,
            render_generation: key_input.render_generation,
            palette_fingerprint: key_input.palette_fingerprint,
            fallback_fg: [
                paint.fallback[0].to_bits(),
                paint.fallback[1].to_bits(),
                paint.fallback[2].to_bits(),
                paint.fallback[3].to_bits(),
            ],
        };

        // Try cache hit first.
        if let Some(cached) = cache
            .borrow_mut()
            .lookup(&key, &source, key_input.styled.as_ref())
        {
            translate_and_append_shifts_only_xy(vertices, &cached, paint.x, paint.y);
            return;
        }

        // Miss: build into a local Vec at origin (0, 0). We construct a
        // paint copy with x=0, y=0 so the existing push_block_output_text
        // writes vertices in local coordinates suitable for caching.
        let mut local = Vec::new();
        self.push_block_output_text(
            &mut local,
            BlockOutputTextPaint {
                x: 0.0,
                y: 0.0,
                text: paint.text,
                style: paint.style,
                char_offset: paint.char_offset,
                fallback: paint.fallback,
                max_cols: paint.max_cols,
                palette: paint.palette,
                row_pitch: paint.row_pitch,
            },
        );

        // Insert into cache (FIFO eviction handles byte budget).
        cache
            .borrow_mut()
            .insert(key, Arc::clone(&source), key_input.styled, local.clone());

        // Translate local vertices to (x, y) and append to the main Vec.
        translate_and_append_shifts_only_xy(vertices, &local, paint.x, paint.y);
    }

    pub(super) fn push_block_output_text(
        &self,
        vertices: &mut Vec<f32>,
        paint: BlockOutputTextPaint<'_>,
    ) {
        let cw = self.cell_width() as f32;
        let mut col = 0;
        let mut x = paint.x;
        for (index, ch) in paint.text.chars().enumerate() {
            let width = weft_core::grid::terminal_char_width(ch);
            if width == 0 {
                continue;
            }
            if col + width > paint.max_cols {
                break;
            }
            let color = paint
                .style
                .and_then(|line| line.foreground_at(paint.char_offset + index))
                .map(|origin| resolve_cell_color(origin, paint.fallback, paint.palette))
                .unwrap_or(paint.fallback);
            if let Some(background) = paint
                .style
                .and_then(|line| line.background_at(paint.char_offset + index))
            {
                let background = resolve_cell_color(background, [0.0; 4], paint.palette);
                let cell_width = width as f32 * cw;
                push_quad(
                    vertices,
                    [x, paint.y, x + cell_width, paint.y + paint.row_pitch],
                    [0.0; 4],
                    [0.0; 4],
                    background,
                );
            }
            let mut encoded = [0; 4];
            let glyph_height = if fills_terminal_cell_edges(ch) {
                paint.row_pitch
            } else {
                self.cell_height() as f32
            };
            self.push_text_with_height(
                vertices,
                [x, paint.y],
                ch.encode_utf8(&mut encoded),
                color,
                width,
                glyph_height,
            );
            col += width;
            x += width as f32 * cw;
        }
    }

    /// v1.4.1: Compute the palette fingerprint for the current frame's
    /// 256-color palette. Called once at the top of
    /// `build_block_view_vertices` and passed to every
    /// `push_block_output_text_cached` call via `CacheKeyInput`.
    pub(super) fn block_palette_fingerprint(&self, palette: &[Color; 256]) -> u64 {
        palette_fingerprint(palette)
    }

    /// v1.4.1: Current render generation for the styled-line cache. Read
    /// once at the top of `build_block_view_vertices` and passed to every
    /// `push_block_output_text_cached` call via `CacheKeyInput`.
    pub(super) fn styled_cache_generation(&self) -> u64 {
        self.styled_line_cache.borrow().generation()
    }
}

#[cfg(test)]
mod tests {
    use super::fills_terminal_cell_edges;

    #[test]
    fn block_and_box_glyphs_bridge_block_view_row_leading() {
        for glyph in ['█', '▀', '▄', '▌', '┌', '─', '│', '┘'] {
            assert!(fills_terminal_cell_edges(glyph), "glyph={glyph}");
        }
        for glyph in ['A', '中', '●'] {
            assert!(!fills_terminal_cell_edges(glyph), "glyph={glyph}");
        }
    }
}
