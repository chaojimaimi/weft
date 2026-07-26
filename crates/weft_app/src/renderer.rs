// MetalRenderer resource ownership and frame orchestration. Runtime geometry
// and configuration methods live in renderer/runtime.rs.
//! Metal GPU renderer for the terminal Grid.

use metal::{Device, MetalLayer};
use std::cell::{Cell, RefCell};

use crate::glyph::GlyphAtlas;
// A5: vertex primitives + color helpers live in paint::primitives.
use crate::paint::grid_cache::BlockLayoutCache;
use crate::paint::overlays::FindDrawState;
use crate::paint::primitives::{color_to_normalized, push_quad};
use crate::paint::tab_bar::TabBarDrawState;
use weft_core::blocks::BlockId;
use weft_core::config::{FontConfig, Theme};
use weft_core::selection::SelectionHandler;
use weft_core::vt::Terminal;

mod atlas_warmup;
mod panes;
mod runtime;
pub use panes::PaneRenderInfo;

/// Metal GPU renderer: draws the terminal Grid to screen.
pub struct MetalRenderer {
    pub(crate) device: Device,
    pub(crate) queue: metal::CommandQueue,
    pub(crate) layer: MetalLayer,
    pub(crate) pipeline: metal::RenderPipelineState,
    pub(crate) sampler: metal::SamplerState,
    pub(crate) atlas: GlyphAtlas,
    pub(crate) viewport: (f32, f32),
    /// Retained for live font/atlas rebuild (config reload).
    pub(crate) scale: f64,
    /// Retained for live font/atlas rebuild (config reload).
    pub(crate) font_config: FontConfig,
    /// Active theme (default fg/bg/cursor/selection). Per-frame, so a
    /// `set_theme` call recolors the screen on the next draw.
    pub(crate) theme: Theme,
    /// Content padding in **physical** pixels (logical config value × scale).
    /// Cells are positioned `pad_x + col·cw`, `pad_y + row·ch`; the usable
    /// area for row/col math is the viewport minus `2·pad`.
    pub(crate) padding_x: f32,
    pub(crate) padding_y: f32,
    /// Window background opacity [0,1]. Below 1.0 the background is
    /// see-through (text/selection/cursor stay opaque); the cell bg alpha is
    /// scaled by this value per-frame, so a live `set_opacity` recolors
    /// instantly. The window's own transparency flag is set at startup, so
    /// crossing the 1.0 boundary needs a relaunch.
    pub(crate) opacity: f32,
    /// Last-rendered hit-test regions (foldable blocks, completion rows, etc.)
    /// in physical pixels, for click dispatch. Repopulated each draw.
    pub hit_regions: Vec<crate::overlay::HitRegion>,
    /// User-adjustable popup width scale (0.3–0.95 of viewport width).
    pub(crate) popup_width_scale: f32,
    /// User-adjustable popup max visible rows.
    pub(crate) popup_max_rows: usize,
    /// Context menu position + target block + keyboard selection. Set per-frame.
    pub context_menu_target: Option<(f32, f32, Option<weft_core::blocks::BlockId>, usize)>,
    /// Layout context for the current frame (viewport + cell + padding + clip).
    /// Constructed at the top of `draw()` and used by overlay builders to derive
    /// coordinates from semantic methods instead of hand-rolled f32 math.
    /// `None` before the first draw or after a resize before the next frame.
    pub layout_ctx: Option<crate::layout::LayoutCtx>,
    /// v0.8 B3 FindInGrid state — set per-frame by the app via `set_find_state`.
    /// `None` when the find bar is closed. The renderer reads this to draw the
    /// top banner + highlight the current match.
    pub find_state: Option<FindDrawState>,
    /// v0.9 H1: Last-rendered tab bar hit-test rects. Each entry is
    /// `(tab_rect, close_rect, tab_index)`. Populated by `draw_tab_bar`
    /// v0.9 W2: block currently highlighted in the terminal view (set from
    /// the history panel click). The renderer draws an accent border around
    /// this block in block view. Cleared by the app after 1.5s.
    pub panel_highlight: Option<BlockId>,
    /// F3-1: Block currently hovered by the mouse (set per-frame by the app
    /// from `InteractionState.block_hovered`). Drives inline copy/fold action
    /// buttons on the block header row.
    pub block_hovered: Option<BlockId>,
    /// F3-2: Spinner phase for the running-command activity indicator.
    /// Normalized to [0,1); the renderer maps it to a braille spinner glyph.
    /// Set to `-1.0` to disable (reduce-motion or no command running).
    pub spinner_phase: f32,
    /// F3-2: macOS Reduce Motion setting. When true, the spinner uses a
    /// static `●` instead of animated braille glyphs.
    pub reduce_motion: bool,
    /// F6: macOS Increase Contrast setting. When true, `UiColors` are
    /// strengthened via `with_increase_contrast()` and focus rings use
    /// thicker, fully-opaque edges. Synced from `window_runtime` at 1Hz.
    pub increase_contrast: bool,
    /// F3-3: User-overridden sidebar width in logical points. `None` falls
    /// back to the responsive `SidebarMetrics::for_logical_width`. Set by
    /// `set_sidebar_width` during drag, or synced from config on load.
    pub(crate) sidebar_width_override: Option<f32>,
    /// v0.9: cached cursor-blink state for the current frame, so overlay
    /// builders (palette, panel) can draw a blinking caret without it being
    /// threaded through every helper signature.
    pub(crate) cursor_blink_on: bool,
    /// v1.0 P0-a: Cache for block view layouts. Eliminates redundant O(n)
    /// per-frame wrapping computation for finished historical blocks.
    /// Uses `RefCell` because `draw()` holds an immutable borrow of
    /// `self.layer` (from `next_drawable`) across the entire frame, so
    /// `&mut self` is unavailable for cache mutation.
    pub(crate) block_layout_cache: RefCell<BlockLayoutCache>,
    /// Step 3: cached scroll metrics (total, visible, max_scroll) from the
    /// last draw() call. Read by `active_scrollbar_layout` in mouse handlers
    /// to avoid re-running `block_content_metrics_with_cache` (O(n)) on every
    /// mouse move event. None in grid view or before the first draw.
    /// Uses `Cell` because draw() holds an immutable borrow of `self.layer`
    /// across the entire frame (same constraint as block_layout_cache).
    pub(crate) cached_scroll_metrics: Cell<Option<(usize, usize, usize)>>,
    /// Batch 5 Step 2: cached panel scrollbar metrics (total_filtered,
    /// visible_rows, max_scroll) from the last `build_panel_vertices` call.
    /// Read by `active_panel_scrollbar_layout` in mouse handlers to avoid
    /// re-running `panel_filtered_count` (O(n) over all blocks) on every
    /// mouse move. None when the panel is closed or before the first draw
    /// that paints it.
    pub(crate) cached_panel_scroll_metrics: Cell<Option<(usize, usize, usize)>>,
    /// Batch 6 Step 1: per-frame `styled.line()` lookup count, read at
    /// `build_end` for `FrameCounters.styled_line_lookups`.
    pub(crate) styled_lookup_counter: Cell<usize>,
    /// Batch 7 Step 4: per-frame wall-clock μs for `push_block_output_text`,
    /// read at `build_end` for `FrameCounters.styled_paint_us`.
    pub(crate) styled_paint_us_counter: Cell<u64>,
    /// v1.4.2 Phase A: per-frame wall-clock μs for `build_grid_instances`,
    /// read at `build_end` for `FrameCounters.grid_build_us`.
    pub(crate) grid_build_us_counter: Cell<u64>,
    /// v1.4.1: bounded FIFO cache for completed-block styled-line vertices.
    pub(crate) styled_line_cache: RefCell<crate::paint::styled_line_cache::StyledLineCache>,
    /// Batch 6 Step 1: per-frame expanded block count from last layout pass.
    pub(crate) last_expanded_block_count: Cell<usize>,
    /// v1.0 P0-b / v1.4.2 Phase B3: Per-row dual-stream grid cache. Each
    /// entry holds one row's bg floats (8 per run) + glyph floats (16 per
    /// cell). Dirty rows are rebuilt; clean rows are reused. Eliminates
    /// per-frame iteration of all cells when only a few rows changed.
    pub(crate) grid_row_cache: RefCell<Vec<crate::paint::grid::GridRowDualCache>>,
    /// v1.0 P0-b: Force a full grid redraw on the next draw. Set by the caller
    /// on resize / theme / tab switch / selection change. Cleared after the
    /// full redraw is performed.
    pub(crate) force_full_grid: Cell<bool>,
    /// v1.0 P0-b: Previous frame's cursor row. The cursor cell renders
    /// differently (block/bar/underline overlay), so both the old and new
    /// cursor rows must be rebuilt when the cursor moves or blinks.
    pub(crate) prev_cursor_row: Cell<Option<usize>>,
    /// v1.0 P1.5-B2: Previous frame's cursor column. Together with
    /// `prev_cursor_row` and `prev_show_cursor`, detects cursor stability
    /// so the cursor row can be skipped when nothing moved or blinked.
    pub(crate) prev_cursor_col: Cell<Option<usize>>,
    /// v1.0 P1.5-B2: Previous frame's `show_cursor` parameter (encodes
    /// `cursor_visible && cursor_blink_on && prompt.is_none()`). When this
    /// flips (blink toggle / focus change / prompt open-close), the cursor
    /// row must be rebuilt to add or remove the cursor overlay.
    pub(crate) prev_show_cursor: Cell<bool>,
    /// v1.0 P1.5-B2: Set by `build_grid_instances` when no rows needed
    /// rebuilding this frame (no dirty rows, no cursor change, no scroll).
    /// `draw()` reads this to skip instance upload + draw call, relying on
    /// the offscreen `Load` action to preserve the previous frame's grid
    /// content. Saves the per-row rebuild loop + flatten memcpy + GPU
    /// upload for idle frames (target: < 0.5ms no-op frame).
    pub(crate) instances_unchanged: Cell<bool>,
    /// v1.0 P1.5-B2: Previous frame's `show_blocks` flag. When the view mode
    /// switches between block view and grid view (alt screen enter/exit), the
    /// grid_row_cache and offscreen content are stale — force a full rebuild.
    pub(crate) prev_show_blocks: Cell<bool>,
    /// Previous primary-screen ownership boundary. A changed boundary can
    /// expose rows whose cached instances were intentionally empty, so it
    /// participates in full-grid cache invalidation.
    pub(crate) prev_primary_screen_row_start: Cell<Option<usize>>,
    /// v1.0 P0-b: Cached grid dimensions (rows × cols) for cache invalidation
    /// on resize.
    pub(crate) grid_cache_dims: Cell<(usize, usize)>,
    /// v1.0 P0-b: Previous frame's scroll offset. When the user scrolls
    /// scrollback, the rendered cells come from history (not dirty-tracked),
    /// so a full redraw is needed.
    pub(crate) prev_scroll_offset: Cell<usize>,
    /// v1.0 P0-c: Persistent offscreen texture used as the render target
    /// instead of drawing directly to the drawable. This enables GPU-side
    /// scroll blit: on scroll, copy the unchanged region within the
    /// offscreen texture (src_y=Δ → dst_y=0) via MTLBlitCommandEncoder,
    /// then render only the newly exposed rows with load_action=Load.
    /// The offscreen is blitted to the drawable at the end of the frame.
    /// Recreated on viewport resize.
    pub(crate) offscreen_texture: RefCell<Option<metal::Texture>>,
    /// v1.0 P0-c: Dimensions of the current offscreen texture (w, h) in
    /// physical pixels. Used to detect resize → recreate offscreen.
    pub(crate) offscreen_dims: Cell<(f32, f32)>,
    /// v1.0 P0-c: Pending scroll delta captured during build_grid_vertices
    /// (via grid.take_pending_scroll()). Used by the draw() epilogue to
    /// decide whether to issue a GPU blit before the render pass.
    pub(crate) pending_scroll_delta: Cell<i32>,
    /// v1.0 P0-c: Cached result of the `force_full` computation from
    /// build_grid_vertices. Stashed on self so the draw() epilogue (which
    /// runs after build_grid_vertices returns) can decide whether to skip
    /// the GPU scroll blit on forced-full frames (resize/theme/tab-switch).
    pub(crate) force_full_cached: Cell<bool>,
    /// v1.0 P1.5-B0: Triple-buffered vertex buffer ring. Avoids per-frame
    /// `new_buffer_with_data` allocation (~11520 vertices × 48 bytes = 540KB
    /// per frame). Three buffers ensure GPU never stalls on CPU writes
    /// (triple-buffering covers 2 frames of GPU latency).
    pub(crate) vertex_buffer_ring: RefCell<Vec<metal::Buffer>>,
    /// v1.0 P1.5-B0: Current index into `vertex_buffer_ring`. Advanced
    /// each frame; the buffer at this index is reused (grown if needed).
    pub(crate) vertex_buffer_ring_idx: Cell<usize>,
    /// v1.0 P1.5-B0: Capacity of each buffer in the ring (in bytes). When
    /// vertex data exceeds this, a new larger buffer is allocated.
    pub(crate) vertex_buffer_capacity: Cell<u64>,
    /// v1.0 P1.5-B1: Instanced render pipeline for grid cells. Renders
    /// each cell as a single 64-byte instance (origin/size/uv/fg/bg) drawn
    /// against a static 4-vertex quad + 6-index buffer, replacing the
    /// legacy 6-vertex-per-cell emission (~288 B/cell).
    pub(crate) instanced_pipeline: metal::RenderPipelineState,
    /// v1.0 P1.5-B1: Static index buffer for the unit quad (6 u16 indices).
    /// Bound once per grid draw; never resized.
    pub(crate) index_buffer: metal::Buffer,
    /// v1.0 P1.5-B1: Triple-buffered instance data ring. Each frame writes
    /// the active tab's grid instances into one buffer; the previous two
    /// buffers stay alive so the GPU can finish reading them.
    pub(crate) instance_ring: RefCell<Vec<metal::Buffer>>,
    /// v1.0 P1.5-B1: Current index into `instance_ring`. Advanced each
    /// frame; the buffer at this index is reused (grown if needed).
    pub(crate) instance_ring_idx: Cell<usize>,
    /// v1.0 P1.5-B1: Capacity of each buffer in `instance_ring` (bytes).
    /// When instance data exceeds this, a new larger buffer is allocated.
    pub(crate) instance_capacity: Cell<u64>,
    /// v1.4.2 Phase B3: Background-stream pipeline + triple-buffered ring.
    /// Drawn before the glyph stream (instanced_pipeline) per pane.
    pub(crate) bg_stream: crate::paint::metal_backend::BgStream,
    /// v1.2 R3 task 6: Per-frame trace recorder (layout/build/encode segments
    /// and counters). Stored in a `RefCell` because `draw()` holds an immutable
    /// borrow of `self.layer` for the whole frame, so `&mut self` is
    /// unavailable for segment bookkeeping.
    pub(crate) frame_trace: RefCell<crate::frame_trace::FrameTraceRecorder>,
    /// v1.2 R3 task 6: Current frame id, stamped onto the Metal command buffer
    /// label so the async `add_completed_handler` can correlate GPU completion
    /// back to the originating frame. Set by `draw()`'s caller each frame.
    pub(crate) frame_id: Cell<u64>,
    /// v1.3 Batch 5 / v1.4.2 Phase B3: Per-pane dual-stream instance segments
    /// for the current frame. Each entry carries a pane's scissor rect + bg
    /// and glyph float ranges. `RefCell` because `draw()` holds an immutable
    /// `self.layer` borrow for the whole frame. Cleared at top of each `draw()`.
    pub(crate) pane_instance_ranges: RefCell<Vec<crate::paint::metal_backend::PaneInstanceSegment>>,
    /// Per-pane ranges in the legacy vertex buffer. Block-view base content
    /// uses this path and needs the same hard GPU clipping as grid instances.
    pub(crate) pane_vertex_ranges: RefCell<Vec<(crate::layout::Rect, std::ops::Range<usize>)>>,
}

