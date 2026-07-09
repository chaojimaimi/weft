//! Metal GPU renderer for the terminal Grid.

use core_graphics_types::geometry::CGSize;
use metal::{
    CompileOptions, Device, MTLClearColor, MTLIndexType, MTLLoadAction, MTLPixelFormat,
    MTLPrimitiveType, MTLResourceOptions, MTLStoreAction, MTLVertexFormat, MetalLayer,
    RenderPassDescriptor, RenderPipelineDescriptor, SamplerDescriptor, VertexDescriptor,
};
use objc2::msg_send;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use tracing::info;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

use crate::glyph::GlyphAtlas;
use weft_core::blocks::{Block, BlockId};
use weft_core::config::{FontConfig, Theme};
use weft_core::grid::{CellColor, CellFlags, CellWidth, Color, CursorStyle};
use weft_core::selection::SelectionHandler;
use weft_core::syntax::{self, TokenKind};
use weft_core::vt::Terminal;

/// Iterator yielding wrapped row chunks of `text` at `cols` columns. Each
/// yielded `String` fits within `cols` columns (respecting wide-char widths).
/// The first yielded chunk is the top row, subsequent chunks are continuation
/// rows below it.
fn wrap_line_chunks(text: &str, cols: usize) -> impl Iterator<Item = String> {
    let mut chunks: Vec<String> = Vec::new();
    if cols == 0 {
        chunks.push(text.to_string());
        return chunks.into_iter();
    }
    let mut current = String::new();
    let mut col = 0usize;
    for c in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width_cjk(c).unwrap_or(0);
        if w == 0 {
            continue;
        }
        if col + w > cols {
            chunks.push(std::mem::take(&mut current));
            col = 0;
        }
        current.push(c);
        col += w;
    }
    chunks.push(current);
    chunks.into_iter()
}

// ── v1.0 P0-a: Block layout cache ──────────────────────────────────────
//
// Finished blocks have immutable `output` (it's a detached snapshot), so
// the per-line wrapping computation (`wrapped_row_count` / `wrap_line_chunks`)
// only needs to run once per block — unless `cols` changes (resize) or the
// block's content/collapse state changes. This cache eliminates the
// O(total_output_chars) per-frame cost reintroduced when the
// `MAX_LAYOUT_LINES` cap was removed from historical blocks.
//
// The live in-flight block is NOT cached (its output streams every frame).

/// Pre-computed wrapping data for a single output line of a block.
#[derive(Clone)]
struct CachedLine {
    /// 0-based line index within the block's output (before trimming).
    idx: usize,
    /// Byte offset of this line's start within `block.output`.
    byte_start: usize,
    /// Byte offset of this line's end (exclusive) within `block.output`.
    byte_end: usize,
    /// Pre-wrapped chunks (owned via `Rc` for cheap sharing between the
    /// cache and the per-frame `LaidRow` entries). Usually 1 element;
    /// more for lines that exceed `cols` columns.
    chunks: Rc<[String]>,
}

/// Cached layout for a single finished block.
#[derive(Clone)]
struct CachedBlockLayout {
    /// Snapshot of `block.output.len()` — if the current block's output
    /// length differs, the cache is stale.
    output_len: usize,
    /// Snapshot of `block.command.len()`.
    command_len: usize,
    /// Snapshot of `block.collapsed` — toggling invalidates.
    collapsed: bool,
    /// `cols` used to compute wrapping — resize invalidates.
    cols: usize,
    /// Whether the block has any non-empty output lines (cached foldable
    /// check, avoids re-scanning the last 500 lines every frame).
    foldable: bool,
    /// Pre-trimmed, pre-wrapped line metadata. Trailing empty/prompt lines
    /// are already removed, matching the original trimming logic.
    lines: Vec<CachedLine>,
}

/// Per-renderer block layout cache. Keyed by `BlockId.0`.
#[derive(Default)]
struct BlockLayoutCache {
    entries: HashMap<u64, CachedBlockLayout>,
}

impl BlockLayoutCache {
    /// Ensure `block` has a cached layout for `cols`. Recomputes only if
    /// the block is new, its output/command changed, `collapsed` was
    /// toggled, or `cols` changed (resize).
    fn ensure_cached(&mut self, block: &Block, cols: usize) {
        let id = block.id.0;
        let needs_rebuild = match self.entries.get(&id) {
            None => true,
            Some(c) => {
                c.output_len != block.output.len()
                    || c.command_len != block.command.len()
                    || c.collapsed != block.collapsed
                    || c.cols != cols
            }
        };
        if needs_rebuild {
            self.entries.insert(id, compute_block_layout(block, cols));
        }
    }

    fn get(&self, id: u64) -> &CachedBlockLayout {
        self.entries
            .get(&id)
            .expect("ensure_cached must be called before get")
    }
}

/// Compute the layout for a single block (expensive — call once, then cache).
fn compute_block_layout(block: &Block, cols: usize) -> CachedBlockLayout {
    // Foldable: does the block have ANY non-empty output line in the last 500?
    let foldable = block
        .output
        .lines()
        .rev()
        .take(500)
        .any(|l| !l.trim().is_empty());

    // Collect raw lines and trim trailing empty/prompt lines.
    let raw_lines: Vec<&str> = block.output.lines().collect();
    let mut trimmed_len = raw_lines.len();
    while trimmed_len > 0 {
        let t = raw_lines[trimmed_len - 1].trim();
        if t.is_empty() || matches!(t, "%" | "$" | "#") {
            trimmed_len -= 1;
        } else {
            break;
        }
    }

    // Pre-compute wrapped chunks for each surviving line.
    let lines: Vec<CachedLine> = raw_lines[..trimmed_len]
        .iter()
        .enumerate()
        .map(|(idx, line)| {
            let byte_start = line.as_ptr() as usize - block.output.as_ptr() as usize;
            let byte_end = byte_start + line.len();
            let chunks: Rc<[String]> = Rc::from(wrap_line_chunks(line, cols).collect::<Vec<_>>());
            CachedLine {
                idx,
                byte_start,
                byte_end,
                chunks,
            }
        })
        .collect();

    CachedBlockLayout {
        output_len: block.output.len(),
        command_len: block.command.len(),
        collapsed: block.collapsed,
        cols,
        foldable,
        lines,
    }
}

/// Metal GPU renderer: draws the terminal Grid to screen.
pub struct MetalRenderer {
    device: Device,
    queue: metal::CommandQueue,
    layer: MetalLayer,
    pipeline: metal::RenderPipelineState,
    sampler: metal::SamplerState,
    atlas: GlyphAtlas,
    viewport: (f32, f32),
    /// Retained for live font/atlas rebuild (config reload).
    scale: f64,
    /// Retained for live font/atlas rebuild (config reload).
    font_config: FontConfig,
    /// Active theme (default fg/bg/cursor/selection). Per-frame, so a
    /// `set_theme` call recolors the screen on the next draw.
    theme: Theme,
    /// Content padding in **physical** pixels (logical config value × scale).
    /// Cells are positioned `pad_x + col·cw`, `pad_y + row·ch`; the usable
    /// area for row/col math is the viewport minus `2·pad`.
    padding_x: f32,
    padding_y: f32,
    /// Window background opacity [0,1]. Below 1.0 the background is
    /// see-through (text/selection/cursor stay opaque); the cell bg alpha is
    /// scaled by this value per-frame, so a live `set_opacity` recolors
    /// instantly. The window's own transparency flag is set at startup, so
    /// crossing the 1.0 boundary needs a relaunch.
    opacity: f32,
    /// Last-rendered hit-test regions (foldable blocks, completion rows, etc.)
    /// in physical pixels, for click dispatch. Repopulated each draw.
    pub hit_regions: Vec<crate::overlay::HitRegion>,
    /// User-adjustable popup width scale (0.3–0.95 of viewport width).
    popup_width_scale: f32,
    /// User-adjustable popup max visible rows.
    popup_max_rows: usize,
    /// Context menu position + target block (F7). Set per-frame by the app.
    pub context_menu_target: Option<(f32, f32, Option<weft_core::blocks::BlockId>)>,
    /// Last-rendered popup rectangles (completion + palette), for border
    /// drag-resize hot-zone detection. None when the popup wasn't drawn.
    pub completion_popup_rect: Option<[f32; 4]>, // [x0, y0, x1, y1]
    pub palette_popup_rect: Option<[f32; 4]>,
    /// v1.0 S1: Last-rendered Settings panel rect (physical pixels).
    /// `None` when the panel wasn't drawn this frame.
    pub settings_popup_rect: Option<[f32; 4]>,
    /// Last-rendered block-view rows (scroll-adjusted y bands + visible text),
    /// for mouse hit-testing and selection in the block view. Repopulated each
    /// draw when `show_block_view()` is true; cleared otherwise. Empty when the
    /// classic grid view is active (selection then uses Grid coordinates).
    pub block_view_rows: Vec<weft_core::selection::BlockViewRow>,
    /// Layout context for the current frame (viewport + cell + padding + clip).
    /// Constructed at the top of `draw()` and used by overlay builders to derive
    /// coordinates from semantic methods instead of hand-rolled f32 math.
    /// `None` before the first draw or after a resize before the next frame.
    pub layout_ctx: Option<crate::layout::LayoutCtx>,
    /// v0.8 B3 FindInGrid state — set per-frame by the app via `set_find_state`.
    /// `None` when the find bar is closed. The renderer reads this to draw the
    /// top banner + highlight the current match.
    pub find_state: Option<FindDrawState>,
    /// Last-rendered find popup button hit-test rects (physical pixels).
    /// `None` when the find popup wasn't drawn this frame. Populated by
    /// `build_find_vertices` each draw; the app reads it from
    /// `handle_mouse_press` to route clicks on the up/down/case/regex buttons.
    pub find_buttons: Option<FindButtons>,
    /// v0.9 H1: Last-rendered tab bar hit-test rects. Each entry is
    /// `(tab_rect, close_rect, tab_index)`. Populated by `draw_tab_bar`
    /// each draw when `tab_bar.tab_counts > 1`; cleared otherwise. The app
    /// reads this from `handle_mouse_press` to route tab clicks.
    pub tab_hits: Vec<TabHit>,
    /// v1.1: Last-rendered "new tab" (+) button hit-test rect, or all-zero
    /// when not drawn (single tab / no tab bar). Populated by
    /// `build_tab_bar_vertices`; the app reads this in `handle_mouse_press`
    /// to open a new tab on click.
    pub new_tab_rect: [f32; 4],
    /// v1.0 S1-b: Last-rendered Settings panel hit-test rects (tabs, theme
    /// rows, footer buttons). Populated by `build_settings_vertices` each
    /// draw when the panel is open; cleared otherwise. The app reads this
    /// from `handle_mouse_press` to route settings clicks.
    pub settings_hits: Vec<SettingsHit>,
    /// v0.9 W2: block currently highlighted in the terminal view (set from
    /// the history panel click). The renderer draws an accent border around
    /// this block in block view. Cleared by the app after 1.5s.
    pub panel_highlight: Option<BlockId>,
    /// v0.9: cached cursor-blink state for the current frame, so overlay
    /// builders (palette, panel) can draw a blinking caret without it being
    /// threaded through every helper signature.
    cursor_blink_on: bool,
    /// v0.9: last-rendered prompt input box rect `[x0, y0, x1, y1]` in
    /// physical pixels. Used by the app to detect clicks on the prompt box
    /// (for select-all-then-copy). None when no prompt was drawn this frame.
    /// Uses `Cell` for interior mutability — `draw` holds an immutable borrow
    /// of `self.layer` (from `next_drawable`) so `&mut self` is unavailable.
    pub prompt_box_rect: Cell<Option<[f32; 4]>>,
    /// v1.0 P0-a: Cache for block view layouts. Eliminates redundant O(n)
    /// per-frame wrapping computation for finished historical blocks.
    /// Uses `RefCell` because `draw()` holds an immutable borrow of
    /// `self.layer` (from `next_drawable`) across the entire frame, so
    /// `&mut self` is unavailable for cache mutation.
    block_layout_cache: RefCell<BlockLayoutCache>,
    /// v1.0 P0-b: Per-row grid vertex cache. Each entry holds the vertices for
    /// one viewport row. Dirty rows are rebuilt; clean rows are reused from
    /// the previous frame. Eliminates per-frame iteration of all
    /// `num_rows × num_cols` cells when only a few rows changed (typical
    /// terminal output: 1-3 rows per frame).
    grid_row_cache: RefCell<Vec<Vec<f32>>>,
    /// v1.0 P0-b: Force a full grid redraw on the next draw. Set by the caller
    /// on resize / theme / tab switch / selection change. Cleared after the
    /// full redraw is performed.
    force_full_grid: Cell<bool>,
    /// v1.0 P0-b: Previous frame's cursor row. The cursor cell renders
    /// differently (block/bar/underline overlay), so both the old and new
    /// cursor rows must be rebuilt when the cursor moves or blinks.
    prev_cursor_row: Cell<Option<usize>>,
    /// v1.0 P1.5-B2: Previous frame's cursor column. Together with
    /// `prev_cursor_row` and `prev_show_cursor`, detects cursor stability
    /// so the cursor row can be skipped when nothing moved or blinked.
    prev_cursor_col: Cell<Option<usize>>,
    /// v1.0 P1.5-B2: Previous frame's `show_cursor` parameter (encodes
    /// `cursor_visible && cursor_blink_on && prompt.is_none()`). When this
    /// flips (blink toggle / focus change / prompt open-close), the cursor
    /// row must be rebuilt to add or remove the cursor overlay.
    prev_show_cursor: Cell<bool>,
    /// v1.0 P1.5-B2: Set by `build_grid_instances` when no rows needed
    /// rebuilding this frame (no dirty rows, no cursor change, no scroll).
    /// `draw()` reads this to skip instance upload + draw call, relying on
    /// the offscreen `Load` action to preserve the previous frame's grid
    /// content. Saves the per-row rebuild loop + flatten memcpy + GPU
    /// upload for idle frames (target: < 0.5ms no-op frame).
    instances_unchanged: Cell<bool>,
    /// v1.0 P1.5-B2: Previous frame's `show_blocks` flag. When the view mode
    /// switches between block view and grid view (alt screen enter/exit), the
    /// grid_row_cache and offscreen content are stale — force a full rebuild.
    prev_show_blocks: Cell<bool>,
    /// v1.0 P0-b: Cached grid dimensions (rows × cols) for cache invalidation
    /// on resize.
    grid_cache_dims: Cell<(usize, usize)>,
    /// v1.0 P0-b: Previous frame's scroll offset. When the user scrolls
    /// scrollback, the rendered cells come from history (not dirty-tracked),
    /// so a full redraw is needed.
    prev_scroll_offset: Cell<usize>,
    /// v1.0 P0-c: Persistent offscreen texture used as the render target
    /// instead of drawing directly to the drawable. This enables GPU-side
    /// scroll blit: on scroll, copy the unchanged region within the
    /// offscreen texture (src_y=Δ → dst_y=0) via MTLBlitCommandEncoder,
    /// then render only the newly exposed rows with load_action=Load.
    /// The offscreen is blitted to the drawable at the end of the frame.
    /// Recreated on viewport resize.
    offscreen_texture: RefCell<Option<metal::Texture>>,
    /// v1.0 P0-c: Dimensions of the current offscreen texture (w, h) in
    /// physical pixels. Used to detect resize → recreate offscreen.
    offscreen_dims: Cell<(f32, f32)>,
    /// v1.0 P0-c: Pending scroll delta captured during build_grid_vertices
    /// (via grid.take_pending_scroll()). Used by the draw() epilogue to
    /// decide whether to issue a GPU blit before the render pass.
    pending_scroll_delta: Cell<i32>,
    /// v1.0 P0-c: Cached result of the `force_full` computation from
    /// build_grid_vertices. Stashed on self so the draw() epilogue (which
    /// runs after build_grid_vertices returns) can decide whether to skip
    /// the GPU scroll blit on forced-full frames (resize/theme/tab-switch).
    force_full_cached: Cell<bool>,
    /// v1.0 P1.5-B0: Triple-buffered vertex buffer ring. Avoids per-frame
    /// `new_buffer_with_data` allocation (~11520 vertices × 48 bytes = 540KB
    /// per frame). Three buffers ensure GPU never stalls on CPU writes
    /// (triple-buffering covers 2 frames of GPU latency).
    vertex_buffer_ring: RefCell<Vec<metal::Buffer>>,
    /// v1.0 P1.5-B0: Current index into `vertex_buffer_ring`. Advanced
    /// each frame; the buffer at this index is reused (grown if needed).
    vertex_buffer_ring_idx: Cell<usize>,
    /// v1.0 P1.5-B0: Capacity of each buffer in the ring (in bytes). When
    /// vertex data exceeds this, a new larger buffer is allocated.
    vertex_buffer_capacity: Cell<u64>,
    /// v1.0 P1.5-B1: Instanced render pipeline for grid cells. Renders
    /// each cell as a single 64-byte instance (origin/size/uv/fg/bg) drawn
    /// against a static 4-vertex quad + 6-index buffer, replacing the
    /// legacy 6-vertex-per-cell emission (~288 B/cell).
    instanced_pipeline: metal::RenderPipelineState,
    /// v1.0 P1.5-B1: Static index buffer for the unit quad (6 u16 indices).
    /// Bound once per grid draw; never resized.
    index_buffer: metal::Buffer,
    /// v1.0 P1.5-B1: Triple-buffered instance data ring. Each frame writes
    /// the active tab's grid instances into one buffer; the previous two
    /// buffers stay alive so the GPU can finish reading them.
    instance_ring: RefCell<Vec<metal::Buffer>>,
    /// v1.0 P1.5-B1: Current index into `instance_ring`. Advanced each
    /// frame; the buffer at this index is reused (grown if needed).
    instance_ring_idx: Cell<usize>,
    /// v1.0 P1.5-B1: Capacity of each buffer in `instance_ring` (bytes).
    /// When instance data exceeds this, a new larger buffer is allocated.
    instance_capacity: Cell<u64>,
}

/// v0.9 H1: Tab bar state passed to the renderer each frame.
#[derive(Clone, Default)]
pub struct TabBarDrawState {
    /// Number of open tabs.
    pub tab_count: usize,
    /// Index of the active tab (0-based).
    pub active_tab: usize,
    /// Tab labels (e.g., shell cwd basename or "Tab N").
    pub labels: Vec<String>,
    /// v0.9 W1+: index of the tab currently hovered by the mouse (0-based),
    /// or `None` when the cursor isn't over any tab. Used to show the close
    /// "×" button on hover (Warp-style) — active tab always shows "×".
    pub hovered_tab: Option<usize>,
    /// v1.2: horizontal scroll offset in physical pixels. 0 = scrolled all
    /// the way left (showing the first tab). Applied as `x0 -= scroll_offset`
    /// in `build_tab_bar_vertices`. The app clamps this to the valid range
    /// (0 .. total_tab_width - visible_width) on each frame.
    pub scroll_offset: f32,
    /// v1.2: true when the mouse is over the "+" (new tab) button. Drives a
    /// hover highlight effect on the button.
    pub plus_hovered: bool,
    /// v1.2: true when the mouse is over the left scroll arrow.
    pub arrow_left_hovered: bool,
    /// v1.2: true when the mouse is over the right scroll arrow.
    pub arrow_right_hovered: bool,
}

/// v0.9 H1: Hit-test rect for a tab label + close button.
#[derive(Clone, Copy, Debug, Default)]
pub struct TabHit {
    /// Full clickable rect of the tab label area: `[x0, y0, x1, y1]`.
    pub tab_rect: [f32; 4],
    /// Clickable rect of the close "×" button: `[x0, y0, x1, y1]`.
    pub close_rect: [f32; 4],
    /// Tab index (0-based) this hit corresponds to.
    pub index: usize,
}

/// v1.0 S1-b: Hit-test rect for a clickable region inside the Settings panel.
/// Repopulated each frame by `build_settings_vertices` (matching the layout
/// it just rendered) and consumed by the app's `handle_mouse_press` to
/// dispatch clicks on tabs, theme rows, and footer buttons.
#[derive(Clone, Copy, Debug)]
pub struct SettingsHit {
    /// What this region refers to — drives the click action.
    pub kind: SettingsHitKind,
    /// Clickable rect in physical pixels: `[x0, y0, x1, y1]`.
    pub rect: [f32; 4],
}

/// v1.0 S1-b: Identifies what a [`SettingsHit`] region targets. `Tab`/`Theme`
/// carry the index so the click handler can update the cursor / pick a value
/// without recomputing layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsHitKind {
    /// A tab-bar entry (Appearance / Font / Keybindings / Window / Logo).
    Tab(crate::overlay::SettingsTab),
    /// A theme row in the Appearance tab (0-based index).
    Theme(usize),
    /// Footer "esc close" pair — click closes the panel without saving.
    CloseButton,
    /// Footer "⌘⏎ save" pair — click persists the draft and closes the panel.
    SaveButton,
    /// Footer "⏎ apply" pair — click persists the draft but keeps the panel
    /// open so the user can keep editing.
    ApplyButton,
}

/// Hit-test rectangles for the find popup's clickable buttons (physical
/// pixels). Each `[x0, y0, x1, y1]` rect is the full clickable area of that
/// button, including padding around the glyph. Set per-frame by the renderer
/// in `build_find_vertices`; consumed by the app's mouse handler.
#[derive(Clone, Copy, Debug, Default)]
pub struct FindButtons {
    /// Up arrow (previous match). `None` when there are no matches to cycle.
    pub up: Option<[f32; 4]>,
    /// Down arrow (next match). `None` when there are no matches to cycle.
    pub down: Option<[f32; 4]>,
    /// "Aa" case-sensitive toggle.
    pub case_sensitive: [f32; 4],
    /// ".*" regex mode toggle.
    pub regex: [f32; 4],
}

/// Per-frame FindInGrid draw state (v0.8 B3). Set by the app before `draw()`.
#[derive(Clone, Debug, Default)]
pub struct FindDrawState {
    /// Live query string (rendered in the bar input).
    pub query: String,
    /// 1-based index of the current match, or 0 when there are no matches.
    pub current: usize,
    /// Total NAVIGABLE matches. In grid view this is grid matches; in block
    /// view this is block matches (Enter cycles through them).
    pub total: usize,
    /// True when `total` hit MAX_MATCHES — surfaced as "too many matches".
    pub truncated: bool,
    /// Current GRID match's `(viewport_row, col, len_in_cells)` — `None` when
    /// no match is selected or in block view. The renderer highlights this
    /// rectangle.
    pub highlight: Option<(usize, usize, usize)>,
    /// Current BLOCK match's `(block_id, line, is_command, col, len)` — used
    /// to highlight the match in block view. `None` in grid view or when no
    /// match is selected.
    pub block_highlight: Option<(u64, usize, bool, usize, usize)>,
    /// Non-navigable matches found in block content (block view only). When
    /// `total == 0` and this is > 0, the status shows "N matches in blocks"
    /// so the user knows the search did find things (just not navigable).
    pub block_matches: usize,
    /// Regex mode toggle (v0.9 U-P2: now actually wired — when true, the
    /// query is compiled as a `regex::Regex` and matched via `find_iter`).
    /// When true, the ".*" indicator lights up in accent color.
    pub regex_mode: bool,
    /// Case-sensitive toggle. When true, the "Aa" indicator lights up in
    /// accent color and the search matches exact character case.
    pub case_sensitive: bool,
    /// Regex compile error message (v0.9 U-P2). When `Some`, the FindUI
    /// shows "invalid regex" in red instead of the match count. Cleared
    /// when the query compiles successfully or regex mode is toggled off.
    pub regex_error: Option<String>,
}

