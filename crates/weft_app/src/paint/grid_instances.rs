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
use crate::paint::underline::underline_color;
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

// v1.11.6 (PLAN_v1116 M6/D-f): HYPERLINK_COLOR const removed — the color is
// now `Theme::link` ([0.36, 0.62, 0.94, 1.0] default), threaded in from the
// caller as `hyperlink_color` (build_row_instances is a free function).

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
    // v1.11.3 (§3.2): `[compat] bold_is_bright` — bold fg palette 0-7 → bright.
    bold_is_bright: bool,
    // v1.11.6 (M6/D-f): OSC 8 hyperlink underline color — `Theme::link`
    // from the caller (this is a free function, no &self.theme here).
    hyperlink_color: [f32; 4],
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
        // v1.11.3 (§3.2): bold→bright substitution at the fg origin (REVERSE
        // swaps resolved colors afterwards, xterm-style).
        let fg_origin =
            crate::paint::underline::bold_to_bright_origin(cell.fg, cell.flags, bold_is_bright);
        let mut fg = resolve_cell_color(fg_origin, default_fg, palette);
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
            crate::paint::color_math::dim_half(final_fg)
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
            // v1.12.2 (PLAN_S2_render A3): text glyphs snap Y edges to
            // integer physical pixels (fractional pane/chrome origins would
            // sample the atlas with a subpixel offset). Bg runs and
            // decorations keep the raw fractional Y (plan scope: text only).
            result.glyph_instances.push(GlyphInstance::Text {
                dst: [x0, y0.floor(), x1, y1.floor()],
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
        // v1.11.3 (§3.2 R7): SGR underline suppresses the link decoration.
        if cell.flags.contains(CellFlags::HYPERLINK)
            && !cell
                .flags
                .intersects(CellFlags::UNDERLINE | CellFlags::DOUBLE_UNDER)
        {
            result.glyph_instances.push(GlyphInstance::Decoration {
                dst: [x0, y1 - UNDERLINE_HEIGHT, x1, y1],
                color: hyperlink_color,
            });
        }

        if cell
            .flags
            .intersects(CellFlags::UNDERLINE | CellFlags::DOUBLE_UNDER)
        {
            // Bit beats style (§1.2); geometry + color via underline.rs.
            let style = if cell.flags.contains(CellFlags::DOUBLE_UNDER) {
                weft_core::grid::UnderlineStyle::Double
            } else {
                cell.underline_style
            };
            let (rects, n) = crate::paint::underline::underline_rects(
                style,
                x0,
                cell_w,
                y1 - UNDERLINE_HEIGHT,
                UNDERLINE_HEIGHT,
                cw,
                col,
            );
            debug_assert!(n <= crate::paint::underline::MAX_UNDERLINE_RECTS);
            let deco_color = underline_color(
                cell.underline_color,
                cell.flags,
                default_fg,
                palette,
                final_fg,
            );
            result.glyph_instances.extend(rects[..n].iter().map(|&dst| {
                GlyphInstance::Decoration {
                    dst,
                    color: deco_color,
                }
            }));
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

// Tests live in the gate-exempt sibling module (repo test-module
// convention, pty/tests.rs precedent) so inline test lines stay out of
// the production-file budget.
#[cfg(test)]
#[path = "grid_instances/tests.rs"]
mod tests;
