//! Text rasterization helpers for the Metal renderer.
//!
//! These remain `impl MetalRenderer` methods (A5 strategy b) because they
//! need `&self.atlas` (glyph UV lookup) and `&self.theme` (syntax colors).
//! Splitting them out of `renderer.rs` keeps the main file focused on GPU
//! pipeline + grid rendering; overlay builders in `paint/overlays.rs` call
//! these via the same `self.push_text(...)` syntax with zero call-site edits.

use crate::paint::primitives::push_quad;
use crate::renderer::MetalRenderer;
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;
use weft_core::syntax::TokenKind;

impl MetalRenderer {
    /// Column width of a character (0 for zero-width combining marks,
    /// 1 for ASCII and ambiguous terminal symbols, 2 for CJK full-width.
    pub(crate) fn char_col_width(c: char) -> usize {
        weft_core::grid::terminal_char_width(c)
    }

    /// Total column width of a string — sum of each char's display width.
    /// Use this instead of `chars().count()` whenever a width/position
    /// calculation must match what `push_text` actually renders (CJK chars
    /// occupy 2 columns each, not 1).
    pub(crate) fn text_col_width(s: &str) -> usize {
        weft_core::grid::terminal_text_width(s)
    }

    /// UV rect of the space glyph (background-only quads need mask 0).
    pub(crate) fn space_uv(&self) -> (f32, f32, f32, f32) {
        self.atlas
            .get(' ')
            .map(|g| {
                let (u, v) = g.uv_origin;
                let (uw, vh) = g.uv_size;
                (u, v, uw, vh)
            })
            .unwrap_or((0.0, 0.0, 0.0, 0.0))
    }

    /// Lay out a string left-to-right, honoring wide-character (CJK) widths.
    /// `max_cols` is a *column* budget (not a character count): a CJK char
    /// consumes 2 columns, ASCII consumes 1. Glyphs must already be in the
    /// atlas (warmed up by the caller). Text exceeding `max_cols` columns is
    /// truncated (callers that need wrapping use `push_text_wrapped`).
    pub(crate) fn push_text(
        &self,
        vertices: &mut Vec<f32>,
        x: f32,
        y: f32,
        text: &str,
        fg: [f32; 4],
        max_cols: usize,
    ) {
        self.push_text_with_height(
            vertices,
            [x, y],
            text,
            fg,
            max_cols,
            self.cell_height() as f32,
            crate::glyph::GlyphStyle::REGULAR,
        );
    }

    /// v1.10.12: `style` selects the atlas face (bold/italic) for the whole
    /// run. UI text (`push_text`) stays regular; block-view history cells
    /// pass their SGR flags-derived style.
    #[allow(clippy::too_many_arguments)] // run geometry + style; mirrors push_text's arity
    pub(crate) fn push_text_with_height(
        &self,
        vertices: &mut Vec<f32>,
        origin: [f32; 2],
        text: &str,
        fg: [f32; 4],
        max_cols: usize,
        glyph_height: f32,
        style: crate::glyph::GlyphStyle,
    ) {
        self.push_text_with_height_inner(
            vertices,
            origin,
            text,
            fg,
            max_cols,
            glyph_height,
            style,
            None,
        );
    }

    /// v1.14.2 (A1): `push_text` with a hard bottom clip line (physical px,
    /// = `content_bottom_y`). Glyph quads fully below the line are skipped;
    /// quads crossing it keep their upper part — bottom edge clamped to the
    /// line, atlas V interpolated proportionally (crop, never stretch).
    #[allow(clippy::too_many_arguments)] // mirrors push_text's arity + clip
    pub(crate) fn push_text_clipped(
        &self,
        vertices: &mut Vec<f32>,
        x: f32,
        y: f32,
        text: &str,
        fg: [f32; 4],
        max_cols: usize,
        clip_bottom: f32,
    ) {
        self.push_text_with_height_inner(
            vertices,
            [x, y],
            text,
            fg,
            max_cols,
            self.cell_height() as f32,
            crate::glyph::GlyphStyle::REGULAR,
            Some(clip_bottom),
        );
    }