impl MetalRenderer {
    pub fn new(
        window: &Window,
        font_config: FontConfig,
        theme: Theme,
        padding_logical: (u32, u32),
        opacity: f32,
    ) -> Self {
        let device = Device::system_default().expect("No Metal device found");
        let queue = device.new_command_queue();

        info!("Metal device: {}", device.name());

        let scale = window.scale_factor();
        let size = window.inner_size();

        // Logical padding (points) → physical pixels for rendering/layout.
        let padding_x = padding_logical.0 as f32 * scale as f32;
        let padding_y = padding_logical.1 as f32 * scale as f32;
        let opacity = opacity.clamp(0.0, 1.0);

        // Use physical pixels for viewport to stay consistent with drawable_size
        // and grid dimensions (which are calculated from physical cell sizes).
        let vp_w = size.width as f32;
        let vp_h = size.height as f32;

        // Build glyph atlas with CJK support
        let atlas = GlyphAtlas::new(&device, &font_config, scale);

        info!(
            "Window: {}x{} physical ({}x scale), viewport: {}x{} physical, atlas cells: {}x{}",
            size.width, size.height, scale, vp_w, vp_h, atlas.cell_width, atlas.cell_height
        );

        // Compile shaders with DEBUGGING MODE
        // Change DEBUG_MODE to 0-5 to test different rendering paths
        // 0 = normal texture rendering
        // 1 = solid red (test if fragment shader runs)
        // 2 = texture alpha as white (test if texture sampling works)
        // 3 = position as color (test if vertices pass position)
        // 4 = UV as color (test if vertices pass UVs)
        // 5 = background color only
        let source = r#"#include <metal_stdlib>
using namespace metal;

    struct TextVertexIn {
        float4 position_tex [[attribute(0)]];  // xy = position, zw = tex_coord
        float4 fg_color [[attribute(1)]];
        float4 bg_color [[attribute(2)]];
    };

struct TextVertexOut {
    float4 position [[position]];
    float2 tex_coord;
    float4 fg_color;
    float4 bg_color;
    float is_bg;
};

vertex TextVertexOut text_vertex(
    TextVertexIn vin [[stage_in]],
    constant float2& viewport_size [[buffer(1)]]
) {
    TextVertexOut out;

    float2 position = vin.position_tex.xy;
    float2 tex_coord = vin.position_tex.zw;

    // Map logical pixels (origin top-left) to clip space. The CAMetalLayer on this
    // NSView composites with an extra vertical flip, so we sample each glyph's V
    // inverted (swapped in build_grid_vertices) to keep letters upright on screen.
    float2 clip = (position / viewport_size) * 2.0 - 1.0;
    clip.y = -clip.y;

    out.position = float4(clip, 0.0, 1.0);
    out.tex_coord = tex_coord;
    out.fg_color = vin.fg_color;
    out.bg_color = vin.bg_color;
    out.is_bg = 0.0;
    return out;
}

// v1.0 P1.5-B1: Instanced vertex shader for grid cells. Each instance is
// one cell: a quad (4 verts indexed as 0,1,2,0,2,3) with per-instance
// origin/size/uv_rect/fg/bg. Corners are derived from `vertex_id` so no
// static vertex buffer is needed — only the index buffer + instance buffer.
// Replaces the 6-vertex-per-cell emission (~288 B/cell) with a single
// 64-byte instance, ~4.5x smaller per-frame upload.
struct CellInstance {
    float2 origin;    // top-left pixel position
    float2 size;      // pixel width/height
    float4 uv_rect;   // (u0, v0, u1, v1) — V already swapped for layer flip
    float4 fg;        // RGBA
    float4 bg;        // RGBA
};

vertex TextVertexOut text_vertex_instanced(
    uint vid [[vertex_id]],
    uint iid [[instance_id]],
    constant float2& viewport_size [[buffer(1)]],
    constant CellInstance* instances [[buffer(2)]]
) {
    TextVertexOut out;
    // vid ∈ {0,1,2,3} via indexed draw (index buffer [0,1,2,0,2,3]).
    // Corners: 0=TL (0,0), 1=BL (0,1), 2=BR (1,1), 3=TR (1,0).
    float2 corner;
    switch (vid) {
        case 0: corner = float2(0.0, 0.0); break;
        case 1: corner = float2(0.0, 1.0); break;
        case 2: corner = float2(1.0, 1.0); break;
        default: corner = float2(1.0, 0.0); break;
    }
    CellInstance inst = instances[iid];
    float2 position = inst.origin + corner * inst.size;
    float2 tex_coord = float2(
        mix(inst.uv_rect.x, inst.uv_rect.z, corner.x),
        mix(inst.uv_rect.y, inst.uv_rect.w, corner.y)
    );
    float2 clip = (position / viewport_size) * 2.0 - 1.0;
    clip.y = -clip.y;
    out.position = float4(clip, 0.0, 1.0);
    out.tex_coord = tex_coord;
    out.fg_color = inst.fg;
    out.bg_color = inst.bg;
    out.is_bg = 0.0;
    return out;
}

fragment float4 text_fragment(
    TextVertexOut in [[stage_in]],
    texture2d<float> atlas [[texture(0)]],
    sampler atlas_sampler [[sampler(0)]]
) {
    // Sample the glyph mask from the R8 atlas and blend fg over bg.
    float mask = atlas.sample(atlas_sampler, in.tex_coord).r;
    float4 color = mix(in.bg_color, in.fg_color, mask);
    // Background opacity: the cell bg's alpha carries the window opacity
    // (scaled in build_grid_vertices). Text (mask→1) stays opaque; empty
    // background (mask→0) takes that alpha so the desktop shows through.
    // For an opaque window the bg alpha is 1.0, so this is identical to
    // `color.a = 1.0`.
    color.a = mix(in.bg_color.a, 1.0, mask);
    return color;
}
"#;

        let compile_opts = CompileOptions::new();
        let library = device
            .new_library_with_source(source, &compile_opts)
            .expect("Failed to compile Metal shader");
        let vertex_fn = library.get_function("text_vertex", None).unwrap();
        let fragment_fn = library.get_function("text_fragment", None).unwrap();

        let pipeline_desc = RenderPipelineDescriptor::new();

        // Explicit vertex descriptor: the vertex buffer packs 3 float4 attributes
        // (position_tex, fg_color, bg_color) per vertex at stride 48. The vertex
        // shader uses [[stage_in]] attribute pulling, which requires this descriptor
        // (manual buffer indexing without one produced scrambled vertex data).
        let vertex_desc = VertexDescriptor::new();
        let attrs = vertex_desc.attributes();
        let attr0 = attrs.object_at(0).unwrap();
        attr0.set_format(MTLVertexFormat::Float4);
        attr0.set_buffer_index(0);
        attr0.set_offset(0);
        let attr1 = attrs.object_at(1).unwrap();
        attr1.set_format(MTLVertexFormat::Float4);
        attr1.set_buffer_index(0);
        attr1.set_offset(16);
        let attr2 = attrs.object_at(2).unwrap();
        attr2.set_format(MTLVertexFormat::Float4);
        attr2.set_buffer_index(0);
        attr2.set_offset(32);
        vertex_desc.layouts().object_at(0).unwrap().set_stride(48);
        pipeline_desc.set_vertex_descriptor(Some(vertex_desc));

        pipeline_desc.set_vertex_function(Some(&vertex_fn));
        pipeline_desc.set_fragment_function(Some(&fragment_fn));
        let color_att = pipeline_desc.color_attachments().object_at(0).unwrap();
        color_att.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        color_att.set_blending_enabled(true);
        color_att.set_source_rgb_blend_factor(metal::MTLBlendFactor::SourceAlpha);
        color_att.set_destination_rgb_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);
        color_att.set_source_alpha_blend_factor(metal::MTLBlendFactor::SourceAlpha);
        color_att.set_destination_alpha_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);

        let pipeline = device
            .new_render_pipeline_state(&pipeline_desc)
            .expect("Failed to create render pipeline");

        // v1.0 P1.5-B1: Instanced pipeline for grid cells. No vertex
        // descriptor — the instanced vertex shader derives corner position
        // from `vertex_id` and pulls per-cell data from the instance buffer
        // at slot 2. Reuses the same fragment shader (atlas sampling +
        // fg/bg blend) and color attachment config as the legacy pipeline.
        let instanced_vertex_fn = library.get_function("text_vertex_instanced", None).unwrap();
        let instanced_desc = RenderPipelineDescriptor::new();
        instanced_desc.set_vertex_function(Some(&instanced_vertex_fn));
        instanced_desc.set_fragment_function(Some(&fragment_fn));
        let instanced_color_att = instanced_desc.color_attachments().object_at(0).unwrap();
        instanced_color_att.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        instanced_color_att.set_blending_enabled(true);
        instanced_color_att.set_source_rgb_blend_factor(metal::MTLBlendFactor::SourceAlpha);
        instanced_color_att
            .set_destination_rgb_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);
        instanced_color_att.set_source_alpha_blend_factor(metal::MTLBlendFactor::SourceAlpha);
        instanced_color_att
            .set_destination_alpha_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);
        let instanced_pipeline = device
            .new_render_pipeline_state(&instanced_desc)
            .expect("Failed to create instanced render pipeline");

        // v1.0 P1.5-B1: Static index buffer for the unit quad: two triangles
        // (TL-BL-BR, TL-BR-TR) indexing 4 corner vertices. The vertex shader
        // maps vertex_id 0..3 → corner (0,0), (0,1), (1,1), (1,0). Created
        // once at startup; reused for every grid draw.
        let indices: [u16; 6] = [0, 1, 2, 0, 2, 3];
        let index_buffer = device.new_buffer_with_data(
            indices.as_ptr() as *const _,
            (indices.len() * std::mem::size_of::<u16>()) as u64,
            MTLResourceOptions::CPUCacheModeDefaultCache,
        );

        // Create and configure Metal layer
        // IMPORTANT: drawable_size must be in PHYSICAL PIXELS, not logical points
        // Metal uses the actual backing store size for rendering
        let layer = MetalLayer::new();
        layer.set_device(&device);
        layer.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        layer.set_presents_with_transaction(false);
        layer.set_maximum_drawable_count(3);
        layer.set_drawable_size(CGSize::new(size.width as f64, size.height as f64));

        unsafe {
            attach_layer_to_nsview(&layer, window, scale);
            // Layer opacity: a non-opaque layer lets the transparent window
            // show the desktop through the (alpha-scaled) cell backgrounds.
            set_layer_opaque(&layer, opacity >= 1.0);
        }

        info!(
            "Renderer initialized: {}x{} @ {}x scale, {}x{} cells",
            size.width, size.height, scale, atlas.cell_width, atlas.cell_height
        );

        // Create sampler for glyph atlas texture
        let sampler_desc = SamplerDescriptor::new();
        sampler_desc.set_min_filter(metal::MTLSamplerMinMagFilter::Linear);
        sampler_desc.set_mag_filter(metal::MTLSamplerMinMagFilter::Linear);
        sampler_desc.set_mip_filter(metal::MTLSamplerMipFilter::NotMipmapped);
        sampler_desc.set_address_mode_r(metal::MTLSamplerAddressMode::ClampToEdge);
        sampler_desc.set_address_mode_s(metal::MTLSamplerAddressMode::ClampToEdge);
        sampler_desc.set_address_mode_t(metal::MTLSamplerAddressMode::ClampToEdge);
        let sampler = device.new_sampler(&sampler_desc);

        Self {
            device,
            queue,
            layer,
            pipeline,
            sampler,
            atlas,
            viewport: (vp_w, vp_h),
            scale,
            font_config,
            theme,
            padding_x,
            padding_y,
            opacity,
            hit_regions: Vec::new(),
            popup_width_scale: 0.6,
            popup_max_rows: 8,
            context_menu_target: None,
            completion_popup_rect: None,
            palette_popup_rect: None,
            settings_popup_rect: None,
            block_view_rows: Vec::new(),
            layout_ctx: None,
            find_state: None,
            find_buttons: None,
            tab_hits: Vec::new(),
            new_tab_rect: [0.0; 4],
            settings_hits: Vec::new(),
            panel_highlight: None,
            cursor_blink_on: true,
            prompt_box_rect: Cell::new(None),
            block_layout_cache: RefCell::new(BlockLayoutCache::default()),
            grid_row_cache: RefCell::new(Vec::new()),
            force_full_grid: Cell::new(true),
            prev_cursor_row: Cell::new(None),
            prev_cursor_col: Cell::new(None),
            // Start true so the first frame forces a rebuild (no previous
            // cursor state to compare against).
            prev_show_cursor: Cell::new(true),
            instances_unchanged: Cell::new(false),
            prev_show_blocks: Cell::new(false),
            grid_cache_dims: Cell::new((0, 0)),
            prev_scroll_offset: Cell::new(0),
            offscreen_texture: RefCell::new(None),
            offscreen_dims: Cell::new((0.0, 0.0)),
            pending_scroll_delta: Cell::new(0),
            force_full_cached: Cell::new(true),
            vertex_buffer_ring: RefCell::new(Vec::new()),
            vertex_buffer_ring_idx: Cell::new(0),
            vertex_buffer_capacity: Cell::new(0),
            instanced_pipeline,
            index_buffer,
            instance_ring: RefCell::new(Vec::new()),
            instance_ring_idx: Cell::new(0),
            instance_capacity: Cell::new(0),
        }
    }

    /// Swap the active theme. Recolors the whole screen on the next draw
    /// (colors are resolved per-frame from cells' color-origins + this theme +
    /// the terminal palette, so no rebuild is needed).
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
        // Colors are baked into cached vertices — force a full rebuild.
        self.force_full_grid.set(true);
    }

    /// v1.0 P0-c: Ensure the offscreen texture exists and matches the current
    /// viewport size. Recreates the texture on resize. Returns true if the
    /// texture is usable (false on first frame or after a failed allocation).
    fn ensure_offscreen_texture(&self) -> bool {
        let (vw, vh) = (self.viewport.0, self.viewport.1);
        if vw <= 0.0 || vh <= 0.0 {
            return false;
        }
        // Recreate if missing or dimensions changed.
        if self.offscreen_dims.get() != (vw, vh) {
            let descriptor = metal::TextureDescriptor::new();
            descriptor.set_texture_type(metal::MTLTextureType::D2);
            descriptor.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
            descriptor.set_width(vw as u64);
            descriptor.set_height(vh as u64);
            descriptor.set_usage(
                metal::MTLTextureUsage::RenderTarget | metal::MTLTextureUsage::ShaderRead,
            );
            let tex = self.device.new_texture(&descriptor);
            *self.offscreen_texture.borrow_mut() = Some(tex);
            self.offscreen_dims.set((vw, vh));
            // New textures have undefined content. Force a full redraw so the
            // offscreen is fully rendered with Clear + all rows before any
            // incremental Load path is used.
            self.force_full_grid.set(true);
        }
        self.offscreen_texture.borrow().is_some()
    }

    /// v1.0 P0-b: Force a full grid redraw on the next draw. Call on resize,
    /// tab switch, selection change, or any event that invalidates the
    /// per-row vertex cache. Takes `&self` (not `&mut self`) because it only
    /// touches `Cell` fields — needed so it can be called from within `draw()`
    /// while a `drawable` borrow is alive.
    pub fn force_full_grid_redraw(&self) {
        self.force_full_grid.set(true);
        // v1.0 P1.5-B2: a forced redraw means instances WILL change — clear
        // the unchanged flag so draw() doesn't skip the instance draw call.
        self.instances_unchanged.set(false);
    }

    /// Set popup dimensions (from App drag state).
    pub fn set_popup_size(&mut self, width_scale: f32, max_rows: usize) {
        self.popup_width_scale = width_scale;
        self.popup_max_rows = max_rows;
    }

    /// Content padding in physical pixels (logical config × scale).
    pub fn padding_x(&self) -> f32 {
        self.padding_x
    }

    /// Content padding in physical pixels (logical config × scale).
    pub fn padding_y(&self) -> f32 {
        self.padding_y
    }

    /// Update content padding (logical config px). Caller must recompute the
    /// grid layout afterwards — padding changes the usable rows/cols.
    pub fn set_padding(&mut self, padding_logical: (u32, u32)) {
        self.padding_x = padding_logical.0 as f32 * self.scale as f32;
        self.padding_y = padding_logical.1 as f32 * self.scale as f32;
    }

    /// Update window background opacity [0,1]. Recolors the next frame (bg
    /// alpha is resolved per-frame); also flips the CAMetalLayer opaque flag.
    pub fn set_opacity(&mut self, opacity: f32) {
        self.opacity = opacity.clamp(0.0, 1.0);
        // SAFETY: layer is a valid CAMetalLayer; setOpaque: is its property setter.
        unsafe {
            set_layer_opaque(&self.layer, self.opacity >= 1.0);
        }
    }

    /// Current background color (for the render-pass clear value).
    #[allow(dead_code)]
    pub fn background(&self) -> Color {
        self.theme.background
    }

    /// Current resolved theme (for applying to new tabs etc.).
    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Viewport dimensions (width, height) in physical pixels.
    pub fn viewport(&self) -> (f32, f32) {
        self.viewport
    }

    /// Cell size (width, height) in physical pixels.
    pub fn cell_size(&self) -> (f32, f32) {
        (self.atlas.cell_width as f32, self.atlas.cell_height as f32)
    }

    pub fn resize(&mut self, window: &Window, size: winit::dpi::PhysicalSize<u32>) {
        // Use physical pixels for viewport to match drawable_size and grid dimensions
        let vp_w = size.width as f32;
        let vp_h = size.height as f32;
        self.viewport = (vp_w, vp_h);
        // IMPORTANT: drawable_size must be in PHYSICAL PIXELS
        self.layer
            .set_drawable_size(CGSize::new(size.width as f64, size.height as f64));
        // v1.0 fix: invalidate per-row vertex cache on viewport change.
        // Without this, macOS live-resize can render a frame with old-cache
        // vertices at the new viewport size before ensure_offscreen_texture()
        // detects the change → text misalignment / flicker.
        self.force_full_grid_redraw();
        window.request_redraw();
    }

    /// Cell width in physical pixels (for terminal size calculation).
    pub fn cell_width(&self) -> u32 {
        self.atlas.cell_width
    }

    /// Cell height in physical pixels (for terminal size calculation).
    pub fn cell_height(&self) -> u32 {
        self.atlas.cell_height
    }

    /// Viewport width in physical pixels.
    pub fn viewport_width(&self) -> f32 {
        self.viewport.0
    }

    /// Backing-store scale (Retina factor).
    pub fn scale(&self) -> f64 {
        self.scale
    }

    /// How many block-view content rows (at `pitch = ch * 1.1`) fit in the
    /// block region — the area above the prompt input box (editor mode) or the
    /// full viewport (command-executing mode). The scroll handler clamps
    /// `block_scroll_offset` to `total - visible`, so this must match the
    /// renderer's actual capacity to avoid blank space when scrolling.
    pub fn block_visible_rows(&self, prompt_lines: usize) -> usize {
        let ch = self.cell_height() as f32;
        let vp_h = self.viewport.1;
        let pad_y = self.padding_y;
        let pitch = ch * 1.1;
        if ch <= 0.0 || vp_h <= 0.0 || pitch <= 0.0 {
            return 1;
        }
        // Editor mode: block region is vp_h - pad_y - box_h, where box_h
        // accounts for the prompt input box + its padding.
        let box_h = ch * (prompt_lines.max(1) as f32 + 2.0);
        let region_bottom = (vp_h - pad_y - box_h).max(0.0);
        let region_h = (region_bottom - pad_y).max(0.0);
        ((region_h / pitch).floor() as usize).max(1)
    }

    /// Rebuild the glyph atlas from a (possibly changed) font config — used on
    /// live config reload when font family/size/line-height changes. Returns
    /// the new cell dimensions so the caller can recompute grid rows/cols and
    /// PTY size.
    pub fn rebuild_atlas(&mut self, font_config: FontConfig) -> (u32, u32) {
        self.font_config = font_config;
        self.atlas = GlyphAtlas::new(&self.device, &self.font_config, self.scale);
        (self.atlas.cell_width, self.atlas.cell_height)
    }

    /// v0.9 H1: Tab bar height in physical pixels. Only drawn when there
    /// are 2+ tabs. Roughly 1.5× cell height, clamped to [28, 40] logical
    /// pixels (× scale for physical). v1.1: lower bound raised 24→28 to
    /// comfortably fit the macOS traffic-light buttons (~28pt) when the tab
    /// bar doubles as the (transparent) titlebar.
    pub fn tab_bar_height(&self) -> f32 {
        let logical = self.cell_height() as f32 / self.scale as f32 * 1.5;
        logical.clamp(28.0, 40.0) * self.scale as f32
    }

    /// v1.1: Reserved top space for the macOS titlebar (traffic lights) in
    /// physical pixels. Used as the minimum chrome_top even with a single tab
    /// so the transparent titlebar never overlaps terminal content. The native
    /// traffic lights are ~28pt tall; we reserve the same height here.
    pub fn titlebar_height(&self) -> f32 {
        28.0 * self.scale as f32
    }

    /// v1.1: Width reserved for the macOS traffic-light buttons at the top-left,
    /// in physical pixels. Tabs and content start to the right of this so they
    /// don't collide with the close/minimize/maximize buttons.
    pub fn traffic_lights_width(&self) -> f32 {
        72.0 * self.scale as f32
    }

    /// v0.9 W5: Left sidebar width in physical pixels (240 logical px × scale).
    /// Used as `chrome_left` when the history panel is in sidebar mode.
    pub fn sidebar_width(&self) -> f32 {
        240.0 * self.scale as f32
    }

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
        // v0.9 H1: tab bar state. When tab_count > 1 the tab bar is drawn
        // at the top of the window and the content area is shifted down.
        tab_bar: &TabBarDrawState,
    ) {
        let drawable = match self.layer.next_drawable() {
            Some(d) => d,
            None => return,
        };

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

        // v0.9 H1: compute chrome_top (tab bar height) and set it on the
        // LayoutCtx so all content is shifted below the tab bar. The bar is
        // only drawn when more than one tab is open (single tab hides it).
        //
        // v1.1: with the transparent (FullSizeContentView) titlebar, the
        // macOS traffic-light buttons float over the Metal content. Even with
        // a single tab we now always render the tab bar (for the "+" button),
        // so chrome_top always reserves tab_bar_height.
        let tab_h = self.tab_bar_height();
        let titlebar_h = self.titlebar_height();
        let chrome_top = tab_h.max(titlebar_h);
        // Whether a standalone titlebar strip (no tab bar) needs painting.
        // v1.2: always false now — the tab bar is always drawn.
        let single_tab_titlebar = false;

        // v0.9 W5: compute chrome_left (sidebar width) when the history panel
        // is open — the panel becomes a left sidebar that pushes content right.
        let panel_open = overlays
            .layers
            .iter()
            .any(|l| l.kind == crate::overlay::OverlayKind::HistoryPanel);
        let chrome_left = if panel_open {
            self.sidebar_width()
        } else {
            0.0
        };

        // Build this frame's LayoutCtx: the single source of truth for
        // coordinate math in every overlay builder (v0.8 stage 1). Stored on
        // self so methods that don't receive it directly can still access it
        // during this draw; rebuilt every frame so resizes/padding changes
        // take effect immediately.
        let mut ctx = crate::layout::LayoutCtx::new(
            self.viewport,
            self.cell_width() as f32,
            self.cell_height() as f32,
            self.padding_x,
            self.padding_y,
        );
        ctx.chrome_top = chrome_top;
        ctx.chrome_left = chrome_left;
        self.layout_ctx = Some(ctx);

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

        // Collect unique on-screen GRID + panel characters not yet in the
        // atlas, then rasterize each exactly once. (Partial borrow of
        // self.atlas here is disjoint from the `drawable` borrow of self.layer,
        // so it coexists.)
        {
            use std::collections::HashSet;
            let mut missing = HashSet::new();
            for row in 0..grid.num_rows {
                for col in 0..grid.num_cols {
                    let ch = grid.cell(row, col).character;
                    if ch != '\0' && ch != ' ' && self.atlas.get(ch).is_none() {
                        missing.insert(ch);
                    }
                }
            }
            if let Some(p) = panel {
                missing.extend("Search:".chars());
                missing.extend(p.query.chars());
                let max_blocks = visible_panel_rows(self.viewport.1, self.cell_height());
                for b in panel_display(p.blocks, p.query, max_blocks) {
                    missing.extend(b.command.chars());
                    missing.extend(block_duration_str(b).chars());
                    if Some(b.id) == p.expanded_id {
                        for line in b.output.lines().take(8) {
                            missing.extend(line.chars());
                        }
                    }
                }
            }
            if let Some(p) = prompt {
                missing.extend("❯ ".chars());
                if let Some(cwd) = p.cwd {
                    missing.extend(cwd.chars());
                }
                for line in p.lines {
                    missing.extend(line.chars());
                }
                if let Some(preedit) = p.preedit {
                    missing.extend(preedit.chars());
                }
                if let Some((q, sel)) = p.search {
                    missing.extend("search: ".chars());
                    missing.extend(q.chars());
                    if let Some(m) = sel {
                        missing.extend(m.chars());
                    }
                }
            }
            // Completion popup warm-up: scan emoji icons + match labels.
            if let Some((completions, _)) = completions {
                // Emoji icons used by the popup (📁📄 via CoreText color path).
                missing.extend(['📁', '📄', '»']);
                for m in completions {
                    missing.extend(m.label.chars());
                }
            }
            // Block-view (Editor mode + CommandExecuting overlay): commands,
            // outputs, durations. Session blocks only — hydrated history stays
            // in the panel.
            if terminal.show_block_view() {
                for b in terminal
                    .block_tracker()
                    .session_blocks()
                    .iter()
                    .rev()
                    .take(64)
                {
                    missing.extend("❯ ".chars());
                    missing.extend(b.command.chars());
                    missing.extend(block_duration_str(b).chars());
                    for line in b.output.lines().take(200) {
                        missing.extend(line.chars());
                    }
                }
                // In-flight (live) block during CommandExecuting.
                if let Some(live) = terminal.block_tracker().in_flight() {
                    missing.extend("❯ ".chars());
                    missing.extend(live.command.chars());
                    for line in live.output.lines().take(200) {
                        missing.extend(line.chars());
                    }
                }
            }
            // Find bar (Cmd+F): warm up the query + status text + button
            // glyphs so CJK / other non-ASCII chars typed via IME render
            // instead of leaving blank cells (the atlas only auto-warms
            // grid/panel content; the find query is independent).
            if let Some(find) = &self.find_state {
                missing.extend("Find: ".chars());
                missing.extend(find.query.chars());
                // Button labels + status fragments.
                missing.extend(['↑', '↓', 'A', 'a', '.', '*', '…']);
                if let Some(err) = &find.regex_error {
                    missing.extend(err.chars());
                }
            }
            // v0.9 fix: warm up the command palette (Cmd+P) query + banner
            // + submode input so CJK / other non-ASCII chars typed via IME
            // render instead of leaving blank cells (same rationale as the
            // find bar above).
            if let Some(p) = palette {
                missing.extend("> ".chars());
                missing.extend(p.query.chars());
                if !p.banner.is_empty() {
                    missing.extend(p.banner.chars());
                }
                missing.extend(p.submode_input.chars());
            }
            // v1.0 S1: warm up the Settings panel (Cmd+,) — tab labels,
            // status text, theme names, font family, keybinding strings.
            if let Some(s) = settings {
                use crate::overlay::OverlayWarmup;
                OverlayContent::Settings(*s).warm_chars(&mut missing);
            }
            // v0.9 fix: warm up the tab bar close button "×" and separator
            // chars so they render instead of being silently skipped by
            // push_text (which drops chars not in the atlas).
            missing.extend(['×', '·', '…']);
            for ch in &missing {
                self.atlas.get_or_rasterize(*ch);
            }
        }

        // Editor mode (at the prompt): full block history + input box.
        // CommandExecuting (a tracked command is running — e.g. an interactive
        // `sudo su` sub-shell): the live grid fills the bottom while the
        // completed block history is overlaid on top, so the history never
        // reverts to raw text (matches Warp). Alt-screen / not-integrated: grid.
        let ch = self.cell_height() as f32;
        let vp_h = self.viewport.1;
        let pad_y = self.padding_y;
        let show_blocks = terminal.show_block_view();
        // v1.0 P1.5-B2: when the view mode switches (alt screen enter/exit),
        // the grid_row_cache and offscreen content are stale — force a full
        // rebuild. Without this, an idle frame after the switch could set
        // instances_unchanged=true and blit the wrong view's content.
        let view_switched = show_blocks != self.prev_show_blocks.get();
        if view_switched {
            self.force_full_grid_redraw();
        }
        self.prev_show_blocks.set(show_blocks);
        let mut pending_hit_regions: Vec<crate::overlay::HitRegion> = Vec::new();
        // Reset popup rects — will be set by build_completion/palette/settings_vertices.
        self.completion_popup_rect = None;
        self.palette_popup_rect = None;
        self.settings_popup_rect = None;
        // Reset find button hit-test rects — set by build_find_vertices.
        self.find_buttons = None;
        // v1.0 S1-b: clear stale settings hit-test rects — repopulated by
        // build_settings_vertices only when the panel is open this frame.
        self.settings_hits.clear();
        // v1.0 P1.5-B1: Grid cells render as instances (instanced pipeline);
        // overlays + block view render as legacy vertices. In grid view,
        // `instances` carries the cells and `vertices` carries only overlays;
        // in block view, `instances` is empty and `vertices` carries everything.
        let mut instances: Vec<f32> = Vec::new();
        let mut vertices: Vec<f32> = if show_blocks {
            let (v, regions, bv_rows) = if let Some(p) = prompt {
                let box_h = ch * (p.lines.len().max(1) as f32 + 2.0);
                let box_top_y = (vp_h - pad_y - box_h).max(0.0);
                self.build_block_view_vertices(
                    terminal.block_tracker().session_blocks(),
                    box_top_y,
                    p.cwd,
                    terminal.git_branch(),
                    None,
                    block_scroll,
                    selection,
                )
            } else {
                // CommandExecuting: full block view with the in-flight command
                // as a live block at the bottom (its streaming output) and the
                // completed history above. No grid — the live session IS the
                // in-flight block's captured output, so the history never
                // reverts to raw and isn't squeezed by the grid cursor.
                self.build_block_view_vertices(
                    terminal.block_tracker().session_blocks(),
                    vp_h - pad_y,
                    None,
                    terminal.git_branch(),
                    terminal.block_tracker().in_flight(),
                    block_scroll,
                    selection,
                )
            };
            pending_hit_regions = regions;
            self.block_view_rows = bv_rows;
            v
        } else {
            // Grid view (alt-screen apps): clear the block-view row cache so
            // hit-testing falls back to Grid coordinates. Build per-cell
            // instances (P1.5-B1) into `instances`; overlays go into
            // `vertices` (appended below).
            self.block_view_rows.clear();
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
            if terminal.is_alt_screen_active() {
                self.force_full_grid_redraw();
            }
            instances = self.build_grid_instances(
                grid,
                terminal.palette(),
                cursor,
                selection,
                terminal.cursor_visible && cursor_blink_on && prompt.is_none(),
                terminal.cursor_style,
            );
            Vec::new()
        };

        // Overlay the history panel on top of the grid (drawn after, so it
        // composites over terminal cells via the enabled alpha blend).
        if let Some(p) = panel {
            vertices.extend_from_slice(&self.build_panel_vertices(p));
        }

        // v0.8 U6 scrollbar: dynamic thumb position + height proportional to
        // visible/total content. The thumb sits in a track spanning the block
        // region; its vertical position reflects block_scroll (scrolled up →
        // thumb near top). v1.0: color uses label_c (was accent_dim —
        // invisible in Nord/Warp themes). Still subtle but always readable.
        // Only drawn when content overflows the viewport.
        if show_blocks {
            if let Some((total, visible, max_scroll)) = scroll_metrics {
                if visible < total && max_scroll > 0 {
                    use crate::layout::Spacing;
                    let track_top = ctx.top();
                    let track_h = ctx.height();
                    let bar_x = ctx.right() - Spacing::sm(&ctx);
                    let bar_w = Spacing::xs(&ctx) + 1.0; // ~3px at 14pt
                                                         // Thumb height: proportional to visible/total, clamped to
                                                         // [3 rows, track_h] so it's always grabbable.
                    let min_thumb = Spacing::row_md(&ctx) * 3.0;
                    let ratio = visible as f32 / total as f32;
                    let thumb_h = (track_h * ratio).max(min_thumb).min(track_h);
                    // Thumb Y: block_scroll counts UP from the bottom (0 =
                    // viewing newest). Map so block_scroll=0 → thumb at bottom,
                    // block_scroll=max_scroll → thumb at top.
                    let scroll_ratio = block_scroll as f32 / max_scroll as f32;
                    let thumb_y = track_top + (track_h - thumb_h) * (1.0 - scroll_ratio);
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
                        [bar_x, thumb_y, bar_x + bar_w, thumb_y + thumb_h],
                        bg_uv,
                        [0.0; 4],
                        thumb_color,
                    );
                }
            }
        }

        // Overlay the editor input box at the bottom (Editor mode only).
        if let Some(p) = prompt {
            vertices.extend_from_slice(&self.build_prompt_vertices(
                p,
                cursor_blink_phase,
                cursor_blink_on,
            ));
        } else {
            self.prompt_box_rect.set(None);
        }

        // Completion popup (split out from prompt — overlay refactor commit 2).
        // Positioned above the prompt input box using the same geometry.
        if let Some((matches, selected)) = completions {
            if !matches.is_empty() {
                // Recompute box_top_y (same formula as build_prompt_vertices).
                let ch_f = self.cell_height() as f32;
                let vp_h = self.viewport.1;
                let n_lines = prompt.map(|p| p.lines.len().max(1)).unwrap_or(1);
                let box_h = ch_f * (n_lines as f32 + 2.0);
                let box_top_y = (vp_h - self.padding_y - box_h).max(0.0);
                // Anchor popup left edge to the cursor's x position (Warp-style),
                // not the window left padding. The prompt glyph "❯ " takes 2
                // columns on line 0; subsequent lines start at the left edge.
                let cw_f = self.cell_width() as f32;
                // v0.9 W5: align the completion popup with the shifted prompt
                // (padding_x + chrome_left) so it tracks the sidebar offset.
                let chrome_left = self.layout_ctx.map(|c| c.chrome_left).unwrap_or(0.0);
                let prompt_cols = prompt
                    .map(|p| {
                        let prompt_indent = if p.cursor.0 == 0 { 2 } else { 0 };
                        (prompt_indent + p.cursor.1) as f32 * cw_f + self.padding_x + chrome_left
                    })
                    .unwrap_or(self.padding_x + chrome_left);
                let box_x0 = prompt_cols;
                let (cv, rect) =
                    self.build_completion_vertices(matches, selected, box_top_y, box_x0);
                self.completion_popup_rect = rect;
                vertices.extend_from_slice(&cv);
            }
        }

        // Command Palette overlay (v0.7) — centered floating window.
        if let Some(p) = palette {
            let (pv, rect) = self.build_palette_vertices(p);
            self.palette_popup_rect = rect;
            vertices.extend_from_slice(&pv);
        }

        // v1.0 S1: Settings panel (Cmd+,) — centered modal overlay.
        if let Some(s) = settings {
            let (sv, rect, hits) = self.build_settings_vertices(*s);
            self.settings_popup_rect = rect;
            self.settings_hits = hits;
            vertices.extend_from_slice(&sv);
        }

        // Context menu overlay (F7) — drawn at mouse position.
        if let Some((x, y, _block_id)) = &self.context_menu_target {
            vertices.extend_from_slice(&self.build_context_menu_vertices(*x, *y));
        }

        // FindInGrid bar (v0.8 B3) — top banner with query + match count,
        // plus a yellow translucent highlight on the current match. Drawn
        // last so it composites above all other overlays.
        let mut find_btns: Option<FindButtons> = None;
        if let Some(find) = &self.find_state.clone() {
            let (find_verts, btns) = self.build_find_vertices(find);
            vertices.extend_from_slice(&find_verts);
            find_btns = Some(btns);
        }

        // v0.9 H1: Tab bar — drawn at the top of the window. The content
        // area is already shifted down by `chrome_top` in the LayoutCtx, so
        // this draws in the space above the content.
        // v1.2: always render the tab bar (even for a single tab) so the
        // "+" button is always available. Previously single-tab mode hid
        // the bar entirely, making "+" inaccessible without first opening a
        // second tab via menu/keyboard.
        if tab_bar.tab_count >= 1 {
            let (tab_verts, hits, new_tab_rect) = self.build_tab_bar_vertices(tab_bar);
            vertices.extend_from_slice(&tab_verts);
            self.tab_hits = hits;
            self.new_tab_rect = new_tab_rect;
        } else {
            self.tab_hits.clear();
            self.new_tab_rect = [0.0; 4]; // no "+" button in single-tab mode
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

        // v1.0 P1.5-B1: early-exit only when BOTH instances (grid) and
        // vertices (overlays / block view) are empty. In grid view with no
        // overlays, `vertices` is empty but `instances` carries the cells.
        if vertices.is_empty() && instances.is_empty() {
            // v1.0 P1.5-B2: if instances didn't change this frame (no dirty
            // rows, no cursor toggle, no scroll) and the offscreen texture
            // is available, the previous frame's grid content is still valid
            // on the offscreen. Just blit offscreen → drawable — no render
            // pass, no vertex upload, no draw calls. This is the idle-frame
            // fast path (target: < 0.5ms).
            let unchanged = self.instances_unchanged.get();
            let has_offscreen = unchanged && self.ensure_offscreen_texture();
            let command_buffer = self.queue.new_command_buffer();
            if has_offscreen {
                let blit = command_buffer.new_blit_command_encoder();
                let cache = self.offscreen_texture.borrow();
                let src_tex = cache.as_ref().unwrap();
                // v1.0 fix: clamp blit to the drawable's actual texture size
                // to prevent out-of-bounds writes during live resize.
                let blit_w = self.viewport.0.min(drawable_tex_size.0) as u64;
                let blit_h = self.viewport.1.min(drawable_tex_size.1) as u64;
                blit.copy_from_texture(
                    src_tex,
                    0,
                    0,
                    metal::MTLOrigin { x: 0, y: 0, z: 0 },
                    metal::MTLSize {
                        width: blit_w,
                        height: blit_h,
                        depth: 1,
                    },
                    drawable.texture(),
                    0,
                    0,
                    metal::MTLOrigin { x: 0, y: 0, z: 0 },
                );
                blit.end_encoding();
            } else {
                // No previous content to preserve — clear to background.
                let pass_desc = RenderPassDescriptor::new();
                let color_att = pass_desc.color_attachments().object_at(0).unwrap();
                color_att.set_texture(Some(drawable.texture()));
                color_att.set_load_action(MTLLoadAction::Clear);
                color_att.set_store_action(MTLStoreAction::Store);
                color_att.set_clear_color(MTLClearColor::new(bg_r, bg_g, bg_b, clear_a));
                let encoder = command_buffer.new_render_command_encoder(pass_desc);
                encoder.end_encoding();
            }
            command_buffer.present_drawable(drawable);
            command_buffer.commit();
            self.hit_regions = pending_hit_regions;
            self.find_buttons = find_btns;
            return;
        }

        // v1.0 P1.5-B0: Upload vertex buffer via triple-buffered ring.
        // Avoids per-frame `new_buffer_with_data` allocation (~540KB/frame).
        // Reuses buffers across frames; only allocates when capacity is
        // exceeded (e.g. on first frame or after resize to a larger grid).
        // v1.0 P1.5-B1: skip upload when `vertices` is empty (grid view with
        // no overlays); only the instance buffer is uploaded in that case.
        let vertex_data_size = (vertices.len() * std::mem::size_of::<f32>()) as u64;
        let mut ring_idx: usize = 0;
        if vertex_data_size > 0 {
            let mut ring = self.vertex_buffer_ring.borrow_mut();
            ring_idx = self.vertex_buffer_ring_idx.get();
            let cur_capacity = self.vertex_buffer_capacity.get();

            // Grow the ring buffer if needed (or allocate on first frame).
            if ring.is_empty() {
                // First frame: allocate 3 buffers with initial capacity.
                let new_capacity = (vertex_data_size * 3 / 2).div_ceil(4096) * 4096;
                for _ in 0..3 {
                    ring.push(
                        self.device.new_buffer(
                            new_capacity,
                            MTLResourceOptions::CPUCacheModeWriteCombined,
                        ),
                    );
                }
                self.vertex_buffer_capacity.set(new_capacity);
            } else if vertex_data_size > cur_capacity {
                // Grow: ALL buffers must be recreated at the new capacity.
                // Only replacing ring[ring_idx] would leave the other two at
                // the old (smaller) capacity, causing a buffer overflow when
                // the ring rotates to them on subsequent frames.
                let new_capacity = (vertex_data_size * 3 / 2).div_ceil(4096) * 4096;
                for buf in ring.iter_mut() {
                    *buf = self
                        .device
                        .new_buffer(new_capacity, MTLResourceOptions::CPUCacheModeWriteCombined);
                }
                self.vertex_buffer_capacity.set(new_capacity);
            }
            // Ensure ring has at least 3 buffers for triple-buffering.
            while ring.len() < 3 {
                let cap = self.vertex_buffer_capacity.get().max(4096);
                ring.push(
                    self.device
                        .new_buffer(cap, MTLResourceOptions::CPUCacheModeWriteCombined),
                );
            }

            // Write vertex data into the current ring buffer.
            let buffer = &ring[ring_idx];
            {
                let ptr = buffer.contents() as *mut u8;
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        vertices.as_ptr() as *const u8,
                        ptr,
                        vertex_data_size as usize,
                    );
                }
                // did_modify_range signals the GPU to re-read this region.
                buffer.did_modify_range(metal::NSRange {
                    location: 0,
                    length: vertex_data_size,
                });
            }

            // Advance ring index for next frame (triple-buffer rotation).
            self.vertex_buffer_ring_idx.set((ring_idx + 1) % ring.len());
        }

        // v1.0 P1.5-B1: Upload instance buffer via triple-buffered ring.
        // Same pattern as the vertex ring: 3 rotating buffers, grown on
        // demand, written via copy_nonoverlapping + did_modify_range.
        // Each instance is 16 floats (64 bytes) — for a 80×30 grid that's
        // ~150KB/frame vs the old ~540KB vertex buffer.
        let instance_data_size = (instances.len() * std::mem::size_of::<f32>()) as u64;
        let mut instance_ring_idx: usize = 0;
        if instance_data_size > 0 {
            let mut ring = self.instance_ring.borrow_mut();
            instance_ring_idx = self.instance_ring_idx.get();
            let cur_capacity = self.instance_capacity.get();

            if ring.is_empty() {
                // First frame: allocate 3 buffers with initial capacity.
                let new_capacity = (instance_data_size * 3 / 2).div_ceil(4096) * 4096;
                for _ in 0..3 {
                    ring.push(
                        self.device.new_buffer(
                            new_capacity,
                            MTLResourceOptions::CPUCacheModeWriteCombined,
                        ),
                    );
                }
                self.instance_capacity.set(new_capacity);
            } else if instance_data_size > cur_capacity {
                // Grow: recreate ALL buffers (see vertex ring comment above).
                let new_capacity = (instance_data_size * 3 / 2).div_ceil(4096) * 4096;
                for buf in ring.iter_mut() {
                    *buf = self
                        .device
                        .new_buffer(new_capacity, MTLResourceOptions::CPUCacheModeWriteCombined);
                }
                self.instance_capacity.set(new_capacity);
            }
            while ring.len() < 3 {
                let cap = self.instance_capacity.get().max(4096);
                ring.push(
                    self.device
                        .new_buffer(cap, MTLResourceOptions::CPUCacheModeWriteCombined),
                );
            }

            let buffer = &ring[instance_ring_idx];
            {
                let ptr = buffer.contents() as *mut u8;
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        instances.as_ptr() as *const u8,
                        ptr,
                        instance_data_size as usize,
                    );
                }
                buffer.did_modify_range(metal::NSRange {
                    location: 0,
                    length: instance_data_size,
                });
            }

            self.instance_ring_idx
                .set((instance_ring_idx + 1) % ring.len());
        }

        // v1.0 P1.5-B0: Use set_vertex_bytes for viewport uniform (8 bytes)
        // instead of allocating a new MTLBuffer every frame. This is the
        // Metal-recommended way to pass small constants (< 4KB).
        let vp_data: [f32; 2] = [self.viewport.0, self.viewport.1];

        // v1.0 P0-c: GPU scroll blit. Render to a persistent offscreen
        // texture instead of drawing directly to the drawable. This enables
        // blitting the unchanged region within the offscreen on scroll,
        // avoiding a full vertex rebuild + rasterization for the majority
        // of the screen that didn't change.
        //
        // v1.0 fix: when the drawable texture size doesn't match the viewport
        // (macOS live-resize async lag), skip the offscreen and render directly
        // to the drawable. Otherwise the offscreen→drawable blit would only
        // cover the top-left portion of the drawable, leaving stale content
        // elsewhere → "content squished to top-left" visual artifact.
        let has_offscreen = !vp_mismatch && self.ensure_offscreen_texture();

        let command_buffer = self.queue.new_command_buffer();

        // v1.0 P0-c: If scrolling and not force_full, blit the unchanged
        // region within the offscreen texture (src_y=Δ → dst_y=0) before
        // rendering. The render pass then uses load_action=Load to preserve
        // the blitted content, only drawing the newly exposed rows.
        let pending_scroll = self.pending_scroll_delta.get();
        let can_blit_scroll = has_offscreen
            && !self.force_full_cached.get()
            && pending_scroll != 0
            && self.offscreen_texture.borrow().is_some();

        if can_blit_scroll {
            let cache = self.offscreen_texture.borrow();
            let tex = cache.as_ref().unwrap();
            let ch = self.cell_height() as f32;
            let vp_h = self.viewport.1;
            // pending_scroll > 0 means content moved up (new lines at bottom).
            // Blit the bottom (vp_h - Δ*ch) region from y=Δ*ch to y=0.
            let delta_px = (pending_scroll.unsigned_abs() as f32 * ch).min(vp_h);
            if delta_px < vp_h {
                let blit = command_buffer.new_blit_command_encoder();
                let src_origin = metal::MTLOrigin {
                    x: 0,
                    y: delta_px as u64,
                    z: 0,
                };
                let dst_origin = metal::MTLOrigin { x: 0, y: 0, z: 0 };
                let size = metal::MTLSize {
                    width: self.viewport.0 as u64,
                    height: (vp_h - delta_px) as u64,
                    depth: 1,
                };
                blit.copy_from_texture(tex, 0, 0, src_origin, size, tex, 0, 0, dst_origin);
                blit.end_encoding();
            }
        }

        // Render pass — target the offscreen texture (or fall back to the
        // drawable if offscreen is unavailable).
        let pass_desc = RenderPassDescriptor::new();
        let color_att = pass_desc.color_attachments().object_at(0).unwrap();
        let target_tex: metal::Texture;
        if has_offscreen {
            let cache = self.offscreen_texture.borrow();
            target_tex = cache.as_ref().unwrap().clone();
            color_att.set_texture(Some(&target_tex));
            // P0-c fix: Only Clear when force_full (full rebuild). Incremental
            // frames must use Load to preserve the previous frame's offscreen
            // content — P0-b only re-renders dirty rows, so Clear would wipe
            // non-dirty rows to background, causing blank/flickering content.
            // When blitting, Load preserves the blitted scroll content.
            //
            // Flicker fix (Step 1): block view always rebuilds ALL vertices
            // every frame (no incremental path), so Load is correct in steady
            // state — Clear would briefly wipe the screen to background
            // between the clear and the redraw, which is the visible flicker.
            // Grid view keeps using force_full_cached to decide Clear vs Load.
            //
            // CRITICAL: must also Clear when force_full_grid is set (new/resize
            // offscreen texture with UNDEFINED content). Without this, Load
            // reads garbage → severe flicker + potential GPU issues. This flag
            // is set by ensure_offscreen_texture on (re)creation. Block view
            // must clear it here because it doesn't call build_grid_instances
            // (which clears it in grid view).
            //
            // v1.0 P1.5-B0 fix: removed `self.prev_show_blocks.get()` from the
            // condition. Block view now uses Load (incremental rendering) just
            // like grid view — the always-Clear was a workaround for the ring
            // bug (ring_idx-1) and itself caused flicker. With the ring bug
            // fixed, Load is correct for block view too.
            let need_clear = view_switched
                || self.force_full_grid.get()
                || (!self.prev_show_blocks.get() && self.force_full_cached.get());
            if need_clear {
                color_att.set_load_action(MTLLoadAction::Clear);
                color_att.set_clear_color(MTLClearColor::new(bg_r, bg_g, bg_b, clear_a));
                // Block view path doesn't go through build_grid_instances,
                // so force_full_grid would never be cleared — clear it here.
                self.force_full_grid.set(false);
            } else {
                color_att.set_load_action(MTLLoadAction::Load);
            }
        } else {
            color_att.set_texture(Some(drawable.texture()));
            // No offscreen: block view uses Clear+redraw (same as grid view)
            // because drawable Load after present is undefined in Metal.
            color_att.set_load_action(MTLLoadAction::Clear);
            color_att.set_clear_color(MTLClearColor::new(bg_r, bg_g, bg_b, clear_a));
        }
        color_att.set_store_action(MTLStoreAction::Store);

        let encoder = command_buffer.new_render_command_encoder(pass_desc);

        // v1.0 P1.5-B1: Draw 1 — grid instances (instanced pipeline). Each
        // instance is one cell (or cursor/hyperlink decoration); the static
        // index buffer + shader-side corner derivation mean only the
        // instance buffer changes per frame.
        let tex = self.atlas.texture();
        if instance_data_size > 0 {
            encoder.set_render_pipeline_state(&self.instanced_pipeline);
            // Instance buffer at slot 2 (matches `[[buffer(2)]]` in shader).
            let ring = self.instance_ring.borrow();
            // v1.0 P1.5-B0 fix: render the buffer we just wrote this frame
            // (ring[instance_ring_idx]), NOT instance_ring_idx-1.
            let cur = instance_ring_idx;
            encoder.set_vertex_buffer(2, Some(&ring[cur]), 0);
            // Viewport uniform at slot 1 (set_vertex_bytes, 8 bytes).
            encoder.set_vertex_bytes(1, 8, vp_data.as_ptr() as *const _);
            // Atlas + sampler (shared with legacy path — bound once here
            // because Metal retains fragment bindings across pipeline switches
            // within the same encoder).
            encoder.set_fragment_texture(0, Some(tex));
            encoder.set_fragment_sampler_state(0, Some(&self.sampler));

            let instance_count = instances.len() / 16;
            encoder.draw_indexed_primitives_instanced(
                MTLPrimitiveType::Triangle,
                6, // index count (two triangles)
                MTLIndexType::UInt16,
                &self.index_buffer,
                0, // index buffer offset
                instance_count as u64,
            );
        }

        // v1.0 P1.5-B1: Draw 2 — overlays / block view (legacy pipeline).
        // Uses the per-vertex descriptor (3×float4, stride 48) + B0 ring.
        if vertex_data_size > 0 {
            encoder.set_render_pipeline_state(&self.pipeline);
            let ring = self.vertex_buffer_ring.borrow();
            // v1.0 P1.5-B0 fix: render the buffer we just wrote this frame
            // (ring[ring_idx]), NOT ring_idx-1 (which is last frame's buffer
            // and caused content-change flicker in block view).
            let cur = ring_idx;
            encoder.set_vertex_buffer(0, Some(&ring[cur]), 0);
            // v1.0 P1.5-B0: set_vertex_bytes for viewport (8 bytes << 4KB limit).
            encoder.set_vertex_bytes(1, 8, vp_data.as_ptr() as *const _);
            // Atlas + sampler may already be bound from the instance draw;
            // rebind defensively in case only this path runs (block view).
            encoder.set_fragment_texture(0, Some(tex));
            encoder.set_fragment_sampler_state(0, Some(&self.sampler));

            let vertex_count = vertices.len() / 12;
            if vertex_count > 0 {
                encoder.draw_primitives(MTLPrimitiveType::Triangle, 0, vertex_count as u64);
            }
        }
        encoder.end_encoding();

        // v1.0 P0-c: If we rendered to the offscreen, blit it to the
        // drawable for presentation.
        if has_offscreen {
            let blit = command_buffer.new_blit_command_encoder();
            let cache = self.offscreen_texture.borrow();
            let src_tex = cache.as_ref().unwrap();
            let drawable_tex = drawable.texture();
            // v1.0 fix: clamp blit to the drawable's actual texture size to
            // prevent out-of-bounds writes during macOS live resize (when
            // next_drawable returns a texture with the previous dimensions).
            let blit_w = self.viewport.0.min(drawable_tex_size.0) as u64;
            let blit_h = self.viewport.1.min(drawable_tex_size.1) as u64;
            blit.copy_from_texture(
                src_tex,
                0,
                0,
                metal::MTLOrigin { x: 0, y: 0, z: 0 },
                metal::MTLSize {
                    width: blit_w,
                    height: blit_h,
                    depth: 1,
                },
                drawable_tex,
                0,
                0,
                metal::MTLOrigin { x: 0, y: 0, z: 0 },
            );
            blit.end_encoding();
        }

        command_buffer.present_drawable(drawable);
        command_buffer.commit();
        self.hit_regions = pending_hit_regions;
        // Store the find popup's button hit-test rects (computed during
        // build_find_vertices) now that the drawable borrow has ended.
        self.find_buttons = find_btns;
    }

    /// v1.0 P1.5-B1: Build per-cell instance data for the grid. Each cell
    /// becomes a single 64-byte instance (origin/size/uv_rect/fg/bg) drawn
    /// against a static 4-vertex quad + 6-index buffer. Replaces the old
    /// 6-vertex-per-cell emission (~288 B/cell) — ~4.5x smaller per-frame
    /// upload. The per-row cache (`grid_row_cache`) stores instance floats
    /// (16 per cell) instead of vertex floats (72 per cell).
    fn build_grid_instances(
        &self,
        grid: &weft_core::grid::Grid,
        palette: &[Color; 256],
        cursor: &weft_core::grid::Cursor,
        selection: &SelectionHandler,
        show_cursor: bool,
        cursor_style: CursorStyle,
    ) -> Vec<f32> {
        // Render at the atlas's native cell size — do NOT stretch cells to
        // fill the viewport (cw = viewport / num_cols). Stretching distorts
        // glyphs and, for full-width CJK, amplifies the baked intra-slot
        // padding into a gap that drifts wider on every resize (cw diverges
        // from cell_width as the window resizes within a column bucket). The
        // grid occupies num_cols * cell_width px; any remainder is background.
        // This also keeps rendered positions aligned with mouse hit-testing,
        // which already divides by cell_width.
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let num_rows = grid.num_rows;
        let num_cols = grid.num_cols;

        // Theme-derived colors (resolved per-frame from the active theme).
        let default_fg = color_to_normalized(self.theme.foreground);
        let default_bg = color_to_normalized(self.theme.background);
        let cursor_color = color_to_normalized(self.theme.cursor);
        // v1.0 fix: use an accent-based blend (35% accent + 65% background)
        // for the selection color. The old approach used `theme.selection`
        // directly, but many themes had selection colors too close to the
        // background (e.g. solarized-dark: bg #002b36, selection #073642 —
        // nearly invisible at 55% opacity). The accent color is designed
        // to contrast with the background, so blending it in ensures the
        // selection is visible across ALL themes. Each theme gets a
        // different selection tint because the accent varies per theme.
        let selection_bg = {
            let accent = color_to_normalized(self.theme.accent);
            let mut c = [
                accent[0] * 0.35 + default_bg[0] * 0.65,
                accent[1] * 0.35 + default_bg[1] * 0.65,
                accent[2] * 0.35 + default_bg[2] * 0.65,
                1.0,
            ];
            // Semi-transparent so the underlying text stays readable.
            c[3] = 0.60;
            c
        };

        // v1.0 P0-b: incremental grid rendering. Instead of iterating every
        // cell every frame, we cache per-row vertices and only rebuild rows
        // that changed (dirty-tracked by Grid). A full redraw is forced when:
        //   - the caller signals a global change (resize/theme/tab-switch)
        //   - the grid scrolled (scroll_offset differs → scrollback cells shown)
        //   - the grid dimensions changed (resize reflow)
        // Cursor row is always rebuilt (blink / move changes the cursor cell).
        let force_full = self.force_full_grid.get()
            || grid.scroll_offset != self.prev_scroll_offset.get()
            || self.grid_cache_dims.get() != (num_rows, num_cols)
            // Selection overlay changes cell colors — rebuild all rows while
            // a selection is active or being dragged.
            || selection.selection.is_some()
            || selection.selecting;

        // v1.0 P0-c: stash force_full on self so draw()'s epilogue (which
        // runs after this method returns) can decide whether to skip the
        // GPU scroll blit on forced-full frames.
        self.force_full_cached.set(force_full);

        // v1.0 P0-c: CPU-side cache shift for viewport scrolls. When the
        // terminal scrolls (newline at bottom), the viewport rows shift up
        // by N — the previously-rendered content at rows 0..rows-N is now
        // at rows N..rows. Instead of rebuilding all rows, we shift the
        // per-row vertex cache to match and only rebuild the newly exposed
        // rows (empty cache entries).
        //
        // Key correctness: pre-scroll cell writes set dirty_occ on those
        // rows, which moves WITH the rows during scroll_up. So dirty rows
        // are still detected and rebuilt even after the cache shift.
        let pending_scroll = grid.take_pending_scroll();
        // v1.0 P0-c: stash on self so draw()'s epilogue can issue a GPU blit.
        self.pending_scroll_delta.set(pending_scroll);
        if !force_full && pending_scroll != 0 {
            let mut cache = self.grid_row_cache.borrow_mut();
            if cache.len() == num_rows {
                if pending_scroll > 0 {
                    // Scroll up: rows moved up, new blank rows at bottom.
                    let d = pending_scroll as usize;
                    if d < cache.len() {
                        cache.drain(0..d);
                        for _ in 0..d {
                            cache.push(Vec::new());
                        }
                    } else {
                        for c in cache.iter_mut() {
                            c.clear();
                        }
                    }
                } else {
                    // Scroll down: rows moved down, new blank rows at top.
                    let d = (-pending_scroll) as usize;
                    if d < cache.len() {
                        for _ in 0..d {
                            cache.insert(0, Vec::new());
                        }
                        cache.truncate(num_rows);
                    } else {
                        for c in cache.iter_mut() {
                            c.clear();
                        }
                    }
                }
            }
        }

        // Determine which rows need rebuilding.
        // v1.0 P1.5-B2: only rebuild the cursor row when its state actually
        // changed (blink toggle, move, or show/hide). Previously the cursor
        // row was rebuilt every frame — wasteful for steady (non-blinking)
        // cursors or idle terminals where nothing changes. `show_cursor`
        // already encodes blink phase (caller passes
        // `cursor_visible && cursor_blink_on && prompt.is_none()`), so a
        // stable `show_cursor` + stable position means the cached cursor
        // cell is still correct.
        let cursor_changed = force_full
            || self.prev_show_cursor.get() != show_cursor
            || self.prev_cursor_row.get() != Some(cursor.row)
            || self.prev_cursor_col.get() != Some(cursor.col);
        let mut rows_to_rebuild: Vec<usize> = if force_full {
            (0..num_rows).collect()
        } else {
            let mut dirty: Vec<usize> = grid.dirty_rows().map(|(r, _)| r).collect();
            if cursor_changed {
                if !dirty.contains(&cursor.row) {
                    dirty.push(cursor.row);
                }
                // Previous cursor row: when the cursor moves, the old row
                // loses its cursor overlay and must be rebuilt to show plain
                // content.
                if let Some(prev) = self.prev_cursor_row.get() {
                    if prev != cursor.row && !dirty.contains(&prev) {
                        dirty.push(prev);
                    }
                }
            }
            dirty
        };

        // Update cached state for next frame's comparison.
        self.force_full_grid.set(false);
        self.prev_cursor_row.set(Some(cursor.row));
        self.prev_cursor_col.set(Some(cursor.col));
        self.prev_show_cursor.set(show_cursor);
        self.prev_scroll_offset.set(grid.scroll_offset);
        self.grid_cache_dims.set((num_rows, num_cols));

        // Build vertices for dirty rows only. We build into a local Vec
        // (not the RefCell) to avoid holding a RefMut while accessing self
        // fields (atlas, layout_ctx, etc.) inside the per-cell loop.
        let mut cache = self.grid_row_cache.borrow_mut();
        if cache.len() != num_rows {
            cache.resize(num_rows, Vec::new());
        }

        // v1.0 P0-c: after cache shift, add rows with empty cache entries
        // (newly exposed by scroll) to the rebuild set.
        if !force_full && pending_scroll != 0 {
            for (i, rv) in cache.iter().enumerate() {
                if rv.is_empty() && !rows_to_rebuild.contains(&i) {
                    rows_to_rebuild.push(i);
                }
            }
        }

        // v1.0 P1.5-B2: if no rows need rebuilding this frame, skip the
        // per-cell loop + flatten memcpy + GPU upload entirely. The draw()
        // method reads `instances_unchanged` and relies on the offscreen
        // render pass's `Load` action to preserve the previous frame's grid
        // content. This is the key optimization for idle frames: no terminal
        // output, no cursor blink toggle, no scroll → no work.
        if rows_to_rebuild.is_empty() {
            drop(cache);
            self.instances_unchanged.set(true);
            return Vec::new();
        }
        self.instances_unchanged.set(false);

        for &row in &rows_to_rebuild {
            // v1.0 P1.5-B1: per-row instance buffer. 16 floats/cell + slack
            // for cursor bar/underline + hyperlink underline decorations.
            let mut instances = Vec::with_capacity(num_cols * 16 + 48);
            for col in 0..num_cols {
                let cell = grid.cell(row, col);

                // Skip wide char spacers (rendered as part of the preceding cell)
                if cell.flags.contains(CellFlags::WIDE_SPACER) {
                    continue;
                }

                let chrome_left = self.layout_ctx.map(|c| c.chrome_left).unwrap_or(0.0);
                let x = self.padding_x + chrome_left + col as f32 * cw;
                // v0.9 H1 fix: shift grid down by chrome_top (tab bar height)
                // so the first row isn't covered by the tab bar. The LayoutCtx
                // is set on self at the top of draw(); chrome_top is 0 when
                // there's only one tab (no tab bar drawn).
                let chrome_top = self.layout_ctx.map(|c| c.chrome_top).unwrap_or(0.0);
                let y = self.padding_y + chrome_top + row as f32 * ch;

                // Determine cell colors (resolve the cell's color-origin against
                // the palette / theme defaults).
                let mut fg = resolve_cell_color(cell.fg, default_fg, palette);
                let mut bg = resolve_cell_color(cell.bg, default_bg, palette);

                // v1.0 fix: honor SGR reverse video (DEC SGR 7 / `CSI 7m`).
                // The VT parser sets CellFlags::REVERSE on cells printed while
                // inverse video is active (e.g. `less` search-match highlight).
                // Without this swap, matched text in `less` jumps to the right
                // place but is never highlighted — it renders with normal
                // fg/bg. Swap BEFORE the alpha scaling below so the opacity is
                // applied to the (now background) color consistently.
                if cell.flags.contains(CellFlags::REVERSE) {
                    std::mem::swap(&mut fg, &mut bg);
                }

                // Scale the plain background alpha by window opacity so empty
                // cells show the desktop through them. Text/selection/cursor
                // pick their own colors with alpha 1.0 in `final_bg` below, so
                // they stay fully opaque regardless of this scaling.
                bg[3] *= self.opacity;

                // Check if this is the cursor position
                let is_cursor = show_cursor && row == cursor.row && col == cursor.col;

                // Check if this cell is in the selection
                let is_selected = selection
                    .selection
                    .as_ref()
                    .is_some_and(|sel| sel.contains(row, col));

                // Look up glyph UV
                let ch_char =
                    if cell.character == '\0' || cell.flags.contains(CellFlags::WIDE_SPACER) {
                        ' '
                    } else {
                        cell.character
                    };

                let (u0, v0, u1, v1) = if let Some(glyph) = self.atlas.get(ch_char) {
                    let (u, v) = glyph.uv_origin;
                    let (uw, vh) = glyph.uv_size;
                    (u, v, u + uw, v + vh)
                } else {
                    // Character not in atlas — use space
                    let (u, v) = self
                        .atlas
                        .get(' ')
                        .map(|g| g.uv_origin)
                        .unwrap_or((0.0, 0.0));
                    let (uw, vh) = self.atlas.get(' ').map(|g| g.uv_size).unwrap_or((0.0, 0.0));
                    (u, v, u + uw, v + vh)
                };
                // Swap V to compensate for the CAMetalLayer's vertical flip: keep the
                // glyph upright on screen while clip.y maps row 0 to the top.
                let (v0, v1) = (v1, v0);

                // Override colors for cursor
                let final_fg = if is_cursor {
                    if cursor_style == CursorStyle::Block {
                        [0.0, 0.0, 0.0, 1.0] // Black text on cursor block
                    } else {
                        cursor_color
                    }
                } else {
                    fg
                };

                let final_bg = if is_cursor && cursor_style == CursorStyle::Block {
                    cursor_color
                } else if is_selected {
                    selection_bg
                } else if is_cursor
                    && (cursor_style == CursorStyle::Bar
                        || cursor_style == CursorStyle::BlinkingBar)
                {
                    // Bar cursor: only highlight the left 2 pixels
                    // We'll draw the full cell with normal bg, then overlay bar later
                    bg
                } else {
                    bg
                };

                // Determine cell width for rendering
                let cell_render_width = if cell.width == CellWidth::Full && col + 1 < num_cols {
                    cw * 2.0
                } else {
                    cw
                };

                let x0 = x;
                let y0 = y;
                let x1 = x + cell_render_width;
                let y1 = y + ch;

                // v1.0 P1.5-B1: emit one instance per cell. The V-swap
                // (`(v0, v1) = (v1, v0)` above) is stored directly in the
                // instance's uv_rect: corner.y=0 (top of cell) samples v0
                // (bottom of glyph in atlas space), compensating for the
                // CAMetalLayer vertical flip — same convention as the old
                // per-vertex emission.
                push_cell_instance(
                    &mut instances,
                    [x0, y0, x1, y1],
                    [u0, v0, u1, v1],
                    final_fg,
                    final_bg,
                );

                // Draw bar/underline cursor overlay
                if is_cursor && show_cursor {
                    match cursor_style {
                        CursorStyle::Bar | CursorStyle::BlinkingBar => {
                            let bar_w = 2.0 * (self.viewport.0 / grid.num_cols as f32 / cw);
                            let bar_w = bar_w.max(1.0).min(cw * 0.15);
                            // v1.0 P1.5-B1: decoration instance. UV rect
                            // (0,0,0,1) samples the atlas at u=0 (empty)
                            // so mask=0 → only bg (cursor color) shows.
                            push_cell_instance(
                                &mut instances,
                                [x0, y0, x0 + bar_w, y1],
                                [0.0, 0.0, 0.0, 1.0],
                                [0.0; 4],
                                cursor_color,
                            );
                        }
                        CursorStyle::Underline | CursorStyle::BlinkingUnderline => {
                            let line_h = 2.0;
                            push_cell_instance(
                                &mut instances,
                                [x0, y1 - line_h, x1, y1],
                                [0.0, 0.0, 0.0, 1.0],
                                [0.0; 4],
                                cursor_color,
                            );
                        }
                        _ => {} // Block cursor handled above
                    }
                }

                // OSC 8 hyperlink underline: a thin cyan line at the cell's
                // baseline. Click handling is in main.rs (Cmd+Click → open URL
                // from the registry's side-map). Wide-char cells span 2 cols.
                if cell.flags.contains(CellFlags::HYPERLINK) {
                    let line_h = 1.5;
                    let link_color = [0.36, 0.62, 0.94, 1.0]; // soft cyan
                    push_cell_instance(
                        &mut instances,
                        [x0, y1 - line_h, x1, y1],
                        [0.0, 0.0, 0.0, 1.0],
                        [0.0; 4],
                        link_color,
                    );
                }
            }
            // v1.0 P1.5-B1: store this row's instances into the cache.
            cache[row] = instances;
        }

        // v1.0 P0-b: flatten the per-row cache into a single instance buffer.
        // Clean rows are reused from the previous frame; dirty rows were
        // rebuilt above. Each cell is 16 floats (one CellInstance).
        let mut out = Vec::with_capacity(num_rows * num_cols * 16);
        for rv in cache.iter() {
            out.extend_from_slice(rv);
        }
        out
    }

    /// Build vertices for the right-side history panel overlay: a translucent
    /// background, a search box, and one row per finished block (newest first,
    /// filtered by the query), color-coded by exit code. The selected row is
    /// highlighted and, if expanded, its output is shown beneath. Drawn after
    /// the grid so it composites on top via the enabled alpha blend.
    fn build_panel_vertices(&self, p: &PanelDrawParams) -> Vec<f32> {
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let _vp_w = self.viewport.0;
        let vp_h = self.viewport.1;
        let width_px = p.width_px;
        if width_px <= 0.0 || cw <= 0.0 || ch <= 0.0 {
            return Vec::new();
        }
        // v0.9 W5: panel is now a LEFT sidebar — anchor to the left edge.
        let panel_x = 0.0;
        let panel_cols = ((width_px / cw) as usize).max(1);
        let mut vertices = Vec::new();

        // v1.0 Warp-style: opaque panel background, slightly darkened.
        let theme_bg = color_to_normalized(self.theme.background);
        let panel_bg = [
            theme_bg[0] * 0.55,
            theme_bg[1] * 0.55,
            theme_bg[2] * 0.55,
            1.0,
        ];
        let separator_color = color_to_normalized(self.theme.separator);
        // v1.0: accent-based selection highlight (consistent with grid/block).
        // v1.0 P3: α 0.95→1.0 to match Settings/Palette selection_bg.
        let sel_bg = {
            let accent = color_to_normalized(self.theme.accent);
            [
                accent[0] * 0.35 + theme_bg[0] * 0.65,
                accent[1] * 0.35 + theme_bg[1] * 0.65,
                accent[2] * 0.35 + theme_bg[2] * 0.65,
                1.0,
            ]
        };
        let (su, sv, suw, svh) = self.space_uv();
        // V-swap to match grid rendering (CAMetalLayer flip compensation).
        let bg_uv = [su, sv + svh, su + suw, sv];
        // v0.9 W5: start the panel bg below the tab bar (chrome_top) so it
        // doesn't cover the tab bar, mirroring the content area.
        let chrome_top = self.layout_ctx.map(|c| c.chrome_top).unwrap_or(0.0);
        push_quad(
            &mut vertices,
            [panel_x, chrome_top, panel_x + width_px, vp_h],
            bg_uv,
            [0.0; 4],
            panel_bg,
        );

        // v0.9 fix: 1px separator on the right edge (Warp-style divider)
        // so the sidebar reads as a distinct surface, not floating content.
        push_quad(
            &mut vertices,
            [
                panel_x + width_px - 1.0,
                chrome_top,
                panel_x + width_px,
                vp_h,
            ],
            bg_uv,
            [0.0; 4],
            separator_color,
        );

        let fg = color_to_normalized(self.theme.foreground);
        // v1.0 P0: replace fg*0.6 dim with label_c (70% fg + 30% bg) —
        // consistent with Settings/Palette/Find and always readable.
        let dim = [
            fg[0] * 0.70 + theme_bg[0] * 0.30,
            fg[1] * 0.70 + theme_bg[1] * 0.30,
            fg[2] * 0.70 + theme_bg[2] * 0.30,
            1.0,
        ];
        let green = [0.53, 0.80, 0.36, 1.0];
        let red = [0.85, 0.36, 0.36, 1.0];

        // v0.9 fix: Warp-style search input field — a distinct rounded-look
        // box with its own background and border, so it reads as an input
        // field rather than blending into the panel. When focused, the border
        // turns accent color and a blinking cursor is drawn.
        let field_pad_x = cw * 0.5;
        // v0.9 fix: add a "History" header above the search field for a
        // clearer panel identity (Warp-style section title).
        let header_y = chrome_top + ch * 0.4;
        self.push_text(
            &mut vertices,
            panel_x + cw * 0.5,
            header_y,
            "History",
            fg,
            panel_cols,
        );
        let field_pad_y = ch * 1.6;
        let field_x0 = panel_x + field_pad_x;
        let field_y0 = chrome_top + field_pad_y;
        let field_x1 = panel_x + width_px - field_pad_x;
        let field_h = ch * 1.4;
        let field_y1 = field_y0 + field_h;
        // Input field background: slightly lighter than panel bg.
        let field_bg = [
            panel_bg[0] + (1.0 - panel_bg[0]) * 0.08,
            panel_bg[1] + (1.0 - panel_bg[1]) * 0.08,
            panel_bg[2] + (1.0 - panel_bg[2]) * 0.08,
            1.0,
        ];
        push_quad(
            &mut vertices,
            [field_x0, field_y0, field_x1, field_y1],
            bg_uv,
            [0.0; 4],
            field_bg,
        );
        // Border: accent when focused (was accent_dim — invisible in
        // Nord/Warp themes), separator otherwise.
        let border_color = if p.search_focused {
            color_to_normalized(self.theme.accent)
        } else {
            separator_color
        };
        let border_w = if p.search_focused { 2.0 } else { 1.0 };
        // Top border
        push_quad(
            &mut vertices,
            [field_x0, field_y0, field_x1, field_y0 + border_w],
            bg_uv,
            [0.0; 4],
            border_color,
        );
        // Bottom border
        push_quad(
            &mut vertices,
            [field_x0, field_y1 - border_w, field_x1, field_y1],
            bg_uv,
            [0.0; 4],
            border_color,
        );
        // Left border
        push_quad(
            &mut vertices,
            [field_x0, field_y0, field_x0 + border_w, field_y1],
            bg_uv,
            [0.0; 4],
            border_color,
        );
        // Right border
        push_quad(
            &mut vertices,
            [field_x1 - border_w, field_y0, field_x1, field_y1],
            bg_uv,
            [0.0; 4],
            border_color,
        );

        // Text inside the field: show query, or placeholder "Search…" when empty.
        let text_y = field_y0 + (field_h - ch) * 0.5;
        let text_x = field_x0 + cw * 0.4;
        let text_cols = ((field_x1 - text_x - cw * 0.4) / cw) as usize;
        if p.query.is_empty() {
            self.push_text(&mut vertices, text_x, text_y, "Search…", dim, text_cols);
        } else {
            self.push_text(&mut vertices, text_x, text_y, p.query, fg, text_cols);
        }

        // Blinking cursor at the end of the query text when focused.
        if p.search_focused {
            let blink_phase = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() % 1200)
                .unwrap_or(0);
            if blink_phase < 600 {
                let query_w = p.query.chars().count() as f32 * cw;
                let cursor_x = text_x + query_w;
                let cursor_w = 2.0_f32.max(cw * 0.12);
                push_quad(
                    &mut vertices,
                    [cursor_x, text_y, cursor_x + cursor_w, text_y + ch],
                    bg_uv,
                    [0.0; 4],
                    fg,
                );
            }
        }

        // Display list: newest-first, filtered by query (capped to fit).
        let max_rows = visible_panel_rows(vp_h, self.cell_height());
        let display = panel_display(p.blocks, p.query, max_rows);
        let row_h = ch * 1.1;
        // v0.9 fix: list starts below the search field + gap.
        let mut y = field_y1 + ch * 0.4;
        let mut drawn = 0usize;

        for (i, block) in display.iter().enumerate() {
            if drawn >= max_rows || y + ch > vp_h {
                break;
            }
            let selected = i == p.selection;
            if selected {
                push_quad(
                    &mut vertices,
                    [panel_x, y - ch * 0.1, panel_x + width_px, y + ch],
                    bg_uv,
                    [0.0; 4],
                    sel_bg,
                );
            }
            let cmd_color = if selected {
                fg
            } else {
                match block.exit_code {
                    Some(0) => green,
                    Some(_) => red,
                    None => dim,
                }
            };
            let dur = block_duration_str(block);
            let dur_len = dur.chars().count();
            let cmd_cols = panel_cols.saturating_sub(dur_len + 2).max(1);
            // v0.9 fix: strip prompt prefix so old blocks (captured via
            // snapshot_command_line) show just the command, matching the
            // clean format of editor-submitted commands.
            let cleaned = strip_prompt_prefix(&block.command);
            let label = truncate_str(&cleaned, cmd_cols);
            self.push_text(
                &mut vertices,
                panel_x + cw * 0.5,
                y,
                &label,
                cmd_color,
                cmd_cols,
            );
            if !dur.is_empty() {
                let dur_x = panel_x + width_px - cw * 0.5 - dur_len as f32 * cw;
                self.push_text(&mut vertices, dur_x, y, &dur, dim, dur_len + 1);
            }
            y += row_h;
            drawn += 1;

            // Expanded output for the selected block.
            if Some(block.id) == p.expanded_id {
                let out_cols = panel_cols.saturating_sub(2).max(1);
                for line in block.output.lines().take(8) {
                    if drawn >= max_rows || y + ch > vp_h {
                        break;
                    }
                    let rendered = truncate_str(line, out_cols);
                    self.push_text(
                        &mut vertices,
                        panel_x + cw * 1.5,
                        y,
                        &rendered,
                        dim,
                        out_cols,
                    );
                    y += row_h;
                    drawn += 1;
                }
            }
        }

        vertices
    }

    /// Build vertices for the bottom editor input box (v0.5 editor takeover):
    /// a translucent panel pinned to the bottom, a `❯ <cwd>` prompt, the editor
    /// buffer lines, a cursor bar, and the Ctrl+R search UI when active. Drawn
    /// after the grid so it composites on top via the enabled alpha blend.
    fn build_prompt_vertices(
        &self,
        p: &PromptDrawParams,
        cursor_blink_phase: f32,
        cursor_blink_on: bool,
    ) -> Vec<f32> {
        let mut verts = Vec::new();
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let vp_w = self.viewport.0;
        let vp_h = self.viewport.1;
        if cw <= 0.0 || ch <= 0.0 || vp_w <= 0.0 || vp_h <= 0.0 {
            self.prompt_box_rect.set(None);
            return verts;
        }

        // v0.8: cursor X must use the DISPLAY width of chars before the cursor,
        // not the char count — CJK chars occupy 2 columns each, so `cc × cw`
        // leaves the caret stranded mid-cell for input like "Weft项目设计.md".
        // Sum the actual rendered columns of the first `cc` chars on this line.
        let (cl, cc) = p.cursor;
        let cursor_offset_cols = p
            .lines
            .get(cl)
            .map(|line| {
                line.chars()
                    .take(cc)
                    .map(Self::char_col_width)
                    .sum::<usize>()
            })
            .unwrap_or(cc);

        // v0.8 stage 4: layout (box rect, text_y0, cursor X/Y, bar_w) is
        // computed by the pure function in `layout.rs`. The renderer keeps
        // responsibility for vertex building, theming, and text rasterization.
        let ctx = self.layout_ctx.expect("LayoutCtx built at draw() entry");
        let layout = crate::layout::layout_prompt(&ctx, p.lines.len(), cl, cursor_offset_cols);
        // v0.9: cache the prompt box rect so the app can detect clicks on it
        // (for select-all-then-copy after double-click sends a command here).
        // Uses Cell for interior mutability — `draw` holds an immutable borrow
        // of `self.layer` (from next_drawable) so `&mut self` is unavailable.
        self.prompt_box_rect.set(Some(layout.box_rect));
        let text_y0 = layout.text_y0;
        let left = layout.left;
        let box_cols = layout.box_cols;
        let first_line_text_x = layout.first_line_text_x;
        let cx = layout.cursor_x;
        let cy = layout.cursor_y;
        let bar_w = layout.bar_w;

        let theme_bg = color_to_normalized(self.theme.background);
        // The input area uses the SAME background as the window (Warp style —
        // no distinct input panel). Still opaque so it cleanly covers the grid
        // rows behind it (the shell's blank prompt sits under the box).
        let box_bg = theme_bg;
        let fg = color_to_normalized(self.theme.foreground);
        let accent = color_to_normalized(self.theme.cursor);
        let (su, sv, suw, svh) = self.space_uv();
        // V-swap to match grid rendering (CAMetalLayer flip compensation).
        let bg_uv = [su, sv + svh, su + suw, sv];

        // Uniform window background for the input area (no distinct panel).
        push_quad(&mut verts, layout.box_rect, bg_uv, [0.0; 4], box_bg);

        // Ctrl+R search UI replaces the normal prompt.
        if let Some((query, selected)) = p.search {
            let label = "search: ";
            self.push_text(&mut verts, left, text_y0, label, fg, box_cols);
            let qx = left + label.chars().count() as f32 * cw;
            self.push_text(&mut verts, qx, text_y0, query, accent, box_cols);
            if let Some(m) = selected {
                self.push_text(&mut verts, left, text_y0 + ch, m, fg, box_cols);
            }
            return verts;
        }

        // Prompt glyph only — cwd lives in the block history, not the input
        // box (Warp style), so the typed command never runs into the path.
        // v1.0 fix: use accent (not accent_dim) for the prompt marker ❯ —
        // accent_dim is invisible in Nord/Warp themes. The prompt ❯ is a
        // primary UI element, not dim chrome, so accent is appropriate.
        let prompt_str = "❯ ";
        let prompt_chars = 2;
        let prompt_c = color_to_normalized(self.theme.accent);
        self.push_text(
            &mut verts,
            left,
            text_y0,
            prompt_str,
            prompt_c,
            prompt_chars,
        );

        // Editor buffer lines (line 0 starts after the prompt).
        // v0.9: draw a selection highlight for the active mouse-drag range.
        // v1.0 fix: accent-based blend for selection visibility across all themes.
        let sel_bg = {
            let accent = color_to_normalized(self.theme.accent);
            let bg = color_to_normalized(self.theme.background);
            [
                accent[0] * 0.35 + bg[0] * 0.65,
                accent[1] * 0.35 + bg[1] * 0.65,
                accent[2] * 0.35 + bg[2] * 0.65,
                0.60,
            ]
        };
        if let Some(((sl, sc), (el, ec))) = p.selection {
            for i in sl..=el {
                let Some(line) = p.lines.get(i) else {
                    continue;
                };
                let y = text_y0 + i as f32 * ch;
                let (line_start_x, max_chars) = if i == 0 {
                    let avail = box_cols.saturating_sub(prompt_chars).max(1);
                    (first_line_text_x, avail)
                } else {
                    (left, box_cols)
                };
                // Char column range within this line.
                let col_start = if i == sl { sc } else { 0 };
                let col_end = if i == el { ec } else { line.chars().count() };
                if col_start >= col_end {
                    continue;
                }
                // Convert char columns → display columns (CJK = 2 cells).
                let chars: Vec<char> = line.chars().collect();
                let disp_start: usize = chars
                    .iter()
                    .take(col_start)
                    .map(|c| Self::char_col_width(*c))
                    .sum();
                let disp_len: usize = chars
                    .iter()
                    .skip(col_start)
                    .take(col_end - col_start)
                    .map(|c| Self::char_col_width(*c))
                    .sum();
                let disp_len = disp_len.min(max_chars.saturating_sub(disp_start));
                if disp_len > 0 {
                    let x0 = line_start_x + disp_start as f32 * cw;
                    push_quad(
                        &mut verts,
                        [x0, y, x0 + disp_len as f32 * cw, y + ch],
                        bg_uv,
                        [0.0; 4],
                        sel_bg,
                    );
                }
            }
        }
        for (i, line) in p.lines.iter().enumerate() {
            let y = text_y0 + i as f32 * ch;
            let (start_x, max_chars) = if i == 0 {
                let avail = box_cols.saturating_sub(prompt_chars).max(1);
                (first_line_text_x, avail)
            } else {
                (left, box_cols)
            };
            self.push_line_tokenized(&mut verts, start_x, y, line, max_chars);
        }

        // ── v0.8 signature: warm cursor breath + amber glow ─────────────
        // Smooth sin() alpha over a 2400ms period (phase in radians).
        // sin maps [0, 2π) → [-1, 1]; we remap to [0.25, 1.0] so the caret
        // never fully disappears (calmer than hard on/off). The glow halo
        // is a wider, very-low-alpha amber quad behind the caret that
        // breathes in sync (peaks at ~0.25 alpha).
        //
        // When `cursor_blink_on` is false (window unfocused OR user is
        // actively selecting — see main.rs::RedrawRequested), the breath
        // freezes at peak alpha so the caret stays visible but calm.
        let s = if cursor_blink_on {
            cursor_blink_phase.sin()
        } else {
            1.0_f32
        };
        let caret_alpha = 0.625 + 0.375 * s; // → [0.25, 1.0]
        let glow_alpha = 0.15 + 0.10 * s; // → [0.05, 0.25]
        let accent_color = [accent[0], accent[1], accent[2], accent[3] * caret_alpha];
        // Glow: a wider quad (~3× bar width, full cell height) behind the
        // caret. Drawn first so the caret composites on top.
        let glow_pad = bar_w * 1.5;
        let glow_color = [accent[0], accent[1], accent[2], glow_alpha.max(0.0)];
        push_quad(
            &mut verts,
            [cx - glow_pad, cy, cx + bar_w + glow_pad, cy + ch],
            bg_uv,
            [0.0; 4],
            glow_color,
        );
        // Caret itself.
        push_quad(
            &mut verts,
            [cx, cy, cx + bar_w, cy + ch],
            bg_uv,
            [0.0; 4],
            accent_color,
        );

        // IME preedit right after the cursor.
        if let Some(preedit) = p.preedit {
            if !preedit.is_empty() {
                self.push_text(&mut verts, cx + bar_w, cy, preedit, accent, box_cols);
            }
        }

        verts
    }

    /// Build the Tab-completion dropdown as a floating popup above the prompt
    /// input box. Split out from `build_prompt_vertices` for the overlay stack
    /// Draw a Warp-style resize drag handle on the right and/or top border
    /// of a popup. The handle is two small triangles pointing inward, with a
    /// short line between them — signaling "drag to resize".
    fn draw_resize_handles(
        &self,
        verts: &mut Vec<f32>,
        popup_x0: f32,
        popup_top: f32,
        popup_x1: f32,
        popup_bottom: f32,
        bg_uv: [f32; 4],
    ) {
        let handle_color = [0.55, 0.55, 0.55, 0.85];
        let s = 4.0; // triangle half-size
        let gap = 6.0; // gap between the two triangles (line length)

        // Right border: two triangles pointing inward (◀ ▶) + connecting line.
        let mid_y = (popup_top + popup_bottom) / 2.0;
        let rx = popup_x1;
        // Upper triangle: points toward center (tip at rx, base at rx-s).
        push_triangle(
            verts,
            [rx - s, mid_y - gap - s],
            [rx, mid_y - gap],
            [rx - s, mid_y - gap],
            handle_color,
            bg_uv,
        );
        // Lower triangle: points toward center.
        push_triangle(
            verts,
            [rx - s, mid_y + gap + s],
            [rx, mid_y + gap],
            [rx - s, mid_y + gap],
            handle_color,
            bg_uv,
        );
        // Connecting line.
        push_quad(
            verts,
            [rx - 1.5, mid_y - gap, rx, mid_y + gap],
            bg_uv,
            [0.0; 4],
            handle_color,
        );

        // Top border: two triangles pointing inward + connecting line.
        let mid_x = (popup_x0 + popup_x1) / 2.0;
        let ty = popup_top;
        // Left triangle: points toward center (tip at mid_x-gap).
        push_triangle(
            verts,
            [mid_x - gap - s, ty],
            [mid_x - gap - s, ty + s],
            [mid_x - gap, ty + s / 2.0],
            handle_color,
            bg_uv,
        );
        // Right triangle: points toward center.
        push_triangle(
            verts,
            [mid_x + gap + s, ty],
            [mid_x + gap + s, ty + s],
            [mid_x + gap, ty + s / 2.0],
            handle_color,
            bg_uv,
        );
        // Connecting line.
        push_quad(
            verts,
            [mid_x - gap, ty, mid_x + gap, ty + 1.5],
            bg_uv,
            [0.0; 4],
            handle_color,
        );
    }

    /// Build the Tab-completion dropdown as a floating popup above the prompt
    /// z-order (Completion) and hit-test regions.
    ///
    /// `anchor_y` is the prompt box's top edge (`box_y0`) — the popup sits
    /// directly above it. This was previously computed inside
    /// `build_prompt_vertices` as `popup_bottom = box_y0`.
    /// Returns (vertices, popup_rect) where popup_rect is the bounding box
    /// for border drag-resize hot-zone detection.
    fn build_completion_vertices(
        &self,
        matches: &[weft_core::complete::Match],
        selected: usize,
        anchor_y: f32,
        box_x0: f32,
    ) -> (Vec<f32>, Option<[f32; 4]>) {
        let mut verts = Vec::new();
        if matches.is_empty() {
            return (verts, None);
        }
        let ch = self.cell_height() as f32;
        let theme_bg = color_to_normalized(self.theme.background);
        let fg = color_to_normalized(self.theme.foreground);
        let (su, sv, suw, svh) = self.space_uv();
        let bg_uv = [su, sv + svh, su + suw, sv];

        let label_color = [
            fg[0] * 0.85 + theme_bg[0] * 0.15,
            fg[1] * 0.85 + theme_bg[1] * 0.15,
            fg[2] * 0.85 + theme_bg[2] * 0.15,
            1.0,
        ];
        let sel_label_color = fg;
        // v1.0 fix: blend with bg (same fix as palette popup). The old
        // fg*0.40 was invisible in Solarized Dark / One Dark / Nord.
        let suffix_color = [
            fg[0] * 0.50 + theme_bg[0] * 0.50,
            fg[1] * 0.50 + theme_bg[1] * 0.50,
            fg[2] * 0.50 + theme_bg[2] * 0.50,
            1.0,
        ];

        // v0.8 stage 4: layout (window + popup rect + column anchors) is
        // computed by the pure functions in `layout.rs`, so it can be unit-
        // tested without the GPU. The renderer keeps responsibility for
        // vertex building, theming, and text rasterization.
        let ctx = self.layout_ctx.expect("LayoutCtx built at draw() entry");
        let (start, end, _shown) = crate::layout::completion_window(
            anchor_y,
            ch,
            self.popup_max_rows,
            selected,
            matches.len(),
        );
        let max_label_cols = matches[start..end]
            .iter()
            .map(|m| Self::text_col_width(&m.label))
            .max()
            .unwrap_or(10);
        let layout = crate::layout::layout_completion(
            &ctx,
            start,
            end,
            max_label_cols,
            anchor_y,
            box_x0,
            self.popup_width_scale,
        );

        let [popup_x0, popup_top, popup_x1, popup_bottom] = layout.popup_rect;
        let icon_x = layout.icon_x;
        let label_x = layout.label_x;
        let suffix_x = layout.suffix_x;
        let label_cols = layout.label_cols;
        let suffix_cols = layout.suffix_cols;

        let border_c = [0.5, 0.5, 0.5, 0.35];
        let popup_bg = [
            theme_bg[0] + (1.0 - theme_bg[0]) * 0.05,
            theme_bg[1] + (1.0 - theme_bg[1]) * 0.05,
            theme_bg[2] + (1.0 - theme_bg[2]) * 0.05,
            1.0,
        ];

        push_quad(&mut verts, layout.popup_rect, bg_uv, [0.0; 4], popup_bg);
        for (bx0, by0, bx1, by1) in [
            (popup_x0, popup_top, popup_x1, popup_top + 1.0),
            (popup_x0, popup_bottom - 1.0, popup_x1, popup_bottom),
            (popup_x0, popup_top, popup_x0 + 1.0, popup_bottom),
            (popup_x1 - 1.0, popup_top, popup_x1, popup_bottom),
        ] {
            push_quad(&mut verts, [bx0, by0, bx1, by1], bg_uv, [0.0; 4], border_c);
        }

        // Warp-style resize handles on right + top borders.
        self.draw_resize_handles(
            &mut verts,
            popup_x0,
            popup_top,
            popup_x1,
            popup_bottom,
            bg_uv,
        );

        // Each row occupies exactly `ch` pixels. The bottom-most row starts at
        // `popup_bottom - ch` and extends to `popup_bottom` — fully inside the
        // popup. Subsequent rows step upward by `ch`.
        let mut y = popup_bottom - ch;
        for i in (layout.start..layout.end).rev() {
            if y < popup_top {
                break;
            }
            let is_sel = i == selected;
            let lcolor = if is_sel { sel_label_color } else { label_color };
            if is_sel {
                // v1.0 P3: unify with Settings selection_bg (accent*0.35 +
                // bg*0.65) — was [prompt_c, 0.20] which is low-contrast.
                let accent = color_to_normalized(self.theme.accent);
                let selection_bg = [
                    accent[0] * 0.35 + theme_bg[0] * 0.65,
                    accent[1] * 0.35 + theme_bg[1] * 0.65,
                    accent[2] * 0.35 + theme_bg[2] * 0.65,
                    1.0,
                ];
                push_quad(
                    &mut verts,
                    [popup_x0 + 1.0, y, popup_x1 - 1.0, y + ch],
                    bg_uv,
                    [0.0; 4],
                    selection_bg,
                );
            }
            let (icon, icon_color, suffix) = match matches[i].kind {
                weft_core::complete::MatchKind::Path => {
                    if matches[i].is_dir {
                        ("📁", [0.90, 0.72, 0.30, 1.0], "Directory")
                    } else {
                        ("📄", [0.45, 0.65, 0.90, 1.0], "File")
                    }
                }
                weft_core::complete::MatchKind::History => {
                    ("»", [0.60, 0.60, 0.60, 1.0], "History")
                }
                weft_core::complete::MatchKind::Command => {
                    ("»", [0.55, 0.80, 0.55, 1.0], "Command")
                }
            };
            self.push_text(&mut verts, icon_x, y, icon, icon_color, 3);
            self.push_text(
                &mut verts,
                label_x,
                y,
                &matches[i].label,
                lcolor,
                label_cols,
            );
            // v0.8 U4: suffix is globally aligned (not per-row). The column
            // anchor derives from max_label_cols — the widest visible label —
            // so every row's suffix starts at the same X. Was per-row
            // `label_x + (this_row_label_w + gap) * cw`, which made suffixes
            // stagger when labels had different widths. The X coordinate
            // itself comes from `layout.suffix_x` (precomputed in layout.rs).
            self.push_text(&mut verts, suffix_x, y, suffix, suffix_color, suffix_cols);
            y -= ch;
        }

        // Return the popup rect for border drag-resize hot-zone detection.
        let popup_rect = Some(layout.popup_rect);

        (verts, popup_rect)
    }

    /// Build the Command Palette as a centered floating window. Renders a
    /// search box at the top, a scrollable results list, and an optional
    /// variable-fill form when a workflow is selected.
    fn build_palette_vertices(
        &self,
        p: &crate::overlay::PaletteDrawParams<'_>,
    ) -> (Vec<f32>, Option<[f32; 4]>) {
        let mut verts = Vec::new();
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let vp_w = self.viewport.0;
        let vp_h = self.viewport.1;
        if cw <= 0.0 || ch <= 0.0 || vp_w <= 0.0 || vp_h <= 0.0 {
            return (verts, None);
        }

        let theme_bg = color_to_normalized(self.theme.background);
        let fg = color_to_normalized(self.theme.foreground);
        // v1.0 fix: replace accent_dim with label_c (70% fg + 30% bg) —
        // accent_dim is too close to bg in Nord/Warp themes, making prompt
        // marks (❯), chevrons, and dim text invisible. label_c is always
        // readable across all themes.
        let prompt_c = [
            fg[0] * 0.70 + theme_bg[0] * 0.30,
            fg[1] * 0.70 + theme_bg[1] * 0.30,
            fg[2] * 0.70 + theme_bg[2] * 0.30,
            1.0,
        ];
        let dim = prompt_c;
        // Block separator: theme.separator (barely-visible warm dark).
        let separator = color_to_normalized(self.theme.separator);
        let (su, sv, suw, svh) = self.space_uv();
        let bg_uv = [su, sv + svh, su + suw, sv];
        // v1.0 Warp-style: thin low-opacity border.
        let border_c = [0.5, 0.5, 0.5, 0.20];
        let popup_bg = [
            theme_bg[0] + (1.0 - theme_bg[0]) * 0.08,
            theme_bg[1] + (1.0 - theme_bg[1]) * 0.08,
            theme_bg[2] + (1.0 - theme_bg[2]) * 0.08,
            1.0,
        ];

        let pad_x = self.padding_x;
        let left = pad_x;
        let right = vp_w - pad_x;
        let cols = (((right - left) / cw).max(1.0)) as usize;

        // v0.8 stage 4: layout (popup rect, query/sep/results Y, column
        // anchors) is computed by the pure functions in `layout.rs`. The
        // renderer keeps responsibility for vertex building, theming, and
        // text rasterization.
        let ctx = self.layout_ctx.expect("LayoutCtx built at draw() entry");

        // If we're in form mode, render the form instead of the search list.
        if let Some(form) = p.form {
            let rect = crate::layout::layout_palette_form_rect(
                &ctx,
                form.fields.len(),
                self.popup_width_scale,
            );
            let v = self.build_palette_form_vertices(form, rect[0], rect[2], vp_h);
            return (v, Some(rect));
        }

        // Search mode: query box + results list.
        let layout = crate::layout::layout_palette_search(
            &ctx,
            p.entries.len(),
            p.selection,
            self.popup_max_rows,
            self.popup_width_scale,
        );
        let [popup_x0, popup_top, popup_x1, popup_bottom] = layout.popup_rect;
        let query_y = layout.query_y;
        let query_x = layout.query_x;
        let sep_y = layout.sep_y;
        let label_x = layout.label_x;
        let suffix_x = layout.suffix_x;
        let start = layout.start;
        let end = layout.end;

        // v1.0 Warp-style: subtle drop shadow behind the popup.
        let shadow_pad = ch * 0.15;
        push_quad(
            &mut verts,
            [
                popup_x0 - shadow_pad,
                popup_top - shadow_pad,
                popup_x1 + shadow_pad,
                popup_bottom + shadow_pad,
            ],
            bg_uv,
            [0.0; 4],
            [0.0, 0.0, 0.0, 0.15],
        );

        // Background + border.
        push_quad(
            &mut verts,
            [popup_x0, popup_top, popup_x1, popup_bottom],
            bg_uv,
            [0.0; 4],
            popup_bg,
        );
        for (bx0, by0, bx1, by1) in [
            (popup_x0, popup_top, popup_x1, popup_top + 1.0),
            (popup_x0, popup_bottom - 1.0, popup_x1, popup_bottom),
            (popup_x0, popup_top, popup_x0 + 1.0, popup_bottom),
            (popup_x1 - 1.0, popup_top, popup_x1, popup_bottom),
        ] {
            push_quad(&mut verts, [bx0, by0, bx1, by1], bg_uv, [0.0; 4], border_c);
        }

        // Warp-style resize handles on right + top borders.
        self.draw_resize_handles(
            &mut verts,
            popup_x0,
            popup_top,
            popup_x1,
            popup_bottom,
            bg_uv,
        );

        // Banner / query row. When a sub-mode banner is active, show it
        // instead of the normal search prompt.
        if !p.banner.is_empty() {
            // Sub-mode: show banner + input buffer.
            self.push_text(&mut verts, query_x, query_y, p.banner, prompt_c, cols);
            let banner_cols = Self::text_col_width(p.banner);
            let input_x = query_x + (banner_cols + 1) as f32 * cw;
            let avail = (((popup_x1 - input_x) / cw).max(1.0)) as usize;
            self.push_text(&mut verts, input_x, query_y, p.submode_input, fg, avail);
        } else {
            // Normal search mode.
            let query_label = "> ";
            self.push_text(&mut verts, query_x, query_y, query_label, prompt_c, cols);
            let qx = query_x + query_label.chars().count() as f32 * cw;
            let avail = (((popup_x1 - qx) / cw).max(1.0)) as usize;
            self.push_text(&mut verts, qx, query_y, p.query, fg, avail);
            // v0.9 fix: blinking caret at end of query so the user sees the
            // input focus (matches the find bar + panel search box behavior).
            if self.cursor_blink_on {
                let qcols = Self::text_col_width(p.query);
                let cx = qx + qcols as f32 * cw;
                let accent = color_to_normalized(self.theme.accent);
                push_quad(
                    &mut verts,
                    [cx, query_y, cx + cw * 0.15, query_y + ch],
                    bg_uv,
                    [0.0; 4],
                    accent,
                );
            }
        }

        // Separator below query.
        push_quad(
            &mut verts,
            [popup_x0, sep_y, popup_x1, sep_y + 1.0],
            bg_uv,
            [0.0; 4],
            separator,
        );

        // Results rows.
        let mut y = layout.results_y;
        for i in start..end {
            if y + ch > popup_bottom {
                break;
            }
            let is_sel = i == p.selection;
            if is_sel {
                // v1.0 P3: unify with Settings selection_bg (accent*0.35 +
                // bg*0.65) — was [prompt_c, 0.20] which is low-contrast.
                let accent = color_to_normalized(self.theme.accent);
                let selection_bg = [
                    accent[0] * 0.35 + theme_bg[0] * 0.65,
                    accent[1] * 0.35 + theme_bg[1] * 0.65,
                    accent[2] * 0.35 + theme_bg[2] * 0.65,
                    1.0,
                ];
                push_quad(
                    &mut verts,
                    [popup_x0 + 1.0, y, popup_x1 - 1.0, y + ch],
                    bg_uv,
                    [0.0; 4],
                    selection_bg,
                );
            }
            let entry = &p.entries[i];
            let lcolor = if is_sel { fg } else { dim };
            // v1.0 fix: blend fg with theme_bg instead of pure fg*0.40.
            // The old fg*0.40 drops foreground towards black — on dark
            // themes with low fg values (Solarized Dark fg=0x93, One Dark
            // fg=0xab), the result is nearly identical to the popup
            // background, making "Workflow"/"Builtin"/"Theme" labels
            // invisible. Blending guarantees the color sits halfway between
            // fg and bg, ensuring readable contrast in ALL themes.
            let suffix_color = [
                fg[0] * 0.50 + theme_bg[0] * 0.50,
                fg[1] * 0.50 + theme_bg[1] * 0.50,
                fg[2] * 0.50 + theme_bg[2] * 0.50,
                1.0,
            ];

            // Label + description.
            let label_avail = (((popup_x1 - label_x) / cw) as usize)
                .saturating_sub(12)
                .max(1);
            self.push_text(&mut verts, label_x, y, entry.label, lcolor, label_avail);

            // Kind suffix (right-aligned area).
            self.push_text(&mut verts, suffix_x, y, entry.kind_label, suffix_color, 10);
            y += ch;
        }

        // Return the popup rect for border drag-resize hot-zone detection.
        (verts, Some(layout.popup_rect))
    }

    /// Render the palette's variable-fill form (sub-mode when a workflow is selected).
    fn build_palette_form_vertices(
        &self,
        form: &crate::overlay::PaletteFormView<'_>,
        popup_x0: f32,
        popup_x1: f32,
        vp_h: f32,
    ) -> Vec<f32> {
        let mut verts = Vec::new();
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let theme_bg = color_to_normalized(self.theme.background);
        let fg = color_to_normalized(self.theme.foreground);
        // v1.0 fix: replace accent_dim with label_c — same fix as Settings
        // and Palette search mode. accent_dim is invisible in Nord/Warp.
        let prompt_c = [
            fg[0] * 0.70 + theme_bg[0] * 0.30,
            fg[1] * 0.70 + theme_bg[1] * 0.30,
            fg[2] * 0.70 + theme_bg[2] * 0.30,
            1.0,
        ];
        let dim = prompt_c;
        let (su, sv, suw, svh) = self.space_uv();
        let bg_uv = [su, sv + svh, su + suw, sv];
        // v1.0 P2: unify with Settings baseline — border α 0.35→0.20,
        // popup_bg 5%→8% lighten.
        let border_c = [0.5, 0.5, 0.5, 0.20];
        let popup_bg = [
            theme_bg[0] + (1.0 - theme_bg[0]) * 0.08,
            theme_bg[1] + (1.0 - theme_bg[1]) * 0.08,
            theme_bg[2] + (1.0 - theme_bg[2]) * 0.08,
            1.0,
        ];

        let n_fields = form.fields.len();
        let popup_h = (n_fields as f32 + 3.0) * ch + ch * 0.5;
        let popup_top = vp_h * 0.15;
        let popup_bottom = popup_top + popup_h;

        // v1.0 P2: add Warp-style shadow (was missing — form mode had no
        // shadow while search mode did, causing visual discontinuity).
        let shadow_pad = ch * 0.15;
        push_quad(
            &mut verts,
            [
                popup_x0 - shadow_pad,
                popup_top - shadow_pad,
                popup_x1 + shadow_pad,
                popup_bottom + shadow_pad,
            ],
            bg_uv,
            [0.0; 4],
            [0.0, 0.0, 0.0, 0.15],
        );

        // Background + border.
        push_quad(
            &mut verts,
            [popup_x0, popup_top, popup_x1, popup_bottom],
            bg_uv,
            [0.0; 4],
            popup_bg,
        );
        for (bx0, by0, bx1, by1) in [
            (popup_x0, popup_top, popup_x1, popup_top + 1.0),
            (popup_x0, popup_bottom - 1.0, popup_x1, popup_bottom),
            (popup_x0, popup_top, popup_x0 + 1.0, popup_bottom),
            (popup_x1 - 1.0, popup_top, popup_x1, popup_bottom),
        ] {
            push_quad(&mut verts, [bx0, by0, bx1, by1], bg_uv, [0.0; 4], border_c);
        }

        // Title row.
        let title_y = popup_top + ch * 0.5;
        let title = format!("{} — 填写参数", form.workflow_name);
        self.push_text(
            &mut verts,
            popup_x0 + cw * 0.5,
            title_y,
            &title,
            prompt_c,
            40,
        );

        // Separator.
        let sep_y = title_y + ch;
        push_quad(
            &mut verts,
            [popup_x0, sep_y, popup_x1, sep_y + 1.0],
            bg_uv,
            [0.0; 4],
            [0.65, 0.65, 0.65, 0.22],
        );

        // Fields.
        let mut y = sep_y + ch;
        for (name, value, is_current) in form.fields.iter() {
            let label_text = format!("{name}: ");
            let color = if *is_current { fg } else { dim };
            self.push_text(&mut verts, popup_x0 + cw * 0.5, y, &label_text, color, 20);

            // Value bracket area.
            let val_x = popup_x0 + cw * 0.5 + 12.0 * cw;
            if *is_current {
                // v1.0 P3: unify with Settings selection_bg (accent*0.35 +
                // bg*0.65) — was [accent_dim, 0.15] which is invisible in
                // Nord/Warp themes.
                let accent = color_to_normalized(self.theme.accent);
                let selection_bg = [
                    accent[0] * 0.35 + theme_bg[0] * 0.65,
                    accent[1] * 0.35 + theme_bg[1] * 0.65,
                    accent[2] * 0.35 + theme_bg[2] * 0.65,
                    1.0,
                ];
                push_quad(
                    &mut verts,
                    [val_x, y, popup_x1 - cw * 0.5, y + ch],
                    bg_uv,
                    [0.0; 4],
                    selection_bg,
                );
            }
            let val_avail = (((popup_x1 - cw * 0.5 - val_x) / cw).max(1.0)) as usize;
            self.push_text(&mut verts, val_x, y, value, color, val_avail);

            y += ch;
        }

        // Footer hint.
        let hint_y = popup_bottom - ch * 0.8;
        let hint = "Enter 执行  Tab 下一项  Esc 返回";
        self.push_text(&mut verts, popup_x0 + cw * 0.5, hint_y, hint, dim, 40);

        verts
    }

    /// v1.0 S1: Build the Settings panel (Cmd+,) as a centered modal
    /// overlay. Renders a title bar, a 4-tab tab bar (Appearance / Font /
    /// Keybindings / Window), the active tab's content, and a footer hint.
    ///
    /// Layout:
    ///   ┌────────────────────────────────────┐
    ///   │ Settings                           │  ← title (2 rows)
    ///   │ Appearance  Font  Keybindings  Win │  ← tab bar (1 row)
    ///   ├────────────────────────────────────┤
    ///   │  › Weft Warm (default)             │  ← content (scrollable)
    ///   │    Weft Light                      │
    ///   │    ...                             │
    ///   ├────────────────────────────────────┤
    ///   │ ↑↓ navigate  Enter apply  Tab …   │  ← footer (1 row)
    ///   └────────────────────────────────────┘
    fn build_settings_vertices(
        &self,
        s: crate::overlay::SettingsDrawParams<'_>,
    ) -> (Vec<f32>, Option<[f32; 4]>, Vec<SettingsHit>) {
        use crate::overlay::SettingsTab;

        let mut verts = Vec::new();
        let mut hits: Vec<SettingsHit> = Vec::new();
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let vp_w = self.viewport.0;
        let vp_h = self.viewport.1;
        if cw <= 0.0 || ch <= 0.0 || vp_w <= 0.0 || vp_h <= 0.0 {
            return (verts, None, hits);
        }

        let theme_bg = color_to_normalized(self.theme.background);
        let fg = color_to_normalized(self.theme.foreground);
        let accent = color_to_normalized(self.theme.accent);
        let separator = color_to_normalized(self.theme.separator);
        // v1.0 fix: compute a "label" color that's always readable across all
        // themes. The old `dim` (= accent_dim) is too close to the background
        // in some themes (e.g. Nord: accent_dim #4c566a vs bg #2e3440). By
        // blending fg with bg (70% fg + 30% bg) we get a muted but always-
        // readable secondary text color that adapts to each theme.
        let label_c = [
            fg[0] * 0.70 + theme_bg[0] * 0.30,
            fg[1] * 0.70 + theme_bg[1] * 0.30,
            fg[2] * 0.70 + theme_bg[2] * 0.30,
            1.0,
        ];
        let (su, sv, suw, svh) = self.space_uv();
        let bg_uv = [su, sv + svh, su + suw, sv];
        // Warp-inspired: very thin 1px border at low opacity, subtle shadow.
        let border_c = [0.5, 0.5, 0.5, 0.20];
        // Popup background: 8% lighter — enough to distinguish from the
        // terminal canvas without being jarring.
        let popup_bg = [
            theme_bg[0] + (1.0 - theme_bg[0]) * 0.08,
            theme_bg[1] + (1.0 - theme_bg[1]) * 0.08,
            theme_bg[2] + (1.0 - theme_bg[2]) * 0.08,
            1.0,
        ];
        let selection_bg = [
            accent[0] * 0.35 + theme_bg[0] * 0.65,
            accent[1] * 0.35 + theme_bg[1] * 0.65,
            accent[2] * 0.35 + theme_bg[2] * 0.65,
            1.0,
        ];

        // Centered modal box: 72% viewport width, 78% viewport height.
        let box_w = vp_w * 0.72;
        let box_h = vp_h * 0.78;
        let box_x0 = (vp_w - box_w) / 2.0;
        let box_x1 = box_x0 + box_w;
        let box_y0 = (vp_h - box_h) / 2.0;
        let box_y1 = box_y0 + box_h;

        let pad_x = cw * 1.5;
        let content_x0 = box_x0 + pad_x;
        let content_x1 = box_x1 - pad_x;
        let content_cols = (((content_x1 - content_x0) / cw).max(1.0)) as usize;

        // Warp-style: very subtle shadow (low opacity, tight offset).
        let shadow_pad = ch * 0.15;
        push_quad(
            &mut verts,
            [
                box_x0 - shadow_pad,
                box_y0 - shadow_pad,
                box_x1 + shadow_pad,
                box_y1 + shadow_pad,
            ],
            bg_uv,
            [0.0; 4],
            [0.0, 0.0, 0.0, 0.15],
        );

        // Background (no top accent border — Warp keeps it minimal).
        push_quad(
            &mut verts,
            [box_x0, box_y0, box_x1, box_y1],
            bg_uv,
            [0.0; 4],
            popup_bg,
        );
        // Single 1px border on all four sides.
        for (bx0, by0, bx1, by1) in [
            (box_x0, box_y0, box_x1, box_y0 + 1.0),
            (box_x0, box_y1 - 1.0, box_x1, box_y1),
            (box_x0, box_y0, box_x0 + 1.0, box_y1),
            (box_x1 - 1.0, box_y0, box_x1, box_y1),
        ] {
            push_quad(&mut verts, [bx0, by0, bx1, by1], bg_uv, [0.0; 4], border_c);
        }

        // Title row — subtle (label_c, not accent) to keep visual hierarchy calm.
        let mut y = box_y0 + ch * 1.0;
        self.push_text(&mut verts, content_x0, y, "Settings", label_c, content_cols);
        y += ch * 1.8;

        // Tab bar: tabs side by side with 1px underline for active.
        let tab_w = box_w / SettingsTab::ALL.len() as f32;
        for (i, tab) in SettingsTab::ALL.iter().enumerate() {
            let tx0 = box_x0 + i as f32 * tab_w;
            let label = tab.label();
            let is_active = *tab == s.active_tab;
            let color = if is_active { fg } else { label_c };
            // Active tab: 1px underline spanning the full slot width.
            if is_active {
                push_quad(
                    &mut verts,
                    [
                        tx0 + cw * 0.5,
                        y + ch * 0.9,
                        tx0 + tab_w - cw * 0.5,
                        y + ch * 0.9 + 1.0,
                    ],
                    bg_uv,
                    [0.0; 4],
                    accent,
                );
            }
            // Center the label within its slot so short labels (Font/Logo)
            // don't clump against the left edge of unequal-width tabs.
            // v1.2-fix: truncate to slot width (tab_w / cw cols), not the
            // full content area width. Previously passed content_cols which
            // let long labels overflow into adjacent slots when the panel
            // was small.
            let slot_cols = ((tab_w / cw) as usize).max(1);
            let label_chars = label.chars().count() as f32;
            let text_x = tx0 + ((tab_w - label_chars * cw) / 2.0).max(cw * 0.5);
            self.push_text(&mut verts, text_x, y, label, color, slot_cols);
            // v1.0 S1-b: register a hit region for the whole tab cell so
            // clicks anywhere in the tab switch tabs (matching the
            // underline's visual span).
            hits.push(SettingsHit {
                kind: SettingsHitKind::Tab(*tab),
                rect: [tx0, y, tx0 + tab_w, y + ch],
            });
        }
        y += ch * 1.5;

        // Separator line between tab bar and content.
        push_quad(
            &mut verts,
            [content_x0, y + ch * 0.3, content_x1, y + ch * 0.3 + 1.0],
            bg_uv,
            [0.0; 4],
            separator,
        );

        // Content area: render the active tab.
        // v1.0 fix: move footer up from ch*0.8 to ch*1.5 so it sits between
        // the separator line and the bottom border with balanced spacing.
        let footer_y = box_y1 - ch * 1.5;
        let content_bottom = footer_y - ch * 0.5;
        let content_base = y + ch * 0.5;

        // v1.0 S2: error bar at the top of the content area when a save
        // failed. Renders a red background strip with the error message,
        // pushing the rest of the content down by one row so nothing
        // overlaps. Cleared by save_settings_draft on the next successful
        // save (or by closing the panel).
        let content_top = if let Some(err) = s.error {
            let err_bg = [0.65, 0.18, 0.18, 1.0];
            push_quad(
                &mut verts,
                [content_x0, content_base, content_x1, content_base + ch],
                bg_uv,
                [0.0; 4],
                err_bg,
            );
            // ⚠ prefix in white, then the message (truncated to fit).
            let msg = format!("\u{26a0} {}", err);
            self.push_text(
                &mut verts,
                content_x0 + cw * 0.3,
                content_base,
                &msg,
                [1.0, 1.0, 1.0, 1.0],
                content_cols,
            );
            content_base + ch
        } else {
            content_base
        };
        // Recompute max_rows after the error bar so content doesn't overflow.
        let content_h = (content_bottom - content_top).max(0.0);
        let max_rows = (content_h / ch).max(1.0) as usize;

        match s.active_tab {
            SettingsTab::Appearance => {
                // Theme list — Warp-style: checkmark for current theme,
                // subtle selection highlight for the cursor row.
                for (i, theme) in s.themes.iter().take(max_rows).enumerate() {
                    let row_y = content_top + i as f32 * ch;
                    let is_current = theme.name == s.theme_name;
                    let is_selected = i == s.selection;
                    // Selection highlight.
                    if is_selected {
                        push_quad(
                            &mut verts,
                            [content_x0, row_y, content_x1, row_y + ch],
                            bg_uv,
                            [0.0; 4],
                            selection_bg,
                        );
                    }
                    // Current theme: accent checkmark; others: blank space.
                    let prefix = if is_current { "● " } else { "  " };
                    let label_color = if is_current { accent } else { fg };
                    let label = format!("{}{}", prefix, theme.label);
                    self.push_text(
                        &mut verts,
                        content_x0,
                        row_y,
                        &label,
                        label_color,
                        content_cols,
                    );
                    // v1.0 S1-b: hit-test row so a click selects + applies
                    // the theme (matching Enter's behavior on that row).
                    hits.push(SettingsHit {
                        kind: SettingsHitKind::Theme(i),
                        rect: [content_x0, row_y, content_x1, row_y + ch],
                    });
                }
            }
            SettingsTab::Font => {
                let rows = [
                    ("Family:", s.font_family),
                    ("Size:", &format!("{:.1} pt", s.font_size)),
                    ("Line height:", &format!("{:.2}", s.line_height)),
                ];
                for (i, (label, value)) in rows.iter().enumerate() {
                    let row_y = content_top + i as f32 * ch;
                    self.push_text(&mut verts, content_x0, row_y, label, label_c, content_cols);
                    let value_x = content_x0 + cw * 12.0;
                    self.push_text(&mut verts, value_x, row_y, value, fg, content_cols);
                }
            }
            SettingsTab::Keybindings => {
                // v1.0 fix: scrollable list — render `[offset .. offset+max_rows]`
                // and auto-clamp offset so the selected row is always visible.
                let total = s.keybindings.len();
                // Reserve one row for the scroll indicator when the list is
                // scrollable (more rows than fit), so the indicator doesn't
                // overlap the last visible keybinding row.
                let scrollable = total > max_rows;
                let usable_rows = if scrollable {
                    max_rows.saturating_sub(1)
                } else {
                    max_rows
                };
                let visible = usable_rows.min(total);
                // Derive offset from selection: keep selection in view.
                let mut offset = s.scroll_offset.min(total);
                if s.selection < offset {
                    offset = s.selection;
                } else if s.selection >= offset + visible {
                    offset = s.selection + 1 - visible;
                }
                let end = (offset + visible).min(total);
                for (i, kb) in s.keybindings[offset..end].iter().enumerate() {
                    let row_y = content_top + i as f32 * ch;
                    let is_selected = offset + i == s.selection;
                    if is_selected {
                        push_quad(
                            &mut verts,
                            [content_x0, row_y, content_x1, row_y + ch],
                            bg_uv,
                            [0.0; 4],
                            selection_bg,
                        );
                    }
                    self.push_text(
                        &mut verts,
                        content_x0,
                        row_y,
                        &kb.action,
                        label_c,
                        content_cols,
                    );
                    let binding_x = content_x1 - cw * 15.0;
                    self.push_text(
                        &mut verts,
                        binding_x,
                        row_y,
                        &kb.binding,
                        accent,
                        content_cols,
                    );
                }
                // v1.0 fix: scroll indicator on its own row below the list
                // (not overlapping the last keybinding row). Shows "↑ more"
                // / "↓ more" / both when scrollable in either direction.
                if scrollable {
                    let indicator_y = content_top + visible as f32 * ch;
                    let mut indicator = String::new();
                    if offset > 0 {
                        indicator.push('↑');
                    }
                    if end < total {
                        if !indicator.is_empty() {
                            indicator.push(' ');
                        }
                        indicator.push('↓');
                    }
                    if !indicator.is_empty() {
                        indicator.push_str(" more");
                        self.push_text(
                            &mut verts,
                            content_x0,
                            indicator_y,
                            &indicator,
                            label_c,
                            content_cols,
                        );
                    }
                }
            }
            SettingsTab::Window => {
                let rows = [
                    ("Opacity:", format!("{:.2}", s.window_opacity)),
                    ("Padding X:", format!("{} cells", s.window_padding_x)),
                    ("Padding Y:", format!("{} cells", s.window_padding_y)),
                    ("Scrollback:", format!("{} lines", s.scrollback_lines)),
                ];
                for (i, (label, value)) in rows.iter().enumerate() {
                    let row_y = content_top + i as f32 * ch;
                    self.push_text(&mut verts, content_x0, row_y, label, label_c, content_cols);
                    let value_x = content_x0 + cw * 12.0;
                    self.push_text(&mut verts, value_x, row_y, value, fg, content_cols);
                }
            }
            // v1.0 Logo: single row — Variant label + current value.
            // ←/→ cycles through LogoVariant::ALL (Cool/Warm/Light/Transparent).
            SettingsTab::Logo => {
                let row_y = content_top;
                self.push_text(
                    &mut verts,
                    content_x0,
                    row_y,
                    "Variant:",
                    label_c,
                    content_cols,
                );
                let value_x = content_x0 + cw * 12.0;
                self.push_text(
                    &mut verts,
                    value_x,
                    row_y,
                    s.logo_variant.label(),
                    fg,
                    content_cols,
                );
            }
        }

        // v1.0 fix: Footer hint with two-color design — accent for shortcut
        // keys, label_c for descriptions. Each pair is rendered separately so
        // we can use different colors and control spacing precisely.
        push_quad(
            &mut verts,
            [
                content_x0,
                footer_y - ch * 0.4,
                content_x1,
                footer_y - ch * 0.4 + 1.0,
            ],
            bg_uv,
            [0.0; 4],
            separator,
        );
        // Compact pairs: "key description" with key in accent, desc in label_c.
        // v1.2 fix: left-to-right layout with right-edge truncation. Pairs
        // start from content_x0 (left edge, matching body text reading
        // direction) and stop when they would exceed content_x1 (right
        // edge). This keeps the leftmost pairs (↑↓ navigate, ⏎ apply) always
        // visible — they are the most-used operations. Pairs that don't fit
        // are simply not rendered (CPU-side cull, no half-glyph cropping).
        // v1.0 S1-b/S2: the apply / close / save pairs register clickable
        // hit regions so mouse users can hit those actions directly.
        let pairs: [(&str, &str, Option<SettingsHitKind>); 6] = [
            ("↑↓", "navigate", None),
            ("⏎", "apply", Some(SettingsHitKind::ApplyButton)),
            ("⇥", "switch", None),
            ("←→", "adjust", None),
            ("esc", "close", Some(SettingsHitKind::CloseButton)),
            ("⌘⏎", "save", Some(SettingsHitKind::SaveButton)),
        ];
        let gap = cw * 1.5; // gap between pairs
        let inner = cw * 0.3; // gap between key and description within a pair
        let scale = 1.0; // match body text scale
        let footer_h = ch * scale;
        let mut fx = content_x0;
        for (key, desc, hit_kind) in &pairs {
            let key_w = cw * scale * Self::text_col_width(key) as f32;
            let desc_w = cw * scale * Self::text_col_width(desc) as f32;
            let pair_w = key_w + inner + desc_w;
            // Stop if this pair would exceed the right content boundary.
            if fx + pair_w > content_x1 {
                break;
            }
            let pair_x0 = fx;
            self.push_text_scaled(&mut verts, fx, footer_y, key, accent, content_cols, scale);
            self.push_text_scaled(
                &mut verts,
                fx + key_w + inner,
                footer_y,
                desc,
                label_c,
                content_cols,
                scale,
            );
            // Register the clickable rect for apply / close / save.
            if let Some(kind) = hit_kind {
                hits.push(SettingsHit {
                    kind: *kind,
                    rect: [pair_x0, footer_y, pair_x0 + pair_w, footer_y + footer_h],
                });
            }
            fx += pair_w + gap;
        }

        (verts, Some([box_x0, box_y0, box_x1, box_y1]), hits)
    }

    /// Build the right-click context menu (F7) as a small popup at (x, y).
    fn build_context_menu_vertices(&self, x: f32, y: f32) -> Vec<f32> {
        let mut verts = Vec::new();
        let cw = self.cell_width() as f32;
        let theme_bg = color_to_normalized(self.theme.background);
        let fg = color_to_normalized(self.theme.foreground);
        // v1.0 fix: replace accent_dim with label_c (70% fg + 30% bg) —
        // accent_dim is invisible in Nord/Warp themes.
        let prompt_c = [
            fg[0] * 0.70 + theme_bg[0] * 0.30,
            fg[1] * 0.70 + theme_bg[1] * 0.30,
            fg[2] * 0.70 + theme_bg[2] * 0.30,
            1.0,
        ];
        let separator = color_to_normalized(self.theme.separator);
        let (su, sv, suw, svh) = self.space_uv();
        let bg_uv = [su, sv + svh, su + suw, sv];

        let items = [
            "Copy Command",
            "Copy Output",
            "Toggle Fold",
            "Send to Input",
        ];

        // v0.8 stage 4: layout (menu rect, per-item Y, separators, text X)
        // is computed by the pure function in `layout.rs`. The renderer keeps
        // responsibility for vertex building, theming, and text rasterization.
        let ctx = self.layout_ctx.expect("LayoutCtx built at draw() entry");
        let layout = crate::layout::layout_context_menu(&ctx, x, y, self.scale as f32);
        let [menu_x0, menu_y0, menu_x1, menu_y1] = layout.menu_rect;
        let menu_w = menu_x1 - menu_x0;
        let text_x = layout.text_x;

        let popup_bg = [
            theme_bg[0] + (1.0 - theme_bg[0]) * 0.08,
            theme_bg[1] + (1.0 - theme_bg[1]) * 0.08,
            theme_bg[2] + (1.0 - theme_bg[2]) * 0.08,
            1.0,
        ];
        // v1.0 Warp-style: thin low-opacity border.
        let border_c = [0.5, 0.5, 0.5, 0.20];

        // Background.
        push_quad(&mut verts, layout.menu_rect, bg_uv, [0.0; 4], popup_bg);
        // Border.
        for (bx0, by0, bx1, by1) in [
            (menu_x0, menu_y0, menu_x1, menu_y0 + 1.0),
            (menu_x0, menu_y1 - 1.0, menu_x1, menu_y1),
            (menu_x0, menu_y0, menu_x0 + 1.0, menu_y1),
            (menu_x1 - 1.0, menu_y0, menu_x1, menu_y1),
        ] {
            push_quad(&mut verts, [bx0, by0, bx1, by1], bg_uv, [0.0; 4], border_c);
        }

        // Items.
        for (i, label) in items.iter().enumerate() {
            let item_y = layout.item_y[i];
            let color = if i >= items.len() - 2 {
                prompt_c // "Toggle Fold" + "Send to Input" in accent
            } else {
                fg
            };
            self.push_text(
                &mut verts,
                text_x,
                item_y,
                label,
                color,
                (menu_w / cw * 0.9) as usize,
            );
            // Separator between items (except last).
            if i + 1 < items.len() {
                let sep_y = layout.separator_ys[i];
                push_quad(
                    &mut verts,
                    [menu_x0 + 2.0, sep_y, menu_x1 - 2.0, sep_y + 1.0],
                    bg_uv,
                    [0.0; 4],
                    separator,
                );
            }
        }

        verts
    }

    /// Warp-style block history. `region_bottom_y` is the bottom edge of the
    /// block region (top of the input box in editor mode, or the screen bottom
    /// in CommandExecuting). The opaque bg covers `[0, region_bottom_y]`.
    /// `cwd`, when `Some` and no `live` block, draws the persistent cwd line
    /// (editor mode) with a divider ABOVE it (cwd grouped with the input box).
    /// `live`, when `Some` (CommandExecuting), draws the in-flight command's
    /// streaming output at the bottom — so a long-running command (interactive
    /// `sudo su`) keeps the full history above it instead of reverting to raw.
    #[allow(clippy::too_many_arguments)]
    fn build_block_view_vertices(
        &self,
        blocks: &[Block],
        region_bottom_y: f32,
        cwd: Option<&str>,
        git_branch: Option<&str>,
        live: Option<weft_core::blocks::InFlightBlock<'_>>,
        block_scroll: usize,
        selection: &mut weft_core::selection::SelectionHandler,
    ) -> (
        Vec<f32>,
        Vec<crate::overlay::HitRegion>,
        Vec<weft_core::selection::BlockViewRow>,
    ) {
        let mut verts = Vec::new();
        let mut hit_regions: Vec<crate::overlay::HitRegion> = Vec::new();
        let mut bv_rows: Vec<weft_core::selection::BlockViewRow> = Vec::new();
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let vp_w = self.viewport.0;
        let vp_h = self.viewport.1;
        if cw <= 0.0 || ch <= 0.0 || vp_w <= 0.0 || vp_h <= 0.0 {
            return (verts, hit_regions, bv_rows);
        }

        let theme_bg = color_to_normalized(self.theme.background);
        let fg = color_to_normalized(self.theme.foreground);
        // v1.0 fix: replace accent_dim with label_c (70% fg + 30% bg) —
        // accent_dim is too close to bg in Nord/Warp themes, making prompt
        // marks (❯), chevrons, and dim text invisible. label_c is always
        // readable across all themes.
        let prompt_c = [
            fg[0] * 0.70 + theme_bg[0] * 0.30,
            fg[1] * 0.70 + theme_bg[1] * 0.30,
            fg[2] * 0.70 + theme_bg[2] * 0.30,
            1.0,
        ];
        let dim = prompt_c;
        // Block separator: theme.separator (barely-visible warm dark).
        let separator = color_to_normalized(self.theme.separator);
        let (su, sv, suw, svh) = self.space_uv();
        let bg_uv = [su, sv + svh, su + suw, sv];

        // v0.8 stage 4: layout (pitch, left/right/cols, clip region, fixed
        // CWD line position) is computed by the pure function in `layout.rs`.
        // The renderer keeps responsibility for vertex building, theming,
        // and text rasterization. Per-row Y is data-driven (each row's
        // distance accumulates from the cumulative output line count of all
        // blocks below it), so it stays in the renderer's render loop.
        let ctx = self.layout_ctx.expect("LayoutCtx built at draw() entry");
        let cwd_header_active = cwd.is_some() && live.is_none();
        let layout = crate::layout::layout_block_view(&ctx, region_bottom_y, cwd_header_active);
        let pitch = layout.pitch;
        let left = layout.left;
        let right = layout.right;
        let cols = layout.cols;
        let content_bottom_y = layout.clip_bottom;

        // Background fill for the block region.
        push_quad(
            &mut verts,
            [0.0, 0.0, vp_w, region_bottom_y.max(0.0)],
            bg_uv,
            [0.0; 4],
            theme_bg,
        );

        // ── Fixed bottom area: CWD header (Editor mode) ──────────────────
        //
        // In Editor mode the CWD line + divider is pinned to the bottom of
        // the block region (directly above the input box). It NEVER scrolls —
        // it's grouped with the input box, not with the scrollable history.
        //
        // The scrollable content area starts ABOVE this fixed CWD line.
        if let Some(cwd) = cwd {
            if live.is_none() {
                let fixed_y = layout.fixed_cwd_y;
                // Divider line.
                push_quad(
                    &mut verts,
                    [left, fixed_y, right, fixed_y + 1.5],
                    bg_uv,
                    [0.0; 4],
                    separator,
                );
                // CWD text (+ optional git branch).
                let display = abbreviate_path(cwd);
                let display = if let Some(b) = git_branch {
                    format!("{display} git:({b})")
                } else {
                    display
                };
                if !display.is_empty() {
                    self.push_text(&mut verts, left, fixed_y, &display, dim, cols);
                }
            }
        }

        // ── Phase 1: Pre-layout scrollable content rows ──────────────────
        //
        // Flatten every history row into a list with its y-distance from
        // `content_bottom_y`. Scrolling applies a pixel offset so the entire
        // content slides as a unit (matching Warp). Only rows within the clip
        // region are rendered — no blank space.

        enum LaidRow<'a> {
            Output {
                text: &'a str,
                /// v1.0 P0-a: pre-wrapped chunks (from cache for historical
                /// blocks, freshly computed for live blocks). Avoids calling
                /// `wrap_line_chunks` per row in the pre-pass + render loop.
                chunks: Rc<[String]>,
                block_id: Option<BlockId>,
                line: usize,
            },
            Command {
                command: &'a str,
                collapsed: bool,
                foldable: bool,
                block_id: BlockId,
            },
            Header {
                text: String,
            },
            Separator,
            LiveCommand {
                command: &'a str,
            },
            /// v1.0: Warp-style clear spacer — viewport-height blank gap
            /// inserted before a `clear` block so the cleared prompt appears
            /// at the top of a fresh "page" while history remains scrollable.
            Blank,
        }

        let mut rows: Vec<f32> = Vec::new();
        let mut row_data: Vec<LaidRow> = Vec::new();
        let mut cursor_dist = 0.0;

        // Bottom of scrollable content: live block (CommandExecuting) or
        // nothing (Editor mode — CWD is already handled above).
        if let Some(live) = live {
            // Live block: line numbers start from 0 in the live output.
            // Collect to Vec first because std::str::Lines doesn't impl
            // DoubleEndedIterator (can't .rev() directly).
            //
            // v0.9 fix: cap the number of lines we layout per frame to avoid
            // O(n) slowdown when a command produces huge output (e.g. 20K
            // lines from a for-loop). Only the tail is visible anyway — older
            // lines have scrolled off the top of the clip region.
            const MAX_LAYOUT_LINES_LIVE: usize = 2000;
            let all_lines: Vec<&str> = live.output.lines().collect();
            let skip = all_lines.len().saturating_sub(MAX_LAYOUT_LINES_LIVE);
            let live_lines: Vec<&str> = all_lines[skip..].to_vec();
            let base_idx = skip;
            for (i, line) in live_lines.iter().enumerate().rev() {
                let line_idx = base_idx + i;
                let chunks: Rc<[String]> =
                    Rc::from(wrap_line_chunks(line, cols).collect::<Vec<_>>());
                let vis_rows = chunks.len();
                cursor_dist += vis_rows as f32 * pitch;
                rows.push(cursor_dist);
                row_data.push(LaidRow::Output {
                    text: line,
                    chunks,
                    block_id: None,
                    line: line_idx,
                });
            }
            cursor_dist += pitch;
            rows.push(cursor_dist);
            row_data.push(LaidRow::LiveCommand {
                command: live.command,
            });
            cursor_dist += pitch;
            rows.push(cursor_dist);
            row_data.push(LaidRow::Separator);
        }

        // v1.0 P0-a: Ensure all historical blocks have a cached layout for
        // the current `cols`. This is the only place the cache is mutated
        // per frame — each block is recomputed only if its content/collapse
        // state changed or `cols` changed (resize). Eliminates the
        // O(total_output_chars) per-frame wrapping cost.
        //
        // Uses `borrow_mut()` (scoped) because `draw()` holds an immutable
        // borrow of `self.layer` — `RefCell` provides interior mutability.
        {
            let mut cache = self.block_layout_cache.borrow_mut();
            for b in blocks.iter() {
                cache.ensure_cached(b, cols);
            }
        }

        // Read cached layouts (immutable borrow, scoped to this loop).
        // The `Rc<[String]>` chunks are cloned (refcount bump) into
        // `row_data`, so the `Ref` can be dropped before the render loop.
        {
            let cache = self.block_layout_cache.borrow();
            for b in blocks.iter().rev() {
                let cached = cache.get(b.id.0);
                if !b.collapsed {
                    for line in cached.lines.iter().rev() {
                        let text = &b.output[line.byte_start..line.byte_end];
                        let vis_rows = line.chunks.len();
                        cursor_dist += vis_rows as f32 * pitch;
                        rows.push(cursor_dist);
                        row_data.push(LaidRow::Output {
                            text,
                            chunks: Rc::clone(&line.chunks),
                            block_id: Some(b.id),
                            line: line.idx,
                        });
                    }
                }
                cursor_dist += pitch;
                rows.push(cursor_dist);
                row_data.push(LaidRow::Command {
                    command: &b.command,
                    collapsed: b.collapsed,
                    foldable: cached.foldable,
                    block_id: b.id,
                });
                let dur = block_duration_str(b);
                let bcwd = b
                    .cwd
                    .as_deref()
                    .map(abbreviate_path)
                    .unwrap_or_else(|| "~".to_string());
                let header = if dur.is_empty() {
                    bcwd
                } else {
                    format!("{bcwd} ({dur})")
                };
                cursor_dist += pitch;
                rows.push(cursor_dist);
                row_data.push(LaidRow::Header { text: header });
                cursor_dist += pitch;
                rows.push(cursor_dist);
                row_data.push(LaidRow::Separator);
                // v1.0: Warp-style clear — insert a viewport-height blank
                // gap above this block so it starts a fresh "page". History
                // above remains reachable by scrolling up. We detect `clear`
                // by the first whitespace token (matches `clear`, `clear;`,
                // `clear && foo`, but not `clearance` / `echo clear`).
                if b.command.split_whitespace().next() == Some("clear") {
                    cursor_dist += vp_h;
                    rows.push(cursor_dist);
                    row_data.push(LaidRow::Blank);
                }
            }
        }

        // ── Phase 2: Render scrollable content with offset ───────────────

        let scroll_px = (block_scroll as f32) * pitch;
        let clip_top = layout.clip_top;
        let clip_bottom = content_bottom_y;

        // Track the block whose content is at the top of the viewport (for
        // the sticky header). We record the topmost visible block's command
        // and cwd as we render.
        let mut topmost_block_info: Option<(String, String)> = None;

        // Selection highlight: for a given row mid-y, look up the char range
        // (in that row's text) that falls inside the active block-view
        // selection. Returns None if the y is outside the selection. The
        // snapshot rows share y-bands with this frame's layout (cloned at
        // drag start); we key on y-band rather than a global index so wrapped
        // multi-chunk Output rows highlight correctly per chunk.
        let selection_bg = {
            let accent = color_to_normalized(self.theme.accent);
            let bg = color_to_normalized(self.theme.background);
            let mut c = [
                accent[0] * 0.35 + bg[0] * 0.65,
                accent[1] * 0.35 + bg[1] * 0.65,
                accent[2] * 0.35 + bg[2] * 0.65,
                1.0,
            ];
            c[3] = 0.60;
            c
        };
        // Pre-pass: build bv_rows (y-bands + text) WITHOUT rendering, so we
        // can sync the selection's row snapshot to the current frame before
        // drawing highlights. This fixes the "selection stays at fixed screen
        // position on scroll" bug — the snapshot's y-bands are refreshed to
        // the current frame's layout, so the highlight tracks the content.
        // NOTE: we do NOT clip here (unlike the render loop below). Including
        // off-screen rows means sync_rows can always find a match by
        // (block_id, text, kind) even when the selection spans content that
        // has scrolled out of the viewport — without this, the proportional
        // fallback would mis-map row_index and the highlight would jump to
        // the wrong row.
        for (i, &dist) in rows.iter().enumerate() {
            let row_top_y = content_bottom_y - dist + scroll_px;
            let row_bottom_y = row_top_y + pitch;
            let _ = row_bottom_y; // unused (no clip in pre-pass)
            let y = row_top_y;
            match &row_data[i] {
                LaidRow::Output {
                    text,
                    chunks,
                    block_id,
                    line: _,
                } => {
                    if chunks.len() <= 1 {
                        bv_rows.push(weft_core::selection::BlockViewRow {
                            kind: weft_core::selection::BlockViewRowKind::Output,
                            text: text.to_string(),
                            block_id: *block_id,
                            y_top: y,
                            y_bottom: y + pitch,
                        });
                    } else {
                        for (ci, chunk) in chunks.iter().enumerate() {
                            let cy = y + ci as f32 * pitch;
                            bv_rows.push(weft_core::selection::BlockViewRow {
                                kind: weft_core::selection::BlockViewRowKind::Output,
                                text: chunk.clone(),
                                block_id: *block_id,
                                y_top: cy,
                                y_bottom: cy + pitch,
                            });
                        }
                    }
                }
                LaidRow::Command {
                    command, block_id, ..
                } => {
                    bv_rows.push(weft_core::selection::BlockViewRow {
                        kind: weft_core::selection::BlockViewRowKind::Command,
                        text: command.to_string(),
                        block_id: Some(*block_id),
                        y_top: y,
                        y_bottom: y + pitch,
                    });
                }
                LaidRow::Header { text: _ } => {
                    bv_rows.push(weft_core::selection::BlockViewRow {
                        kind: weft_core::selection::BlockViewRowKind::Header,
                        text: String::new(),
                        block_id: None,
                        y_top: y,
                        y_bottom: y + pitch,
                    });
                }
                LaidRow::Separator => {
                    bv_rows.push(weft_core::selection::BlockViewRow {
                        kind: weft_core::selection::BlockViewRowKind::Separator,
                        text: String::new(),
                        block_id: None,
                        y_top: y,
                        y_bottom: y + pitch,
                    });
                }
                LaidRow::LiveCommand { command } => {
                    bv_rows.push(weft_core::selection::BlockViewRow {
                        kind: weft_core::selection::BlockViewRowKind::LiveCommand,
                        text: command.to_string(),
                        block_id: None,
                        y_top: y,
                        y_bottom: y + pitch,
                    });
                }
                LaidRow::Blank => {
                    // No selectable content; skip (selection can't land here).
                }
            }
        }
        // Sync the selection's row snapshot to the current frame's bv_rows.
        // This remaps start/end row_index by matching (block_id, text, kind),
        // so the highlight scrolls WITH the content instead of staying pinned
        // to a stale screen y-position.
        if let Some(sel) = selection.block_view_selection.as_mut() {
            sel.sync_rows(bv_rows.clone());
        }
        // NOTE: we intentionally do NOT clear bv_rows here. The pre-pass above
        // built the UNCLIPPED row list (visible + off-screen rows). The render
        // loop below used to rebuild a CLIPPED version for hit-testing, but
        // that created an index mismatch: sync_rows remaps the selection's
        // row_index into the UNCLIPPED array, while the hit-test
        // (pixel_to_block_view_pos) returned indices into the CLIPPED array.
        // During a drag, extend_block_view received CLIPPED indices but the
        // snapshot was UNCLIPPED — producing a wrong range. Using UNCLIPPED
        // for both sync_rows AND hit-test keeps the indices consistent.
        // The render loop no longer pushes to bv_rows (the pre-pass already
        // has every row with the correct y-band + text).
        // Re-borrow after the mutable sync above.
        let sel_bv = selection.block_view_selection.as_ref();
        // Find highlight for block view: (block_id, line, is_command, col, len).
        let find_block_highlight = self.find_state.as_ref().and_then(|f| f.block_highlight);
        let sel_range_for_y = |row_mid_y: f32| -> Option<(usize, usize)> {
            let s = sel_bv?;
            let snap_idx = s.rows.iter().position(|r| r.contains_y(row_mid_y))?;
            let top = s.start.row_index.max(s.end.row_index);
            let bottom = s.start.row_index.min(s.end.row_index);
            if snap_idx < bottom || snap_idx > top {
                return None;
            }
            let max_char = s.rows[snap_idx].text.chars().count();
            let (c_start, c_end) = if top == bottom {
                let lo = s.start.char_index.min(s.end.char_index).min(max_char);
                let hi = s.start.char_index.max(s.end.char_index).min(max_char);
                (lo, hi)
            } else if snap_idx == top {
                // Top boundary: tail of row [anchor, max). Matches the text
                // extraction in BlockViewSelection::text() — drag starts at
                // the anchor and extends downward, so the top row contributes
                // its tail, not its head.
                let anchor = if s.start.row_index >= s.end.row_index {
                    s.start.char_index
                } else {
                    s.end.char_index
                };
                (anchor.min(max_char), max_char)
            } else if snap_idx == bottom {
                // Bottom boundary: head of row [0, anchor).
                let anchor = if s.start.row_index >= s.end.row_index {
                    s.end.char_index
                } else {
                    s.start.char_index
                };
                (0, anchor.min(max_char))
            } else {
                (0, max_char)
            };
            (c_end > c_start).then_some((c_start, c_end))
        };
        // True if a row band (mid-y) falls inside the selection's row range,
        // regardless of whether the row carries selectable text. Used to
        // fill Header/Separator rows with the selection color so the
        // highlight reads as a continuous band instead of broken segments.
        let row_in_selection = |row_mid_y: f32| -> bool {
            let Some(s) = sel_bv else { return false };
            let Some(snap_idx) = s.rows.iter().position(|r| r.contains_y(row_mid_y)) else {
                return false;
            };
            let top = s.start.row_index.max(s.end.row_index);
            let bottom = s.start.row_index.min(s.end.row_index);
            snap_idx >= bottom && snap_idx <= top
        };

        for (i, &dist) in rows.iter().enumerate() {
            let row_top_y = content_bottom_y - dist + scroll_px;
            let row_bottom_y = row_top_y + pitch;

            if row_bottom_y < clip_top || row_top_y > clip_bottom {
                continue;
            }

            let y = row_top_y;

            match &row_data[i] {
                LaidRow::Output {
                    text,
                    chunks,
                    block_id,
                    line,
                } => {
                    if chunks.len() <= 1 {
                        // Selection highlight (under the text).
                        if let Some((cs, ce)) = sel_range_for_y(y + pitch * 0.5) {
                            self.push_block_view_highlight(
                                &mut verts,
                                left,
                                y,
                                ch,
                                text,
                                cs,
                                ce,
                                selection_bg,
                                bg_uv,
                            );
                        }
                        // Find highlight: if this row matches the current
                        // block match, draw a yellow highlight at (col, len).
                        if let Some(bh) = find_block_highlight {
                            if bh.0 == block_id.map(|b| b.0).unwrap_or(0) && bh.1 == *line && !bh.2
                            {
                                let hx0 = left + bh.3 as f32 * cw;
                                let hx1 = hx0 + bh.4 as f32 * cw;
                                let hl_bg = [0.95, 0.78, 0.20, 0.50];
                                push_quad(
                                    &mut verts,
                                    [hx0, y, hx1, y + ch],
                                    bg_uv,
                                    [0.0; 4],
                                    hl_bg,
                                );
                            }
                        }
                        self.push_text(&mut verts, left, y, text, fg, cols);
                        // bv_rows entry is built by the pre-pass (UNCLIPPED).
                    } else {
                        for (ci, chunk) in chunks.iter().enumerate() {
                            let cy = y + ci as f32 * pitch;
                            if cy + ch > clip_top && cy < clip_bottom {
                                if let Some((cs, ce)) = sel_range_for_y(cy + pitch * 0.5) {
                                    self.push_block_view_highlight(
                                        &mut verts,
                                        left,
                                        cy,
                                        ch,
                                        chunk,
                                        cs,
                                        ce,
                                        selection_bg,
                                        bg_uv,
                                    );
                                }
                                // Find highlight for wrapped chunks: only
                                // highlight on the first chunk (col is relative
                                // to the original line).
                                if ci == 0 {
                                    if let Some(bh) = find_block_highlight {
                                        if bh.0 == block_id.map(|b| b.0).unwrap_or(0)
                                            && bh.1 == *line
                                            && !bh.2
                                        {
                                            let hx0 = left + bh.3 as f32 * cw;
                                            let hx1 = hx0 + bh.4 as f32 * cw;
                                            let hl_bg = [0.95, 0.78, 0.20, 0.50];
                                            push_quad(
                                                &mut verts,
                                                [hx0, cy, hx1, cy + ch],
                                                bg_uv,
                                                [0.0; 4],
                                                hl_bg,
                                            );
                                        }
                                    }
                                }
                                self.push_text(&mut verts, left, cy, chunk, fg, cols);
                            }
                            // Wrapped chunk's bv_rows entry is in the pre-pass.
                        }
                    }
                }
                LaidRow::Command {
                    command,
                    collapsed,
                    foldable,
                    block_id,
                } => {
                    let (chev_w, avail_sub) = if *foldable {
                        let chev = if *collapsed { "▸" } else { "▾" };
                        self.push_text(&mut verts, left, y, chev, prompt_c, cols);
                        (cw, 3)
                    } else {
                        (0.0, 2)
                    };
                    self.push_text(&mut verts, left + chev_w, y, "❯ ", prompt_c, cols);
                    let cmd_x = left + chev_w + 2.0 * cw;
                    let avail = cols.saturating_sub(avail_sub).max(1);
                    // Selection highlight under the command text (excludes the
                    // chevron/❯ prefix — those aren't part of the copyable text).
                    if let Some((cs, ce)) = sel_range_for_y(y + pitch * 0.5) {
                        self.push_block_view_highlight(
                            &mut verts,
                            cmd_x,
                            y,
                            ch,
                            command,
                            cs,
                            ce,
                            selection_bg,
                            bg_uv,
                        );
                    }
                    // Find highlight on command text.
                    if let Some(bh) = find_block_highlight {
                        if bh.0 == block_id.0 && bh.2 {
                            let hx0 = cmd_x + bh.3 as f32 * cw;
                            let hx1 = hx0 + bh.4 as f32 * cw;
                            let hl_bg = [0.95, 0.78, 0.20, 0.50];
                            push_quad(&mut verts, [hx0, y, hx1, y + ch], bg_uv, [0.0; 4], hl_bg);
                        }
                    }
                    // v0.9 fix: strip prompt prefix for display consistency.
                    let cleaned_cmd = strip_prompt_prefix(command);
                    self.push_line_tokenized(&mut verts, cmd_x, y, &cleaned_cmd, avail);
                    if *foldable {
                        // v0.9 (revised): the fold hit region covers only the
                        // chevron cell, not the whole command line — so the
                        // rest of the line can be click-drag-selected for copy.
                        hit_regions.push(crate::overlay::HitRegion {
                            x0: left,
                            y0: y,
                            x1: left + cw,
                            y1: y + pitch,
                            target: crate::overlay::HitTarget::BlockFold(*block_id),
                        });
                    }
                    // Command row's bv_rows entry is in the pre-pass.
                }
                LaidRow::Header { text } => {
                    // Fill the row band with selection color when this Header
                    // row sits inside the active selection — otherwise the
                    // highlight reads as broken segments between Command
                    // and Output rows (Header isn't selectable, so
                    // sel_range_for_y returns None here).
                    if row_in_selection(y + pitch * 0.5) {
                        push_quad(
                            &mut verts,
                            [left, y, right, y + pitch],
                            bg_uv,
                            [0.0; 4],
                            selection_bg,
                        );
                    }
                    self.push_text(&mut verts, left, y, text, dim, cols);
                    // Header's bv_rows entry is in the pre-pass.
                }
                LaidRow::Separator => {
                    if row_in_selection(y + pitch * 0.5) {
                        push_quad(
                            &mut verts,
                            [left, y, right, y + pitch],
                            bg_uv,
                            [0.0; 4],
                            selection_bg,
                        );
                    }
                    let ly = y + pitch * 0.5;
                    push_quad(
                        &mut verts,
                        [left, ly, right, ly + 1.5],
                        bg_uv,
                        [0.0; 4],
                        separator,
                    );
                    // Separator's bv_rows entry is in the pre-pass.
                }
                LaidRow::LiveCommand { command } => {
                    self.push_text(&mut verts, left, y, "❯ ", prompt_c, cols);
                    let cmd_x = left + 2.0 * cw;
                    let avail = cols.saturating_sub(2).max(1);
                    if let Some((cs, ce)) = sel_range_for_y(y + pitch * 0.5) {
                        self.push_block_view_highlight(
                            &mut verts,
                            cmd_x,
                            y,
                            ch,
                            command,
                            cs,
                            ce,
                            selection_bg,
                            bg_uv,
                        );
                    }
                    self.push_line_tokenized(&mut verts, cmd_x, y, command, avail);
                    // LiveCommand's bv_rows entry is in the pre-pass.
                }
                LaidRow::Blank => {
                    // Warp-style clear spacer: nothing to draw — the
                    // background fill already covers this region. Skip.
                }
            }

            // Track the topmost visible block for the sticky header. We want
            // the block whose content is closest to (but not below) the clip
            // top. Since rows are ordered bottom-to-top, the LAST header/command
            // row we see that's above clip_top wins.
            if block_scroll > 0 && row_top_y <= clip_top + pitch {
                if let LaidRow::Command { command, .. } = &row_data[i] {
                    let cmd_str: &str = command;
                    for b in blocks.iter().rev() {
                        if b.command == cmd_str {
                            topmost_block_info = Some((
                                cmd_str.to_string(),
                                b.cwd.as_deref().map(abbreviate_path).unwrap_or_default(),
                            ));
                            break;
                        }
                    }
                }
            }
        }

        // ── Sticky top header (when scrolled) ────────────────────────────
        //
        // Warp-style: when the block view is scrolled, the top of the viewport
        // shows a sticky line with the command (and cwd) of the block whose
        // output is currently at the top. This gives context about what output
        // you're looking at without seeing the block's own header.
        if block_scroll > 0 {
            if let Some((cmd, block_cwd)) = &topmost_block_info {
                let sticky_y = layout.clip_top;
                // Background bar (slightly different shade to distinguish).
                let sticky_bg = [
                    theme_bg[0] + (1.0 - theme_bg[0]) * 0.08,
                    theme_bg[1] + (1.0 - theme_bg[1]) * 0.08,
                    theme_bg[2] + (1.0 - theme_bg[2]) * 0.08,
                    1.0,
                ];
                push_quad(
                    &mut verts,
                    [0.0, sticky_y, vp_w, sticky_y + pitch],
                    bg_uv,
                    [0.0; 4],
                    sticky_bg,
                );
                // Bottom border for the sticky bar.
                push_quad(
                    &mut verts,
                    [0.0, sticky_y + pitch, vp_w, sticky_y + pitch + 1.0],
                    bg_uv,
                    [0.0; 4],
                    separator,
                );
                // Content: `❯ command  cwd` (command in prompt color, cwd in dim).
                self.push_text(&mut verts, left, sticky_y, "❯ ", prompt_c, cols);
                let cmd_x = left + 2.0 * cw;
                let avail = cols.saturating_sub(2).max(1);
                self.push_line_tokenized(&mut verts, cmd_x, sticky_y, cmd, avail);
                if !block_cwd.is_empty() {
                    let cmd_cols = Self::text_col_width(cmd);
                    let cwd_x = cmd_x + (cmd_cols + 2) as f32 * cw;
                    let cwd_avail = cols.saturating_sub(2 + cmd_cols + 2).max(1);
                    self.push_text(&mut verts, cwd_x, sticky_y, block_cwd, dim, cwd_avail);
                }
            }
        }

        // v0.9 W2: draw an accent border around the panel-highlighted block.
        // The highlight is armed by `scroll_to_panel_selection()` for 1.5s
        // after a panel click. We scan the laid-out rows to find the y-range
        // of rows belonging to the highlighted block (Output + Command rows),
        // then draw a rounded accent border around that range.
        if let Some(hl_id) = self.panel_highlight {
            let mut hl_top: Option<f32> = None;
            let mut hl_bottom: Option<f32> = None;
            for (i, &dist) in rows.iter().enumerate() {
                let row_top_y = content_bottom_y - dist + scroll_px;
                let row_bottom_y = row_top_y + pitch;
                // Check if this row belongs to the highlighted block.
                let belongs = match &row_data[i] {
                    LaidRow::Output { block_id, .. } => *block_id == Some(hl_id),
                    LaidRow::Command { block_id, .. } => *block_id == hl_id,
                    _ => false,
                };
                if belongs {
                    hl_top = Some(match hl_top {
                        Some(t) => t.min(row_top_y),
                        None => row_top_y,
                    });
                    hl_bottom = Some(match hl_bottom {
                        Some(b) => b.max(row_bottom_y),
                        None => row_bottom_y,
                    });
                }
            }
            // Clamp to clip region.
            if let (Some(top), Some(bottom)) = (hl_top, hl_bottom) {
                let y0 = top.max(clip_top);
                let y1 = bottom.min(clip_bottom);
                if y1 > y0 {
                    let accent = color_to_normalized(self.theme.accent);
                    let accent_alpha = [accent[0], accent[1], accent[2], 0.85];
                    let border_w = 2.0 * self.scale as f32;
                    // Four-sided border.
                    push_quad(
                        &mut verts,
                        [left - border_w, y0, right + border_w, y0 + border_w],
                        bg_uv,
                        [0.0; 4],
                        accent_alpha,
                    );
                    push_quad(
                        &mut verts,
                        [left - border_w, y1 - border_w, right + border_w, y1],
                        bg_uv,
                        [0.0; 4],
                        accent_alpha,
                    );
                    push_quad(
                        &mut verts,
                        [left - border_w, y0, left, y1],
                        bg_uv,
                        [0.0; 4],
                        accent_alpha,
                    );
                    push_quad(
                        &mut verts,
                        [right, y0, right + border_w, y1],
                        bg_uv,
                        [0.0; 4],
                        accent_alpha,
                    );
                }
            }
        }

        (verts, hit_regions, bv_rows)
    }

    /// Column width of a character (0 for zero-width combining marks,
    /// 1 for ASCII/narrow, 2 for CJK full-width including ambiguous-width
    /// characters like ①②③ which are rendered full-width in CJK context).
    fn char_col_width(c: char) -> usize {
        unicode_width::UnicodeWidthChar::width_cjk(c).unwrap_or(0)
    }

    /// Total column width of a string — sum of each char's display width.
    /// Use this instead of `chars().count()` whenever a width/position
    /// calculation must match what `push_text` actually renders (CJK chars
    /// occupy 2 columns each, not 1).
    fn text_col_width(s: &str) -> usize {
        s.chars()
            .map(|c| unicode_width::UnicodeWidthChar::width_cjk(c).unwrap_or(0))
            .sum()
    }

    /// Push a selection-highlight background quad for a character range of
    /// `text`, honoring CJK double-width so the highlight exactly covers the
    /// selected glyphs. Called before `push_text` so the text renders on top
    /// of the highlight (matching grid-view selection rendering).
    #[allow(clippy::too_many_arguments)]
    fn push_block_view_highlight(
        &self,
        vertices: &mut Vec<f32>,
        x_left: f32,
        y_top: f32,
        height: f32,
        text: &str,
        c_start: usize,
        c_end: usize,
        bg_color: [f32; 4],
        bg_uv: [f32; 4],
    ) {
        if c_end <= c_start || text.is_empty() {
            return;
        }
        let cw = self.cell_width() as f32;
        let mut px = x_left;
        let mut col = 0f32; // column units consumed
        for (ci, c) in text.chars().enumerate() {
            let w = unicode_width::UnicodeWidthChar::width_cjk(c).unwrap_or(0);
            if w == 0 {
                continue;
            }
            if ci >= c_end {
                break;
            }
            let cell_w = w as f32 * cw;
            if ci >= c_start {
                // This char is inside the highlight range.
                push_quad(
                    vertices,
                    [px, y_top, px + cell_w, y_top + height],
                    bg_uv,
                    [0.0; 4], // fg mask: no text contribution (pure background)
                    bg_color,
                );
            }
            px += cell_w;
            col += w as f32;
            let _ = col; // (kept for symmetry with push_text's col accounting)
        }
    }

    /// Lay out a string left-to-right, honoring wide-character (CJK) widths.
    /// `max_cols` is a *column* budget (not a character count): a CJK char
    /// consumes 2 columns, ASCII consumes 1. Glyphs must already be in the
    /// atlas (warmed up by the caller). Text exceeding `max_cols` columns is
    /// truncated (callers that need wrapping use `push_text_wrapped`).
    fn push_text(
        &self,
        vertices: &mut Vec<f32>,
        x: f32,
        y: f32,
        text: &str,
        fg: [f32; 4],
        max_cols: usize,
    ) {
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let mut col = 0usize;
        let mut px = x;
        for c in text.chars() {
            let w = Self::char_col_width(c);
            if w == 0 {
                continue; // skip combining marks / zero-width
            }
            if col + w > max_cols {
                break; // column budget exhausted
            }
            let Some(g) = self.atlas.get(c) else {
                col += w;
                px += w as f32 * cw;
                continue;
            };
            let (u, v) = g.uv_origin;
            let (uw, vh) = g.uv_size;
            let cell_w = w as f32 * cw;
            push_quad(
                vertices,
                [px, y, px + cell_w, y + ch],
                [u, v + vh, u + uw, v],
                fg,
                [0.0; 4],
            );
            col += w;
            px += cell_w;
        }
    }

    /// v1.0: Like `push_text` but renders each glyph quad scaled by `scale`
    /// (1.0 == identical to push_text). Used for the Settings footer where
    /// the hint pairs benefit from being slightly more prominent than the
    /// body text. The glyph atlas is rasterized at the base cell size, so
    /// scaling up samples with mild magnification (acceptable for ≤1.2x).
    /// Column advance is scaled too so layout math stays consistent.
    #[allow(clippy::too_many_arguments)]
    fn push_text_scaled(
        &self,
        vertices: &mut Vec<f32>,
        x: f32,
        y: f32,
        text: &str,
        fg: [f32; 4],
        max_cols: usize,
        scale: f32,
    ) {
        let cw = self.cell_width() as f32 * scale;
        let ch = self.cell_height() as f32 * scale;
        // Vertically center the scaled glyph within the original cell row
        // so the footer baseline stays aligned with the separator line.
        let y_off = (self.cell_height() as f32 - ch) * 0.5;
        let mut col = 0usize;
        let mut px = x;
        for c in text.chars() {
            let w = Self::char_col_width(c);
            if w == 0 {
                continue;
            }
            if col + w > max_cols {
                break;
            }
            let Some(g) = self.atlas.get(c) else {
                col += w;
                px += w as f32 * cw;
                continue;
            };
            let (u, v) = g.uv_origin;
            let (uw, vh) = g.uv_size;
            let cell_w = w as f32 * cw;
            push_quad(
                vertices,
                [px, y + y_off, px + cell_w, y + y_off + ch],
                [u, v + vh, u + uw, v],
                fg,
                [0.0; 4],
            );
            col += w;
            px += cell_w;
        }
    }

    /// Like `push_text` but wraps long text across multiple visual rows
    /// Lay out a line left-to-right, coloring each shell token by its kind
    /// (syntax highlight). `default_fg` is used for Whitespace/Default tokens.
    /// Wide-character aware: CJK chars occupy 2 columns. Glyphs must already
    /// be in the atlas (warmed up by the caller).
    fn push_line_tokenized(
        &self,
        vertices: &mut Vec<f32>,
        x: f32,
        y: f32,
        line: &str,
        max_cols: usize,
    ) {
        let cw = self.cell_width() as f32;
        let mut col = 0usize;
        let mut px = x;
        for token in syntax::tokenize(line) {
            if col >= max_cols {
                break;
            }
            let color = syntax_color(token.kind, &self.theme);
            for c in token.text.chars() {
                let w = Self::char_col_width(c);
                if w == 0 {
                    continue;
                }
                if col + w > max_cols {
                    break;
                }
                if let Some(g) = self.atlas.get(c) {
                    let (u, v) = g.uv_origin;
                    let (uw, vh) = g.uv_size;
                    let cell_w = w as f32 * cw;
                    push_quad(
                        vertices,
                        [px, y, px + cell_w, y + self.cell_height() as f32],
                        [u, v + vh, u + uw, v],
                        color,
                        [0.0; 4],
                    );
                    px += cell_w;
                }
                col += w;
            }
        }
    }

    /// Build the FindInGrid overlay (v0.8 B3): a Warp-style popup card in
    /// the top-right corner showing the query + match count, plus a yellow
    /// translucent highlight over the current match's cells.
    ///
    /// v0.8 user testing asked for a Warp-style independent popup (instead
    /// of a full-width top banner) floating in the top-right corner. The
    /// card is a layered surface: drop shadow + tinted background + 1px
    /// border + accent-colored left stripe, with a single row of
    /// "Find: <query>  <status>" inside. Auto-focus is already handled at
    /// the app layer — `handle_find_key` captures keystrokes when `find_open`
    /// is true, so the input is functionally focused whenever the popup is
    /// visible.
    ///
    /// Drawing uses the same fg/bg vertex pipeline as the rest of the
    /// renderer: text is sampled from the glyph atlas, card surfaces are
    /// bg-only quads sampling the space glyph (mask 0 → solid bg color).
    fn build_find_vertices(&self, find: &FindDrawState) -> (Vec<f32>, FindButtons) {
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let ctx = match &self.layout_ctx {
            Some(c) => *c,
            None => return (Vec::new(), FindButtons::default()),
        };

        let mut verts = Vec::new();
        let (su, sv, suw, svh) = self.space_uv();
        let bg_uv = [su, sv + svh, su + suw, sv]; // V-flipped for layer

        // ── Popup geometry ──────────────────────────────────────────────
        // Target ~500px wide (Warp's default), capped by available content
        // width so it never overflows the left edge. Height: at least one
        // cell + 16px padding, at most 1.75× cell height for legibility.
        let target_w = 500.0_f32;
        let right_margin = 20.0;
        let top_margin = 10.0;
        let min_w = cw * 48.0; // v0.9 fix: wider min so query + buttons + status fit
        let popup_w = target_w.min(ctx.width() - right_margin - 20.0).max(min_w);
        let popup_h = (ch * 1.75).max(ch + 16.0);
        let popup_x1 = ctx.right() - right_margin;
        let popup_x0 = popup_x1 - popup_w;
        let popup_y0 = ctx.top() + top_margin;
        let popup_y1 = popup_y0 + popup_h;

        let theme_bg = color_to_normalized(self.theme.background);
        let accent = color_to_normalized(self.theme.accent);
        let sep = color_to_normalized(self.theme.separator);
        let fg = color_to_normalized(self.theme.foreground);
        // v1.0 fix: replace accent_dim with label_c (70% fg + 30% bg) —
        // accent_dim is too close to bg in Nord (#4c566a vs #2e3440) and
        // Warp themes, making buttons/status text invisible. label_c is
        // always readable across all themes.
        let accent_dim = [
            fg[0] * 0.70 + theme_bg[0] * 0.30,
            fg[1] * 0.70 + theme_bg[1] * 0.30,
            fg[2] * 0.70 + theme_bg[2] * 0.30,
            1.0,
        ];

        // ── Drop shadow (Warp-style: tight offset, low opacity) ──────────
        let shadow_offset = 2.0;
        push_quad(
            &mut verts,
            [
                popup_x0 - shadow_offset,
                popup_y0 - shadow_offset,
                popup_x1 + shadow_offset,
                popup_y1 + shadow_offset,
            ],
            bg_uv,
            [0.0; 4],
            [0.0, 0.0, 0.0, 0.15],
        );

        // ── Card background — v1.0: unified with Settings/Palette to 8%
        // lighten (was a dual-branch 45% darken / 60% lighten, which made
        // Find read as "darker than window" while other popups read as
        // "lighter than window" — visually inconsistent). Opaque (α=1.0)
        // to fully occlude underlying grid content.
        let card_bg = [
            theme_bg[0] + (1.0 - theme_bg[0]) * 0.08,
            theme_bg[1] + (1.0 - theme_bg[1]) * 0.08,
            theme_bg[2] + (1.0 - theme_bg[2]) * 0.08,
            1.0,
        ];
        push_quad(
            &mut verts,
            [popup_x0, popup_y0, popup_x1, popup_y1],
            bg_uv,
            [0.0; 4],
            card_bg,
        );

        // ── 1px border — Warp-style: low opacity, subtle ──────────────────
        let border_w = 1.0;
        let border_bg = [0.5, 0.5, 0.5, 0.20];
        push_quad(
            &mut verts,
            [popup_x0, popup_y0, popup_x1, popup_y0 + border_w],
            bg_uv,
            [0.0; 4],
            border_bg,
        );
        push_quad(
            &mut verts,
            [popup_x0, popup_y1 - border_w, popup_x1, popup_y1],
            bg_uv,
            [0.0; 4],
            border_bg,
        );
        push_quad(
            &mut verts,
            [popup_x0, popup_y0, popup_x0 + border_w, popup_y1],
            bg_uv,
            [0.0; 4],
            border_bg,
        );
        push_quad(
            &mut verts,
            [popup_x1 - border_w, popup_y0, popup_x1, popup_y1],
            bg_uv,
            [0.0; 4],
            border_bg,
        );

        // ── Accent-colored left stripe (3px) — v0.8 signature accent ────
        // Replaces the previous top accent stripe so the popup still reads
        // as branded without occupying vertical space at the card edge.
        let stripe_w = 3.0;
        push_quad(
            &mut verts,
            [
                popup_x0 + border_w,
                popup_y0 + border_w,
                popup_x0 + border_w + stripe_w,
                popup_y1 - border_w,
            ],
            bg_uv,
            [0.0; 4],
            [accent[0], accent[1], accent[2], 1.0],
        );

        // ── Match highlight (yellow translucent overlay on grid cells) ──
        // Drawn over the grid content area, independent of the popup card.
        if let Some((row, col, len)) = find.highlight {
            let hx0 = ctx.col_x(col);
            let hy0 = ctx.row_y(row);
            let hx1 = hx0 + len as f32 * cw;
            let hy1 = hy0 + ch;
            // Soft yellow with 0.5 alpha so the underlying text stays readable.
            push_quad(
                &mut verts,
                [hx0, hy0, hx1, hy1],
                bg_uv,
                [0.0; 4],
                [0.95, 0.78, 0.20, 0.50],
            );
        }

        // ── Popup text row ─────────────────────────────────────────────
        // Layout (left to right):
        //   [pad]Find: <query>│    <status> [↑][↓] [Aa] [.*][pad]
        //   │ = blinking cursor at end of query
        //   <status> = compact match count (right-aligned)
        //   ↑/↓ = prev/next match buttons (clickable)
        //   Aa = case-sensitive toggle (lit when case_sensitive is on)
        //   .* = regex toggle indicator (lit when regex_mode is on)
        //
        // Button hit-test rects (with generous click padding) are stored in
        // self.find_buttons for the app's mouse handler to read.
        let inner_pad_x = 8.0;
        let text_x0 = popup_x0 + border_w + stripe_w + inner_pad_x;
        let text_x1 = popup_x1 - border_w - inner_pad_x;
        let line_y = popup_y0 + border_w + ((popup_h - border_w * 2.0 - ch) * 0.5).max(0.0);
        let right_x = text_x1;

        // ── Right-aligned button cluster (rightmost first) ───────────────
        // Each button: 2 chars wide glyph + 1 char gap on its left side.
        // Click padding: extend the hit-test rect 2px above/below the line
        // and 1px left/right so the clickable area is forgiving.
        let btn_click_pad = 2.0;
        let gap_cols = 1;
        let gap_w = gap_cols as f32 * cw;

        // ".*" regex toggle (rightmost).
        let regex_label = ".*";
        let regex_w = Self::text_col_width(regex_label);
        let regex_text_w = regex_w as f32 * cw;
        let regex_x = right_x - regex_text_w;
        let regex_color = if find.regex_mode { accent } else { accent_dim };
        self.push_text(
            &mut verts,
            regex_x,
            line_y,
            regex_label,
            regex_color,
            regex_w,
        );
        let regex_rect = [
            regex_x - btn_click_pad,
            line_y - btn_click_pad,
            right_x + btn_click_pad,
            line_y + ch + btn_click_pad,
        ];

        // "Aa" case-sensitive toggle.
        let case_label = "Aa";
        let case_w = Self::text_col_width(case_label);
        let case_text_w = case_w as f32 * cw;
        let case_x = regex_x - gap_w - case_text_w;
        let case_color = if find.case_sensitive {
            accent
        } else {
            accent_dim
        };
        self.push_text(&mut verts, case_x, line_y, case_label, case_color, case_w);
        let case_rect = [
            case_x - btn_click_pad,
            line_y - btn_click_pad,
            case_x + case_text_w + btn_click_pad,
            line_y + ch + btn_click_pad,
        ];

        // "↓" down arrow (next match) — 1 char wide.
        let down_label = "↓";
        let down_w = Self::text_col_width(down_label);
        let down_text_w = down_w as f32 * cw;
        let down_x = case_x - gap_w - down_text_w;
        let down_color = if find.total > 0 { accent_dim } else { sep };
        self.push_text(&mut verts, down_x, line_y, down_label, down_color, down_w);
        let down_rect = if find.total > 0 {
            Some([
                down_x - btn_click_pad,
                line_y - btn_click_pad,
                down_x + down_text_w + btn_click_pad,
                line_y + ch + btn_click_pad,
            ])
        } else {
            None
        };

        // "↑" up arrow (previous match) — 1 char wide.
        let up_label = "↑";
        let up_w = Self::text_col_width(up_label);
        let up_text_w = up_w as f32 * cw;
        let up_x = down_x - gap_w - up_text_w;
        let up_color = if find.total > 0 { accent_dim } else { sep };
        self.push_text(&mut verts, up_x, line_y, up_label, up_color, up_w);
        let up_rect = if find.total > 0 {
            Some([
                up_x - btn_click_pad,
                line_y - btn_click_pad,
                up_x + up_text_w + btn_click_pad,
                line_y + ch + btn_click_pad,
            ])
        } else {
            None
        };

        // Store hit-test rects for the app's mouse handler. Returned to the
        // caller (draw()) rather than written to `self` directly to avoid a
        // borrow conflict with the Metal drawable (which borrows `self.layer`
        // for the whole frame).
        let buttons = FindButtons {
            up: up_rect,
            down: down_rect,
            case_sensitive: case_rect,
            regex: regex_rect,
        };

        // ── Status text (left of the up arrow, compact) ──────────────────
        // Compact format to avoid overflow:
        //   empty query → "" (nothing, keep it clean)
        //   no matches   → "no matches  "
        //   has matches  → "current/total  "
        //   truncated    → "total+  "
        //   block-only   → "N in blocks  "
        //   regex error  → "invalid regex  " (red — v0.9 U-P2)
        //
        // v0.9 fix: if the status text would push `status_x` so far left that
        // the query has < MIN_QUERY_BUDGET cols, skip rendering the status
        // text entirely. The query visibility is more important than status.
        let (status, status_color) = if find.regex_error.is_some() {
            ("invalid regex  ".to_string(), accent)
        } else if find.query.is_empty() {
            (String::new(), accent_dim)
        } else if find.truncated {
            (format!("{}+  ", find.total), accent_dim)
        } else if find.total == 0 {
            if find.block_matches > 0 {
                (format!("{} in blocks  ", find.block_matches), accent_dim)
            } else {
                ("no matches  ".to_string(), accent_dim)
            }
        } else {
            (
                format!("{}/{}  ", find.current.max(1), find.total),
                accent_dim,
            )
        };
        let status_w = Self::text_col_width(&status);
        let status_x = up_x - gap_w - status_w as f32 * cw;
        // Check if there's room for both status and a minimum-width query.
        let query_start_x_test = text_x0 + Self::text_col_width("Find: ") as f32 * cw;
        let avail_for_query = ((status_x - query_start_x_test) / cw).floor() as isize;
        const MIN_QUERY_BUDGET: isize = 10;
        let show_status = avail_for_query >= MIN_QUERY_BUDGET;
        if show_status {
            self.push_text(
                &mut verts,
                status_x,
                line_y,
                &status,
                status_color,
                status_w,
            );
        }

        // ── "Find: " label ───────────────────────────────────────────────
        let label = "Find: ";
        let label_cols = Self::text_col_width(label);
        self.push_text(&mut verts, text_x0, line_y, label, accent, label_cols);

        // ── Query text (truncated from left to fit) ──────────────────────
        // The cursor sits at the END of the query, so we show the tail when
        // the query is too long (prepend "…" to indicate truncation).
        let query_start_x = text_x0 + label_cols as f32 * cw;
        // v0.9 fix: when status is hidden (show_status == false), the query
        // extends to up_x (the left edge of the ↑ button). Otherwise it
        // extends to status_x.
        let query_right_x = if show_status { status_x } else { up_x };
        let query_max_w = ((query_right_x - query_start_x) / cw).floor().max(0.0) as usize;
        // Reserve 1 col for the cursor.
        let query_budget = query_max_w.saturating_sub(1);
        let query_full_w = Self::text_col_width(&find.query);
        let (query_display, cursor_x): (String, f32) = if query_full_w <= query_budget {
            // Full query fits — cursor goes right after the last char.
            let cx = query_start_x + query_full_w as f32 * cw;
            (find.query.clone(), cx)
        } else {
            // Truncate from left: walk chars in reverse, keep the tail.
            let mut kept: Vec<char> = Vec::new();
            let mut w = 1usize; // reserve 1 for "…"
            for c in find.query.chars().rev() {
                let cw_char = unicode_width::UnicodeWidthChar::width_cjk(c).unwrap_or(0);
                if w + cw_char > query_budget {
                    break;
                }
                kept.push(c);
                w += cw_char;
            }
            kept.reverse();
            let mut s = String::from("…");
            s.extend(kept.iter());
            let cx = query_start_x + w as f32 * cw;
            (s, cx)
        };
        let query_cols = Self::text_col_width(&query_display);
        self.push_text(
            &mut verts,
            query_start_x,
            line_y,
            &query_display,
            fg,
            query_cols,
        );

        // ── Blinking cursor (vertical bar at end of query) ───────────────
        // 600ms on, 600ms off — standard terminal cursor blink rate.
        let blink_phase = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() % 1200)
            .unwrap_or(0);
        if blink_phase < 600 {
            let cursor_w = 2.0_f32.max(cw * 0.12);
            push_quad(
                &mut verts,
                [cursor_x, line_y, cursor_x + cursor_w, line_y + ch],
                bg_uv,
                [0.0; 4],
                fg,
            );
        }

        let _ = ch;
        (verts, buttons)
    }

    /// UV rect of the space glyph (background-only quads need mask 0).
    fn space_uv(&self) -> (f32, f32, f32, f32) {
        self.atlas
            .get(' ')
            .map(|g| {
                let (u, v) = g.uv_origin;
                let (uw, vh) = g.uv_size;
                (u, v, uw, vh)
            })
            .unwrap_or((0.0, 0.0, 0.0, 0.0))
    }

    /// v0.9 H1: Build the tab bar vertices (background + tab labels + close
    /// buttons). Returns `(vertices, tab_hits)` where `tab_hits` is the
    /// click hit-test data for the app's mouse handler.
    ///
    /// Layout:
    /// - Tab bar spans the full viewport width at the top.
    /// - Each tab is ~16 cells wide, with a 1px divider between tabs.
    /// - Active tab gets a brighter background + accent underline.
    /// - Close "×" button at the right of each tab.
    fn build_tab_bar_vertices(
        &self,
        tab_bar: &TabBarDrawState,
    ) -> (Vec<f32>, Vec<TabHit>, [f32; 4]) {
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let bar_h = self.tab_bar_height();
        let vp_w = self.viewport.0;
        let pad_x = self.padding_x;
        let chrome_left = self.layout_ctx.map(|c| c.chrome_left).unwrap_or(0.0);

        let bg = color_to_normalized(self.theme.background);
        let fg = color_to_normalized(self.theme.foreground);
        let accent = color_to_normalized(self.theme.accent);
        let separator = color_to_normalized(self.theme.separator);

        let bar_bg = if bg[0] + bg[1] + bg[2] < 1.5 {
            [bg[0] * 0.85, bg[1] * 0.85, bg[2] * 0.85, 1.0]
        } else {
            [
                bg[0] + (1.0 - bg[0]) * 0.5,
                bg[1] + (1.0 - bg[1]) * 0.5,
                bg[2] + (1.0 - bg[2]) * 0.5,
                1.0,
            ]
        };

        let mut vertices = Vec::new();
        let mut hits = Vec::new();

        push_quad(
            &mut vertices,
            [0.0, 0.0, vp_w, bar_h],
            [0.0; 4],
            [0.0; 4],
            bar_bg,
        );

        let tl_w = self.traffic_lights_width();

        // v1.2: Tab sizing — tabs have a minimum width (15 cells ~120px) and
        // maximum (20 cells ~200px). When total width exceeds available space,
        // tabs scroll horizontally instead of being compressed.
        //
        // v1.2-fix: Three-tier overflow detection:
        //   1. Tabs at max width fit → use max, no scroll
        //   2. Tabs at min width fit → shrink to fit, no scroll
        //   3. Even min width doesn't fit → scroll mode (fixed min width)
        // Previously only checked max-width fit vs available, incorrectly
        // entering scroll mode when min-width tabs would have fit fine.
        let max_tab_w = cw * 20.0;
        let min_tab_w = cw * 15.0;
        let arrow_w = cw * 2.5; // scroll arrow slot width
        let plus_w = cw * 3.0; // "+" button width (~24px, matches demo)
        let right_pad = pad_x * 0.5; // gap between "+" and window edge

        // v1.2-fix: when the sidebar is open (chrome_left > 0), the traffic
        // lights are over the sidebar, not to its right — so don't add tl_w.
        let tl_offset = if chrome_left > 0.0 { 0.0 } else { tl_w };
        let tabs_start = chrome_left + tl_offset + pad_x;
        let right_reserve = plus_w + right_pad;
        let avail_for_tabs = vp_w - tabs_start - right_reserve;

        let total_at_max = tab_bar.tab_count as f32 * max_tab_w;
        let total_at_min = tab_bar.tab_count as f32 * min_tab_w;

        let (tab_w, overflowing) = if total_at_max <= avail_for_tabs {
            (max_tab_w, false)
        } else if total_at_min <= avail_for_tabs {
            let fitted = avail_for_tabs / tab_bar.tab_count as f32;
            (fitted.clamp(min_tab_w, max_tab_w), false)
        } else {
            (min_tab_w, true)
        };

        let total_tab_w = tab_bar.tab_count as f32 * tab_w;
        // v1.2-fix: defensively clamp scroll_offset in the renderer. Even if
        // the app's stored value is stale (e.g. after a resize that hasn't
        // been clamped yet), this prevents tabs from being over-scrolled
        // past the last tab (which would leave a blank gap on the right).
        let scroll_offset = if overflowing {
            // vis_w is computed below, but we need max_scroll here. Compute
            // it inline (must match the vis_left/vis_right formulas below).
            let vl = tabs_start + arrow_w;
            let vr = vp_w - right_reserve - arrow_w;
            let vw = (vr - vl).max(0.0);
            let ms = (total_tab_w - vw).max(0.0);
            tab_bar.scroll_offset.clamp(0.0, ms)
        } else {
            0.0
        };

        // Visible region for tab content. Arrows take space on both sides
        // when overflowing.
        let vis_left = if overflowing {
            tabs_start + arrow_w
        } else {
            tabs_start
        };
        let vis_right = if overflowing {
            vp_w - right_reserve - arrow_w
        } else {
            vp_w - right_reserve
        };

        let close_w = cw * 2.0;
        let label_w = tab_w - close_w;
        let y0 = 0.0f32;
        let y1 = bar_h;

        for i in 0..tab_bar.tab_count {
            // Apply scroll offset to x position.
            let x0 = tabs_start + i as f32 * tab_w - scroll_offset;
            let x1 = x0 + tab_w;

            // CPU-side cull: skip tabs entirely outside the visible region.
            if x1 < vis_left || x0 > vis_right {
                continue;
            }

            let is_active = i == tab_bar.active_tab;
            let is_hovered = tab_bar.hovered_tab == Some(i);

            // Clamp rendering to [vis_left, vis_right] so tab backgrounds
            // don't bleed under the arrows or the "+" button.
            let draw_x0 = x0.max(vis_left);
            let draw_x1 = x1.min(vis_right);

            // Tab background: active tab gets the main bg; hovered tab gets
            // a subtle highlight (Warp-style hover feedback).
            if is_active {
                push_quad(
                    &mut vertices,
                    [draw_x0, y0, draw_x1, y1],
                    [0.0; 4],
                    [0.0; 4],
                    bg,
                );
                push_quad(
                    &mut vertices,
                    [draw_x0, y1 - 2.0, draw_x1, y1],
                    [0.0; 4],
                    [0.0; 4],
                    accent,
                );
            } else if is_hovered {
                // v1.2: hover highlight — a subtle light overlay on inactive
                // tabs when the mouse is over them (matches demo behavior).
                let hover_bg = [
                    fg[0] * 0.08 + bar_bg[0] * 0.92,
                    fg[1] * 0.08 + bar_bg[1] * 0.92,
                    fg[2] * 0.08 + bar_bg[2] * 0.92,
                    1.0,
                ];
                push_quad(
                    &mut vertices,
                    [draw_x0, y0, draw_x1, y1],
                    [0.0; 4],
                    [0.0; 4],
                    hover_bg,
                );
            }

            // Divider between tabs.
            if i > 0 && x0 >= vis_left {
                push_quad(
                    &mut vertices,
                    [draw_x0, y0, draw_x0 + 1.0, y1],
                    [0.0; 4],
                    [0.0; 4],
                    separator,
                );
            }

            // Tab label — skip if the text would start left of vis_left
            // (avoids overlapping the scroll arrow).
            let label_x = x0 + cw * 0.5;
            if label_x >= vis_left {
                let label = tab_bar.labels.get(i).map(|s| s.as_str()).unwrap_or("");
                let max_cols = (label_w / cw) as usize;
                let display = truncate_str(label, max_cols.saturating_sub(1));
                let label_color = if is_active {
                    fg
                } else if is_hovered {
                    // v1.2: brighter text on hover for better focus feedback.
                    [fg[0] * 0.85, fg[1] * 0.85, fg[2] * 0.85, 1.0]
                } else {
                    [fg[0] * 0.6, fg[1] * 0.6, fg[2] * 0.6, 1.0]
                };
                self.push_text(
                    &mut vertices,
                    label_x,
                    y0 + (bar_h - ch) * 0.5,
                    &display,
                    label_color,
                    max_cols,
                );
            }

            // Close button.
            let close_x0 = x0 + label_w;
            let close_x1 = x1;
            if (is_active || is_hovered) && close_x0 < vis_right {
                let close_color = if is_active {
                    fg
                } else {
                    [fg[0] * 0.5, fg[1] * 0.5, fg[2] * 0.5, 1.0]
                };
                let cx = close_x0 + close_w * 0.5;
                let cy = y0 + bar_h * 0.5;
                let r = if is_active { ch * 0.16 } else { ch * 0.13 };
                let line_w = 1.0 * self.scale as f32;
                push_line(
                    &mut vertices,
                    cx - r,
                    cy - r,
                    cx + r,
                    cy + r,
                    line_w,
                    close_color,
                );
                push_line(
                    &mut vertices,
                    cx - r,
                    cy + r,
                    cx + r,
                    cy - r,
                    line_w,
                    close_color,
                );
            }

            hits.push(TabHit {
                tab_rect: [x0, y0, x1, y1],
                close_rect: [close_x0, y0, close_x1, y1],
                index: i,
            });
        }

        // v1.2: Scroll arrows — drawn when tabs overflow.
        // Opaque background quads under the arrows prevent partially-visible
        // tabs from showing through behind the arrow icons.
        if overflowing {
            let max_scroll = {
                let vis_w = vis_right - vis_left;
                (total_tab_w - vis_w).max(0.0)
            };

            // ── Left arrow (‹) ──
            let la_cx = tabs_start + arrow_w * 0.5;
            let la_cy = bar_h * 0.5;
            let la_r = ch * 0.14;
            let la_w = 1.5 * self.scale as f32;
            // Background: bar_bg + hover highlight if the mouse is over it.
            let la_bg = if tab_bar.arrow_left_hovered && scroll_offset > 0.0 {
                [
                    fg[0] * 0.12 + bar_bg[0] * 0.88,
                    fg[1] * 0.12 + bar_bg[1] * 0.88,
                    fg[2] * 0.12 + bar_bg[2] * 0.88,
                    1.0,
                ]
            } else {
                bar_bg
            };
            push_quad(
                &mut vertices,
                [tabs_start, 0.0, tabs_start + arrow_w, bar_h],
                [0.0; 4],
                [0.0; 4],
                la_bg,
            );
            let la_color = if scroll_offset > 0.0 {
                if tab_bar.arrow_left_hovered {
                    fg
                } else {
                    [fg[0] * 0.7, fg[1] * 0.7, fg[2] * 0.7, 1.0]
                }
            } else {
                [fg[0] * 0.25, fg[1] * 0.25, fg[2] * 0.25, 1.0]
            };
            push_line(
                &mut vertices,
                la_cx + la_r,
                la_cy - la_r,
                la_cx - la_r,
                la_cy,
                la_w,
                la_color,
            );
            push_line(
                &mut vertices,
                la_cx - la_r,
                la_cy,
                la_cx + la_r,
                la_cy + la_r,
                la_w,
                la_color,
            );

            // ── Right arrow (›) ──
            let ra_cx = vis_right + arrow_w * 0.5;
            let ra_cy = bar_h * 0.5;
            let ra_r = ch * 0.14;
            let ra_w = 1.5 * self.scale as f32;
            let ra_bg = if tab_bar.arrow_right_hovered && scroll_offset < max_scroll {
                [
                    fg[0] * 0.12 + bar_bg[0] * 0.88,
                    fg[1] * 0.12 + bar_bg[1] * 0.88,
                    fg[2] * 0.12 + bar_bg[2] * 0.88,
                    1.0,
                ]
            } else {
                bar_bg
            };
            push_quad(
                &mut vertices,
                [vis_right, 0.0, vis_right + arrow_w, bar_h],
                [0.0; 4],
                [0.0; 4],
                ra_bg,
            );
            let ra_color = if scroll_offset < max_scroll {
                if tab_bar.arrow_right_hovered {
                    fg
                } else {
                    [fg[0] * 0.7, fg[1] * 0.7, fg[2] * 0.7, 1.0]
                }
            } else {
                [fg[0] * 0.25, fg[1] * 0.25, fg[2] * 0.25, 1.0]
            };
            push_line(
                &mut vertices,
                ra_cx - ra_r,
                ra_cy - ra_r,
                ra_cx + ra_r,
                ra_cy,
                ra_w,
                ra_color,
            );
            push_line(
                &mut vertices,
                ra_cx + ra_r,
                ra_cy,
                ra_cx - ra_r,
                ra_cy + ra_r,
                ra_w,
                ra_color,
            );

            // Register arrow hit rects — inserted at the FRONT of the hits
            // array so the click handler finds them before any tab hit whose
            // tab_rect might overlap the arrow region.
            hits.insert(
                0,
                TabHit {
                    tab_rect: [tabs_start, 0.0, tabs_start + arrow_w, bar_h],
                    close_rect: [0.0; 4],
                    index: usize::MAX, // left arrow sentinel
                },
            );
            hits.insert(
                1,
                TabHit {
                    tab_rect: [vis_right, 0.0, vis_right + arrow_w, bar_h],
                    close_rect: [0.0; 4],
                    index: usize::MAX - 1, // right arrow sentinel
                },
            );
        }

        // v1.2: "+" button position:
        //   - Non-overflowing: right after the last tab (natural flow).
        //   - Overflowing: after the right scroll arrow (fixed position).
        let plus_x0 = if overflowing {
            vis_right + arrow_w
        } else {
            tabs_start + tab_bar.tab_count as f32 * tab_w
        };
        let plus_cx = plus_x0 + plus_w * 0.5;
        let plus_cy = bar_h * 0.5;
        let plus_r = ch * 0.22;
        let plus_line_w = 1.5 * self.scale as f32;
        // v1.2: hover highlight.
        if tab_bar.plus_hovered {
            let hover_bg = [
                fg[0] * 0.12 + bar_bg[0] * 0.88,
                fg[1] * 0.12 + bar_bg[1] * 0.88,
                fg[2] * 0.12 + bar_bg[2] * 0.88,
                1.0,
            ];
            push_quad(
                &mut vertices,
                [plus_x0, y0, plus_x0 + plus_w, y1],
                [0.0; 4],
                [0.0; 4],
                hover_bg,
            );
        }
        let plus_color = if tab_bar.plus_hovered {
            fg
        } else {
            [fg[0] * 0.7, fg[1] * 0.7, fg[2] * 0.7, 1.0]
        };
        push_line(
            &mut vertices,
            plus_cx - plus_r,
            plus_cy,
            plus_cx + plus_r,
            plus_cy,
            plus_line_w,
            plus_color,
        );
        push_line(
            &mut vertices,
            plus_cx,
            plus_cy - plus_r,
            plus_cx,
            plus_cy + plus_r,
            plus_line_w,
            plus_color,
        );
        let new_tab_rect = [plus_x0, y0, plus_x0 + plus_w, y1];

        (vertices, hits, new_tab_rect)
    }
}

