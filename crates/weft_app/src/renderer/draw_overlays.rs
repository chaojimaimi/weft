//! `MetalRenderer` overlay-stack + present phases, moved verbatim out of
//! `renderer/draw_phases.rs` (v1.15.2 governance cart, zero-rewrite).
//! Sibling file of `draw_phases.rs` so the `pub(super)` visibility keeps
//! resolving against the same `renderer` parent module.

use crate::paint::primitives::color_to_normalized;
use crate::paint::tab_bar::TabBarDrawState;
use crate::renderer::draw_phases::{FrameDrawCore, FrameOverlays};
use weft_core::vt::Terminal;

impl crate::renderer::MetalRenderer {
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
        if let Some((matches, selected, hovered)) = completions {
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
                        &self.build_completion_vertices(matches, selected, hovered, layout),
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
            // v1.14.0 (T1): alpha post-pass — every layer's fg/bg alpha is
            // scaled by the reveal factor in one place (miss-proof; see
            // `scale_vertex_alpha`).
            let mut verts = self.build_palette_vertices(p);
            crate::paint::primitives::scale_vertex_alpha(&mut verts, p.reveal);
            vertices.extend_from_slice(&verts);
        }

        // v1.0 S1: Settings panel (Cmd+,) — centered modal overlay.
        if let Some(s) = settings {
            let mut verts = self.build_settings_vertices(*s);
            crate::paint::primitives::scale_vertex_alpha(&mut verts, s.reveal);
            vertices.extend_from_slice(&verts);
        }

        // Context menu overlay (F7) — drawn at mouse position.
        if let Some((x, y, _block_id, selection)) = &self.context_menu_target {
            vertices.extend_from_slice(&self.build_context_menu_vertices(*x, *y, *selection));
        }

        // FindInGrid bar (v0.8 B3) — top banner with query + match count,
        // plus a yellow translucent highlight on the current match. Drawn
        // last so it composites above all other overlays.
        if let Some(find) = self.find_state.as_ref() {
            let mut verts = self.build_find_vertices(find);
            // v1.14.0 (T1): alpha post-pass (palette/settings twin).
            crate::paint::primitives::scale_vertex_alpha(&mut verts, find.reveal);
            vertices.extend_from_slice(&verts);
        }

        // v1.7.3-C: Inline note editor — top-center card with "Note: [buffer|]".
        // Drawn after find so it composites above when both are open (rare).
        if let Some(note) = self.note_editor_state.as_ref() {
            let mut verts = self.build_note_editor_vertices(note);
            // v1.14.0 (T1): alpha post-pass (palette/settings twin).
            crate::paint::primitives::scale_vertex_alpha(&mut verts, note.reveal);
            vertices.extend_from_slice(&verts);
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
