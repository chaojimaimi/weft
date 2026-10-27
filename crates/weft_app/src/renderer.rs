// MetalRenderer ownership; runtime configuration lives in renderer/runtime.rs.
//! Metal GPU renderer for the terminal Grid.

use metal::{Device, MetalLayer};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use crate::glyph::GlyphAtlas;
use crate::paint::grid_cache::BlockLayoutCache;
use crate::paint::live_cache::{BlockScrollMetricsMemo, LiveLayoutCache};
use crate::paint::overlays::FindDrawState;
use weft_core::blocks::BlockId;
use weft_core::config::{FontConfig, Theme};

pub(crate) mod atlas_warmup;
// v1.13.5 T16b: bg block-pane vertex cache (gate in renderer/bg_block_cache.rs).
pub(crate) mod bg_block_cache;
// v1.15.2: overlay-stack + present phases, moved verbatim (zero-rewrite;
// sibling file keeps `pub(super)` scoped to the same renderer parent).
mod draw_overlays;
// v1.12.27b (P1-01): draw()'s phase family (all `&self`) + the pure-data
// frame structs, moved verbatim out of the 791-line draw() body.
mod draw_phases;
// v1.15.2: the draw() skeleton, moved verbatim (zero-rewrite; struct stays).
mod frame;
mod panes;
mod runtime;
mod tui_caret_anchor;
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
    pub(crate) minimum_contrast: f32,
    pub(crate) semantic_output_enabled: bool,
    /// v1.11.3 (PLAN_v1113 §3.3): SGR 1 bold on ANSI fg indexes 0-7
    /// resolves to the bright variants (palette[i+8]) when enabled.
    /// Origin-level substitution happens at the two fg resolve points
    /// (grid_instances / block_view), so flipping it needs BOTH cache
    /// invalidations (see `set_bold_is_bright`).
    pub(crate) bold_is_bright: bool,
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
    /// v1.7.3-C: Inline note editor draw state — set per-frame by the app.
    /// `None` when the note editor is closed. The renderer reads this to draw
    /// the top-center "Note: [buffer|]" card.
    pub note_editor_state: Option<crate::paint::overlays::NoteEditorDrawState>,
    /// v0.9 H1: Last-rendered tab bar hit-test rects. Each entry is
    /// `(tab_rect, close_rect, tab_index)`. Populated by `draw_tab_bar`
    /// v0.9 W2: block currently highlighted in the terminal view (set from
    /// the history panel click). The renderer draws an accent border around
    /// this block in block view. Cleared by the app after 1.5s.
    pub panel_highlight: Option<BlockId>,
    /// Block hover drives inline header actions.
    pub block_hovered: Option<BlockId>,
    pub block_selected: Option<BlockId>,
    pub block_action_hovered: Option<crate::block_component::BlockHeaderAction>,
    /// v1.14.0 (PLAN_v1140 T6): active pane's grid sub-row scroll offset
    /// (physical px, projected per-frame from `Pane::grid_scroll_px`).
    /// Consumed by `build_grid_instances` — bg instances shift by the exact
    /// px, glyph instances by `floor(px)`. Always 0.0 outside the grid view.
    pub(crate) grid_scroll_px: f32,
    /// v1.14.0 (PLAN_v1140 T4): per-frame hover fade progress snapshot
    /// ((key, progress) pairs projected off `InteractionState.hover_fades`)
    /// — block action buttons + the scrollbar thumb lerp their idle/hover
    /// visuals by it. Absent key = 0.0 (idle fallback).
    pub hover_fades: Vec<(crate::app::motion::HoverKey, f32)>,
    /// v1.7.3-C: Set of bookmarked block IDs (set per-frame by the app from
    /// `AnnotationStore`); block view draws a ★ before bookmarked headers.
    /// v1.11 audit C3 (PLAN_audit_fix_batch3): Arc — refcount-bump handoff.
    pub bookmarked_blocks: std::sync::Arc<std::collections::HashSet<BlockId>>,
    /// v1.8.2: Per-block AI diagnose state (set per-frame by the app).
    /// The block view renders an inline panel below the output of blocks
    /// that have a diagnose result or a pending request.
    pub block_diagnose_state:
        std::collections::HashMap<BlockId, crate::app_state::BlockDiagnoseState>,
    /// v1.8.2: Whether the local Ollama backend is configured. Mirrors
    /// `App::ai_state.is_configured()`; the renderer uses this to decide
    /// whether to draw the diagnose button on failed block headers.
    pub ai_configured: bool,
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
    /// v1.5.3: Brief config error shown in the bottom-left status badge
    /// when Settings is closed. See `set_config_status_hint` for semantics.
    pub(crate) config_status_hint: Option<String>,
    /// v1.11.1 (PLAN_v1111 §4.5): transient post-paste feedback
    /// ("已粘贴 …") drawn at the bottom-right of the content area.
    /// Cleared by `clear_expired_paste_toast` on the 1 Hz autosave tick
    /// after `PASTE_TOAST_TTL`. Suppressed while a config error hint is
    /// showing (config error takes priority).
    pub(crate) paste_toast: Option<(String, std::time::Instant)>,
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
    /// v1.10.23 (FIX_LIVE_BLOCK_SCROLL_PERF): live-block cumulative layout
    /// cache (visible-window O(log n), see paint::live_cache).
    pub(crate) live_layout_cache: RefCell<LiveLayoutCache>,
    /// v1.10.25 Batch 3 (FIX_SELECTION_AND_RESIZE_REMAINING): RESIZE_PROBE
    /// stage-4 gate — armed on resize(), consumed once at the next draw to
    /// log the first present after a Resized.
    pub(crate) resize_present_probe: std::cell::Cell<Option<std::time::Instant>>,
    /// v1.11.6 (PLAN_v1116 M2/D-i): true while macOS is live-resizing the
    /// window. The layer flips `presentsWithTransaction` on state change so
    /// frames commit atomically with the resized bounds (Warp precedent);
    /// `encode_and_present` also flushes the Core Animation transaction
    /// while active. Polled per-frame by the redraw controller.
    pub(crate) live_resize_active: bool,
    /// v1.12.2 B2 (PLAN_S2_render): `[window]
    /// presents_with_transaction_live_resize` config gate, injected once at
    /// construction. `true` restores the v1.11.6 rollback carrier (flip the
    /// layer + flush the transaction during live resize); `false` (the
    /// default) keeps async present and never flushes — the 0-33ms
    /// WindowServer wait stopped paying for itself once resize commits fell
    /// to ~3ms and every vsync has a frame.
    pub(crate) live_resize_flip_enabled: bool,
    /// v1.12.2 B2 (PLAN_S2_render): count of `CATransaction::flush()` calls
    /// that passed the config gate. Diagnostic/observability field — unit
    /// tests assert the gate (config off ⇒ the counter never moves);
    /// production cost is one increment per live-resize frame.
    pub(crate) core_animation_flushes: Cell<u32>,
    /// v1.10.23 change 2: exact `block_scroll_metrics` memo — the fingerprint
    /// covers every input, so wheel + same-frame scrollbar share one scan.
    pub(crate) scroll_metrics_memo: Cell<BlockScrollMetricsMemo>,
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
    /// Actual BlockView TUI caret painted this frame; also anchors native IME.
    pub(crate) block_view_tui_caret_area: Cell<Option<crate::ime::ImeCursorArea>>,
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
    /// T15b (PLAN_v11217 §3.10): previous frame's baked active-grid layout
    /// origin (x, y). A pane move (split drag, pane close re-layout) changes
    /// the baked row coordinates without touching dims, scroll, or dirty
    /// rows — same invalidation role as the background path's `entry.origin`
    /// fingerprint. NaN initial value so the first comparison always
    /// differs → first frame forces a full rebuild.
    pub(crate) prev_grid_origin: Cell<(f32, f32)>,
    /// T15b (PLAN_v11217 §3.10): identity of the pane whose grid the global
    /// per-row cache held during the last multi-pane frame. The cache is
    /// global (not per-pane), so a changed active pane forces one full
    /// rebuild; layout moves of the *same* pane are covered by
    /// `prev_grid_origin` instead. `None` until the first multi-pane frame.
    pub(crate) last_drawn_active_pane: Option<weft_core::pane_layout::PaneId>,
    /// v1.0 P0-c: Persistent offscreen texture used as the render target
    /// instead of drawing directly to the drawable. This enables GPU-side
    /// scroll blit: on scroll, copy the unchanged region within the
    /// offscreen texture (src_y=Δ → dst_y=0) via MTLBlitCommandEncoder,
    /// then render only the newly exposed rows with load_action=Load.
    /// The offscreen is blitted to the drawable at the end of the frame.
    /// Recreated on viewport resize.
    pub(crate) offscreen_texture: RefCell<Option<metal::Texture>>,
    /// v1.0 P0-c: Dimensions of the **valid rendered region** (w, h) in
    /// physical pixels — always equal to the current viewport, NOT the
    /// texture size. v1.12.2 B3-1 (PLAN_S2_render): the offscreen texture is
    /// capacity-sized (grow-only, 256-px rounded) and survives shrinks;
    /// `ensure_offscreen_texture` uses this field to decide whether the valid
    /// region grew into never-rendered texture (→ force full redraw).
    pub(crate) offscreen_dims: Cell<(f32, f32)>,
    /// v1.12.2 B3-2 (PLAN_S2_render): per-background-pane dual-stream row
    /// caches, keyed by `pane_session_id` (global, monotonic — closed panes'
    /// entries are pruned each frame from the live pane list). Gives
    /// background panes the same dirty-row incremental rebuild the active
    /// pane has had since v1.4.2; entries rest on PTY dirty rows and go full
    /// on dimension change (live-resize), scroll change, layout-origin move,
    /// or primary-screen mask movement.
    pub(crate) background_grid_row_caches:
        RefCell<HashMap<u64, crate::paint::grid::BackgroundGridRowCache>>,
    /// v1.13.5 T16b (PLAN_v11217 §3.11): per-background-pane block-view vertex
    /// cache, keyed by `pane_session_id` (monotonic — retained against the live
    /// pane list each frame). Gate: composite fingerprint + 60ms throttle
    /// (`renderer/bg_block_cache.rs`); color/font hooks clear it wholesale.
    pub(crate) background_block_caches:
        RefCell<HashMap<u64, crate::renderer::bg_block_cache::BgBlockCache>>,
    /// v1.12.2 B3-2 P1 fix (rust-reviewer): monotonically increasing invalida-
    /// tion counter for [`Self::background_grid_row_caches`]. Bumped by
    /// `set_theme` / `set_minimum_contrast` / `set_bold_is_bright` (colors are
    /// baked into cached rows) and `update_scale`. The background builder
    /// fingerprints this instead of `force_full_grid`, which multi-pane frames
    /// set unconditionally every frame.
    pub(crate) background_grid_generation: Cell<u64>,
    /// v1.12.2 B3-3 (PLAN_S2_render): per-namespace block output scan
    /// watermarks for the drag-time incremental atlas warmup. Namespaces are
    /// per-pane `pane_session_id`s — the active pane warms under its own id
    /// (P2 fix: BlockIds are per-Terminal, so a shared constant would leak
    /// watermarks across tab/focus switches). Cleared by `update_scale` /
    /// `rebuild_atlas` — a rebuilt atlas is empty, so stale "already
    /// scanned" marks would skip glyphs the new atlas needs (the
    /// fragmented-output bug class).
    pub(crate) block_scan_watermarks: RefCell<crate::renderer::atlas_warmup::BlockScanWatermarks>,
    /// v1.13.11 N1.4: per-background-pane VISIBLE block ids from the pane's
    /// last rebuild frame — the warm window for background panes (the global
    /// `block_layout_cache` cannot provide per-pane geometry). Written by
    /// `build_background_pane_content` (via the row-loop sink below) and
    /// consumed by `warm_background_pane_atlases`; pruned against the live
    /// pane list each frame (close/promote both prune).
    pub(crate) bg_visible_blocks: RefCell<HashMap<u64, Vec<weft_core::blocks::BlockId>>>,
    /// v1.13.11 N1.4 collection sink: `Some((pane, ids))` only while
    /// `build_background_pane_content` rebuilds a background block pane.
    /// The shared paint loop in `build_block_view_vertices` appends visible
    /// `LaidRow::Output` block ids; the active pane never sets the sink, so
    /// it can never write here.
    pub(crate) bg_visible_blocks_sink: RefCell<Option<(u64, Vec<weft_core::blocks::BlockId>)>>,
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
    pub(crate) upload_low_usage_frames: [crate::paint::metal_backend::LowUsageCounter; 2],
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