/// What the sidebar history panel should draw. Built by the app only when the
/// panel is open and passed to [`MetalRenderer::draw`].
pub struct PanelDrawParams<'a> {
    /// Finished blocks (oldest-first; the renderer shows newest first).
    pub blocks: &'a [Block],
    /// Panel width in physical pixels.
    pub width_px: f32,
    /// Live search filter (matches command or output, case-insensitive).
    pub query: &'a str,
    /// Index of the selected row within the newest-first filtered list.
    pub selection: usize,
    /// Id of the block whose output is expanded inline (None = all collapsed).
    pub expanded_id: Option<BlockId>,
    /// v0.9 fix: whether the search box has keyboard focus (draws accent
    /// underline so the user knows typing will go to the filter).
    pub search_focused: bool,
}

/// What the bottom editor input box should draw (v0.5 editor takeover). Built
/// by the app only in Editor mode and passed to [`MetalRenderer::draw`].
pub struct PromptDrawParams<'a> {
    /// Current working directory (from OSC 7) shown after the `❯` glyph.
    pub cwd: Option<&'a str>,
    /// Editor buffer lines (line 0 follows the prompt).
    pub lines: &'a [String],
    /// Cursor position (line index, char column).
    pub cursor: (usize, usize),
    /// Active IME preedit string, drawn right after the cursor.
    pub preedit: Option<&'a str>,
    /// `(query, selected_match)` when Ctrl+R search is active (replaces the
    /// normal prompt rendering).
    pub search: Option<(&'a str, Option<&'a str>)>,
    /// v0.9: active mouse-drag selection range `((start_line, start_col),
    /// (end_line, end_col))` in document order, or None when no selection.
    pub selection: Option<((usize, usize), (usize, usize))>,
}

