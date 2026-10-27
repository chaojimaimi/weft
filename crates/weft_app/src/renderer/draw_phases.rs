//! `MetalRenderer::draw` phase family, moved verbatim out of `renderer.rs`
//! (v1.12.27b P1-01) — modeled after the 3-B-2 zero-rewrite split pattern.
//!
//! Every phase below is `&self` (build_* precedent): `draw()` holds an
//! immutable `self.layer` borrow (from `next_drawable`) for the whole frame,
//! so `&mut self` extraction is impossible. Interior mutability goes through
//! the existing `Cell`/`RefCell` fields. The per-pane `layout_ctx` swap loop
//! and the bare field writes (`cursor_blink_on`, `layout_ctx`, `hit_regions`)
//! stay in the `draw()` skeleton in `renderer.rs` — `layout_ctx` is NOT
//! Cell-ized.
//!
//! Zero-rewrite: phase bodies are byte-identical to the original draw()
//! segments except (a) locals shared across phases arrive via the pure-data
//! [`FrameDrawCore`] / [`FrameOverlays`] structs (plain-literal fields, no
//! construction logic) and (b) the active-pane outputs are collected into
//! [`ActivePaneOutput`] instead of mutated locals.

use crate::paint::panel::PanelDrawParams;
use crate::paint::preedit::TuiPreeditDrawParams;
use crate::paint::primitives::{color_to_normalized, push_quad};
use crate::paint::prompt::PromptDrawParams;
use crate::renderer::PaneRenderInfo;
use weft_core::selection::SelectionHandler;
use weft_core::vt::Terminal;

/// v1.12.27b (P1-01): the overlay refs extracted from the frame's
/// [`crate::overlay::OverlayStack`] (baseline draw() :515-559). Pure data —
/// plain struct literal in the phase, no constructor, no `Default`.
#[derive(Clone, Copy)]
pub(super) struct FrameOverlays<'a> {
    pub panel: Option<&'a PanelDrawParams<'a>>,
    pub prompt: Option<&'a PromptDrawParams<'a>>,
    pub tui_preedit: Option<TuiPreeditDrawParams<'a>>,
    /// `(matches, selected, hovered)` — v1.13.17 S2 added the hover mirror
    /// of `CompletionDrawParams::hovered` so the popup paints the hovered row.
    pub completions: Option<(&'a [weft_core::complete::Match], usize, Option<usize>)>,
    pub palette: Option<&'a crate::overlay::PaletteDrawParams<'a>>,
    pub settings: Option<&'a crate::overlay::SettingsDrawParams<'a>>,
}

/// v1.12.27b (P1-01): the Copy frame values shared by the phase methods
/// (baseline draw() :436-676 scattered locals). Pure data — plain struct
/// literal in `draw()`, no constructor, no `Default`.
#[derive(Clone, Copy)]
pub(super) struct FrameDrawCore {
    /// The active-pane `LayoutCtx` anchored at baseline :727 (v1.12.27a
    /// single-expect anchor; `LayoutCtx` is `Copy`).
    pub draw_ctx: crate::layout::LayoutCtx,
    pub show_blocks: bool,
    pub view_switched: bool,
    pub cursor_blink_on: bool,
    pub cursor_blink_phase: f32,
    pub block_scroll: f32,
    pub active_pane_rect: crate::layout::Rect,
    pub active_pane_id: weft_core::pane_layout::PaneId,
    pub active_pane_session_id: u64,
    pub chrome_left: f32,
    pub bg_r: f64,
    pub bg_g: f64,
    pub bg_b: f64,
    pub clear_a: f64,
    pub drawable_tex_size: (f32, f32),
    pub vp_mismatch: bool,
}

