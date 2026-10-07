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
use crate::paint::tab_bar::TabBarDrawState;
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
    pub completions: Option<(&'a [weft_core::complete::Match], usize)>,
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
    pub scrollbar_emphasized: bool,
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
                    return Some((c.matches, c.selected));
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
        background_panes: &[PaneRenderInfo<'_>],
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
            // if/else) so both block view and grid view show them. Here we
            // only need to ensure `layout_ctx` has the active pane's origin
            // set (it was set above if background_panes is non-empty; for
            // single-pane tabs it was never changed).
            if !background_panes.is_empty() {
                // layout_ctx already restored to active pane's origin above.
            }

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
            self.pane_instance_ranges.borrow_mut().push((
                active_pane_rect,
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
            scrollbar_emphasized,
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
                    let fg_v = color_to_normalized(self.theme.foreground);
                    let bg_v = color_to_normalized(self.theme.background);
                    let thumb_color = crate::paint::color_math::mix_fg_over_bg(fg_v, bg_v, 0.30);
                    let (su, sv, suw, svh) = self.space_uv();
                    let bg_uv = [su, sv + svh, su + suw, sv];
                    push_quad(
                        vertices,
                        crate::scrollbar_component::visual_thumb(&scrollbar, scrollbar_emphasized),
                        bg_uv,
                        [0.0; 4],
                        thumb_color,
                    );
                }
            }
        }
    }

    /// v1.12.27b (P1-01): verbatim move of draw()'s overlay-stack segment
    /// (baseline :974-1109) — 实况层级序: status hint → paste toast →
    /// grid-path preedit → prompt → completion → panel → palette → settings
    /// → context menu → find → note → tab bar → pane dividers.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn draw_overlay_stack(
        &self,
        frame: &FrameDrawCore,
        terminal: &Terminal,
        grid: &weft_core::grid::Grid,
        fx: &FrameOverlays<'_>,
        // v0.9 H1: tab bar state. When tab_count > 1 the tab bar is drawn
        // at the top of the window and the content area is shifted down.
        tab_bar: &TabBarDrawState,
        // v1.3.1 Batch 7: full pane-layout snapshot (one `(PaneId, Rect)` per
        // pane, from `SplitTree::layout(content_rect)`). Used to derive pane
        // divider edges + the active-pane focus ring. Single-pane tabs pass a
        // one-element vec; the divider path is a no-op when only one pane
        // exists.
        pane_layouts: &[(weft_core::pane_layout::PaneId, crate::layout::Rect)],
        vertices: &mut Vec<f32>,
    ) {
        let FrameDrawCore {
            draw_ctx,
            show_blocks,
            cursor_blink_on,
            cursor_blink_phase,
            active_pane_id,
            chrome_left,
            ..
        } = *frame;
        let FrameOverlays {
            panel,
            prompt,
            tui_preedit,
            completions,
            palette,
            settings,
        } = *fx;
        // F2 P0-2: subtle status hint for passthrough/running states.
        if prompt.is_none() {
            vertices.extend_from_slice(&self.build_status_hint_vertices(terminal));
        }

        // v1.11.1 (PLAN_v1111 §4.5): transient paste feedback. Drawn
        // regardless of editor/prompt state — a toast often fires right
        // after an editor-mode paste, where the prompt suppresses the
        // left-side hints above.
        vertices.extend_from_slice(&self.build_paste_toast_vertices());

        // v1.10.6: only draw the grid-path TUI preedit in grid view. In
        // BlockView the preedit is painted inside `build_block_view_vertices`
        // (on the mapped live-block row); drawing it again here at the grid
        // cursor position produces a duplicate preedit ("two pinyin strings").
        // v1.10.26 (FIX_IME_PREEDIT): the A-path PREEDIT_DIAG logs from
        // `build_tui_preedit_for_grid` (paint/preedit.rs).
        if let Some(preedit) = tui_preedit {
            if !show_blocks {
                vertices.extend_from_slice(&self.build_tui_preedit_for_grid(
                    preedit,
                    grid,
                    !terminal.is_alt_screen_active() && terminal.primary_screen_owns_live_view(),
                    terminal.is_alt_screen_active(),
                ));
            }
        }

        // Overlay the editor input box at the bottom (Editor mode only).
        if let Some(p) = prompt {
            vertices.extend_from_slice(&self.build_prompt_vertices(
                p,
                cursor_blink_phase,
                cursor_blink_on,
            ));
        }

        // Completion popup (split out from prompt — overlay refactor commit 2).
        // Positioned above the prompt input box using the same geometry.
        if let Some((matches, selected)) = completions {
            if !matches.is_empty() {
                let visual_prompt = prompt.map(|p| {
                    let ctx = draw_ctx;
                    crate::paint::prompt::prompt_layout_for_buffer(&ctx, p.lines, p.cursor).0
                });
                let n_lines = visual_prompt
                    .as_ref()
                    .map(|visual| visual.rows.len())
                    .unwrap_or(1);
                let cursor = visual_prompt
                    .as_ref()
                    .map(|visual| (visual.cursor_row, visual.cursor_display_col))
                    .unwrap_or((0, 0));
                let ctx = draw_ctx;
                if let Some(layout) = crate::completion_component::derive_completion_layout(
                    &ctx,
                    matches,
                    selected,
                    n_lines,
                    cursor,
                    self.popup_max_rows,
                    self.popup_width_scale,
                ) {
                    vertices.extend_from_slice(
                        &self.build_completion_vertices(matches, selected, layout),
                    );
                }
            }
        }

        // Paint Compact drawer above terminal-local overlays; modals stay above it.
        if let Some(p) = panel {
            vertices.extend_from_slice(&self.build_panel_vertices(p));
        }

        // Command Palette overlay (v0.7) — centered floating window.
        if let Some(p) = palette {
            vertices.extend_from_slice(&self.build_palette_vertices(p));
        }

        // v1.0 S1: Settings panel (Cmd+,) — centered modal overlay.
        if let Some(s) = settings {
            vertices.extend_from_slice(&self.build_settings_vertices(*s));
        }

        // Context menu overlay (F7) — drawn at mouse position.
        if let Some((x, y, _block_id, selection)) = &self.context_menu_target {
            vertices.extend_from_slice(&self.build_context_menu_vertices(*x, *y, *selection));
        }

        // FindInGrid bar (v0.8 B3) — top banner with query + match count,
        // plus a yellow translucent highlight on the current match. Drawn
        // last so it composites above all other overlays.
        if let Some(find) = self.find_state.as_ref() {
            vertices.extend_from_slice(&self.build_find_vertices(find));
        }

        // v1.7.3-C: Inline note editor — top-center card with "Note: [buffer|]".
        // Drawn after find so it composites above when both are open (rare).
        if let Some(note) = self.note_editor_state.as_ref() {
            vertices.extend_from_slice(&self.build_note_editor_vertices(note));
        }

        // v0.9 H1: Tab bar — drawn at the top of the window. The content
        // area is already shifted down by `chrome_top` in the LayoutCtx, so
        // this draws in the space above the content.
        // v1.2: always render the tab bar (even for a single tab) so the
        // "+" button is always available. Previously single-tab mode hid
        // the bar entirely, making "+" inaccessible without first opening a
        // second tab via menu/keyboard.
        if tab_bar.tab_count >= 1 {
            let tab_verts = self.build_tab_bar_vertices(tab_bar);
            vertices.extend_from_slice(&tab_verts);
        }
        // v1.11.0: the `else` branch (single-tab titlebar strip with a bg
        // brightness heuristic) was removed — it was unreachable since the
        // flag was hardcoded false (AUDIT_v1.10.39 / PLAN_v111).

        // v1.3.1 Batch 7: pane dividers + active-pane focus ring, drawn last
        // so they stay visible atop the panes. No-op in single-pane tabs.
        // content_rect x0 includes chrome_left so horizontal dividers don't
        // over-extend across the sidebar push region.
        crate::paint::pane_dividers::push_pane_overlays(
            vertices,
            pane_layouts,
            active_pane_id,
            [
                self.padding_x + chrome_left,
                self.padding_y,
                self.viewport.0 - self.padding_x,
                self.viewport.1 - self.padding_y,
            ],
            color_to_normalized(self.theme.separator),
            color_to_normalized(self.theme.accent),
            self.increase_contrast,
        );
    }

    /// v1.12.27b (P1-01): verbatim move of draw()'s present segment
    /// (baseline :1111-1175) — frame-trace counters, build_end/encode_start,
    /// then `encode_and_present`. The `hit_regions` assignment (baseline
    /// :1176-1180) stays in the `draw()` skeleton (bare field write).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn draw_present_frame(
        &self,
        frame: &FrameDrawCore,
        drawable: &metal::MetalDrawableRef,
        vertices: &mut Vec<f32>,
        bg_stream: &mut Vec<f32>,
        glyph_stream: &mut Vec<f32>,
        // v1.12.27b (P1-01): the three [`ActivePaneOutput`] counters — passed
        // individually so the skeleton can consume `active_vertices` /
        // `hit_regions` by value without a partial move.
        dirty_row_count: usize,
        session_block_count: usize,
        bv_rows_count: usize,
    ) {
        let FrameDrawCore {
            view_switched,
            bg_r,
            bg_g,
            bg_b,
            clear_a,
            drawable_tex_size,
            vp_mismatch,
            ..
        } = *frame;
        // R3 task 6: BUILD-VERTICES segment ends; ENCODE segment starts.
        // Step 1: drain per-frame block layout cache hit/miss counters.
        let (cache_hits, cache_misses) =
            self.block_layout_cache.borrow_mut().take_hit_miss_counts();
        // M6-c (PLAN_M6 §三): layout-table budget observability — total
        // cached table bytes + blocks still deferred above the sync band.
        let (layout_table_bytes, deferred_blocks) = {
            let cache = self.block_layout_cache.borrow();
            (cache.table_bytes_total() as u64, cache.deferred_blocks())
        };
        let (styled_cache_hits, styled_cache_misses) =
            self.styled_line_cache.borrow_mut().take_hit_miss_counts();
        let styled_cache_bytes = self.styled_line_cache.borrow().bytes() as u64;
        let visible_block_count = self.last_expanded_block_count.get();
        let styled_line_lookups = self.styled_lookup_counter.get();
        let styled_paint_us = self.styled_paint_us_counter.get();
        // v1.4.2 Phase B3: dual-stream grid counters (bg=8 floats/run,
        // glyph=16 floats/cell; upload_bytes = (verts+bg+glyph)·4).
        let grid_bg_instances = bg_stream.len() / 8;
        let grid_glyph_instances = glyph_stream.len() / 16;
        let grid_upload_bytes = (vertices.len() + bg_stream.len() + glyph_stream.len()) as u64 * 4;
        let grid_build_us = self.grid_build_us_counter.get();
        self.frame_trace
            .borrow_mut()
            .build_end(crate::frame_trace::FrameCounters {
                vertex_count: vertices.len() / 12,
                instance_count: grid_glyph_instances,
                dirty_rows: dirty_row_count,
                session_block_count,
                visible_block_count,
                bv_rows_count,
                block_layout_cache_hits: cache_hits,
                block_layout_cache_misses: cache_misses,
                styled_line_lookups,
                styled_paint_us,
                // R5 task 4: resident_bytes is captured at begin() and
                // preserved by build_end(); 0 here is overwritten.
                resident_bytes: 0,
                grid_bg_instances,
                grid_glyph_instances,
                grid_upload_bytes,
                styled_cache_hits,
                styled_cache_misses,
                styled_cache_bytes,
                grid_build_us,
                layout_table_bytes,
                deferred_blocks,
            });
        self.frame_trace.borrow_mut().encode_start();

        // PLAN_zoom Z-f: the three streams are handed to the zoom frame cache
        // inside `encode_and_present` (mem::take at its present->flush gaps),
        // so they come back EMPTY. Nothing below re-reads them: the frame
        // counters above were captured before the call, and only hit regions
        // are assigned afterwards.
        self.encode_and_present(
            drawable,
            vertices,
            bg_stream,
            glyph_stream,
            (bg_r, bg_g, bg_b, clear_a),
            drawable_tex_size,
            vp_mismatch,
            view_switched,
        );
    }
}