/// Whether a block matches the panel search query (empty query = match all).
/// Shared by the renderer (list layout) and the app (selection clamping).
///
/// v0.9 fix: only match the command line, NOT the output. Matching output
/// caused false positives (e.g. searching "ls" matched `git pull` whose output
/// contained "ls"; searching "wha" matched `claude`/`git status` whose output
/// contained "wha"). Warp's history search only filters by command line.
///
/// v0.9 fix: match against the prompt-stripped command, not the raw grid
/// snapshot. `block.command` may include the full prompt line (e.g.
/// `user@host weft git:(main) % ls`) when the command was captured via
/// `snapshot_command_line` rather than the editor. Searching "git" would
/// match every command run inside a git repo. Stripping the prompt first
/// ensures only the actual command text is searched.
///
/// v0.9 fix (round 5): match only the command name (first token), via
/// case-insensitive **substring** (fuzzy) match.
///
/// Earlier iterations tried prefix/separator matching across all tokens, but
/// that conflated the command with its arguments. The cleanest semantic — and
/// the one matching Warp's history search — is: the query is a substring of
/// the command *name* (the first whitespace-separated token after stripping
/// the prompt).
///
/// Examples (query → command):
///   - "git"  → "git status"      ✅ (command name "git" contains "git")
///   - "git"  → "gitconfig"        ✅ (command name "gitconfig" contains "git")
///   - "git"  → "cd GitHub/"       ❌ (command name "cd" doesn't contain "git")
///   - "ls"   → "ls -al /test"     ✅ (command name "ls" contains "ls")
///   - "l"    → "ls -al /test"     ✅ (command name "ls" contains "l")
///   - "al"   → "ls -al /test"     ❌ ("al" is in the argument, not "ls")
///
/// Arguments are intentionally excluded: otherwise "git" would match
/// `cd GitHub/` (lowercased "github/" contains "git") — exactly the false
/// positive we're trying to avoid.
pub fn block_matches_query(block: &Block, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let q = query.to_lowercase();
    let cleaned = strip_prompt_prefix(&block.command);
    // Only the command name (first token) participates in matching.
    match cleaned.split_whitespace().next() {
        Some(cmd_name) => cmd_name.to_lowercase().contains(&q),
        None => false,
    }
}

