//! v1.4.2 Phase B2: Dual-stream grid instance collector (pure logic).
//!
//! Splits the per-cell grid rendering into two streams:
//! - **Background stream**: 8-float instances per run of same-bg cells.
//!   Adjacent cells with identical resolved background colors are merged
//!   into a single background quad, drastically reducing the instance
//!   count for typical terminal rows (often 80+ cells of default bg).
//! - **Glyph stream**: 16-float instances per content cell, matching the
//!   existing `push_cell_instance` layout. Emitted only for cells with
//!   visible text content or decoration (cursor bar/underline, hyperlink
//!   underline), skipping empty/space cells whose background is already
//!   covered by the bg stream.
//!
//! This module is **pure logic** — no `MetalRenderer` dependency. All
//! renderer state (theme colors, atlas, opacity, layout) is passed in as
//! parameters so the collector can be unit-tested in isolation and the
//! per-row cache can be rebuilt by any caller. Glyph UV resolution is
//! deferred to serialization time ([`GridInstanceBatch::push_row`]) so
//! the builder itself does not depend on the glyph atlas.

use crate::paint::primitives::{push_cell_instance, resolve_cell_color};
use weft_core::grid::{CellColor, CellFlags, CellWidth, Color, Cursor, CursorStyle, Grid};
use weft_core::selection::SelectionHandler;

#[cfg(test)]
mod contrast_tests;
#[cfg(test)]
mod startup_replay_tests;
mod style_tests;

// ── Data structures ───────────────────────────────────────────────────

/// Background-stream instance: one per run of same-bg cells.
///
/// 8 floats: `origin(2) + size(2) + bg(4)`. Serialized via [`Self::push`]
/// into a flat `Vec<f32>` for GPU upload. The bg stream is drawn first
/// (no texture sampling, just solid color quads), then the glyph stream
/// is drawn on top with atlas sampling.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct BgInstance {
    pub x0: f32,
    pub y0: f32,
    pub w: f32,
    pub h: f32,
    pub bg: [f32; 4],
}

impl BgInstance {
    /// Push 8 floats into the flat bg instance buffer.
    #[inline]
    pub fn push(&self, out: &mut Vec<f32>) {
        out.extend_from_slice(&[
            self.x0, self.y0, self.w, self.h, self.bg[0], self.bg[1], self.bg[2], self.bg[3],
        ]);
    }
}

/// UV resolver passed to [`GridInstanceBatch::push_row`]. Returns the
/// glyph's UV rect plus whether the glyph lives in the RGBA color atlas
/// (v1.10.4 color emoji → fg.a=2.0 sentinel in the emitted instance).
/// The `'a` lifetime lets callers pass closures borrowing the renderer.
pub(crate) type ResolveUv<'a> =
    dyn Fn(char, Option<&str>, crate::glyph::GlyphStyle) -> ([f32; 4], bool) + 'a;

/// A glyph instance before UV resolution. The caller resolves UVs from
/// the glyph atlas during serialization ([`GridInstanceBatch::push_row`]).
///
/// - `Text`: an atlas-sampled glyph. UV is resolved from `ch` (single scalar)
///   or from `cluster` (multi-scalar grapheme, v1.6.0) when present; `fg` is
///   the text color; bg is transparent (`[0; 4]`) because the bg stream
///   already painted the cell background.
/// - `Decoration`: a solid-color rectangle (cursor bar, cursor underline,
///   hyperlink underline). UV = `[0, 0, 0, 1]` (mask = 0 → only bg shows);
///   `color` is the visible decoration color.
///
/// v1.6.0: `Text` now carries an optional `cluster: Arc<str>` for multi-scalar
/// graphemes (combining marks, ZWJ emoji, regional flags). When `Some`, the
/// UV resolver should call `atlas.get_or_rasterize_cluster(&cluster)` instead
/// of `atlas.get(ch)`. This forced removing `Copy` (Arc is not Copy); callers
/// use `Clone` where needed.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum GlyphInstance {
    Text {
        dst: [f32; 4],
        ch: char,
        fg: [f32; 4],
        /// v1.6.0: full cluster string when `CellFlags::EXTRA` is set.
        /// `None` for single-scalar cells (the common case).
        cluster: Option<std::sync::Arc<str>>,
        /// v1.10.12: SGR bold/italic style variant (drives the atlas face).
        style: crate::glyph::GlyphStyle,
    },
    Decoration {
        dst: [f32; 4],
        color: [f32; 4],
    },
}

/// Per-row dual-stream instance collection. Built by [`build_row_instances`]
/// and cached in the renderer's per-row cache (`grid_row_cache`). When the
/// row is dirty, the cache entry is rebuilt; otherwise it's reused.
#[derive(Clone, Debug, Default)]
pub(crate) struct GridRowInstances {
    /// Background instances for this row (typically 1-5 runs).
    pub bg_instances: Vec<BgInstance>,
    /// Glyph instances for this row (text + decoration).
    pub glyph_instances: Vec<GlyphInstance>,
}

/// Per-pane ranges into the flat dual-stream buffers. Records where each
/// pane's instances begin and end so the Metal backend can issue per-pane
/// draw calls with scissor rects (v1.3 multi-pane support).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PaneInstanceRanges {
    /// `[start, end)` float offsets into the bg stream.
    pub bg_range: (usize, usize),
    /// `[start, end)` float offsets into the glyph stream.
    pub glyph_range: (usize, usize),
}

/// Full batch of grid instances for a single frame, split into two streams.
///
/// Built by flattening per-row caches (or direct collection for background
/// panes). The Metal backend uploads each stream to its own ring buffer
/// and issues two draw calls: bg stream (solid color pipeline) then glyph
/// stream (atlas sampling pipeline).
///
/// Per-pane ranges are tracked separately on `MetalRenderer::pane_instance_ranges`
/// (paired with pane rects for scissor draws) rather than on this struct,
/// because the renderer pairs each range with its pane rect at push time.
#[derive(Clone, Debug, Default)]
pub(crate) struct GridInstanceBatch {
    /// Flat bg instance buffer: 8 floats per run.
    pub bg_stream: Vec<f32>,
    /// Flat glyph instance buffer: 16 floats per content/decoration cell.
    pub glyph_stream: Vec<f32>,
}