    /// v1.14.2 (A1): [`Self::push_text_with_height`] with the same hard
    /// bottom clip — the block-view output path (SGR styles, variable glyph
    /// heights for box-drawing cells) rides it via `BlockOutputTextPaint`.
    #[allow(clippy::too_many_arguments)] // mirrors push_text_with_height + clip
    pub(crate) fn push_text_with_height_clipped(
        &self,
        vertices: &mut Vec<f32>,
        origin: [f32; 2],
        text: &str,
        fg: [f32; 4],
        max_cols: usize,
        glyph_height: f32,
        style: crate::glyph::GlyphStyle,
        clip_bottom: f32,
    ) {
        self.push_text_with_height_inner(
            vertices,
            origin,
            text,
            fg,
            max_cols,
            glyph_height,
            style,
            Some(clip_bottom),
        );
    }

    /// Shared emission body of [`Self::push_text_with_height`] and
    /// [`Self::push_text_clipped`]. `clip_bottom: None` = unclipped.
    #[allow(clippy::too_many_arguments)] // run geometry + style + clip; wraps the two public fns
    fn push_text_with_height_inner(
        &self,
        vertices: &mut Vec<f32>,
        origin: [f32; 2],
        text: &str,
        fg: [f32; 4],
        max_cols: usize,
        glyph_height: f32,
        style: crate::glyph::GlyphStyle,
        clip_bottom: Option<f32>,
    ) {
        let cw = self.cell_width() as f32;
        let [x, y] = origin;
        // v1.12.2 (PLAN_S2_render A3): snap text quad Y edges to integer
        // physical pixels (the whole coordinate chain is already physical —
        // viewport, padding, atlas rasterization). Fractional row origins
        // (pane splits, chrome offsets) otherwise sample the glyph atlas
        // with a subpixel offset → blurry text on Retina. X is deliberately
        // NOT snapped (plan scope: Y only); background/color-block quads
        // are snapped nowhere to avoid vertical seams.
        let y0 = y.floor();
        let y1 = (y + glyph_height).floor();
        let mut col = 0usize;
        let mut px = x;
        for grapheme in text.graphemes(true) {
            let w = weft_core::grid::terminal_text_width(grapheme);
            if w == 0 {
                continue;
            }
            if col + w > max_cols {
                break; // column budget exhausted
            }
            // v1.13.11 N2.1: shared ladder — multi-scalar graphemes try the
            // cluster atlas first (wide + color ℹ️/⚠️ like the live grid);
            // the FFFD/FF1F lead keeps the transient placeholder semantics
            // when the cluster isn't rasterized yet.
            let glyph = weft_core::grid::terminal_grapheme_glyph(grapheme);
            let Some((g, is_color)) = self.atlas.resolve_grapheme(grapheme, glyph, style) else {
                col += w;
                px += w as f32 * cw;
                continue;
            };
            let (u, v) = g.uv_origin;
            let (uw, vh) = g.uv_size;
            let cell_w = w as f32 * cw;
            // v1.14.2 (A1): per-quad bottom crop. The band and its V pairing
            // are identical for every glyph in this run — only u/uw differ.
            let (quad_y1, quad_v1) = match clip_bottom {
                None => (y1, v),
                Some(clip) => {
                    match crate::paint::block_view::clip::crop_bottom(y0, y1, v + vh, v, clip) {
                        crate::paint::block_view::clip::BottomCrop::Below => {
                            col += w;
                            px += cell_w;
                            continue;
                        }
                        crate::paint::block_view::clip::BottomCrop::Full => (y1, v),
                        crate::paint::block_view::clip::BottomCrop::Crop { y1, v1 } => (y1, v1),
                    }
                }
            };
            // v1.10.4: color emoji live in the RGBA color atlas — the shader
            // routes them there via the fg.a=2.0 sentinel (fg itself is
            // meaningless for color glyphs; the color comes from the atlas).
            let fg = if is_color { [0.0, 0.0, 0.0, 2.0] } else { fg };
            push_quad(
                vertices,
                [px, y0, px + cell_w, quad_y1],
                [u, v + vh, u + uw, quad_v1],
                fg,
                [0.0; 4],
            );
            col += w;
            px += cell_w;
        }
    }