/// Newest-first, query-filtered block list capped to `max` entries. Shared by
/// the warm-up pass and `build_panel_vertices` so they render the same set.
fn panel_display<'a>(blocks: &'a [Block], query: &str, max: usize) -> Vec<&'a Block> {
    blocks
        .iter()
        .rev()
        .filter(|b| block_matches_query(b, query))
        .take(max)
        .collect()
}

/// Append a two-triangle quad (6 vertices, stride-48 layout matching the grid
/// vertex descriptor) to `vertices`. `dst`/`uv` are `[x0, y0, x1, y1]`.
fn push_quad(vertices: &mut Vec<f32>, dst: [f32; 4], uv: [f32; 4], fg: [f32; 4], bg: [f32; 4]) {
    let [x0, y0, x1, y1] = dst;
    let [u0, v0, u1, v1] = uv;
    for (x, y, u, v) in [
        (x0, y0, u0, v0),
        (x0, y1, u0, v1),
        (x1, y1, u1, v1),
        (x0, y0, u0, v0),
        (x1, y1, u1, v1),
        (x1, y0, u1, v0),
    ] {
        vertices.extend_from_slice(&[
            x, y, u, v, fg[0], fg[1], fg[2], fg[3], bg[0], bg[1], bg[2], bg[3],
        ]);
    }
}

