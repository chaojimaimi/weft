use std::sync::Arc;

use crate::paint::primitives::{
    color_to_normalized, ensure_minimum_text_contrast, push_quad, resolve_cell_color,
};
use crate::paint::styled_line_cache::{
    palette_fingerprint, translate_and_append_shifts_only_xy, StyledLineCache, StyledLineCacheKey,
};
use crate::renderer::MetalRenderer;
use weft_core::blocks::{OutputSemanticRole, SemanticSpan, StyledLine};
use weft_core::config::Theme;
use weft_core::grid::{CellFlags, Color};

pub(super) struct BlockOutputTextPaint<'a> {
    pub(super) x: f32,
    pub(super) y: f32,
    pub(super) text: &'a str,
    pub(super) semantic_text: &'a str,
    pub(super) style: Option<&'a StyledLine>,
    pub(super) char_offset: usize,
    pub(super) fallback: [f32; 4],
    pub(super) canvas: [f32; 4],
    pub(super) selection: Option<(usize, usize, [f32; 4])>,
    pub(super) max_cols: usize,
    pub(super) palette: &'a [Color; 256],
    pub(super) row_pitch: f32,
}

fn semantic_role_at(spans: &[SemanticSpan], index: usize) -> Option<OutputSemanticRole> {
    let index = u32::try_from(index).ok()?;
    let position = spans.partition_point(|span| span.end_char <= index);
    spans
        .get(position)
        .filter(|span| span.start_char <= index && index < span.end_char)
        .map(|span| span.role)
}

fn semantic_color(theme: &Theme, role: OutputSemanticRole) -> [f32; 4] {
    let color = match role {
        OutputSemanticRole::Label | OutputSemanticRole::Metadata => theme.output.metadata,
        OutputSemanticRole::Path => theme.syntax.path,
        OutputSemanticRole::Url | OutputSemanticRole::Address => theme.accent,
        OutputSemanticRole::Number | OutputSemanticRole::Version => theme.syntax.number,
        OutputSemanticRole::Success => theme.output.success,
        OutputSemanticRole::Warning => theme.palette[3],
        OutputSemanticRole::Failure => theme.output.failure,
    };
    color_to_normalized(color)
}

fn semantic_fallback_at(
    theme: &Theme,
    enabled: bool,
    spans: &[SemanticSpan],
    index: usize,
    ansi_owned: bool,
    fallback: [f32; 4],
) -> [f32; 4] {
    if !enabled || ansi_owned {
        return fallback;
    }
    semantic_role_at(spans, index)
        .map(|role| semantic_color(theme, role))
        .unwrap_or(fallback)
}

fn reverse_colors(
    foreground: [f32; 4],
    background: Option<[f32; 4]>,
    canvas: [f32; 4],
) -> ([f32; 4], Option<[f32; 4]>) {
    (background.unwrap_or(canvas), Some(foreground))
}

