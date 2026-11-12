//! Background-pane base-content construction (v1.20.3 WP1 1A: moved out of
//! `panes.rs` for line-count debt — pure code motion, no behavior change).
//!
//! Contents: the shared `PaneBaseView` discriminator, the unfocused editor
//! prompt probe, and the `MetalRenderer` warm/build pair that produces one
//! background pane's glyphs and base vertices (grid dual-stream or cached
//! block-view legacy vertices). Reachability is unchanged: the two renderer
//! methods stay visible to the whole `crate::renderer` module tree, and the
//! three free items stay visible to `panes` and its descendants (its tests).

use std::collections::HashMap;

use super::PaneRenderInfo;
use crate::glyph::GlyphAtlas;
use crate::paint::block_view_model::BlockViewPaintModel;
use crate::paint::overlays::FindDrawState;
use crate::paint::prompt::PromptDrawParams;
use crate::paint::region_cache::{live_output_version_source, region_source_for};
use crate::paint::tab_bar::TabBarDrawState;
use crate::renderer::bg_block_cache;
use crate::renderer::MetalRenderer;
use weft_core::input::InputMode;
use weft_core::selection::SelectionHandler;
use weft_core::vt::Terminal;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PaneBaseView {
    Block,
    Grid,
}

#[inline]
pub(super) fn pane_base_view(terminal: &Terminal) -> PaneBaseView {
    if terminal.show_block_view() {
        PaneBaseView::Block
    } else {
        PaneBaseView::Grid
    }
}

#[inline]
pub(super) fn background_prompt<'a>(pane: &'a PaneRenderInfo<'a>) -> Option<PromptDrawParams<'a>> {
    (pane.terminal.effective_input_mode() == InputMode::Editor).then(|| PromptDrawParams {
        focused: false,
        cwd: pane.terminal.cwd(),
        lines: &pane.terminal.editor().buffer.lines,
        cursor: pane.terminal.editor().buffer.cursor,
        preedit: None,
        preedit_cursor: None,
        search: pane.terminal.editor().search_view(),
        selection: None,
        scroll_offset: pane.terminal.editor().buffer.scroll_offset,
        submit_on_ctrl_enter: pane.submit_on_ctrl_enter,
    })
}