/// Draw a line segment as a thin rotated rectangle (two triangles).
/// Used for vector-drawn UI elements like the tab close button × icon.
fn push_line(
    vertices: &mut Vec<f32>,
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    width: f32,
    color: [f32; 4],
) {
    let dx = x2 - x1;
    let dy = y2 - y1;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 0.5 {
        return;
    }
    // Perpendicular unit vector × half-width
    let hw = width * 0.5;
    let px = -dy / len * hw;
    let py = dx / len * hw;
    // Four corners of the rotated rectangle
    let (ax, ay) = (x1 + px, y1 + py);
    let (bx, by) = (x1 - px, y1 - py);
    let (cx, cy) = (x2 + px, y2 + py);
    let (dx, dy) = (x2 - px, y2 - py);
    // UV=[0;4] and fg=[0;4] → mask=0 → only bg (color) shows
    let uv = [0.0f32; 4];
    let fg = [0.0f32; 4];
    for (x, y) in [(ax, ay), (bx, by), (cx, cy), (bx, by), (dx, dy), (cx, cy)] {
        vertices.extend_from_slice(&[
            x, y, uv[0], uv[1], fg[0], fg[1], fg[2], fg[3], color[0], color[1], color[2], color[3],
        ]);
    }
}

/// v1.0 P1.5-B1: Push a single grid-cell instance (16 floats = 64 bytes).
/// Layout matches the Metal `CellInstance` struct: origin(2) + size(2) +
/// uv_rect(4) + fg(4) + bg(4). The instance is rendered against a static
/// 4-vertex unit quad indexed as [0,1,2,0,2,3] — the shader maps
/// vertex_id 0..3 to corners (0,0), (0,1), (1,1), (1,0) and scales by
/// `size`/offsets by `origin`. Compared to `push_quad` (6 verts × 12 floats
/// = 72 floats = 288 B per cell), this emits 16 floats = 64 B per cell, a
/// 4.5x reduction in per-frame upload size.
fn push_cell_instance(
    instances: &mut Vec<f32>,
    dst: [f32; 4],
    uv: [f32; 4],
    fg: [f32; 4],
    bg: [f32; 4],
) {
    let [x0, y0, x1, y1] = dst;
    let [u0, v0, u1, v1] = uv;
    instances.extend_from_slice(&[
        x0,
        y0,
        x1 - x0,
        y1 - y0,
        u0,
        v0,
        u1,
        v1,
        fg[0],
        fg[1],
        fg[2],
        fg[3],
        bg[0],
        bg[1],
        bg[2],
        bg[3],
    ]);
}