/// v1.12.27b (P1-01): the active-pane build outputs (baseline locals
/// `active_vertices` :742, `pending_hit_regions` :662, `dirty_row_count` /
/// `session_block_count` / `bv_rows_count` :670-674 — the latter three feed
/// the frame trace at baseline :1111-1159). Pure data.
pub(super) struct ActivePaneOutput {
    pub active_vertices: Vec<f32>,
    pub hit_regions: Vec<crate::overlay::HitRegion>,
    pub dirty_row_count: usize,
    pub session_block_count: usize,
    pub bv_rows_count: usize,
}

impl crate::renderer::MetalRenderer {
    /// v1.12.27b (P1-01): verbatim move of draw()'s overlay-ref extraction
    /// (baseline :515-559). Returns them packed as [`FrameOverlays`].
    pub(super) fn extract_frame_overlays<'a>(
        &self,
        overlays: &'a crate::overlay::OverlayStack<'a>,
    ) -> FrameOverlays<'a> {
        // Extract overlay params from the stack. We look up each kind once;
        // at most one of each exists per frame.
        use crate::overlay::{OverlayContent, OverlayKind};
        let panel = overlays.layers.iter().find_map(|l| {
            if l.kind == OverlayKind::HistoryPanel {
                if let OverlayContent::HistoryPanel(p) = &l.content {
                    return Some(p);
                }
            }
            None
        });
        let prompt = overlays.layers.iter().find_map(|l| {
            if l.kind == OverlayKind::Prompt {
                if let OverlayContent::Prompt(p) = &l.content {
                    return Some(p);
                }
            }
            None
        });
        let tui_preedit = overlays.tui_preedit();
        let completions = overlays.layers.iter().find_map(|l| {
            if l.kind == OverlayKind::Completion {
                if let OverlayContent::Completion(c) = &l.content {
                    return Some((c.matches, c.selected, c.hovered));
                }
            }
            None
        });
        let palette = overlays.layers.iter().find_map(|l| {
            if l.kind == OverlayKind::CommandPalette {
                if let OverlayContent::CommandPalette(p) = &l.content {
                    return Some(p);
                }
            }
            None
        });
        // v1.0 S1: Settings panel (Cmd+,).
        let settings = overlays.layers.iter().find_map(|l| {
            if l.kind == OverlayKind::Settings {
                if let OverlayContent::Settings(s) = &l.content {
                    return Some(s);
                }
            }
            None
        });
        FrameOverlays {
            panel,
            prompt,
            tui_preedit,
            completions,
            palette,
            settings,
        }
    }

    /// v1.12.27b (P1-01): verbatim move of draw()'s view-mode sync segment
    /// (baseline :617-661). Returns `(show_blocks, view_switched)` — both
    /// feed [`FrameDrawCore`].
    pub(super) fn draw_sync_view_mode(
        &self,
        terminal: &Terminal,
        background_panes: &[PaneRenderInfo<'_>],
    ) -> (bool, bool) {
        // Editor mode (at the prompt): full block history + input box.
        // CommandExecuting (a tracked command is running — e.g. an interactive
        // `sudo su` sub-shell): the live grid fills the bottom while the
        // completed block history is overlaid on top, so the history never
        // reverts to raw text (matches Warp). Alt-screen / not-integrated: grid.
        // v1.3 multi-pane: block-view paint now honors pane_origin + clip via
        // LayoutCtx (background quad, sticky header, and layout_block_view all
        // confined to [left..right] × [clip_top..region_bottom_y]). So we no
        // longer force grid view when background panes exist — the active pane
        // can show block view in a split.
        let show_blocks = terminal.show_block_view();
        tracing::debug!(
            show_blocks_terminal = terminal.show_block_view(),
            background_panes_empty = background_panes.is_empty(),
            show_blocks,
            "renderer view mode"
        );
        // v1.0 P1.5-B2: when the view mode switches (alt screen enter/exit),
        // the grid_row_cache and offscreen content are stale — force a full
        // rebuild. Without this, an idle frame after the switch could set
        // instances_unchanged=true and blit the wrong view's content.
        let view_switched = show_blocks != self.prev_show_blocks.get();
        if view_switched {
            // v1.11.8 (PLAN_v1118 M-B): edge log for the block/grid view
            // flip — the renderer-side handoff of the data plane. Fields
            // mirror the screen-exit defer/settle trio (mode/exempt/mouse)
            // so flip forensics can reconstruct the `show_block_view()`
            // decision without re-instrumenting the per-frame query.
            tracing::info!(
                mode = ?terminal.tui_render_mode(),
                exempt = terminal.interactive_stdin_seen()
                    || terminal.mouse_protocol() != weft_core::input::MouseProtocol::Off,
                mouse = ?terminal.mouse_protocol(),
                "renderer view mode switched"
            );
            self.force_full_grid_redraw();
            // Batch 6 Step 1: reset block-view-only counters so grid-view
            // frames report 0 for visible_block_count / styled_line_lookups
            // instead of the last block-view frame's stale values.
            self.last_expanded_block_count.set(0);
            self.styled_lookup_counter.set(0);
            self.styled_paint_us_counter.set(0);
            self.grid_build_us_counter.set(0);
        }
        self.prev_show_blocks.set(show_blocks);
        (show_blocks, view_switched)
    }

    /// v1.12.27b (P1-01): verbatim move of draw()'s active-pane content
    /// build (baseline :742-927) — the block-view / grid-view two branches.
    /// `pending_hit_regions` / counter locals (baseline :662-674) are
    /// declared here and returned as [`ActivePaneOutput`].
    #[allow(clippy::too_many_arguments)]
    pub(super) fn draw_active_pane_content(
        &self,
        frame: &FrameDrawCore,
        terminal: &Terminal,
        selection: &mut SelectionHandler,
        grid: &weft_core::grid::Grid,
        fx: &FrameOverlays<'_>,
        // v1.14.1 (F2): kept for signature symmetry; its no-op reader is gone.
        _background_panes: &[PaneRenderInfo<'_>],
        bg_stream: &mut Vec<f32>,
        glyph_stream: &mut Vec<f32>,
    ) -> ActivePaneOutput {
        let FrameDrawCore {
            draw_ctx,
            show_blocks,
            cursor_blink_on,
            block_scroll,
            active_pane_rect,
            active_pane_session_id,
            ..
        } = *frame;
        let FrameOverlays {
            prompt,
            tui_preedit,
            ..
        } = *fx;
        // v1.12.27b (P1-01): baseline :513 local, re-derived here (same
        // borrow of the `grid` parameter).
        let cursor = &grid.cursor;
        let mut pending_hit_regions: Vec<crate::overlay::HitRegion> = Vec::new();
        // Reset popup rects — settings still uses renderer-owned hit data.
        // v1.0 P1.5-B1: grid cells render as instances (instanced pipeline);
        // overlays + block view render as legacy vertices.
        // v1.4.2 Phase B3: dual-stream — bg runs (8 floats) + glyph instances
        // (16 floats) replace the single `instances` buffer.
        let mut dirty_row_count: usize = 0;
        // Step 1: block-specific counters for frame trace. Captured here in
        // the block view branch; remain 0 in grid view.
        let mut session_block_count: usize = 0;
        let mut bv_rows_count: usize = 0;

        let active_vertices: Vec<f32> = if show_blocks {
            let (v, regions, bv_rows) = if let Some(p) = prompt {
                // v1.3 multi-pane: use the active pane's LayoutCtx (which
                // carries pane_origin + clip set by the background-pane block
                // above) instead of the stale full-viewport `ctx` local.
                let active_ctx = &draw_ctx;
                let (_, prompt_layout, _) =
                    crate::paint::prompt::prompt_layout_for_buffer(active_ctx, p.lines, p.cursor);
                let box_top_y = prompt_layout.box_rect[1];
                self.build_block_view_vertices(
                    crate::paint::block_view_model::BlockViewPaintModel {
                        blocks: terminal.block_tracker().session_blocks(),
                        live_head_lines: terminal.screen_head_lines(),
                        region_bottom_y: box_top_y,
                        cwd: p.cwd,
                        git_branch: terminal.git_branch(),
                        live: None,
                        block_scroll,
                        viewport_rows: terminal.grid().num_rows,
                        block_hovered: self.block_hovered,
                        block_selected: self.block_selected,
                        block_action_hovered: self.block_action_hovered,
                        spinner_phase: self.spinner_phase,
                        find_block_highlight: self
                            .find_state
                            .as_ref()
                            .and_then(|find| find.block_highlight),
                        palette: terminal.palette(),
                        cache_namespace: active_pane_session_id,
                        block_diagnose_state: &self.block_diagnose_state,
                        ai_configured: self.ai_configured,
                        tui_cursor: None,
                        tui_preedit: None,
                        cursor_blink_on: false,
                        is_alt: terminal.is_alt_screen_active(),
                        now: std::time::SystemTime::now(),
                    },
                    selection,
                )
            } else {
                // CommandExecuting: full block view with the in-flight command
                // as a live block at the bottom (its streaming output) and the
                // completed history above. No grid — the live session IS the
                // in-flight block's captured output, so the history never
                // reverts to raw and isn't squeezed by the grid cursor.
                self.build_block_view_vertices(
                    crate::paint::block_view_model::BlockViewPaintModel {
                        blocks: terminal.block_tracker().session_blocks(),
                        live_head_lines: terminal.screen_head_lines(),
                        // v1.3 multi-pane: region_bottom_y must be pane-local
                        // (clip's bottom edge), not the full vp_h - pad_y.
                        region_bottom_y: draw_ctx.bottom(),
                        cwd: terminal.cwd(),
                        git_branch: terminal.git_branch(),
                        live: terminal.block_tracker().in_flight(),
                        block_scroll,
                        viewport_rows: terminal.grid().num_rows,
                        block_hovered: self.block_hovered,
                        block_selected: self.block_selected,
                        block_action_hovered: self.block_action_hovered,
                        spinner_phase: self.spinner_phase,
                        find_block_highlight: self
                            .find_state
                            .as_ref()
                            .and_then(|find| find.block_highlight),
                        palette: terminal.palette(),
                        cache_namespace: active_pane_session_id,
                        block_diagnose_state: &self.block_diagnose_state,
                        ai_configured: self.ai_configured,
                        // v1.10.5: BlockView-mode TUI caret + IME preedit —
                        // the grid cursor mapped into the live block's
                        // snapshot text. Primary-screen TUIs (openclaw/pi)
                        // input at their own bottom row; while the BlockView
                        // renders the document the grid cursor is invisible,
                        // so caret and marked text paint on the live row.
                        // No in-flight block (e.g. prompt bootstrapping) →
                        // no caret: there is no document row to anchor on.
                        // v1.10.6: prefer the precisely-tracked cursor
                        // snapshot line (from snapshot construction) over
                        // the formula guess. The formula breaks when the
                        // snapshot skips empty rows; the tracked value is
                        // exact. Fallback to the formula + clamp when no
                        // snapshot has been taken yet.
                        tui_cursor: self.block_view_tui_cursor(terminal, grid),
                        tui_preedit: tui_preedit.map(|p| (p.text, p.cursor)),
                        cursor_blink_on,
                        is_alt: terminal.is_alt_screen_active(),
                        now: std::time::SystemTime::now(),
                    },
                    selection,
                )
            };
            pending_hit_regions = regions;
            // Step 1: capture block-specific counters for frame trace.
            session_block_count = terminal.block_tracker().session_blocks().len();
            // Step 2 will make this << session_block_count via visibility culling;
            // for now (pre-Step-2) it equals the total expanded row count.
            bv_rows_count = bv_rows.len();
            // v1.10.21: alt-screen history peek — gesture-hint pill at the
            // pane top, drawn after the block content so it sits on top
            // (no layout involvement; see paint/alt_peek_pill).
            if terminal.is_alt_screen_history_peek() {
                let mut v = v;
                self.push_alt_peek_pill(&mut v);
                v
            } else {
                v
            }
        } else {
            // Grid view (alt-screen apps): build per-cell instances
            // (P1.5-B1) into `instances`; overlays go into `vertices`
            // (appended below).
            // v1.0 fix (vim scroll): alt-screen TUIs (vim/less/man) scroll via
            // IL/DL (CSI L/M) which PHYSICALLY move viewport rows, then repaint
            // the moved rows. The renderer's per-row vertex cache is indexed by
            // row position — after an IL/DL the cache at a given index holds the
            // PREVIOUS frame's content for that row, and even though the VT marks
            // the moved rows dirty (triggering a rebuild), subtle ordering /
            // partial-frame interactions left the screen showing stale cached
            // content ("only the top row moves, rows overlap and merge").
            // Forcing a full grid rebuild every frame on the alt screen bypasses
            // the cache entirely and renders directly from the live grid,
            // eliminating the corruption. Cost: full redraw while in a TUI app
            // (acceptable — TUIs don't stream like shell output).
            if (terminal.is_alt_screen_active() || terminal.primary_screen_app_active())
                && !terminal.show_block_view()
            {
                // v1.11.7 (P2-2): `&& !show_block_view()` — while a
                // screen-owned session renders as a block, the grid is
                // invisible; the per-frame full rebuild would only burn the
                // dirty-all path. Skips slightly more than pre-v1.11.7
                // (classic-tier alt-peek/history views skip too); the
                // takeover path still forces the rebuild as before.
                self.force_full_grid_redraw();
            }

            // v1.3 Batch 5: background panes are rendered above (before the
            // if/else) so both block view and grid view show them; the
            // layout_ctx was restored to the active pane's origin above.

            let cursor_visible_this_frame = crate::terminal_geometry::grid_cursor_visible(
                terminal.cursor_style,
                terminal.cursor_visible,
                cursor_blink_on,
                prompt.is_some() || tui_preedit.is_some(),
            );
            let grid_build_start = std::time::Instant::now();
            let (grid_batch, grid_dirty_rows) = self.build_grid_instances(
                grid,
                terminal.palette(),
                cursor,
                selection,
                crate::paint::grid::GridViewPolicy {
                    show_cursor: cursor_visible_this_frame,
                    cursor_style: terminal.cursor_style,
                    hidden_before_row: terminal.primary_screen_visible_row_start(),
                    owned_rows: terminal.primary_screen_viewport_ownership(),
                    is_alt_screen: terminal.is_alt_screen_active(),
                    inset_block_gutter: !terminal.is_alt_screen_active()
                        && terminal.primary_screen_owns_live_view(),
                },
            );
            self.grid_build_us_counter
                .set(grid_build_start.elapsed().as_micros() as u64);
            // v1.4.2 Phase B3: append active pane's dual-stream instances.
            let active_bg_start = bg_stream.len();
            let active_glyph_start = glyph_stream.len();
            bg_stream.extend(&grid_batch.bg_stream);
            glyph_stream.extend(&grid_batch.glyph_stream);
            let active_bg_end = bg_stream.len();
            let active_glyph_end = glyph_stream.len();
            // v1.14.1 (F2): bake the sub-row scroll clip into the stored rect
            // (px≠0 frames scissor at the editor box top; px==0 is identity).
            // zoom_render's replay copies this segment, so it inherits.
            let grid_pane_rect = crate::paint::grid::grid_pane_clip_rect(
                active_pane_rect,
                &draw_ctx,
                prompt,
                grid.num_rows,
                self.grid_scroll_px != 0.0,
            );
            self.pane_instance_ranges.borrow_mut().push((
                grid_pane_rect,
                crate::paint::grid_instances::PaneInstanceRanges {
                    bg_range: (active_bg_start, active_bg_end),
                    glyph_range: (active_glyph_start, active_glyph_end),
                },
            ));
            dirty_row_count = grid_dirty_rows;
            Vec::new()
        };
        ActivePaneOutput {
            active_vertices,
            hit_regions: pending_hit_regions,
            dirty_row_count,
            session_block_count,
            bv_rows_count,
        }
    }

    /// v1.12.27b (P1-01): verbatim move of draw()'s block scrollbar segment
    /// (baseline :937-972).
    pub(super) fn draw_block_scrollbar(
        &self,
        frame: &FrameDrawCore,
        // v0.8 U6: block-content metrics (total_rows, visible_rows,
        // max_scroll) for the dynamic scrollbar thumb. None in grid view.
        scroll_metrics: Option<(usize, usize, usize)>,
        vertices: &mut Vec<f32>,
    ) {
        let FrameDrawCore {
            draw_ctx,
            show_blocks,
            block_scroll,
            ..
        } = *frame;
        // v0.8 U6 scrollbar: dynamic thumb position + height proportional to
        // visible/total content. The thumb sits in a track spanning the block
        // region; its vertical position reflects block_scroll (scrolled up →
        // thumb near top). v1.0: color uses label_c (was accent_dim —
        // invisible in Nord/Warp themes). Still subtle but always readable.
        // Only drawn when content overflows the viewport.
        // Step 3: cache metrics for active_scrollbar_layout (mouse handlers).
        self.cached_scroll_metrics.set(scroll_metrics);
        if show_blocks {
            if let Some((total, visible, max_scroll)) = scroll_metrics {
                // v1.3 multi-pane: scrollbar uses the active pane's ctx (it
                // carries pane_origin + clip), not the stale full-viewport `ctx`.
                let scrollbar_ctx = &draw_ctx;
                if let Some(scrollbar) = crate::scrollbar_component::scrollbar_layout(
                    scrollbar_ctx,
                    total,
                    visible,
                    max_scroll,
                    block_scroll.floor() as usize,
                ) {
                    // v1.0: label_c (70% fg + 30% bg) — was accent_dim.
                    // v1.14.0 (PLAN_v1140 T4): the thumb's 加宽+色 hover
                    // transition rides the ScrollbarThumb fade progress —
                    // width lerps idle→hover, color deepens toward a more
                    // present 55% mix. Fade 0.0 reproduces the idle pair
                    // exactly (bit-faithful with the pre-T4 idle look).
                    let thumb_fade = crate::app::motion::fade_progress(
                        &self.hover_fades,
                        crate::app::motion::HoverKey::ScrollbarThumb,
                    );
                    let fg_v = color_to_normalized(self.theme.foreground);
                    let bg_v = color_to_normalized(self.theme.background);
                    let thumb_color = crate::paint::color_math::mix_fg_over_bg(fg_v, bg_v, 0.30);
                    let emphasized_color =
                        crate::paint::color_math::mix_fg_over_bg(fg_v, bg_v, 0.55);
                    let thumb_color = [
                        thumb_color[0] + (emphasized_color[0] - thumb_color[0]) * thumb_fade,
                        thumb_color[1] + (emphasized_color[1] - thumb_color[1]) * thumb_fade,
                        thumb_color[2] + (emphasized_color[2] - thumb_color[2]) * thumb_fade,
                        thumb_color[3],
                    ];
                    let (su, sv, suw, svh) = self.space_uv();
                    let bg_uv = [su, sv + svh, su + suw, sv];
                    push_quad(
                        vertices,
                        crate::scrollbar_component::visual_thumb_faded(&scrollbar, thumb_fade),
                        bg_uv,
                        [0.0; 4],
                        thumb_color,
                    );
                }
            }
        }
    }
}