impl GridInstanceBatch {
    /// Allocate with capacity hints based on grid dimensions. Estimates:
    /// - bg: ~4 runs/row × 8 floats × num_rows
    /// - glyph: ~num_cols/2 cells × 16 floats × num_rows (rough)
    pub(crate) fn with_capacity(num_rows: usize, num_cols: usize) -> Self {
        let bg_cap = num_rows.saturating_mul(4 * 8);
        let glyph_cap = num_rows.saturating_mul(num_cols / 2 + 1).saturating_mul(16);
        Self {
            bg_stream: Vec::with_capacity(bg_cap),
            glyph_stream: Vec::with_capacity(glyph_cap),
        }
    }

    /// Extend both streams with a row's instances, resolving glyph UVs via
    /// `resolve_uv`. Returns the float offsets occupied by this row.
    ///
    /// The caller passes a UV resolver closure that wraps the glyph atlas.
    /// v1.6.0: the closure now receives `(ch, cluster)` so multi-scalar
    /// graphemes can be resolved via `atlas.get_or_rasterize_cluster`.
    /// When `cluster` is `Some`, the closure should use the cluster path;
    /// otherwise it falls back to `atlas.get(ch)`.
    ///
    /// v1.10.4: the closure returns `(uv, is_color)`. `is_color` marks
    /// glyphs stored in the RGBA color atlas (color emoji); their `fg` is
    /// replaced by an alpha=2.0 sentinel so the fragment shader can route
    /// them to the color texture. Normal glyphs keep their own `fg`.
    pub(crate) fn push_row(
        &mut self,
        row: &GridRowInstances,
        resolve_uv: &ResolveUv<'_>,
    ) -> PaneInstanceRanges {
        let bg_start = self.bg_stream.len();
        for bi in &row.bg_instances {
            bi.push(&mut self.bg_stream);
        }
        let bg_end = self.bg_stream.len();

        let glyph_start = self.glyph_stream.len();
        for gi in &row.glyph_instances {
            match gi {
                GlyphInstance::Text {
                    dst,
                    ch,
                    fg,
                    cluster,
                    style,
                } => {
                    let (uv, is_color) = resolve_uv(*ch, cluster.as_deref(), *style);
                    // fg.a = 2.0 sentinel (normal fg alpha ≤ 1.0): the shader
                    // detects color-atlas glyphs via `fg.a > 1.5` and samples
                    // color_atlas instead of tinting the mask with fg.
                    let fg = if is_color { [0.0, 0.0, 0.0, 2.0] } else { *fg };
                    push_cell_instance(&mut self.glyph_stream, *dst, uv, fg, [0.0; 4]);
                }
                GlyphInstance::Decoration { dst, color } => {
                    // UV [0,0,0,1] → mask=0 → only bg (color) shows.
                    push_cell_instance(
                        &mut self.glyph_stream,
                        *dst,
                        [0.0, 0.0, 0.0, 1.0],
                        [0.0; 4],
                        *color,
                    );
                }
            }
        }
        let glyph_end = self.glyph_stream.len();

        PaneInstanceRanges {
            bg_range: (bg_start, bg_end),
            glyph_range: (glyph_start, glyph_end),
        }
    }
}

// ── Row instance builder ──────────────────────────────────────────────

/// Soft cyan for OSC 8 hyperlink underlines (matches the existing
/// single-stream renderer color).
const HYPERLINK_COLOR: [f32; 4] = [0.36, 0.62, 0.94, 1.0];

/// Thickness of hyperlink and cursor-underline decorations, in physical px.
const UNDERLINE_HEIGHT: f32 = 2.0;

/// v1.10.4: Whether a character is a terminal *graphic* glyph (Box Drawing
/// U+2500-U+257F, Block Elements U+2580-U+259F) rather than readable text.
/// These are drawn as borders, bars, and gauges by TUIs (opencode input-box
/// edge, htop table rules, ollama progress bars). The app controls their
/// exact color — often intentionally low-contrast — so the minimum-contrast
/// booster must not brighten them. Mirrors block_view's
/// `fills_terminal_cell_edges` but is shared with the grid path.
fn is_terminal_graphic_char(ch: char) -> bool {
    matches!(ch, '\u{2500}'..='\u{259f}')
}