fn contrast_background(
    explicit_background: Option<[f32; 4]>,
    canvas: [f32; 4],
    selection: Option<(usize, usize, [f32; 4])>,
    source_index: usize,
) -> [f32; 4] {
    explicit_background.unwrap_or_else(|| {
        crate::paint::primitives::text_background_for_range(
            canvas,
            selection,
            source_index..source_index + 1,
        )
    })
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
        // Selection changes independently of immutable block content and its
        // translucent background affects the final text contrast.
        if paint.selection.is_some() {
            self.push_block_output_text(vertices, paint);
            return;
        }
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
            canvas: [
                paint.canvas[0].to_bits(),
                paint.canvas[1].to_bits(),
                paint.canvas[2].to_bits(),
                paint.canvas[3].to_bits(),
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
                semantic_text: paint.semantic_text,
                style: paint.style,
                char_offset: paint.char_offset,
                fallback: paint.fallback,
                canvas: paint.canvas,
                selection: paint.selection,
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
        let semantic_spans = if self.semantic_output_enabled {
            weft_core::blocks::classify_line(paint.semantic_text, &[])
        } else {
            Vec::new()
        };
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

            let source_index = paint.char_offset + index;
            let flags = paint
                .style
                .map(|line| line.attributes_at(source_index))
                .unwrap_or(CellFlags::empty());

            let explicit_fg = paint
                .style
                .and_then(|line| line.foreground_at(source_index));
            let bg = paint
                .style
                .and_then(|line| line.background_at(source_index))
                .map(|origin| resolve_cell_color(origin, [0.0; 4], paint.palette));
            let ansi_owned = explicit_fg.is_some() || bg.is_some() || !flags.is_empty();
            let fg = explicit_fg
                .map(|origin| resolve_cell_color(origin, paint.fallback, paint.palette))
                .unwrap_or_else(|| {
                    semantic_fallback_at(
                        &self.theme,
                        self.semantic_output_enabled,
                        &semantic_spans,
                        source_index,
                        ansi_owned,
                        paint.fallback,
                    )
                });

            // SGR reverse video: swap fg and bg before emitting quads.
            let (fg_final, bg_final) = if flags.contains(CellFlags::REVERSE) {
                reverse_colors(fg, bg, paint.canvas)
            } else {
                (fg, bg)
            };

            let contrast_background =
                contrast_background(bg_final, paint.canvas, paint.selection, source_index);
            let fg_final =
                ensure_minimum_text_contrast(fg_final, contrast_background, self.minimum_contrast);

            // DIM remains an explicit application-owned hierarchy signal and
            // is applied after the optional display contrast correction.
            let fg_final = if flags.contains(CellFlags::DIM) {
                [
                    fg_final[0] * 0.5,
                    fg_final[1] * 0.5,
                    fg_final[2] * 0.5,
                    fg_final[3],
                ]
            } else {
                fg_final
            };

            // Draw background quad if non-transparent.
            if let Some(background) = bg_final {
                let cell_width = width as f32 * cw;
                push_quad(
                    vertices,
                    [x, paint.y, x + cell_width, paint.y + paint.row_pitch],
                    [0.0; 4],
                    [0.0; 4],
                    background,
                );
            }

            // HIDDEN: skip glyph emission, background already drawn.
            if !flags.contains(CellFlags::HIDDEN) {
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
                    fg_final,
                    width,
                    glyph_height,
                );
            }

            // v1.7.0-A: Draw decoration quads for underline/strikethrough.
            let cell_width = width as f32 * cw;
            if flags.contains(CellFlags::UNDERLINE) || flags.contains(CellFlags::DOUBLE_UNDER) {
                let underline_y = paint.y + paint.row_pitch - 2.0;
                push_quad(
                    vertices,
                    [x, underline_y, x + cell_width, underline_y + 2.0],
                    [0.0; 4],
                    [0.0; 4],
                    fg_final,
                );
                if flags.contains(CellFlags::DOUBLE_UNDER) {
                    let second_y = underline_y - 3.0;
                    push_quad(
                        vertices,
                        [x, second_y, x + cell_width, second_y + 2.0],
                        [0.0; 4],
                        [0.0; 4],
                        fg_final,
                    );
                }
            }
            if flags.contains(CellFlags::STRIKETHROUGH) {
                let strike_y = paint.y + paint.row_pitch * 0.5 - 1.0;
                push_quad(
                    vertices,
                    [x, strike_y, x + cell_width, strike_y + 2.0],
                    [0.0; 4],
                    [0.0; 4],
                    fg_final,
                );
            }

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
    use super::{
        contrast_background, fills_terminal_cell_edges, reverse_colors, semantic_fallback_at,
    };
    use crate::paint::primitives::color_to_normalized;
    use weft_core::config::Theme;

    #[test]
    fn block_and_box_glyphs_bridge_block_view_row_leading() {
        for glyph in ['█', '▀', '▄', '▌', '┌', '─', '│', '┘'] {
            assert!(fills_terminal_cell_edges(glyph), "glyph={glyph}");
        }
        for glyph in ['A', '中', '●'] {
            assert!(!fills_terminal_cell_edges(glyph), "glyph={glyph}");
        }
    }

    #[test]
    fn reverse_without_explicit_background_uses_canvas_color() {
        let foreground = [0.8, 0.7, 0.6, 1.0];
        let canvas = [0.1, 0.2, 0.3, 1.0];
        let (reversed_fg, reversed_bg) = reverse_colors(foreground, None, canvas);
        assert_eq!(reversed_fg, canvas);
        assert_eq!(reversed_bg, Some(foreground));
    }

    #[test]
    fn selection_background_is_used_unless_reverse_provides_an_explicit_background() {
        let canvas = [0.1, 0.2, 0.3, 1.0];
        let selected = [0.4, 0.5, 0.6, 1.0];
        let explicit = [0.7, 0.3, 0.2, 1.0];
        let selection = Some((2, 5, selected));
        assert_eq!(contrast_background(None, canvas, selection, 3), selected);
        assert_eq!(
            contrast_background(Some(explicit), canvas, selection, 3),
            explicit
        );
        assert_eq!(contrast_background(None, canvas, selection, 7), canvas);
    }

    #[test]
    fn semantic_toggle_colors_unstyled_status_without_overriding_ansi() {
        let theme = Theme::weft_warm();
        let fallback = color_to_normalized(theme.output.output_default);
        let spans = weft_core::blocks::classify_line("Status: ok", &[]);
        let success = color_to_normalized(theme.output.success);

        assert_eq!(
            semantic_fallback_at(&theme, true, &spans, 8, false, fallback),
            success
        );
        assert_eq!(
            semantic_fallback_at(&theme, false, &spans, 8, false, fallback),
            fallback
        );
        assert_eq!(
            semantic_fallback_at(&theme, true, &spans, 8, true, fallback),
            fallback
        );
    }
}