    /// Draw one visual row of a prompt line from pre-tokenized spans.
    ///
    /// `line` is the full logical line; `char_start`/`char_end` are the visual
    /// row's char range into it (from PromptVisualRow). Spans come from
    /// `syntax::tokenize_spans(line, true)` — a whole-logical-line
    /// tokenization, so string/command state survives visual wrapping.
    /// `selection` is relative to this visual row (as before).
    #[allow(clippy::too_many_arguments)] // run geometry + syntax state; mirrors push_text_with_height's arity
    pub(crate) fn push_line_spans_on_canvas(
        &self,
        vertices: &mut Vec<f32>,
        origin: [f32; 2],
        line: &str,
        char_start: usize,
        char_end: usize,
        spans: &[(Range<usize>, TokenKind)],
        max_cols: usize,
        canvas: [f32; 4],
        selection: Option<(usize, usize, [f32; 4])>,
    ) {
        let cw = self.cell_width() as f32;
        let [x, y] = origin;
        // v1.12.2 (PLAN_S2_render A3): same Y snap as `push_text_with_height`
        // — this span path directly emits text quads (prompt/input canvas),
        // so it is a third text-quad outlet beside the two named choke
        // points; leaving it fractional would keep prompt text blurry.
        let y0 = y.floor();
        let y1 = (y + self.cell_height() as f32).floor();
        let mut col = 0usize;
        let mut px = x;
        let mut char_index; // relative to this visual row; set per segment below
        'span_loop: for (range, kind) in spans {
            let seg_start = range.start.max(char_start);
            let seg_end = range.end.min(char_end);
            if seg_start >= seg_end {
                continue;
            }
            // Character offset of this segment within the visual row.
            char_index = seg_start - char_start;
            let seg: String = line
                .chars()
                .skip(seg_start)
                .take(seg_end - seg_start)
                .collect();
            let source_color = crate::paint::primitives::syntax_color(*kind, &self.theme);
            for grapheme in seg.graphemes(true) {
                let grapheme_end = char_index + grapheme.chars().count();
                let background = crate::paint::primitives::text_background_for_range(
                    canvas,
                    selection,
                    char_index..grapheme_end,
                );
                // Note: unlike the grid/block-view paths, this call site is
                // NOT exempted for terminal graphic glyphs (box drawing /
                // block elements). It renders user-typed, syntax-highlighted
                // prompt/command text — readable content — not TUI border
                // cells, so the minimum-contrast boost always applies here.
                let color = crate::paint::primitives::ensure_minimum_text_contrast(
                    source_color,
                    background,
                    self.minimum_contrast,
                );
                let w = weft_core::grid::terminal_text_width(grapheme);
                if w == 0 {
                    char_index = grapheme_end;
                    continue;
                }
                if col + w > max_cols {
                    break 'span_loop;
                }
                // v1.13.11 N2.1: same shared ladder as `push_text_with_height`
                // (regular face only — this path renders user-typed prompt
                // text without SGR face styles).
                let glyph = weft_core::grid::terminal_grapheme_glyph(grapheme);
                if let Some((g, is_color)) =
                    self.atlas
                        .resolve_grapheme(grapheme, glyph, crate::glyph::GlyphStyle::REGULAR)
                {
                    let (u, v) = g.uv_origin;
                    let (uw, vh) = g.uv_size;
                    let cell_w = w as f32 * cw;
                    // v1.10.4: color emoji live in the RGBA color atlas —
                    // same fg.a=2.0 sentinel as `push_text_with_height`.
                    let color = if is_color {
                        [0.0, 0.0, 0.0, 2.0]
                    } else {
                        color
                    };
                    push_quad(
                        vertices,
                        [px, y0, px + cell_w, y1],
                        [u, v + vh, u + uw, v],
                        color,
                        [0.0; 4],
                    );
                    px += cell_w;
                }
                col += w;
                char_index = grapheme_end;
            }
        }
    }
}

// v1.14.2 (A1): the note-card / overlay-IME-anchor pure layout lives in the
// companion file above; re-exported so every existing
// `crate::paint::text::<item>` path (redraw_controller, overlays/note.rs)
// keeps resolving unchanged.
pub(crate) use note_layout::{
    find_ime_area, note_card_width, note_display_window, note_editor_ime_area, panel_ime_area,
    NOTE_CARD_ACCENT_STRIPE_W, NOTE_CARD_INNER_PAD, NOTE_CARD_LABEL, NOTE_CARD_LINE_H_CELLS,
    NOTE_CARD_LINE_H_MIN_PX, NOTE_CARD_TOP_OFFSET,
};

#[path = "text_anchors.rs"]
mod note_layout;

#[cfg(test)]
mod footer_glyph_probe {
    use super::*;