/// Build the dual-stream instances for a single grid row.
///
/// This is the pure-logic core of the grid renderer. For each cell in the
/// row it:
/// 1. Resolves the cell's fg/bg colors against the palette and theme.
/// 2. Applies REVERSE (swap fg/bg), opacity (scale bg alpha).
/// 3. Determines the final bg (cursor block / selection / normal) and fg.
/// 4. Merges adjacent cells with identical `final_bg` into background runs.
/// 5. Emits glyph instances for cells with visible content or decoration.
///
/// **Run merging**: a run breaks when `final_bg` changes (different cell
/// bg, cursor block, or selection). Wide cells (CJK) span 2 columns within
/// a run. The default background is still emitted as an explicit run (not
/// skipped) so transparent windows paint a base layer.
///
/// **Glyph emission**: a `Text` glyph is emitted when the cell has a
/// visible character (not space/NUL, not HIDDEN). `Decoration` glyphs are
/// emitted for cursor bar/underline and hyperlink underlines. Space cells
/// with no decoration skip the glyph stream (their bg is covered by the
/// bg stream).
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_row_instances(
    grid: &Grid,
    palette: &[Color; 256],
    row: usize,
    default_fg: [f32; 4],
    default_bg: [f32; 4],
    cursor_color: [f32; 4],
    selection_bg: [f32; 4],
    // v1.10.22: painted selection (quad composited onto theme bg) — the
    // text-contrast benchmark for selected cells.
    selection_painted: [f32; 4],
    cursor: &Cursor,
    cursor_style: CursorStyle,
    show_cursor: bool,
    selection: &SelectionHandler,
    opacity: f32,
    minimum_contrast: f32,
    cw: f32,
    ch: f32,
    origin_x: f32,
    origin_y: f32,
) -> GridRowInstances {
    let num_cols = grid.num_cols;
    let mut result = GridRowInstances::default();

    let y0 = origin_y;
    let y1 = origin_y + ch;

    // Run-merge state for the background stream.
    let mut run_active = false;
    let mut run_x0: f32 = 0.0;
    let mut run_x1: f32 = 0.0;
    let mut run_bg: [f32; 4] = default_bg;

    for col in 0..num_cols {
        let cell = grid.cell(row, col);

        // Skip wide-char spacers — rendered as part of the preceding wide cell.
        if cell.flags.contains(CellFlags::WIDE_SPACER) {
            continue;
        }

        // ── Resolve cell colors ────────────────────────────────────
        let mut fg = resolve_cell_color(cell.fg, default_fg, palette);
        let mut bg = resolve_cell_color(cell.bg, default_bg, palette);

        // SGR reverse video: swap fg/bg before cursor/selection overrides
        // so the swap applies to the cell's own colors.
        if cell.flags.contains(CellFlags::REVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }

        // Scale plain background alpha by window opacity so empty cells
        // show the desktop through them. Cursor/selection colors are
        // fully opaque (they override bg below).
        bg[3] *= opacity;

        // ── Determine cell state ───────────────────────────────────
        let is_cursor = show_cursor && row == cursor.row && col == cursor.col;
        let is_selected = selection
            .selection
            .as_ref()
            .is_some_and(|sel| sel.contains(row, col));

        // ── Final bg (precedence: cursor block > selection > normal) ─
        let final_bg = if is_cursor && cursor_style.is_block() {
            cursor_color
        } else if is_selected {
            selection_bg
        } else {
            bg
        };

        // ── Final fg ───────────────────────────────────────────────
        // Cursor block: black text on cursor color. Cursor bar/underline:
        // cursor-colored text (matches existing single-stream behavior).
        let explicit_terminal_color =
            cell.fg != CellColor::Default || cell.flags.contains(CellFlags::REVERSE);
        let final_fg = if is_cursor {
            if cursor_style.is_block() {
                [0.0, 0.0, 0.0, 1.0]
            } else {
                cursor_color
            }
        } else {
            // v1.10.22: benchmark vs painted — the GPU composites the α=0.6
            // quad before display, so the raw quad under-measured contrast.
            let benchmark = if is_selected {
                selection_painted
            } else {
                final_bg
            };
            crate::paint::primitives::ensure_minimum_text_contrast(fg, benchmark, minimum_contrast)
        };

        // v1.10.4 fix: skip the minimum-contrast correction for terminal
        // graphic characters (Box Drawing U+2500-U+257F and Block Elements
        // U+2580-U+259F). TUIs like opencode intentionally draw borders with
        // low-contrast dark glyphs (e.g. ▀ fg=rgb(21,20,27) on
        // bg=rgb(15,15,15) — a subtle input-box edge). The contrast booster
        // read that as unreadable text and brightened it toward white,
        // turning the intended dim border into a glaring "white bar".
        // Graphic glyphs and explicit SGR/ANSI foregrounds are presentation
        // owned by the terminal application. Preserve them exactly so its
        // text hierarchy and subtle decorations survive; default/theme text
        // still receives Weft's configured readability correction.
        let final_fg = if !is_cursor
            && (is_terminal_graphic_char(cell.character) || explicit_terminal_color)
        {
            fg
        } else {
            final_fg
        };

        // SGR DIM (faint) is an application-owned hierarchy signal
        // (opencode/vim use it to dim secondary labels, model pickers,
        // borders). Applied AFTER the optional contrast correction, the
        // same ordering as block_view/style.rs:286-297, so alt-screen
        // TUIs match the shell-output path. Not applied to cursor text:
        // the cursor already pins black-on-cursor-color and dimming it
        // would hurt visibility.
        let final_fg = if !is_cursor && cell.flags.contains(CellFlags::DIM) {
            [
                final_fg[0] * 0.5,
                final_fg[1] * 0.5,
                final_fg[2] * 0.5,
                final_fg[3],
            ]
        } else {
            final_fg
        };

        // ── Cell render width ──────────────────────────────────────
        let cell_w = if cell.width == CellWidth::Full && col + 1 < num_cols {
            cw * 2.0
        } else {
            cw
        };
        let x0 = origin_x + col as f32 * cw;
        let x1 = x0 + cell_w;

        // ── Background run merge ───────────────────────────────────
        // Extend the current run when bg matches and the cell is
        // contiguous (x0 ≈ run_x1); otherwise flush and start a new run.
        if !run_active {
            run_active = true;
            run_x0 = x0;
            run_x1 = x1;
            run_bg = final_bg;
        } else if final_bg == run_bg && (x0 - run_x1).abs() < 0.01 {
            run_x1 = x1;
        } else {
            if run_x1 > run_x0 {
                result.bg_instances.push(BgInstance {
                    x0: run_x0,
                    y0,
                    w: run_x1 - run_x0,
                    h: ch,
                    bg: run_bg,
                });
            }
            run_x0 = x0;
            run_x1 = x1;
            run_bg = final_bg;
        }

        // ── Glyph stream: text ─────────────────────────────────────
        let has_visible_text = !cell.flags.contains(CellFlags::HIDDEN)
            && cell.character != ' '
            && cell.character != '\0';

        if has_visible_text {
            // v1.6.0: when CellFlags::EXTRA is set, look up the full
            // multi-scalar grapheme cluster from RowExtras so the renderer
            // can rasterize it via the cluster atlas path. Falls back to
            // None for single-scalar cells (the common case — no alloc).
            // v1.6.0 review M1: use grapheme_arc_at to clone the existing
            // Arc<str> (refcount bump) instead of Arc::from(&str) (alloc +
            // copy) on every frame for every EXTRA cell.
            let cluster: Option<std::sync::Arc<str>> = if cell.flags.contains(CellFlags::EXTRA) {
                grid.grapheme_arc_at(row, col)
            } else {
                None
            };
            result.glyph_instances.push(GlyphInstance::Text {
                dst: [x0, y0, x1, y1],
                ch: cell.character,
                fg: final_fg,
                cluster,
                // v1.10.12: bold/italic SGR styles select the atlas face.
                style: crate::glyph::GlyphStyle::from_flags(cell.flags),
            });
        }

        // ── Glyph stream: cursor bar/underline decoration ──────────
        if is_cursor && show_cursor {
            if cursor_style.is_bar() {
                let bar_w = (2.0_f32).max(1.0).min(cw * 0.15);
                result.glyph_instances.push(GlyphInstance::Decoration {
                    dst: [x0, y0, x0 + bar_w, y1],
                    color: cursor_color,
                });
            } else if cursor_style.is_underline() {
                result.glyph_instances.push(GlyphInstance::Decoration {
                    dst: [x0, y1 - UNDERLINE_HEIGHT, x1, y1],
                    color: cursor_color,
                });
            }
        }

        // ── Glyph stream: hyperlink underline ──────────────────────
        if cell.flags.contains(CellFlags::HYPERLINK) {
            result.glyph_instances.push(GlyphInstance::Decoration {
                dst: [x0, y1 - UNDERLINE_HEIGHT, x1, y1],
                color: HYPERLINK_COLOR,
            });
        }

        // ── Glyph stream: SGR text attribute decorations ──────────
        // v1.10.4: honor UNDERLINE / DOUBLE_UNDER / STRIKETHROUGH on the
        // alt-screen (grid) path, mirroring block_view/style.rs:331-360 so
        // TUIs (less/man/vim/opencode) render text decorations identically
        // to shell output. Colors follow final_fg so DIM'd decorations stay
        // visually grouped with their text.
        if cell.flags.contains(CellFlags::UNDERLINE) || cell.flags.contains(CellFlags::DOUBLE_UNDER)
        {
            let underline_y = y1 - UNDERLINE_HEIGHT;
            result.glyph_instances.push(GlyphInstance::Decoration {
                dst: [x0, underline_y, x1, underline_y + UNDERLINE_HEIGHT],
                color: final_fg,
            });
            if cell.flags.contains(CellFlags::DOUBLE_UNDER) {
                let second_y = underline_y - 3.0;
                result.glyph_instances.push(GlyphInstance::Decoration {
                    dst: [x0, second_y, x1, second_y + UNDERLINE_HEIGHT],
                    color: final_fg,
                });
            }
        }
        if cell.flags.contains(CellFlags::STRIKETHROUGH) {
            let strike_y = y0 + ch * 0.5 - 1.0;
            result.glyph_instances.push(GlyphInstance::Decoration {
                dst: [x0, strike_y, x1, strike_y + UNDERLINE_HEIGHT],
                color: final_fg,
            });
        }
    }

    // Flush the final run.
    if run_active && run_x1 > run_x0 {
        result.bg_instances.push(BgInstance {
            x0: run_x0,
            y0,
            w: run_x1 - run_x0,
            h: ch,
            bg: run_bg,
        });
    }

    result
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paint::primitives::color_to_normalized;
    use weft_core::grid::{Cell, CellColor, CellFlags, Color, Cursor, CursorStyle, Grid};
    use weft_core::selection::{GridPos, Selection, SelectionHandler, SelectionMode};

    // Test constants.
    const FG: [f32; 4] = [0.8, 0.8, 0.8, 1.0];
    const BG: [f32; 4] = [0.1, 0.1, 0.2, 1.0];
    const CURSOR: [f32; 4] = [1.0, 1.0, 1.0, 1.0];
    const SELECTION: [f32; 4] = [0.3, 0.5, 0.7, 0.6];
    // v1.10.22: painted selection = SELECTION composited onto BG.
    const SELECTION_PAINTED: [f32; 4] = [0.22, 0.34, 0.50, 1.0];
    const CW: f32 = 10.0;
    const CH: f32 = 20.0;

    // v1.10.4: the color-emoji sentinel is fg.a = 2.0; every real fg alpha
    // source (palette u8/255, contrast boost, REVERSE swap, DIM, opacity)
    // stays ≤ 1.0, so `fg.a > 1.5` in the shader is unambiguous. Assert the
    // test constants here at compile time.
    const _: () = {
        assert!(FG[3] <= 1.0 && BG[3] <= 1.0 && CURSOR[3] <= 1.0 && SELECTION[3] <= 1.0);
    };

    /// Build instances for row 0 of a 1-row grid.
    fn build(
        grid: &Grid,
        cursor: &Cursor,
        style: CursorStyle,
        show: bool,
        sel: &SelectionHandler,
        opacity: f32,
    ) -> GridRowInstances {
        let palette = Color::standard_palette();
        build_row_instances(
            grid,
            &palette,
            0,
            FG,
            BG,
            CURSOR,
            SELECTION,
            SELECTION_PAINTED,
            cursor,
            style,
            show,
            sel,
            opacity,
            1.0,
            CW,
            CH,
            0.0,
            0.0,
        )
    }

    /// Build with default cursor (hidden) and no selection.
    fn build_plain(grid: &Grid) -> GridRowInstances {
        let cursor = Cursor::default();
        let sel = SelectionHandler::new();
        build(grid, &cursor, CursorStyle::Block, false, &sel, 1.0)
    }

    fn make_grid(cells: &[Cell]) -> Grid {
        let cols = cells.len().max(1);
        let mut grid = Grid::new(1, cols);
        for (i, cell) in cells.iter().enumerate() {
            grid.viewport[0].cells[i] = cell.clone();
        }
        grid
    }

    // ── Test 1: empty row ──────────────────────────────────────────

    #[test]
    fn empty_row_emits_single_default_bg_run_no_glyphs() {
        let grid = Grid::new(1, 5);
        let result = build_plain(&grid);

        // One bg run covering the full row, no glyphs.
        assert_eq!(result.bg_instances.len(), 1);
        assert_eq!(result.bg_instances[0].x0, 0.0);
        assert_eq!(result.bg_instances[0].w, 50.0); // 5 cols * 10 px
        assert_eq!(result.bg_instances[0].bg, BG);
        assert_eq!(result.glyph_instances.len(), 0);
    }

    // ── Test 2: single text cell ───────────────────────────────────

    #[test]
    fn single_text_cell_emits_bg_run_and_text_glyph() {
        let grid = make_grid(&[Cell::with_char('A')]);
        let result = build_plain(&grid);

        assert_eq!(result.bg_instances.len(), 1);
        assert_eq!(result.glyph_instances.len(), 1);
        match &result.glyph_instances[0] {
            GlyphInstance::Text {
                dst,
                ch,
                fg,
                cluster: _,
                style: _,
            } => {
                assert_eq!(*ch, 'A');
                assert_eq!(*fg, FG);
                assert_eq!(*dst, [0.0, 0.0, CW, CH]);
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    // ── Test 3: wide char spans two columns ────────────────────────

    #[test]
    fn wide_char_cell_spans_two_columns_in_bg_run() {
        // '中' is a wide char → width = Full, spans 2 cols.
        // Need a 2-col grid with a WIDE_SPACER at col 1 (as the VT parser
        // would set up) for the wide char to actually span 2 columns.
        let mut grid = Grid::new(1, 2);
        grid.viewport[0].cells[0] = Cell::with_char('中');
        grid.viewport[0].cells[1].flags = CellFlags::WIDE_SPACER;
        let result = build_plain(&grid);

        assert_eq!(result.bg_instances.len(), 1);
        assert_eq!(result.bg_instances[0].w, CW * 2.0); // 2 cols
        assert_eq!(result.glyph_instances.len(), 1);
        match &result.glyph_instances[0] {
            GlyphInstance::Text { dst, ch, .. } => {
                assert_eq!(*ch, '中');
                assert_eq!(dst[2] - dst[0], CW * 2.0); // width = 2 cells
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    // ── Test 4: wide spacer is skipped ─────────────────────────────

    #[test]
    fn wide_spacer_cell_is_skipped_entirely() {
        // A standalone WIDE_SPACER (edge case) produces nothing.
        let spacer = Cell {
            flags: CellFlags::WIDE_SPACER,
            ..Cell::default()
        };
        let grid = make_grid(&[spacer]);
        let result = build_plain(&grid);

        assert_eq!(result.bg_instances.len(), 0);
        assert_eq!(result.glyph_instances.len(), 0);
    }

    // ── Test 5: consecutive same-bg cells merge ────────────────────

    /// v1.10.26 Batch B (FIX_WRAP_EPOCH_AND_VIEWPORT_KEEP): the viewport may
    /// hold a row wider than `num_cols` right after a narrowing resize (rows
    /// only grow). The renderer must stay bounded by `num_cols` — the leftover
    /// right half is clipped, never drawn.
    #[test]
    fn wide_viewport_row_renders_exactly_num_cols() {
        let mut grid = Grid::new(1, 4);
        // Post-narrowing state: the row kept its original 8-cell width.
        grid.viewport[0].cells = (0..8u8)
            .map(|i| Cell::with_char(char::from(b'A' + i)))
            .collect();
        let result = build_plain(&grid);

        assert_eq!(
            result.glyph_instances.len(),
            4,
            "only the first num_cols glyphs render; E..H are outside the window"
        );
        // Background run spans exactly num_cols * cw.
        assert_eq!(result.bg_instances.len(), 1);
        assert_eq!(result.bg_instances[0].w, 4.0 * CW);
    }

    #[test]
    fn consecutive_same_bg_cells_merge_into_one_run() {
        let mut cells: Vec<Cell> = "hello".chars().map(Cell::with_char).collect();
        // All default bg → should merge into one run.
        for c in &mut cells {
            c.bg = CellColor::Default;
        }
        let grid = make_grid(&cells);
        let result = build_plain(&grid);

        assert_eq!(result.bg_instances.len(), 1);
        assert_eq!(result.bg_instances[0].w, 50.0); // 5 * 10
        assert_eq!(result.glyph_instances.len(), 5); // 5 text glyphs
    }

    // ── Test 6: different bg colors break the run ──────────────────

    #[test]
    fn different_bg_colors_break_into_separate_runs() {
        let red = CellColor::Rgb(Color::rgb(255, 0, 0));
        let green = CellColor::Rgb(Color::rgb(0, 255, 0));

        let mut cell_a = Cell::with_char('A');
        cell_a.bg = red;
        let mut cell_b = Cell::with_char('B');
        cell_b.bg = green;

        let grid = make_grid(&[cell_a, cell_b]);
        let result = build_plain(&grid);

        assert_eq!(result.bg_instances.len(), 2);
        // First run: red bg, width = CW
        assert_eq!(result.bg_instances[0].w, CW);
        assert_eq!(result.bg_instances[0].bg, [1.0, 0.0, 0.0, 1.0]);
        // Second run: green bg, width = CW
        assert_eq!(result.bg_instances[1].w, CW);
        assert_eq!(result.bg_instances[1].bg, [0.0, 1.0, 0.0, 1.0]);
    }

    // ── Test 7: reverse video swaps fg and bg ──────────────────────

    #[test]
    fn reverse_video_swaps_fg_and_bg() {
        let mut cell = Cell::with_char('A');
        cell.fg = CellColor::Rgb(Color::rgb(255, 0, 0)); // red fg
        cell.bg = CellColor::Rgb(Color::rgb(0, 0, 255)); // blue bg
        cell.flags = CellFlags::REVERSE;

        let grid = make_grid(&[cell]);
        let result = build_plain(&grid);

        // After swap: fg = blue, bg = red.
        assert_eq!(result.bg_instances[0].bg, [1.0, 0.0, 0.0, 1.0]); // red bg
        match &result.glyph_instances[0] {
            GlyphInstance::Text { fg, .. } => {
                assert_eq!(*fg, [0.0, 0.0, 1.0, 1.0]); // blue fg
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    // ── Test 8: hidden cell emits bg but no glyph ──────────────────

    #[test]
    fn hidden_cell_emits_bg_but_no_glyph() {
        let mut cell = Cell::with_char('X');
        cell.flags = CellFlags::HIDDEN;
        let grid = make_grid(&[cell]);
        let result = build_plain(&grid);

        assert_eq!(result.bg_instances.len(), 1); // bg still painted
        assert_eq!(result.glyph_instances.len(), 0); // no text glyph
    }

    // ── Test 9: cursor block overrides bg, uses black fg ───────────

    #[test]
    fn cursor_block_overrides_bg_and_uses_black_fg() {
        let grid = make_grid(&[Cell::with_char('A')]);
        let cursor = Cursor {
            row: 0,
            col: 0,
            visible: true,
            wrap_pending: false,
        };
        let sel = SelectionHandler::new();
        let result = build(&grid, &cursor, CursorStyle::Block, true, &sel, 1.0);

        // bg = cursor_color (overrides default).
        assert_eq!(result.bg_instances[0].bg, CURSOR);
        // fg = black (text on cursor block).
        match &result.glyph_instances[0] {
            GlyphInstance::Text { fg, .. } => assert_eq!(*fg, [0.0, 0.0, 0.0, 1.0]),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    // ── Test 10: cursor bar emits decoration glyph ─────────────────

    #[test]
    fn cursor_bar_emits_decoration_glyph() {
        let grid = make_grid(&[Cell::with_char('A')]);
        let cursor = Cursor {
            row: 0,
            col: 0,
            visible: true,
            wrap_pending: false,
        };
        let sel = SelectionHandler::new();
        let result = build(&grid, &cursor, CursorStyle::Bar, true, &sel, 1.0);

        // Text glyph + bar decoration.
        assert_eq!(result.glyph_instances.len(), 2);
        let has_deco = result
            .glyph_instances
            .iter()
            .any(|g| matches!(g, GlyphInstance::Decoration { color, .. } if *color == CURSOR));
        assert!(has_deco, "expected a cursor-colored Decoration glyph");

        // bg should NOT be cursor_color (bar doesn't override bg).
        assert_eq!(result.bg_instances[0].bg, BG);
    }

    // ── Test 11: cursor underline emits decoration glyph ───────────

    #[test]
    fn cursor_underline_emits_decoration_glyph() {
        let grid = make_grid(&[Cell::with_char('A')]);
        let cursor = Cursor {
            row: 0,
            col: 0,
            visible: true,
            wrap_pending: false,
        };
        let sel = SelectionHandler::new();
        let result = build(&grid, &cursor, CursorStyle::Underline, true, &sel, 1.0);

        assert_eq!(result.glyph_instances.len(), 2);
        let has_deco = result.glyph_instances.iter().any(|g| {
            matches!(g, GlyphInstance::Decoration { dst, color }
                if *color == CURSOR && (dst[3] - dst[1]) == UNDERLINE_HEIGHT)
        });
        assert!(has_deco, "expected a cursor-colored underline Decoration");
    }

    // ── Test 12: selection overrides cell bg ───────────────────────

    #[test]
    fn selection_overrides_cell_bg() {
        let grid = make_grid(&[Cell::with_char('A')]);
        let cursor = Cursor::default();
        let mut sel = SelectionHandler::new();
        sel.selection = Some(Selection::new(
            GridPos::new(0, 0),
            GridPos::new(0, 0),
            SelectionMode::Simple,
        ));
        let result = build(&grid, &cursor, CursorStyle::Block, false, &sel, 1.0);

        assert_eq!(result.bg_instances[0].bg, SELECTION);
    }

    // ── Test 13: hyperlink emits underline decoration ──────────────

    #[test]
    fn hyperlink_cell_emits_underline_decoration() {
        let mut cell = Cell::with_char('A');
        cell.flags = CellFlags::HYPERLINK;
        let grid = make_grid(&[cell]);
        let result = build_plain(&grid);

        // Text glyph + hyperlink underline decoration.
        assert_eq!(result.glyph_instances.len(), 2);
        let has_link = result.glyph_instances.iter().any(|g| {
            matches!(g, GlyphInstance::Decoration { color, dst }
                if *color == HYPERLINK_COLOR && (dst[3] - dst[1]) == UNDERLINE_HEIGHT)
        });
        assert!(has_link, "expected a hyperlink underline Decoration");
    }

    // ── Test 14: opacity scales background alpha ───────────────────

    #[test]
    fn opacity_scales_background_alpha() {
        let grid = Grid::new(1, 1);
        let result = build_plain(&grid);
        // opacity = 1.0 → bg alpha unchanged.
        assert_eq!(result.bg_instances[0].bg[3], BG[3]);

        let cursor = Cursor::default();
        let sel = SelectionHandler::new();
        let result_half = build(&grid, &cursor, CursorStyle::Block, false, &sel, 0.5);
        // opacity = 0.5 → bg alpha halved.
        assert!((result_half.bg_instances[0].bg[3] - BG[3] * 0.5).abs() < 1e-6);
    }

    // ── Serialization smoke test ───────────────────────────────────

    #[test]
    fn batch_push_row_serializes_both_streams() {
        let grid = make_grid(&[Cell::with_char('A')]);
        let row = build_plain(&grid);

        let mut batch = GridInstanceBatch::default();
        let resolve_uv = |_ch: char, _cluster: Option<&str>, _style: crate::glyph::GlyphStyle| {
            ([0.1, 0.2, 0.3, 0.4], false)
        };
        let ranges = batch.push_row(&row, &resolve_uv);

        // Bg stream: 8 floats per run.
        assert_eq!(batch.bg_stream.len(), 8);
        assert_eq!(ranges.bg_range, (0, 8));

        // Glyph stream: 16 floats per glyph.
        assert_eq!(batch.glyph_stream.len(), 16);
        assert_eq!(ranges.glyph_range, (0, 16));
    }

    // ── v1.10.4: color-emoji fg sentinel ─────────────────────────────

    #[test]
    fn color_glyph_replaces_fg_with_alpha_2_sentinel() {
        let grid = make_grid(&[Cell::with_char('A')]);
        let row = build_plain(&grid);

        // Color-atlas glyph (emoji): fg must become the [0,0,0,2.0] sentinel
        // so the shader routes the quad to the RGBA color texture. Normal
        // fg alpha is ≤ 1.0, so 2.0 is unambiguous (checked below).
        let mut batch = GridInstanceBatch::default();
        let color_uv = |_ch: char, _cluster: Option<&str>, _style: crate::glyph::GlyphStyle| {
            ([0.1, 0.2, 0.3, 0.4], true)
        };
        batch.push_row(&row, &color_uv);
        // Layout: origin(2) size(2) uv(4) fg(4) bg(4) → fg = floats[8..12].
        assert_eq!(&batch.glyph_stream[8..12], &[0.0, 0.0, 0.0, 2.0]);
        // UV unchanged.
        assert_eq!(&batch.glyph_stream[4..8], &[0.1, 0.2, 0.3, 0.4]);

        // Mask-atlas glyph: fg passes through untouched.
        let mut batch = GridInstanceBatch::default();
        let mask_uv = |_ch: char, _cluster: Option<&str>, _style: crate::glyph::GlyphStyle| {
            ([0.1, 0.2, 0.3, 0.4], false)
        };
        batch.push_row(&row, &mask_uv);
        assert_eq!(&batch.glyph_stream[8..12], &FG[..]);
    }

    // ── Test 16: multi-pane ranges don't overlap ──────────────────

    #[test]
    fn multi_pane_ranges_are_sequential_and_non_overlapping() {
        // Two rows, each producing 1 bg run (8 floats) + 1 glyph (16 floats).
        let grid_a = make_grid(&[Cell::with_char('A')]);
        let grid_b = make_grid(&[Cell::with_char('B')]);
        let row_a = build_plain(&grid_a);
        let row_b = build_plain(&grid_b);

        let mut batch = GridInstanceBatch::default();
        let resolve_uv = |_ch: char, _cluster: Option<&str>, _style: crate::glyph::GlyphStyle| {
            ([0.1, 0.2, 0.3, 0.4], false)
        };
        let ranges_a = batch.push_row(&row_a, &resolve_uv);
        let ranges_b = batch.push_row(&row_b, &resolve_uv);

        // Pane A occupies floats [0, 8) in bg and [0, 16) in glyph.
        assert_eq!(ranges_a.bg_range, (0, 8));
        assert_eq!(ranges_a.glyph_range, (0, 16));

        // Pane B occupies floats [8, 16) in bg and [16, 32) in glyph.
        assert_eq!(ranges_b.bg_range, (8, 16));
        assert_eq!(ranges_b.glyph_range, (16, 32));

        // Total: 2 bg runs × 8 floats + 2 glyphs × 16 floats.
        assert_eq!(batch.bg_stream.len(), 16);
        assert_eq!(batch.glyph_stream.len(), 32);
    }

    // ── Test 17: pixel-equivalence harness ─────────────────────────
    //
    // v1.4.2 Phase B3 exit criterion: verify that dual-stream rendering
    // (bg runs + glyph instances with transparent bg) produces the same
    // pixels as single-stream rendering (per-cell instances with baked-in
    // bg). Since we can't rasterize in a unit test (no Metal device), we
    // verify the data-level equivalence properties:
    //
    // 1. **Coverage**: the union of all bg run x-ranges == [0, num_cols·cw).
    //    No gaps → no unpainted pixels.
    // 2. **Non-overlap**: bg runs don't overlap in x. No double-blending.
    // 3. **Glyph transparency**: all Text glyph instances have bg=[0;4].
    //    The bg stream is solely responsible for the background.
    // 4. **Glyph coverage**: every cell with visible text has a matching
    //    Text glyph at the same dst rect.
    // 5. **Run color consistency**: within a run, all covered cells share
    //    the same final_bg (cursor/selection/normal).
    //
    // Uses a complex row mixing: default bg, custom bg, text, empty cells,
    // cursor (bar style), and selection — exercising all merge-break paths.

    #[test]
    fn pixel_equivalence_dual_stream_matches_single_stream() {
        // Build a row: [empty | text 'A' | custom-bg 'B' | default | selected 'C']
        // Col indices:     0      1          2                3       4      5
        let mut grid = Grid::new(1, 6);
        grid.viewport[0].cells[1] = Cell::with_char('A');
        let mut cell_b = Cell::with_char('B');
        cell_b.bg = CellColor::Palette(1); // custom bg → breaks the run
        grid.viewport[0].cells[2] = cell_b;
        // col 3: default Cell (default bg) → breaks the custom-bg run
        grid.viewport[0].cells[4] = Cell::with_char('C');
        // col 5: empty

        // Cursor at col 1 (bar style → decoration glyph + cursor fg).
        let cursor = Cursor {
            row: 0,
            col: 1,
            visible: true,
            wrap_pending: false,
        };
        // Selection covering col 4 → selection bg override.
        let mut sel = SelectionHandler::new();
        sel.start(GridPos { row: 0, col: 4 }, SelectionMode::Simple);
        sel.end();

        let row_instances = build(&grid, &cursor, CursorStyle::Bar, true, &sel, 1.0);

        // ── Property 1: Coverage ───────────────────────────────────
        // The full row [0, 6·cw) must be covered by bg runs.
        let full_width = 6.0 * CW;
        let mut covered: Vec<(f32, f32)> = row_instances
            .bg_instances
            .iter()
            .map(|r| (r.x0, r.x0 + r.w))
            .collect();
        covered.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        assert!(!covered.is_empty(), "bg stream must have at least one run");
        assert!(
            (covered[0].0 - 0.0).abs() < 0.01,
            "first run must start at x=0, got {}",
            covered[0].0
        );
        let mut prev_end = covered[0].1;
        for &(start, end) in covered.iter().skip(1) {
            assert!(
                (start - prev_end).abs() < 0.01,
                "gap between runs: prev_end={}, next_start={}",
                prev_end,
                start
            );
            prev_end = end;
        }
        assert!(
            (prev_end - full_width).abs() < 0.01,
            "last run must end at {}, got {}",
            full_width,
            prev_end
        );

        // ── Property 2: Non-overlap ────────────────────────────────
        for i in 0..covered.len() {
            for j in (i + 1)..covered.len() {
                let (a0, a1) = covered[i];
                let (b0, b1) = covered[j];
                let overlap = a0 < b1 && b0 < a1;
                assert!(
                    !overlap,
                    "runs {} and {} overlap: [{},{}) vs [{},{})",
                    i, j, a0, a1, b0, b1
                );
            }
        }

        // ── Property 3: Glyph transparency ─────────────────────────
        // Text glyphs must have bg = [0;4] (the bg stream paints bg).
        // Verified via batch serialization: floats [12..16) are the bg field.
        let mut batch = GridInstanceBatch::default();
        let resolve_uv = |_ch: char, _cluster: Option<&str>, _style: crate::glyph::GlyphStyle| {
            ([0.5, 0.5, 0.6, 0.6], false)
        };
        batch.push_row(&row_instances, &resolve_uv);
        let gs = &batch.glyph_stream;
        for chunk in gs.chunks(16) {
            if chunk.len() == 16 {
                let bg_field = [chunk[12], chunk[13], chunk[14], chunk[15]];
                // Text glyphs (uv != [0,0,0,1]) must have transparent bg.
                let uv = [chunk[4], chunk[5], chunk[6], chunk[7]];
                if uv != [0.0, 0.0, 0.0, 1.0] {
                    assert_eq!(
                        bg_field,
                        [0.0, 0.0, 0.0, 0.0],
                        "Text glyph must have transparent bg (uv={:?})",
                        uv
                    );
                }
            }
        }

        // ── Property 4: Glyph coverage ────────────────────────────
        // Every cell with visible text (A, B, C) must have a matching Text
        // glyph at the correct dst rect.
        let text_cells = [(1usize, 'A'), (2, 'B'), (4, 'C')];
        for (col, ch) in text_cells {
            let x0 = col as f32 * CW;
            let x1 = x0 + CW;
            let found = row_instances.glyph_instances.iter().any(|gi| {
                if let GlyphInstance::Text { dst, ch: gc, .. } = gi {
                    let [dx0, _dy0, dx1, _dy1] = dst;
                    (dx0 - x0).abs() < 0.01 && (dx1 - x1).abs() < 0.01 && *gc == ch
                } else {
                    false
                }
            });
            assert!(
                found,
                "col {} char '{}' must have a Text glyph at [{},{})",
                col, ch, x0, x1
            );
        }

        // ── Property 5: Run color consistency ────────────────────
        // The cursor cell (col 1, bar style) uses cursor_color as fg (not bg),
        // so its bg run still uses the default bg. The selected cell (col 4)
        // uses SELECTION as its bg run color. Verify the run covering col 4
        // has the selection color.
        let sel_x0 = 4.0 * CW;
        let sel_run = row_instances
            .bg_instances
            .iter()
            .find(|r| r.x0 <= sel_x0 && r.x0 + r.w > sel_x0);
        assert!(
            sel_run.is_some(),
            "must have a bg run covering col 4 (selection)"
        );
        let sel_run = sel_run.unwrap();
        assert_eq!(
            sel_run.bg, SELECTION,
            "selection cell's bg run must use selection color"
        );

        // The custom-bg cell (col 2) must have a run with palette[1] color.
        let cb_x0 = 2.0 * CW;
        let cb_run = row_instances
            .bg_instances
            .iter()
            .find(|r| r.x0 <= cb_x0 && r.x0 + r.w > cb_x0);
        assert!(
            cb_run.is_some(),
            "must have a bg run covering col 2 (custom bg)"
        );
        let palette = Color::standard_palette();
        let expected_bg = color_to_normalized(palette[1]);
        let cb_run = cb_run.unwrap();
        assert_eq!(
            cb_run.bg, expected_bg,
            "custom-bg cell's bg run must use palette[1] color"
        );

        // ── Property 6: Cursor decoration present ───────────────────
        // Bar-style cursor at col 1 must emit a Decoration glyph.
        let cursor_decoration = row_instances.glyph_instances.iter().any(|gi| {
            if let GlyphInstance::Decoration { dst, color } = gi {
                let [dx0, _dy0, dx1, _dy1] = dst;
                (dx0 - 1.0 * CW).abs() < 0.01 && *color == CURSOR && dx1 - dx0 < CW
            } else {
                false
            }
        });
        assert!(
            cursor_decoration,
            "bar cursor at col 1 must emit a Decoration glyph"
        );
    }
}