impl MetalRenderer {
    #[allow(clippy::too_many_arguments)] // shared warm inputs; mirrors warm_atlas's arity
    #[inline]
    pub(in crate::renderer) fn warm_background_pane_atlases(
        atlas: &mut GlyphAtlas,
        viewport_h: f32,
        panes: &[PaneRenderInfo<'_>],
        tab_bar: &TabBarDrawState,
        // v1.12.2 B3-3 (PLAN_S2_render): per-pane namespace block scan —
        // namespaces must be per-pane, never a shared constant.
        watermarks: &std::cell::RefCell<crate::renderer::atlas_warmup::BlockScanWatermarks>,
        // v1.13.11 N1/N1.4: the shared layout/live caches + per-pane visible
        // block sets feed warm_block_view_glyphs (background panes cannot
        // derive per-pane geometry from the global layout cache).
        block_cache: &crate::paint::grid_cache::BlockLayoutCache,
        live_cache: &crate::paint::live_cache::LiveLayoutCache,
        bg_visible_blocks: &HashMap<u64, Vec<weft_core::blocks::BlockId>>,
        // v1.13.11: the header-band height needs the renderer's scale (the
        // associated-fn form keeps the `&mut atlas` partial borrow).
        scale: f64,
    ) {
        let no_find: Option<FindDrawState> = None;
        let no_note: Option<crate::paint::overlays::NoteEditorDrawState> = None;
        for pane in panes {
            let prompt = background_prompt(pane);
            Self::warm_atlas(
                atlas,
                viewport_h,
                &no_find,
                &no_note,
                pane.terminal,
                pane.terminal.grid(),
                None,
                prompt.as_ref(),
                None,
                None,
                None,
                None,
                tab_bar,
                // Background panes don't render diagnose panels.
                &[],
                // v1.20.3 WP4: background panes don't render the paste toast
                // either (it lives on the active renderer state).
                None,
            );
            // v1.13.11 N1.4: this pane's block outputs warm from its previous
            // rebuild's visible set ∪ its newest 16 blocks (the once-ever
            // watermark machinery makes the fixed window cheap).
            Self::warm_block_view_glyphs(
                atlas,
                pane.terminal.block_tracker(),
                pane.terminal.show_block_view(),
                pane.pane_session_id,
                bg_visible_blocks
                    .get(&pane.pane_session_id)
                    .map(|v| v.as_slice()),
                pane.terminal.cwd(),
                pane.terminal.git_branch(),
                viewport_h,
                pane.block_scroll,
                pane.terminal.grid().num_rows,
                scale,
                block_cache,
                live_cache,
                watermarks,
            );
        }
    }

    /// Build one background pane's base content. Block-view panes emit legacy
    /// vertices bounded by LayoutCtx.clip; grid panes emit a dual-stream
    /// `GridInstanceBatch` whose bg/glyph float offsets are appended to the
    /// caller's flat buffers and recorded as a `PaneInstanceRanges` entry
    /// (paired with the pane's scissor rect). Interactive state stays
    /// active-only.
    ///
    /// v1.13.5 T16b (PLAN_v11217 §3.11): the block branch is gated by the
    /// per-pane vertex cache (`renderer/bg_block_cache.rs`) — composite
    /// fingerprint + 60ms throttle. Vertices are appended into
    /// `background_vertices` (legacy stream, like the grid branch's dual
    /// streams); returns whether the cache REBUILT this frame (test /
    /// observability; false for grid panes).
    #[inline]
    pub(in crate::renderer) fn build_background_pane_content(
        &self,
        pane: &PaneRenderInfo<'_>,
        background_vertices: &mut Vec<f32>,
        bg_stream: &mut Vec<f32>,
        glyph_stream: &mut Vec<f32>,
    ) -> bool {
        match pane_base_view(pane.terminal) {
            PaneBaseView::Grid => {
                let bg_start = bg_stream.len();
                let glyph_start = glyph_stream.len();
                // v1.12.2 B3-2: per-pane incremental rebuild — the returned
                // count reports how many rows were dirty this frame.
                let (pane_batch, rebuilt_rows) = self
                    .build_grid_instances_for_background_pane(pane.terminal, pane.pane_session_id);
                bg_stream.extend(pane_batch.bg_stream);
                glyph_stream.extend(pane_batch.glyph_stream);
                let bg_end = bg_stream.len();
                let glyph_end = glyph_stream.len();
                // v1.14.1 (F2): the clip decision is per-pane at segment
                // construction — background panes bake rows at a 0 px shift
                // (grid.rs), so no sub-row clip (identity rect; the active
                // pane's px must not clip this pane). No prompt is passed:
                // a background grid pane paints no editor box.
                let pane_clip_rect = crate::paint::grid::grid_pane_clip_rect(
                    pane.rect,
                    &self.layout_ctx.expect("background pane LayoutCtx"),
                    None,
                    pane.terminal.grid().num_rows,
                    false,
                );
                self.pane_instance_ranges.borrow_mut().push((
                    pane_clip_rect,
                    crate::paint::grid_instances::PaneInstanceRanges {
                        bg_range: (bg_start, bg_end),
                        glyph_range: (glyph_start, glyph_end),
                    },
                ));
                tracing::debug!(
                    bg_rect = ?pane.rect,
                    bg_run_count = (bg_end - bg_start) / 8,
                    glyph_instance_count = (glyph_end - glyph_start) / 16,
                    grid_rows = pane.terminal.grid().num_rows,
                    grid_cols = pane.terminal.grid().num_cols,
                    rebuilt_rows,
                    "built background grid pane"
                );
                false
            }
            PaneBaseView::Block => {
                // F7-2 (PLAN_v1184 §2-F7-2): the background pane's BLOCK
                // rebuild is skipped WHOLESALE while the rewrap freeze is
                // armed — not merely fingerprint-gated. The rebuild runs
                // `build_block_view_vertices`, whose internal `sync_blocks`
                // pushes THIS pane's block set into the SHARED
                // `block_layout_cache`; since F7-1d the panes' entries
                // coexist within budget (mutual retention), but the
                // over-budget foreign-eviction arm can still drop the
                // active pane's tables mid-animation — churn the freeze
                // exists to stop, so the early return stays. Trade (plan
                // §2-F7-2): the previous build keeps serving (content
                // frozen, absolutely baked) for the whole freeze window (≤
                // the animation + flush tail) — the same stale-render the
                // 60ms throttle already shows, widened. v1.18.10: the
                // served stream is re-based onto the current pane rect at
                // draw time (renderer/panes/affine.rs — identity rect keeps
                // the byte-for-byte path), so the window shows
                // proportionally stretched-in-place content instead of
                // misplaced blocks; a cold pane (no cached entry) renders
                // nothing until the freeze lifts.
                if crate::paint::zoom_render::rewrap_freeze_active() {
                    if let Some(entry) = self
                        .background_block_caches
                        .borrow()
                        .get(&pane.pane_session_id)
                    {
                        // v1.18.10: identity rect keeps the byte-for-byte serve.
                        super::affine::extend_cached_vertices(
                            &entry.vertices,
                            entry.fingerprint.rect,
                            pane.rect,
                            background_vertices,
                        );
                    }
                    tracing::debug!(
                        bg_rect = ?pane.rect,
                        pane_session_id = pane.pane_session_id,
                        "background block pane rebuild frozen (rewrap freeze armed)"
                    );
                    return false;
                }
                let editor_mode = pane.terminal.effective_input_mode() == InputMode::Editor;
                let prompt = background_prompt(pane);
                // v1.13.5 T16b: composite gate key — every input that bakes
                // into the vertices (see BgBlockFingerprint's field docs).
                let fingerprint = bg_block_cache::BgBlockFingerprint {
                    // v1.19.0 (PLAN_v1190 §4 P1-4/O2/N6): when the region
                    // track is ON the live block's geometry derives from the
                    // grid, so the version component rides `grid.revision`
                    // (any layout-affecting grid mutation bumps it); flag OFF
                    // keeps the capture ledger's `live_output_version`.
                    live_output_version: live_output_version_source(pane.terminal),
                    screen_head_lines: pane.terminal.screen_head_lines(),
                    cwd: pane.terminal.cwd().map(str::to_string),
                    git_branch: pane.terminal.git_branch().map(str::to_string),
                    theme_generation: self.background_grid_generation.get(),
                    cell_dims: (self.cell_width(), self.cell_height()),
                    rect: pane.rect,
                    block_scroll: pane.block_scroll,
                    editor_mode,
                    // v1.20.0 (D2e): the chrome bakes into these vertices.
                    complete_block_overlay: self.complete_block_overlay,
                };
                let now = std::time::Instant::now();
                let action = {
                    let caches = self.background_block_caches.borrow();
                    caches.get(&pane.pane_session_id).map_or(
                        bg_block_cache::BgBlockCacheAction::RebuildNow,
                        |entry| {
                            bg_block_cache::bg_block_cache_action(
                                Some((&entry.fingerprint, entry.last_rebuild)),
                                &fingerprint,
                                now,
                                bg_block_cache::BG_BLOCK_CACHE_THROTTLE,
                            )
                        },
                    )
                };
                if matches!(
                    action,
                    bg_block_cache::BgBlockCacheAction::Reuse
                        | bg_block_cache::BgBlockCacheAction::Throttled
                ) {
                    // Reuse (or throttle-hold): serve the previous build —
                    // same v1.18.10 rect rule as the freeze branch (Reuse
                    // implies equal rects; a Throttled rect delta inside the
                    // 60ms composite window serves the affine re-base).
                    let caches = self.background_block_caches.borrow();
                    let entry = caches
                        .get(&pane.pane_session_id)
                        .expect("Reuse/Throttled imply a cache entry");
                    super::affine::extend_cached_vertices(
                        &entry.vertices,
                        entry.fingerprint.rect,
                        pane.rect,
                        background_vertices,
                    );
                    tracing::debug!(
                        bg_rect = ?pane.rect,
                        ?action,
                        "reused background block pane vertices"
                    );
                    return false;
                }
                let ctx = self.layout_ctx.expect("background pane LayoutCtx");
                let region_bottom_y = prompt.as_ref().map_or_else(
                    || ctx.bottom(),
                    |p| {
                        crate::paint::prompt::prompt_layout_for_buffer(&ctx, p.lines, p.cursor)
                            .1
                            .box_rect[1]
                    },
                );
                let mut selection = SelectionHandler::new();
                // v1.13.11 N1.4: arm the visible-block collection sink — the
                // shared paint loop appends this rebuild's visible output
                // block ids; drained into `bg_visible_blocks` right after the
                // build. The active pane never arms the sink.
                *self.bg_visible_blocks_sink.borrow_mut() =
                    Some((pane.pane_session_id, Vec::new()));
                let (mut vertices, _, _) = self.build_block_view_vertices(
                    BlockViewPaintModel {
                        blocks: pane.terminal.block_tracker().session_blocks(),
                        live_head_lines: pane.terminal.screen_head_lines(),
                        region_bottom_y,
                        cwd: pane.terminal.cwd(),
                        git_branch: pane.terminal.git_branch(),
                        live: (!editor_mode)
                            .then(|| pane.terminal.block_tracker().in_flight())
                            .flatten(),
                        block_scroll: pane.block_scroll,
                        viewport_rows: pane.terminal.grid().num_rows,
                        block_hovered: None,
                        block_selected: None,
                        block_action_hovered: None,
                        spinner_phase: -1.0,
                        find_block_highlight: None,
                        palette: pane.terminal.palette(),
                        cache_namespace: pane.pane_session_id,
                        block_diagnose_state: &std::collections::HashMap::new(),
                        ai_configured: false,
                        tui_cursor: None,
                        tui_preedit: None,
                        cursor_blink_on: false,
                        is_alt: pane.terminal.is_alt_screen_active(),
                        now: std::time::SystemTime::now(),
                        // v1.19.0 (PLAN_v1190 §4 P1-1): region track gated
                        // exactly like `live` — editor-mode panes never enter
                        // the region layout guard.
                        region: (!editor_mode)
                            .then(|| region_source_for(pane.terminal, pane.terminal.grid()))
                            .flatten(),
                    },
                    &mut selection,
                );
                // Drain the collection sink into the pane's warm window
                // (this rebuild's visible block set). P3-2: the sink appends
                // once per visible OUTPUT row (O(visible rows), one block
                // spans many rows) — blocks are contiguous in the block-view
                // layout, so a sort+dedup (order is irrelevant to the
                // warm-window consumer) keeps the stored window
                // O(visible blocks) instead of O(visible rows).
                if let Some((ns, mut ids)) = self.bg_visible_blocks_sink.borrow_mut().take() {
                    ids.sort_unstable();
                    ids.dedup();
                    self.bg_visible_blocks.borrow_mut().insert(ns, ids);
                }
                if let Some(prompt) = prompt {
                    vertices.extend(self.build_prompt_vertices(&prompt, 0.0, false));
                }
                // Store, then append the stored copy (one authoritative blob).
                {
                    let mut caches = self.background_block_caches.borrow_mut();
                    caches.insert(
                        pane.pane_session_id,
                        bg_block_cache::BgBlockCache {
                            vertices,
                            fingerprint,
                            last_rebuild: now,
                        },
                    );
                    let entry = caches.get(&pane.pane_session_id).expect("just inserted");
                    background_vertices.extend_from_slice(&entry.vertices);
                }
                tracing::debug!(bg_rect = ?pane.rect, "built background block pane");
                true
            }
        }
    }
}
