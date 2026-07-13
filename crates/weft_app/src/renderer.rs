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
// A5: vertex primitives + color helpers live in paint::primitives.
use crate::paint::primitives::{
    color_to_normalized, push_cell_instance, push_quad, resolve_cell_color,
};
use weft_core::blocks::{Block, BlockId};
use weft_core::config::{FontConfig, Theme};
use weft_core::grid::{CellFlags, CellWidth, Color, CursorStyle};
use weft_core::selection::SelectionHandler;
use weft_core::vt::Terminal;

/// Iterator yielding wrapped row chunks of `text` at `cols` columns. Each
/// yielded `String` fits within `cols` columns (respecting wide-char widths).
/// The first yielded chunk is the top row, subsequent chunks are continuation
/// rows below it.
pub(crate) fn wrap_line_chunks(text: &str, cols: usize) -> impl Iterator<Item = String> {
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
pub(crate) struct CachedLine {
    /// 0-based line index within the block's output (before trimming).
    pub(crate) idx: usize,
    /// Byte offset of this line's start within `block.output`.
    pub(crate) byte_start: usize,
    /// Byte offset of this line's end (exclusive) within `block.output`.
    pub(crate) byte_end: usize,
    /// Pre-wrapped chunks (owned via `Rc` for cheap sharing between the
    /// cache and the per-frame `LaidRow` entries). Usually 1 element;
    /// more for lines that exceed `cols` columns.
    pub(crate) chunks: Rc<[String]>,
}

/// Cached layout for a single finished block.
#[derive(Clone)]
pub(crate) struct CachedBlockLayout {
    /// Snapshot of `block.output.len()` — if the current block's output
    /// length differs, the cache is stale.
    pub(crate) output_len: usize,
    /// Snapshot of `block.command.len()`.
    pub(crate) command_len: usize,
    /// Snapshot of `block.collapsed` — toggling invalidates.
    pub(crate) collapsed: bool,
    /// `cols` used to compute wrapping — resize invalidates.
    pub(crate) cols: usize,
    /// Whether the block has any non-empty output lines (cached foldable
    /// check, avoids re-scanning the last 500 lines every frame).
    pub(crate) foldable: bool,
    /// Pre-trimmed, pre-wrapped line metadata. Trailing empty/prompt lines
    /// are already removed, matching the original trimming logic.
    pub(crate) lines: Vec<CachedLine>,
}

/// Per-renderer block layout cache. Keyed by `BlockId.0`.
#[derive(Default)]
pub(crate) struct BlockLayoutCache {
    entries: HashMap<u64, CachedBlockLayout>,
}

impl BlockLayoutCache {
    /// Ensure `block` has a cached layout for `cols`. Recomputes only if
    /// the block is new, its output/command changed, `collapsed` was
    /// toggled, or `cols` changed (resize).
    pub(crate) fn ensure_cached(&mut self, block: &Block, cols: usize) {
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

    pub(crate) fn get(&self, id: u64) -> &CachedBlockLayout {
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
    pub(crate) atlas: GlyphAtlas,
    pub(crate) viewport: (f32, f32),
    /// Retained for live font/atlas rebuild (config reload).
    pub(crate) scale: f64,
    /// Retained for live font/atlas rebuild (config reload).
    font_config: FontConfig,
    /// Active theme (default fg/bg/cursor/selection). Per-frame, so a
    /// `set_theme` call recolors the screen on the next draw.
    pub(crate) theme: Theme,
    /// Content padding in **physical** pixels (logical config value × scale).
    /// Cells are positioned `pad_x + col·cw`, `pad_y + row·ch`; the usable
    /// area for row/col math is the viewport minus `2·pad`.
    pub(crate) padding_x: f32,
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
    pub(crate) popup_width_scale: f32,
    /// User-adjustable popup max visible rows.
    pub(crate) popup_max_rows: usize,
    /// Context menu position + target block (F7). Set per-frame by the app.
    pub context_menu_target: Option<(f32, f32, Option<weft_core::blocks::BlockId>)>,
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
            layout_ctx: None,
            find_state: None,
            panel_highlight: None,
            cursor_blink_on: true,
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

    /// Refresh every DPI-derived renderer value after a window moves between
    /// displays. Winit reports physical window sizes, while font size and
    /// configured padding are logical, so keeping an old scale would make the
    /// PTY geometry and Metal placement diverge again on the new display.
    pub fn update_scale(&mut self, scale: f64, padding_logical: (u32, u32)) -> bool {
        if !scale.is_finite() || scale <= 0.0 || (self.scale - scale).abs() < f64::EPSILON {
            return false;
        }

        self.scale = scale;
        self.padding_x = padding_logical.0 as f32 * scale as f32;
        self.padding_y = padding_logical.1 as f32 * scale as f32;
        self.atlas = GlyphAtlas::new(&self.device, &self.font_config, scale);

        // CAMetalLayer contentsScale is not exposed by metal-rs. Keep the raw
        // Objective-C message contained and unwind-protected per project rule.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            use objc2::msg_send;
            let layer_ptr: *mut objc2::runtime::AnyObject =
                (&*self.layer) as *const _ as *mut objc2::runtime::AnyObject;
            let _: () = msg_send![layer_ptr, setContentsScale: scale];
        }));

        self.force_full_grid_redraw();
        info!(
            scale,
            cell_width = self.atlas.cell_width,
            cell_height = self.atlas.cell_height,
            "renderer scale factor updated"
        );
        true
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
        let metrics = crate::ui_tokens::UiMetrics::for_scale(self.scale);
        (logical.clamp(28.0, 40.0) * self.scale as f32).max(metrics.control_compact)
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
        let logical_viewport = self.viewport.0 / self.scale as f32;
        crate::ui_tokens::SidebarMetrics::for_logical_width(logical_viewport).panel_width
            * self.scale as f32
    }

    /// Width reserved beside the terminal. In compact windows the history
    /// panel becomes an overlay drawer, so it keeps its visual width without
    /// shrinking the PTY or pushing tab controls outside the viewport.
    pub fn sidebar_push_width(&self) -> f32 {
        let logical_viewport = self.viewport.0 / self.scale as f32;
        crate::ui_tokens::SidebarMetrics::for_logical_width(logical_viewport).push_width
            * self.scale as f32
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
        scrollbar_emphasized: bool,
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
            self.sidebar_push_width()
        } else {
            0.0
        };

        let terminal_layout = crate::terminal_geometry::terminal_layout_for_renderer(
            self,
            winit::dpi::PhysicalSize::new(
                self.viewport.0.round().max(0.0) as u32,
                self.viewport.1.round().max(0.0) as u32,
            ),
            chrome_left as f64,
        );
        let chrome_top = terminal_layout.chrome_top as f32;

        // Build this frame's LayoutCtx: the single source of truth for
        // coordinate math in every overlay builder (v0.8 stage 1). Stored on
        // self so methods that don't receive it directly can still access it
        // during this draw; rebuilt every frame so resizes/padding changes
        // take effect immediately.
        let mut ctx = crate::layout::LayoutCtx::new(
            (
                terminal_layout.viewport.right as f32,
                terminal_layout.viewport.bottom as f32,
            ),
            terminal_layout.cell_width as f32,
            terminal_layout.cell_height as f32,
            terminal_layout.padding_x as f32,
            terminal_layout.padding_y as f32,
        );
        ctx.chrome_top = chrome_top;
        ctx.chrome_left = terminal_layout.chrome_left as f32;
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
        // Reset popup rects — settings still uses renderer-owned hit data.
        // v1.0 P1.5-B1: Grid cells render as instances (instanced pipeline);
        // overlays + block view render as legacy vertices. In grid view,
        // `instances` carries the cells and `vertices` carries only overlays;
        // in block view, `instances` is empty and `vertices` carries everything.
        let mut instances: Vec<f32> = Vec::new();
        let mut vertices: Vec<f32> = if show_blocks {
            let (v, regions, _) = if let Some(p) = prompt {
                let box_h = ch * (p.lines.len().max(1) as f32 + 2.0);
                let box_top_y = (vp_h - pad_y - box_h).max(0.0);
                self.build_block_view_vertices(
                    crate::paint::block_view_model::BlockViewPaintModel {
                        blocks: terminal.block_tracker().session_blocks(),
                        region_bottom_y: box_top_y,
                        cwd: p.cwd,
                        git_branch: terminal.git_branch(),
                        live: None,
                        block_scroll,
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
                        region_bottom_y: vp_h - pad_y,
                        cwd: None,
                        git_branch: terminal.git_branch(),
                        live: terminal.block_tracker().in_flight(),
                        block_scroll,
                    },
                    selection,
                )
            };
            pending_hit_regions = regions;
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
            if terminal.is_alt_screen_active() {
                self.force_full_grid_redraw();
            }
            let cursor_visible_this_frame = crate::terminal_geometry::grid_cursor_visible(
                terminal.cursor_style,
                terminal.cursor_visible,
                cursor_blink_on,
                prompt.is_some(),
            );
            instances = self.build_grid_instances(
                grid,
                terminal.palette(),
                cursor,
                selection,
                cursor_visible_this_frame,
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
                if let Some(scrollbar) = crate::scrollbar_component::scrollbar_layout(
                    &ctx,
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

        // Command Palette overlay (v0.7) — centered floating window.
        if let Some(p) = palette {
            vertices.extend_from_slice(&self.build_palette_vertices(p));
        }

        // v1.0 S1: Settings panel (Cmd+,) — centered modal overlay.
        if let Some(s) = settings {
            vertices.extend_from_slice(&self.build_settings_vertices(*s));
        }

        // Context menu overlay (F7) — drawn at mouse position.
        if let Some((x, y, _block_id)) = &self.context_menu_target {
            vertices.extend_from_slice(&self.build_context_menu_vertices(*x, *y));
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
                    if cursor_style.is_block() {
                        [0.0, 0.0, 0.0, 1.0] // Black text on cursor block
                    } else {
                        cursor_color
                    }
                } else {
                    fg
                };

                let final_bg = if is_cursor && cursor_style.is_block() {
                    cursor_color
                } else if is_selected {
                    selection_bg
                } else if is_cursor && cursor_style.is_bar() {
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
                    if cursor_style.is_bar() {
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
                    } else if cursor_style.is_underline() {
                        let line_h = 2.0;
                        push_cell_instance(
                            &mut instances,
                            [x0, y1 - line_h, x1, y1],
                            [0.0, 0.0, 0.0, 1.0],
                            [0.0; 4],
                            cursor_color,
                        );
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

    /// Push a selection-highlight background quad for a character range of
    /// `text`, honoring CJK double-width so the highlight exactly covers the
    /// selected glyphs. Called before `push_text` so the text renders on top
    /// of the highlight (matching grid-view selection rendering).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn push_block_view_highlight(
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
pub(crate) fn panel_display<'a>(blocks: &'a [Block], query: &str, max: usize) -> Vec<&'a Block> {
    blocks
        .iter()
        .rev()
        .filter(|b| block_matches_query(b, query))
        .take(max)
        .collect()
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
pub(crate) fn abbreviate_path(path: &str) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        if let Some(h) = home.to_str() {
            if !h.is_empty() && path.starts_with(h) {
                return format!("~{}", &path[h.len()..]);
            }
        }
    }
    path.to_string()
}

pub(crate) fn block_duration_str(b: &Block) -> String {
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
pub(crate) fn truncate_str(s: &str, max: usize) -> String {
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
pub(crate) fn strip_prompt_prefix(command: &str) -> String {
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
/// `titleVisibility:hidden` (no title text). Window dragging is intentionally
/// *not* enabled for the full Metal background: doing so lets AppKit steal
/// scrollbar and terminal drags. The tab-bar controller explicitly calls
/// winit's native `drag_window()` only for empty titlebar regions.
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
        ns_window.setMovableByWindowBackground(false);
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

#[cfg(test)]
mod tests {
    use super::*;

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