/// Push a filled triangle (3 vertices) into the vertex buffer. Uses the
/// same vertex layout as `push_quad` (12 floats each).
fn push_triangle(
    vertices: &mut Vec<f32>,
    p0: [f32; 2],
    p1: [f32; 2],
    p2: [f32; 2],
    color: [f32; 4],
    bg_uv: [f32; 4],
) {
    let [u0, v0, u1, v1] = bg_uv;
    let mid_u = (u0 + u1) * 0.5;
    let mid_v = (v0 + v1) * 0.5;
    for [x, y] in [p0, p1, p2] {
        vertices.extend_from_slice(&[
            x, y, mid_u, mid_v, color[0], color[1], color[2], color[3], color[0], color[1],
            color[2], color[3],
        ]);
    }
}

/// How many block rows fit below the title (in whole grid rows).
pub(crate) fn visible_panel_rows(viewport_h: f32, cell_h: u32) -> usize {
    if cell_h == 0 {
        return 0;
    }
    ((viewport_h / cell_h as f32) as usize).saturating_sub(2)
}

/// Human-readable elapsed time for a finished block.
/// Abbreviate an absolute path for display: replace a `$HOME` prefix with `~`
/// (e.g. `/Users/andylee/proj` → `~/proj`). Falls back to the raw path when
/// `$HOME` is unset or isn't a prefix.
fn abbreviate_path(path: &str) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        if let Some(h) = home.to_str() {
            if !h.is_empty() && path.starts_with(h) {
                return format!("~{}", &path[h.len()..]);
            }
        }
    }
    path.to_string()
}