impl MetalRenderer {
    // new() moved to paint/metal_backend.rs (M4 step 3).

    /// Draw the terminal Grid (and optional overlays) to screen.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        terminal: &Terminal,
        selection: &mut SelectionHandler,
        cursor_blink_on: bool,
        cursor_blink_phase: f32,
        overlays: &crate::overlay::OverlayStack<'_>,
        block_scroll: usize,
        // v0.8 U6: block-content metrics (total_rows, visible_rows,
        // max_scroll) for the dynamic scrollbar thumb. None in grid view.
        scroll_metrics: Option<(usize, usize, usize)>,
        scrollbar_emphasized: bool,
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
        // Whether a standalone titlebar strip (no tab bar) needs painting.
        // v1.2: always false now — the tab bar is always drawn.
        let single_tab_titlebar = false;

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
        let cursor = &grid.cursor;

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
        // Passed as partial borrows (`&mut self.atlas`, `&self.find_state`)
        // rather than a `&mut self` method call so they stay disjoint from
        // the immutable `self.layer` borrow held by `drawable` for the frame.
        Self::warm_atlas(
            &mut self.atlas,
            self.viewport.1,
            &self.find_state,
            terminal,
            grid,
            panel,
            prompt,
            completions,
            palette,
            settings,
            tab_bar,
        );
        Self::warm_background_pane_atlases(
            &mut self.atlas,
            self.viewport.1,
            background_panes,
            tab_bar,
        );

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
        let mut pending_hit_regions: Vec<crate::overlay::HitRegion> = Vec::new();
        // R3 task 6: BUILD-VERTICES segment starts here (first vertex builder).
        self.frame_trace.borrow_mut().build_start();
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