    // 取证探针 (2026-10-24)：Cmd+Shift+B 历史侧栏 footer 只显示孤字「更」。
    // 待证机制：footer 标签「加载更早」不在任何 warm_atlas 名单里，而
    // push_text 对图集缺失的字形静默跳过但保留步进 —— 于是可见字符集
    // 取决于会话内容碰巧预热过哪些字。两个测试把该契约钉死在顶点层。

    fn headless() -> Option<MetalRenderer> {
        if metal::Device::system_default().is_none() {
            eprintln!("skipping: no Metal device");
            return None;
        }
        Some(MetalRenderer::new_headless_paint(
            weft_core::config::Theme::weft_dark(),
        ))
    }

    /// push_quad = 6 顶点 × 12 float；每 quad 取第 0 顶点的 x0。
    fn quad_x0s(verts: &[f32]) -> Vec<f32> {
        verts
            .chunks(72)
            .filter(|q| q.len() == 72)
            .map(|q| q[0])
            .collect()
    }

    #[test]
    fn fresh_atlas_footer_label_emits_zero_quads() {
        let Some(renderer) = headless() else {
            return;
        };
        let label = crate::panel_component::FOOTER_LABEL;
        for ch in label.chars() {
            assert!(
                renderer.atlas.get(ch).is_none(),
                "{ch} unexpectedly present in a fresh atlas"
            );
        }
        let mut verts = Vec::new();
        renderer.push_text(&mut verts, 100.0, 50.0, label, [1.0; 4], 64);
        assert!(
            quad_x0s(&verts).is_empty(),
            "paint path is lookup-only: an unwarmed label must emit nothing"
        );
    }

    #[test]
    fn only_geng_warmed_emits_single_quad_at_third_slot() {
        let Some(mut renderer) = headless() else {
            return;
        };
        renderer
            .atlas
            .get_or_rasterize('更', crate::glyph::GlyphStyle::REGULAR)
            .expect("更 must rasterize via the CJK ladder");
        for ch in "加载早".chars() {
            assert!(
                renderer.atlas.get(ch).is_none(),
                "{ch} unexpectedly present; fixture requires only 更 warmed"
            );
        }
        let x0 = 100.0;
        let cw = renderer.cell_width() as f32;
        let mut verts = Vec::new();
        renderer.push_text(
            &mut verts,
            x0,
            50.0,
            crate::panel_component::FOOTER_LABEL,
            [1.0; 4],
            64,
        );
        let xs = quad_x0s(&verts);
        assert_eq!(
            xs.len(),
            1,
            "exactly the warmed glyph emits; got x0s={xs:?}"
        );
        let expected = x0 + 4.0 * cw; // 加(2列)+载(2列) 的步进保留在 更 之前
        assert!(
            (xs[0] - expected).abs() < 0.5,
            "surviving glyph keeps its true slot: {} vs {}",
            xs[0],
            expected
        );
    }

    /// v1.14.4 (批A A1)：四字全部预热 → 恰 4 个 quad，且各自落在真实
    /// 列槽位（x = x0 + {0,2,4,6}×cw，CJK 每字 2 列）。与上面两个测试
    /// 共同钉死预热契约：零预热零 quad / 部分预热部分 quad / 全预热全 quad。
    /// A1 落地后 warm_atlas 的 panel 分支会预热 FOOTER_LABEL 全部四字，
    /// 本测试即为该契约的顶点层等价物。
    #[test]
    fn fully_warmed_footer_label_emits_four_quads_at_true_slots() {
        let Some(mut renderer) = headless() else {
            return;
        };
        let label = crate::panel_component::FOOTER_LABEL;
        for ch in label.chars() {
            renderer
                .atlas
                .get_or_rasterize(ch, crate::glyph::GlyphStyle::REGULAR)
                .expect("footer label glyph must rasterize via the CJK ladder");
        }
        let x0 = 100.0;
        let cw = renderer.cell_width() as f32;
        let mut verts = Vec::new();
        renderer.push_text(&mut verts, x0, 50.0, label, [1.0; 4], 64);
        let xs = quad_x0s(&verts);
        assert_eq!(xs.len(), 4, "every warmed glyph emits; got x0s={xs:?}");
        for (slot, &x) in xs.iter().enumerate() {
            let expected = x0 + (2 * slot) as f32 * cw;
            assert!(
                (x - expected).abs() < 0.5,
                "slot {slot}: {} vs expected {}",
                x,
                expected
            );
        }
    }
}