fn block_duration_str(b: &Block) -> String {
    let Some(finished) = b.finished_at else {
        return String::new();
    };
    let ms = finished
        .duration_since(b.started_at)
        .unwrap_or_default()
        .as_millis();
    if ms < 1000 {
        format!("{}ms", ms)
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m", ms / 60_000)
    }
}

/// Truncate `s` to `max` chars, appending an ellipsis if it was cut.
fn truncate_str(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
    t.push('…');
    t
}

/// v0.9 fix: strip a shell prompt prefix from a captured command line.
///
/// `snapshot_command_line` grabs the whole prompt row when the command wasn't
/// submitted via the editor (e.g. before shell integration is ready, or loaded
/// from an old DB). This produces strings like `andylee@AndyHQ weft % echo hi`
/// instead of just `echo hi`. We only strip when the command contains `@`
/// (a `user@host` prompt signature) — this avoids false positives on commands
/// like `echo 50% done` or `echo $HOME`. When the `@` is found, we take the
/// last `% `/`$ `/`# ` occurrence as the prompt→command boundary.
fn strip_prompt_prefix(command: &str) -> String {
    let trimmed = command.trim_start();
    // Only attempt stripping when a user@host prompt signature is present.
    if !trimmed.contains('@') {
        return trimmed.to_string();
    }
    let prompts = ["% ", "$ ", "# "];
    let mut best: Option<usize> = None;
    for p in &prompts {
        let mut start = 0;
        while let Some(idx) = trimmed[start..].find(p) {
            let abs = start + idx;
            best = Some(abs + p.len());
            start = abs + p.len();
        }
    }
    match best {
        Some(idx) => trimmed[idx..].trim().to_string(),
        None => trimmed.to_string(),
    }
}

/// Convert a grid Color to normalized RGBA floats.
fn color_to_normalized(color: Color) -> [f32; 4] {
    [
        color.r as f32 / 255.0,
        color.g as f32 / 255.0,
        color.b as f32 / 255.0,
        color.a as f32 / 255.0,
    ]
}

/// Resolve a cell's color-origin against the current palette / theme default.
/// `Default` → theme default; `Palette(i)` → palette slot; `Rgb` → as-is.
/// Because this runs per-frame, changing the palette (theme switch or OSC)
/// recolors the whole screen on the next draw without rewriting cells.
fn resolve_cell_color(cc: CellColor, default: [f32; 4], palette: &[Color; 256]) -> [f32; 4] {
    match cc {
        CellColor::Default => default,
        CellColor::Palette(i) => color_to_normalized(palette[i as usize]),
        CellColor::Rgb(c) => color_to_normalized(c),
    }
}

/// Attach a Metal layer to the winit window's NSView.
unsafe fn attach_layer_to_nsview(layer: &MetalLayer, window: &Window, scale: f64) {
    let handle = window.window_handle().expect("Failed to get window handle");
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        panic!("Weft requires macOS (AppKit)");
    };

    let ns_view: *mut objc2::runtime::AnyObject = appkit.ns_view.as_ptr().cast();
    let layer_ptr: *mut objc2::runtime::AnyObject =
        (&**layer) as *const _ as *mut objc2::runtime::AnyObject;

    let _: () = msg_send![ns_view, setWantsLayer: true];
    let _: () = msg_send![ns_view, setLayer: layer_ptr];
    // Retina: the backing store (drawable) is physical pixels; tell the layer its
    // contents are at the window scale so it isn't displayed at the wrong density.
    let _: () = msg_send![layer_ptr, setContentsScale: scale];
    // Metal renders with a top-left origin (framebuffer row 0 = top). The vertex
    // shader already maps logical-top → clip-top, so the drawable is upright; do NOT
    // set geometryFlipped (it would composite the framebuffer upside-down).
    let _: () = msg_send![layer_ptr, setGeometryFlipped: false];
}

/// v1.1: Configure a Warp-style transparent titlebar on the native NSWindow.
///
/// Sets `NSWindowStyleMaskFullSizeContentView` (Metal layer extends under the
/// titlebar), `titlebarAppearsTransparent` (no system titlebar chrome), and
/// `titleVisibility:hidden` (no title text). `movableByWindowBackground` lets
/// the user drag the window by any non-interactive background area (the tab
/// bar's empty regions), matching Warp. The traffic-light buttons stay native
/// and float over the Metal content at the top-left.
///
/// Uses the typed `objc2-app-kit` `NSWindow` methods (safe functions) rather
/// than raw `msg_send!` to avoid the nounwind-abort panic that disabled
/// `set_dock_icon`.
pub fn configure_titlebar(window: &Window) {
    use objc2::rc::Retained;
    use objc2_app_kit::{NSView, NSWindowStyleMask, NSWindowTitleVisibility};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    // Wrap in catch_unwind as a belt-and-suspenders guard against any ObjC
    // runtime assertion (matching the set_dock_icon defensive pattern), even
    // though these typed setters are nominally safe.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let raw = match window.window_handle() {
            Ok(h) => h.as_raw(),
            Err(_) => return,
        };
        let RawWindowHandle::AppKit(appkit) = raw else {
            return; // Not macOS — nothing to configure.
        };
        // Retain the NSView from the raw handle, then reach its NSWindow.
        let ns_view: Retained<NSView> = match Retained::retain(appkit.ns_view.as_ptr().cast()) {
            Some(v) => v,
            None => return,
        };
        let ns_window = match ns_window_of(&ns_view) {
            Some(w) => w,
            None => return,
        };
        // Add FullSizeContentView (1 << 15) to the existing style mask without
        // dropping Titled/Closable/etc. (those keep the traffic lights).
        let mask = ns_window.styleMask();
        ns_window.setStyleMask(mask | NSWindowStyleMask::FullSizeContentView);
        ns_window.setTitlebarAppearsTransparent(true);
        ns_window.setTitleVisibility(NSWindowTitleVisibility::NSWindowTitleHidden);
        ns_window.setMovableByWindowBackground(true);
    }));
}

/// Helper: get the NSWindow owning an NSView (`[view window]`), retained.
unsafe fn ns_window_of(
    view: &objc2_app_kit::NSView,
) -> Option<objc2::rc::Retained<objc2_app_kit::NSWindow>> {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    let ptr: *mut AnyObject = msg_send![view, window];
    if ptr.is_null() {
        None
    } else {
        // Retain via the NSWindow type so the returned Retained<NSWindow> is
        // properly managed. `[view window]` returns an unretained reference.
        Retained::retain(ptr.cast())
    }
}

/// Toggle the CAMetalLayer's `opaque` flag. A non-opaque layer lets a
/// transparent NSWindow show the desktop through alpha-scaled cell backgrounds.
///
/// # Safety
/// `layer` must be a live `CAMetalLayer` (or subclass). `setOpaque:` is the
/// `CALayer` property setter, so the selector is valid.
unsafe fn set_layer_opaque(layer: &MetalLayer, opaque: bool) {
    let layer_ptr: *mut objc2::runtime::AnyObject =
        (&**layer) as *const _ as *mut objc2::runtime::AnyObject;
    let _: () = msg_send![layer_ptr, setOpaque: opaque];
}

/// Theme-driven syntax-highlight color (v0.8 — replaces the hardcoded
/// v0.5 palette). Resolves a `TokenKind` against `theme.syntax`.
fn syntax_color(kind: TokenKind, theme: &weft_core::config::Theme) -> [f32; 4] {
    let s = &theme.syntax;
    match kind {
        TokenKind::Command => color_to_normalized(s.command),
        TokenKind::Flag => color_to_normalized(s.flag),
        TokenKind::Path => color_to_normalized(s.path),
        TokenKind::String => color_to_normalized(s.string),
        TokenKind::Number => color_to_normalized(s.number),
        TokenKind::Variable => color_to_normalized(s.variable),
        TokenKind::Operator => color_to_normalized(s.operator),
        TokenKind::Comment => color_to_normalized(s.comment),
        TokenKind::Whitespace | TokenKind::Default => color_to_normalized(s.default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syntax_color_distinct_and_default_fallback() {
        // v0.8: syntax colors are now theme-driven. Verify the warm theme
        // produces distinct colors for every token kind, and that
        // Default/Whitespace resolve to theme.syntax.default.
        let theme = weft_core::config::Theme::weft_warm();
        let kinds = [
            TokenKind::Command,
            TokenKind::Flag,
            TokenKind::Path,
            TokenKind::String,
            TokenKind::Number,
            TokenKind::Variable,
            TokenKind::Operator,
            TokenKind::Comment,
        ];
        let colors: Vec<[f32; 4]> = kinds.iter().map(|&k| syntax_color(k, &theme)).collect();
        // All 8 should be distinct (no two token kinds share a color).
        for i in 0..colors.len() {
            for j in (i + 1)..colors.len() {
                assert_ne!(colors[i], colors[j], "syntax colors at {i}/{j} collide");
            }
        }
        // Default/Whitespace resolve to theme.syntax.default.
        let default_c = color_to_normalized(theme.syntax.default);
        assert_eq!(syntax_color(TokenKind::Default, &theme), default_c);
        assert_eq!(syntax_color(TokenKind::Whitespace, &theme), default_c);
    }

    #[test]
    fn strip_prompt_prefix_zsh() {
        assert_eq!(
            strip_prompt_prefix("andylee@AndyHQ weft % echo first"),
            "echo first"
        );
    }

    #[test]
    fn strip_prompt_prefix_bash() {
        assert_eq!(
            strip_prompt_prefix("andylee@host:~/proj $ echo hi"),
            "echo hi"
        );
    }

    #[test]
    fn strip_prompt_prefix_root() {
        assert_eq!(strip_prompt_prefix("root@host:~# ls -la"), "ls -la");
    }

    #[test]
    fn strip_prompt_prefix_already_clean() {
        // Clean commands (no prompt) pass through unchanged.
        assert_eq!(strip_prompt_prefix("echo first"), "echo first");
        assert_eq!(strip_prompt_prefix("ls -la /tmp"), "ls -la /tmp");
    }

    #[test]
    fn strip_prompt_prefix_no_at_sign_passes_through() {
        // Commands without @ (no user@host prompt) pass through unchanged,
        // even if they contain % or $ characters.
        assert_eq!(strip_prompt_prefix("echo 50% done"), "echo 50% done");
        assert_eq!(strip_prompt_prefix("# comment line"), "# comment line");
    }

    // ── block_matches_query (v0.9 round 5: command-name substring match) ──
    fn mk_block(cmd: &str) -> Block {
        Block {
            id: BlockId(1),
            command: cmd.to_string(),
            cwd: None,
            output: String::new(),
            exit_code: None,
            started_at: std::time::SystemTime::UNIX_EPOCH,
            finished_at: None,
            collapsed: false,
        }
    }

    #[test]
    fn panel_search_git_matches_git_commands() {
        // "git" matches the command name "git" (exact or as substring).
        assert!(block_matches_query(&mk_block("git status"), "git"));
        assert!(block_matches_query(
            &mk_block("git push origin main"),
            "git"
        ));
        assert!(block_matches_query(&mk_block("git"), "git"));
        // Case-insensitive.
        assert!(block_matches_query(&mk_block("GIT STATUS"), "git"));
        assert!(block_matches_query(&mk_block("Git Status"), "git"));
    }

    #[test]
    fn panel_search_git_matches_fuzzy_substring() {
        // v0.9 round 5: fuzzy substring match on the command name.
        // "git" matches "gitconfig" because the command name contains "git".
        assert!(block_matches_query(&mk_block("gitconfig"), "git"));
        assert!(block_matches_query(
            &mk_block("gitconfig --global user.name"),
            "git"
        ));
        // Partial substring: "gi" matches "git status".
        assert!(block_matches_query(&mk_block("git status"), "gi"));
        // "it" matches "git status" (substring of command name "git").
        assert!(block_matches_query(&mk_block("git status"), "it"));
    }

    #[test]
    fn panel_search_git_rejects_argument_only_match() {
        // v0.9 round 5: arguments don't participate in matching.
        // "git" must NOT match "cd GitHub/" — command name is "cd", which
        // doesn't contain "git"; even though the argument "GitHub/" contains
        // "git" after lowercasing, arguments are excluded from matching.
        assert!(!block_matches_query(&mk_block("cd GitHub/"), "git"));
        assert!(!block_matches_query(&mk_block("cd github/"), "git"));
        // "git" is not in the command name "cd".
        assert!(!block_matches_query(&mk_block("cd github"), "git"));
    }

    #[test]
    fn panel_search_ls_rejects_argument_substring() {
        // v0.9 round 5: "al" is in the argument "-al", not in the command
        // name "ls" — must NOT match.
        assert!(!block_matches_query(&mk_block("ls -al /test"), "al"));
        // But "ls" matches the command name, and "l" matches as a substring.
        assert!(block_matches_query(&mk_block("ls -al /test"), "ls"));
        assert!(block_matches_query(&mk_block("ls -al /test"), "l"));
        assert!(block_matches_query(&mk_block("ls"), "ls"));
    }

    #[test]
    fn panel_search_skills_rejects_when_command_is_cd() {
        // Regression: "ls" must NOT match "cd andrej-karpathy-skills" —
        // command name is "cd", arguments are excluded from matching.
        assert!(!block_matches_query(
            &mk_block("cd andrej-karpathy-skills"),
            "ls"
        ));
    }

    #[test]
    fn panel_search_cd_matches_cd_command() {
        // "cd" matches the command name "cd" even when the argument contains
        // a coincidental substring.
        assert!(block_matches_query(&mk_block("cd GitHub/"), "cd"));
        assert!(block_matches_query(&mk_block("cd .."), "cd"));
    }

    #[test]
    fn panel_search_empty_query_matches_all() {
        assert!(block_matches_query(&mk_block("git status"), ""));
        assert!(block_matches_query(&mk_block("ls -l"), ""));
    }

    #[test]
    fn panel_search_strips_prompt_prefix() {
        // The query runs against the prompt-stripped command, so a user@host
        // prompt prefix doesn't leak into matching.
        let b = mk_block("andylee@AndyHQ weft % git status");
        assert!(block_matches_query(&b, "git"));
        // "weft" is part of the prompt (cwd), not the command — must not match
        // the stripped command "git status".
        assert!(!block_matches_query(&b, "weft"));
    }

    // ── v1.0 P0-a: BlockLayoutCache tests ──────────────────────────────

    fn mk_block_with_output(id: u64, command: &str, output: &str) -> Block {
        Block {
            id: BlockId(id),
            command: command.to_string(),
            cwd: None,
            output: output.to_string(),
            exit_code: None,
            started_at: std::time::SystemTime::UNIX_EPOCH,
            finished_at: None,
            collapsed: false,
        }
    }

    #[test]
    fn block_layout_cache_computes_on_first_access() {
        let block = mk_block_with_output(1, "echo hello", "hello\nworld\n");
        let layout = compute_block_layout(&block, 80);
        assert_eq!(layout.lines.len(), 2);
        assert_eq!(layout.lines[0].idx, 0);
        assert_eq!(layout.lines[1].idx, 1);
        assert!(layout.foldable);
    }

    #[test]
    fn block_layout_cache_trims_trailing_empty() {
        let block = mk_block_with_output(1, "echo", "output\n\n\n");
        let layout = compute_block_layout(&block, 80);
        assert_eq!(
            layout.lines.len(),
            1,
            "trailing empty lines should be trimmed"
        );
        assert_eq!(
            &block.output[layout.lines[0].byte_start..layout.lines[0].byte_end],
            "output"
        );
    }

    #[test]
    fn block_layout_cache_trims_trailing_prompt() {
        let block = mk_block_with_output(1, "echo", "output\n%\n$\n#\n");
        let layout = compute_block_layout(&block, 80);
        assert_eq!(
            layout.lines.len(),
            1,
            "trailing prompt lines should be trimmed"
        );
    }

    #[test]
    fn block_layout_cache_byte_offsets_correct() {
        let block = mk_block_with_output(1, "echo", "first\nsecond\nthird\n");
        let layout = compute_block_layout(&block, 80);
        assert_eq!(layout.lines.len(), 3);
        assert_eq!(
            &block.output[layout.lines[0].byte_start..layout.lines[0].byte_end],
            "first"
        );
        assert_eq!(
            &block.output[layout.lines[1].byte_start..layout.lines[1].byte_end],
            "second"
        );
        assert_eq!(
            &block.output[layout.lines[2].byte_start..layout.lines[2].byte_end],
            "third"
        );
    }

    #[test]
    fn block_layout_cache_wraps_long_lines() {
        // 20 chars at cols=10 → 2 chunks
        let block = mk_block_with_output(1, "echo", "0123456789abcdefghij");
        let layout = compute_block_layout(&block, 10);
        assert_eq!(layout.lines.len(), 1);
        assert_eq!(
            layout.lines[0].chunks.len(),
            2,
            "20 chars at cols=10 → 2 chunks"
        );
        assert_eq!(layout.lines[0].chunks[0], "0123456789");
        assert_eq!(layout.lines[0].chunks[1], "abcdefghij");
    }

    #[test]
    fn block_layout_cache_foldable_false_for_empty_output() {
        let block = mk_block_with_output(1, "true", "\n\n\n");
        let layout = compute_block_layout(&block, 80);
        assert!(!layout.foldable, "all-empty output should not be foldable");
        assert_eq!(layout.lines.len(), 0, "all lines trimmed");
    }

    #[test]
    fn block_layout_cache_ensure_cached_reuses() {
        let mut cache = BlockLayoutCache::default();
        let block = mk_block_with_output(1, "echo", "hello\n");
        cache.ensure_cached(&block, 80);
        let layout1 = cache.get(1).clone();

        // Same content + cols → should NOT rebuild (same instance).
        cache.ensure_cached(&block, 80);
        let layout2 = cache.get(1).clone();
        assert_eq!(layout1.lines.len(), layout2.lines.len());
        assert_eq!(layout1.cols, layout2.cols);
    }

    #[test]
    fn block_layout_cache_rebuilds_on_output_change() {
        let mut cache = BlockLayoutCache::default();
        let block = mk_block_with_output(1, "echo", "hello\n");
        cache.ensure_cached(&block, 80);
        assert_eq!(cache.get(1).lines.len(), 1);

        // Output grew → cache should detect and rebuild.
        let block2 = mk_block_with_output(1, "echo", "hello\nworld\n");
        cache.ensure_cached(&block2, 80);
        assert_eq!(
            cache.get(1).lines.len(),
            2,
            "output change should trigger rebuild"
        );
    }

    #[test]
    fn block_layout_cache_rebuilds_on_cols_change() {
        let mut cache = BlockLayoutCache::default();
        let block = mk_block_with_output(1, "echo", "0123456789abcdefghij");
        cache.ensure_cached(&block, 10);
        assert_eq!(
            cache.get(1).lines[0].chunks.len(),
            2,
            "20 chars / cols=10 → 2 chunks"
        );

        // Resize to cols=20 → should rebuild with 1 chunk.
        cache.ensure_cached(&block, 20);
        assert_eq!(
            cache.get(1).lines[0].chunks.len(),
            1,
            "20 chars / cols=20 → 1 chunk"
        );
    }

    #[test]
    fn block_layout_cache_rebuilds_on_collapse_toggle() {
        let mut cache = BlockLayoutCache::default();
        let block = mk_block_with_output(1, "echo", "hello\n");
        cache.ensure_cached(&block, 80);
        assert!(!cache.get(1).collapsed);

        let mut block2 = block.clone();
        block2.collapsed = true;
        cache.ensure_cached(&block2, 80);
        assert!(
            cache.get(1).collapsed,
            "collapse toggle should trigger rebuild"
        );
    }

    #[test]
    fn block_layout_cache_empty_output() {
        let block = mk_block_with_output(1, "true", "");
        let layout = compute_block_layout(&block, 80);
        assert_eq!(layout.lines.len(), 0);
        assert!(!layout.foldable);
    }
}
