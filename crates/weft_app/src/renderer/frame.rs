//! `MetalRenderer::draw` skeleton, moved verbatim out of `renderer.rs`
//! (v1.15.2 governance cart, zero-rewrite) — modeled after the
//! `renderer/draw_phases.rs` precedent. The struct + field table stay
//! in `renderer.rs`.

use crate::paint::tab_bar::TabBarDrawState;
use crate::renderer::draw_phases::{ActivePaneOutput, FrameDrawCore, FrameOverlays};
use crate::renderer::PaneRenderInfo;
use weft_core::selection::SelectionHandler;
use weft_core::vt::Terminal;

impl crate::renderer::MetalRenderer {
    // new() moved to paint/metal_backend.rs (M4 step 3).

    /// Draw the terminal Grid (and optional overlays) to screen.
    ///
    /// v1.12.27b (P1-01): this is now the编排骨架 only — the phase methods
    /// live in `renderer/draw_phases.rs` (all `&self`, build_* precedent).
    /// Kept here by plan mandate: the `next_drawable` early return (the only
    /// one), the bare field writes (`self.cursor_blink_on`, the
    /// `self.layout_ctx` builds — the per-pane swap loop is NOT Cell-ized),
    /// the `&mut self.atlas` warm-atlas partial borrows, and the final
    /// `self.hit_regions` assignment.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        terminal: &Terminal,
        selection: &mut SelectionHandler,
        cursor_blink_on: bool,
        cursor_blink_phase: f32,
        overlays: &crate::overlay::OverlayStack<'_>,
        block_scroll: f32,
        // v0.8 U6: block-content metrics (total_rows, visible_rows,
        // max_scroll) for the dynamic scrollbar thumb. None in grid view.
        scroll_metrics: Option<(usize, usize, usize)>,
        // v0.9 H1: tab bar state. When tab_count > 1 the tab bar is drawn
        // at the top of the window and the content area is shifted down.
        tab_bar: &TabBarDrawState,
        // v1.3 Batch 5: Multi-pane rendering. `active_pane_rect` is the
        // split-tree-computed rect for the active pane (the `terminal` /
        // `selection` args belong to it). `background_panes` carries the
        // other panes. Each pane chooses block/grid view from its own Terminal
        // state; cursor, selection, and interactive overlays remain active-only.
        // Empty for single-pane tabs (the common case).
        active_pane_rect: crate::layout::Rect,
        background_panes: &[PaneRenderInfo<'_>],
        // v1.3.1 Batch 7: full pane-layout snapshot (one `(PaneId, Rect)` per
        // pane, from `SplitTree::layout(content_rect)`). Used to derive pane
        // divider edges + the active-pane focus ring. Single-pane tabs pass a
        // one-element vec; the divider path is a no-op when only one pane
        // exists.
        pane_layouts: &[(weft_core::pane_layout::PaneId, crate::layout::Rect)],
        active_pane_id: weft_core::pane_layout::PaneId,
        active_pane_session_id: u64,
    ) {
        self.block_view_tui_caret_area.set(None);
        let drawable = match self.layer.next_drawable() {
            Some(d) => d,
            None => return,
        };

        // v1.3 Batch 5: reset per-pane scissor ranges for this frame. Each
        // grid-instance segment (one per pane) is recorded here as
        // `(rect, byte_range_in_instances)` so `encode_and_present` can
        // set a scissor rect per segment before its draw call.
        self.pane_instance_ranges.borrow_mut().clear();
        self.pane_vertex_ranges.borrow_mut().clear();

        // v1.0 fix: detect drawable/viewport size mismatch during macOS live
        // resize. CAMetalLayer.set_drawable_size() is asynchronous — the
        // drawable returned here may still have the PREVIOUS size. If we blit
        // viewport-sized data to a smaller drawable, Metal writes past the
        // texture bounds → corruption/tearing. If the drawable is smaller
        // than the layer bounds, Core Animation stretches it → character
        // spacing distortion.
        //
        // Mitigation: clamp the blit to the drawable's actual texture size,
        // and force a full redraw so the offscreen is rebuilt at the correct
        // size on the next frame (when the drawable has caught up).
        let drawable_tex_size = {
            let tex = drawable.texture();
            (tex.width() as f32, tex.height() as f32)
        };
        let vp_mismatch = (drawable_tex_size.0 - self.viewport.0).abs() > 0.5
            || (drawable_tex_size.1 - self.viewport.1).abs() > 0.5;
        if vp_mismatch {
            self.force_full_grid_redraw();
        }
        // v0.9: cache the blink state so overlay builders can draw a caret.
        self.cursor_blink_on = cursor_blink_on;

        // v1.0 P0-c: reset the cached scroll/force_full state to safe
        // defaults. build_grid_vertices (grid view only) overwrites these
        // with real values. In block view they stay at the defaults, which
        // disables the GPU scroll blit (block-view scrolling isn't a simple
        // viewport shift, so blit doesn't apply there).
        self.pending_scroll_delta.set(0);
        self.force_full_cached.set(true);
        // R3 task 6: LAYOUT segment starts at draw entry (geometry + layout
        // computation). build_start is marked below, just before the first
        // vertex builder runs.
        self.frame_trace.borrow_mut().layout_start();

        // v0.9 H1: compute chrome_top (tab bar height) and set it on the
        // LayoutCtx so all content is shifted below the tab bar. The bar is
        // only drawn when more than one tab is open (single tab hides it).
        //
        // v1.1: with the transparent (FullSizeContentView) titlebar, the
        // macOS traffic-light buttons float over the Metal content. Even with
        // a single tab we now always render the tab bar (for the "+" button),
        // so chrome_top always reserves tab_bar_height.
        // v1.11.0: `single_tab_titlebar` removed — it was hardcoded `false`
        // since v1.2 (dead branch, see AUDIT_v1.10.39 / PLAN_v111).

        // v0.9 W5: compute chrome_left (sidebar width) when the history panel
        // is open — the panel becomes a left sidebar that pushes content right.
        let panel_open = overlays
            .layers
            .iter()
            .any(|l| l.kind == crate::overlay::OverlayKind::HistoryPanel);
        let sidebar_placement = crate::ui_tokens::sidebar_placement(
            panel_open,
            self.sidebar_width(),
            self.sidebar_push_width(),
        );
        let chrome_left = sidebar_placement.terminal_push_width;

        let terminal_layout = crate::terminal_geometry::terminal_layout_for_renderer(
            self,
            winit::dpi::PhysicalSize::new(
                self.viewport.0.round().max(0.0) as u32,
                self.viewport.1.round().max(0.0) as u32,
            ),
            chrome_left as f64,
        );
        // Build this frame's LayoutCtx: the single source of truth for
        // coordinate math in every overlay builder (v0.8 stage 1). Stored on
        // self so methods that don't receive it directly can still access it
        // during this draw; rebuilt every frame so resizes/padding changes
        // take effect immediately.
        self.layout_ctx = Some(terminal_layout.layout_ctx());
        // R3 task 6: LAYOUT segment ends once the LayoutCtx is built.
        self.frame_trace.borrow_mut().layout_end();

        let grid = terminal.grid();
        // v1.12.27b (P1-01): baseline :513 `let cursor = &grid.cursor;` moved
        // into draw_phases::draw_active_pane_content (its only consumer — the
        // grid-instance build — lives there now).

        // v1.12.27b (P1-01): overlay-ref extraction moved to
        // draw_phases::extract_frame_overlays (baseline :515-559); the
        // locals are re-bound here by name for the warm-atlas calls below.
        let overlays_fx = self.extract_frame_overlays(overlays);
        let FrameOverlays {
            panel,
            prompt,
            tui_preedit,
            completions,
            palette,
            settings,
        } = overlays_fx;

        // Clear color from the theme background, scaled by window opacity so a
        // transparent window's uncovered area shows the desktop.
        let bg = self.theme.background;
        let (bg_r, bg_g, bg_b, bg_a) = (
            bg.r as f64 / 255.0,
            bg.g as f64 / 255.0,
            bg.b as f64 / 255.0,
            bg.a as f64 / 255.0,
        );
        let clear_a = bg_a * self.opacity as f64;

        // Collect + rasterize on-screen characters into the glyph atlas.
        // v1.8.9: collect diagnose panel texts before the partial borrows
        // below so the atlas can warm up their (often CJK) glyphs.
        let diagnose_texts: Vec<&str> = self
            .block_diagnose_state
            .values()
            .filter_map(|s| s.result.as_ref().and_then(|r| r.as_ref().ok()))
            .map(String::as_str)
            .collect();
        // Passed as partial borrows (`&mut self.atlas`, `&self.find_state`)
        // rather than a `&mut self` method call so they stay disjoint from
        // the immutable `self.layer` borrow held by `drawable` for the frame.
        // v1.12.27b (P1-01): these two stay in the skeleton — they need
        // `&mut self.atlas`, unavailable through the `&self` phase family.
        Self::warm_atlas(
            &mut self.atlas,
            self.viewport.1,
            &self.find_state,
            &self.note_editor_state,
            terminal,
            grid,
            panel,
            prompt,
            tui_preedit,
            completions,
            palette,
            settings,
            tab_bar,
            &diagnose_texts,
        );
        Self::warm_background_pane_atlases(
            &mut self.atlas,
            self.viewport.1,
            background_panes,
            tab_bar,
            &self.block_scan_watermarks,
            &self.block_layout_cache.borrow(),
            &self.live_layout_cache.borrow(),
            &self.bg_visible_blocks.borrow(),
            self.scale,
        );
        // v1.13.11 N1: the active pane's block-view glyph warmup — the
        // visible-window, once-ever, byte-budgeted successor of the B3-3
        // `.rev().take(64)` × 200-line sweep (which lived inside warm_atlas).
        // Same partial-borrow contract as warm_atlas above; the active pane
        // warms under its OWN session id (BlockIds are per-Terminal, so a
        // shared constant namespace would leak watermarks across tab/focus
        // switches). `visible_override = None` → geometric window from the
        // previous frame's layout prefix sum.
        Self::warm_block_view_glyphs(
            &mut self.atlas,
            terminal.block_tracker(),
            terminal.show_block_view(),
            active_pane_session_id,
            None,
            terminal.cwd(),
            terminal.git_branch(),
            self.viewport.1,
            block_scroll,
            grid.num_rows,
            self.scale,
            &self.block_layout_cache.borrow(),
            &self.live_layout_cache.borrow(),
            &self.block_scan_watermarks,
        );

        // v1.12.27b (P1-01): view-mode sync moved to
        // draw_phases::draw_sync_view_mode (baseline :617-661).
        let (show_blocks, view_switched) = self.draw_sync_view_mode(terminal, background_panes);
        // R3 task 6: BUILD-VERTICES segment starts here (first vertex builder).
        self.frame_trace.borrow_mut().build_start();

        // Render background panes before active-pane content. Each pane uses
        // its own Terminal view mode; LayoutCtx is swapped per pane so block
        // vertices and grid instances share the same split-tree geometry.
        // v1.4.2 Phase B3: background grid panes append to dual-stream buffers
        // (bg_stream + glyph_stream) and push a `PaneInstanceSegment` (rect +
        // ranges). Background block panes still emit legacy vertices.
        let mut background_vertices = Vec::new();
        let mut bg_stream: Vec<f32> = Vec::new();
        let mut glyph_stream: Vec<f32> = Vec::new();
        // v1.12.27b (P1-01): the per-pane layout_ctx swap loop stays in the
        // skeleton per plan mandate — bare `self.layout_ctx` writes
        // (baseline :691/:713); `layout_ctx` is NOT Cell-ized.
        if !background_panes.is_empty() {
            let base_ctx = self.layout_ctx.expect("LayoutCtx built at draw() entry");
            // Split-tree rects are absolute viewport coordinates; `for_pane`
            // converts them to the offset + clip expected by LayoutCtx.
            for bg in background_panes {
                let bg_ctx = base_ctx.for_pane(bg.rect);
                self.layout_ctx = Some(bg_ctx);
                let start = background_vertices.len();
                // v1.13.5 T16b: block panes append their (cache-gated)
                // vertices directly into the legacy stream.
                let _ = self.build_background_pane_content(
                    bg,
                    &mut background_vertices,
                    &mut bg_stream,
                    &mut glyph_stream,
                );
                let end = background_vertices.len();
                if end > start {
                    self.pane_vertex_ranges
                        .borrow_mut()
                        .push((bg.rect, start..end));
                }
            }
            // Restore layout_ctx to the active pane's origin before building
            // its instances / vertices. The active pane's overlays (prompt,
            // find, etc.) also read pane_origin from this context.
            let active_ctx = base_ctx.for_pane(active_pane_rect);
            // v1.3: confine pane-local overlays (prompt, find, completion,
            // status hint) to the active pane's rect so they don't bleed
            // across background panes. Global overlays such as Settings use
            // the full viewport directly and are unaffected.
            self.layout_ctx = Some(active_ctx);
            // The renderer's per-row grid cache is global, not per-pane.
            // When the active pane changes (e.g. after a split) the cache
            // may hold content from a different pane; a newly-created pane
            // may also have no dirty rows. Force a full rebuild of the
            // active pane ONLY when its identity changed since the last
            // multi-pane frame (first frame = None); same-pane layout moves
            // are covered by the prev_grid_origin fingerprint inside
            // build_grid_instances (T15b, PLAN_v11217 §3.10).
            if crate::renderer::panes::should_force_full_redraw(
                self.last_drawn_active_pane,
                active_pane_id,
            ) {
                self.force_full_grid_redraw();
            }
            // Record every background frame unconditionally (T15b): the
            // comparison above already consumed the previous value.
            self.last_drawn_active_pane = Some(active_pane_id);
        }
        // v1.12.27a (P1-04): the ONE `expect` anchor for the rest of draw().
        // `LayoutCtx` is `Copy`, and this sits AFTER the background-pane
        // block above (which intentionally swaps pane-local contexts into
        // `self.layout_ctx` and keeps its own base_ctx binding), so the five
        // former per-site expects below all read the same active-pane
        // context without five separate unwrap chains.
        let draw_ctx = self.layout_ctx.expect("LayoutCtx built at draw() entry");
        // B3-2 (PLAN_S2_render): prune row caches of panes that no longer
        // exist this frame (session ids are monotonic, so a plain retain
        // against the live background list keeps the map bounded). Deliberately
        // OUTSIDE the `if !background_panes.is_empty()` block above: with no
        // background panes there is no build consumer, and the retain must
        // still run so closing the LAST background pane clears the map; a pane
        // promoted to active is pruned here too — its fingerprint is re-checked
        // from scratch when it returns to the background set.
        let live_ids: std::collections::HashSet<u64> =
            background_panes.iter().map(|p| p.pane_session_id).collect();
        self.background_grid_row_caches
            .borrow_mut()
            .retain(|id, _| live_ids.contains(id));
        // v1.13.5 T16b: same retain lifecycle — close/promotion both prune.
        self.background_block_caches
            .borrow_mut()
            .retain(|id, _| live_ids.contains(id));
        // v1.13.11 N1.4: the visible-block warm windows follow the same
        // lifecycle — a closed/promoted pane's entry would otherwise pin its
        // last window forever.
        self.bg_visible_blocks
            .borrow_mut()
            .retain(|id, _| live_ids.contains(id));

        // v1.12.27b (P1-01): the Copy frame values shared by the phase
        // methods, assembled as a plain struct literal (pure data — no
        // constructor, no Default; 27a draw_ctx-anchor constraint).
        let frame = FrameDrawCore {
            draw_ctx,
            show_blocks,
            view_switched,
            cursor_blink_on,
            cursor_blink_phase,
            block_scroll,
            active_pane_rect,
            active_pane_id,
            active_pane_session_id,
            chrome_left,
            bg_r,
            bg_g,
            bg_b,
            clear_a,
            drawable_tex_size,
            vp_mismatch,
        };
        // v1.12.27b (P1-01): active-pane two-branch build moved to
        // draw_phases::draw_active_pane_content (baseline :742-927); its
        // `pending_hit_regions` / counter locals return as ActivePaneOutput.
        let pane_out = self.draw_active_pane_content(
            &frame,
            terminal,
            selection,
            grid,
            &overlays_fx,
            background_panes,
            &mut bg_stream,
            &mut glyph_stream,
        );
        // v1.12.27b (P1-01): destructure by value — active_vertices/hit_regions
        // feed the merge + the final bare field write, the counters go to the
        // present phase (baseline :928-935 + :1180).
        let ActivePaneOutput {
            active_vertices,
            hit_regions: pending_hit_regions,
            dirty_row_count,
            session_block_count,
            bv_rows_count,
        } = pane_out;
        if show_blocks && !active_vertices.is_empty() {
            let start = background_vertices.len();
            self.pane_vertex_ranges
                .borrow_mut()
                .push((active_pane_rect, start..start + active_vertices.len()));
        }
        let mut vertices = background_vertices;
        vertices.extend(active_vertices);

        // v1.12.27b (P1-01): scrollbar + overlay stack + present moved to
        // draw_phases (baseline :937-972 / :974-1109 / :1111-1175).
        self.draw_block_scrollbar(&frame, scroll_metrics, &mut vertices);
        self.draw_overlay_stack(
            &frame,
            terminal,
            grid,
            &overlays_fx,
            tab_bar,
            pane_layouts,
            &mut vertices,
        );
        self.draw_present_frame(
            &frame,
            drawable,
            &mut vertices,
            &mut bg_stream,
            &mut glyph_stream,
            dirty_row_count,
            session_block_count,
            bv_rows_count,
        );
        // Assign hit-test regions after encoding. Done here (not inside
        // `encode_and_present`) because `drawable` borrows `self.layer` for the
        // whole frame, so `&mut self` is unavailable — but `self.hit_regions` is
        // a disjoint field from `self.layer`, so the disjoint borrow is fine.
        self.hit_regions = pending_hit_regions;
    }
}