        // Render background panes before active-pane content. Each pane uses
        // its own Terminal view mode; LayoutCtx is swapped per pane so block
        // vertices and grid instances share the same split-tree geometry.
        // v1.4.2 Phase B3: background grid panes append to dual-stream buffers
        // (bg_stream + glyph_stream) and push a `PaneInstanceSegment` (rect +
        // ranges). Background block panes still emit legacy vertices.
        let mut background_vertices = Vec::new();
        let mut bg_stream: Vec<f32> = Vec::new();
        let mut glyph_stream: Vec<f32> = Vec::new();
        if !background_panes.is_empty() {
            let base_ctx = self.layout_ctx.expect("LayoutCtx built at draw() entry");
            // Split-tree rects are absolute viewport coordinates; `for_pane`
            // converts them to the offset + clip expected by LayoutCtx.
            for bg in background_panes {
                let bg_ctx = base_ctx.for_pane(bg.rect);
                self.layout_ctx = Some(bg_ctx);
                let start = background_vertices.len();
                background_vertices.extend(self.build_background_pane_content(
                    bg,
                    &mut bg_stream,
                    &mut glyph_stream,
                ));
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
            // When the active pane changes (e.g. after a split) the cache may
            // hold content from a different pane; a newly-created pane may
            // also have no dirty rows. Force a full rebuild of the active
            // pane so its grid renders correctly on the first multi-pane frame.
            self.force_full_grid_redraw();
        }

        let active_vertices: Vec<f32> = if show_blocks {
            let (v, regions, bv_rows) = if let Some(p) = prompt {
                // v1.3 multi-pane: use the active pane's LayoutCtx (which
                // carries pane_origin + clip set by the background-pane block
                // above) instead of the stale full-viewport `ctx` local.
                let active_ctx = self
                    .layout_ctx
                    .as_ref()
                    .expect("LayoutCtx built at draw() entry");
                let box_top_y = crate::layout::layout_prompt(
                    active_ctx,
                    p.lines.len(),
                    p.cursor.0,
                    0,
                    p.scroll_offset,
                )
                .box_rect[1];
                self.build_block_view_vertices(
                    crate::paint::block_view_model::BlockViewPaintModel {
                        blocks: terminal.block_tracker().session_blocks(),
                        region_bottom_y: box_top_y,
                        cwd: p.cwd,
                        git_branch: terminal.git_branch(),
                        live: None,
                        block_scroll,
                        viewport_rows: terminal.grid().num_rows,
                        block_hovered: self.block_hovered,
                        spinner_phase: self.spinner_phase,
                        find_block_highlight: self
                            .find_state
                            .as_ref()
                            .and_then(|find| find.block_highlight),
                        palette: terminal.palette(),
                        cache_namespace: active_pane_session_id,
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
                        // v1.3 multi-pane: region_bottom_y must be pane-local
                        // (clip's bottom edge), not the full vp_h - pad_y.
                        region_bottom_y: self
                            .layout_ctx
                            .as_ref()
                            .expect("LayoutCtx built at draw() entry")
                            .bottom(),
                        cwd: terminal.cwd(),
                        git_branch: terminal.git_branch(),
                        live: terminal.block_tracker().in_flight(),
                        block_scroll,
                        viewport_rows: terminal.grid().num_rows,
                        block_hovered: self.block_hovered,
                        spinner_phase: self.spinner_phase,
                        find_block_highlight: self
                            .find_state
                            .as_ref()
                            .and_then(|find| find.block_highlight),
                        palette: terminal.palette(),
                        cache_namespace: active_pane_session_id,
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
            v
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
            if terminal.is_alt_screen_active() || terminal.primary_screen_app_active() {
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
                prompt.is_some(),
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
        if show_blocks && !active_vertices.is_empty() {
            let start = background_vertices.len();
            self.pane_vertex_ranges
                .borrow_mut()
                .push((active_pane_rect, start..start + active_vertices.len()));
        }
        let mut vertices = background_vertices;
        vertices.extend(active_vertices);

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
                let scrollbar_ctx = self
                    .layout_ctx
                    .as_ref()
                    .expect("LayoutCtx built at draw() entry");
                if let Some(scrollbar) = crate::scrollbar_component::scrollbar_layout(
                    scrollbar_ctx,
                    total,
                    visible,
                    max_scroll,
                    block_scroll,
                ) {
                    // v1.0: label_c (70% fg + 30% bg) — was accent_dim.
                    let fg_v = color_to_normalized(self.theme.foreground);
                    let bg_v = color_to_normalized(self.theme.background);
                    let thumb_color = [
                        fg_v[0] * 0.70 + bg_v[0] * 0.30,
                        fg_v[1] * 0.70 + bg_v[1] * 0.30,
                        fg_v[2] * 0.70 + bg_v[2] * 0.30,
                        1.0,
                    ];
                    let (su, sv, suw, svh) = self.space_uv();
                    let bg_uv = [su, sv + svh, su + suw, sv];
                    push_quad(
                        &mut vertices,
                        crate::scrollbar_component::visual_thumb(&scrollbar, scrollbar_emphasized),
                        bg_uv,
                        [0.0; 4],
                        thumb_color,
                    );
                }
            }
        }

        // F2 P0-2: subtle status hint for passthrough/running states.
        if prompt.is_none() {
            vertices.extend_from_slice(&self.build_status_hint_vertices(terminal));
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
                let n_lines = prompt.map(|p| p.lines.len().max(1)).unwrap_or(1);
                let cursor = prompt.map(|p| p.cursor).unwrap_or((0, 0));
                let ctx = self.layout_ctx.expect("LayoutCtx built at draw() entry");
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
        if let Some(find) = &self.find_state.clone() {
            vertices.extend_from_slice(&self.build_find_vertices(find));
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
        } else {
            // v1.1: single-tab mode — still paint a theme-color strip at the
            // top (titlebar_height tall) so the transparent titlebar's traffic
            // lights sit on a themed background instead of overlapping text.
            // No tabs/dividers/close buttons; just the background quad.
            if single_tab_titlebar {
                let bg = color_to_normalized(self.theme.background);
                let strip_bg = if bg[0] + bg[1] + bg[2] < 1.5 {
                    [bg[0] * 0.85, bg[1] * 0.85, bg[2] * 0.85, 1.0]
                } else {
                    [
                        bg[0] + (1.0 - bg[0]) * 0.5,
                        bg[1] + (1.0 - bg[1]) * 0.5,
                        bg[2] + (1.0 - bg[2]) * 0.5,
                        1.0,
                    ]
                };
                let h = self.titlebar_height();
                let vp_w = self.viewport.0;
                push_quad(
                    &mut vertices,
                    [0.0, 0.0, vp_w, h],
                    [0.0; 4],
                    [0.0; 4],
                    strip_bg,
                );
            }
        }

        // v1.3.1 Batch 7: pane dividers + active-pane focus ring, drawn last
        // so they stay visible atop the panes. No-op in single-pane tabs.
        // content_rect x0 includes chrome_left so horizontal dividers don't
        // over-extend across the sidebar push region.
        crate::paint::pane_dividers::push_pane_overlays(
            &mut vertices,
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

        // R3 task 6: BUILD-VERTICES segment ends; ENCODE segment starts.
        // Step 1: drain per-frame block layout cache hit/miss counters.
        let (cache_hits, cache_misses) =
            self.block_layout_cache.borrow_mut().take_hit_miss_counts();
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
        let grid_upload_bytes =
            (vertices.len() + bg_stream.len() + glyph_stream.len()) as u64 * 4;
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
            });
        self.frame_trace.borrow_mut().encode_start();

        self.encode_and_present(
            drawable,
            &vertices,
            &bg_stream,
            &glyph_stream,
            (bg_r, bg_g, bg_b, clear_a),
            drawable_tex_size,
            vp_mismatch,
            view_switched,
        );
        // Assign hit-test regions after encoding. Done here (not inside
        // `encode_and_present`) because `drawable` borrows `self.layer` for the
        // whole frame, so `&mut self` is unavailable — but `self.hit_regions` is
        // a disjoint field from `self.layer`, so the disjoint borrow is fine.
        self.hit_regions = pending_hit_regions;
    }
}
