//! Metal GPU renderer for the terminal Grid.

use core_graphics_types::geometry::CGSize;
use metal::{
    CompileOptions, Device, MTLClearColor, MTLIndexType, MTLLoadAction, MTLPixelFormat,
    MTLPrimitiveType, MTLResourceOptions, MTLStoreAction, MTLVertexFormat, MetalLayer,
    RenderPassDescriptor, RenderPipelineDescriptor, SamplerDescriptor, VertexDescriptor,
};
use objc2::msg_send;
use std::cell::{Cell, RefCell};
use tracing::info;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

use crate::glyph::GlyphAtlas;
// A5: vertex primitives + color helpers live in paint::primitives.
use crate::paint::grid_cache::BlockLayoutCache;
use crate::paint::overlays::FindDrawState;
use crate::paint::primitives::{color_to_normalized, push_quad};
use crate::paint::tab_bar::TabBarDrawState;
use crate::paint::ui_helpers::{block_duration_str, panel_display, visible_panel_rows};
use weft_core::blocks::BlockId;
use weft_core::config::{FontConfig, Theme};
use weft_core::grid::Color;
use weft_core::selection::SelectionHandler;
use weft_core::vt::Terminal;

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
    pub(crate) grid_row_cache: RefCell<Vec<Vec<f32>>>,
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
    prev_show_blocks: Cell<bool>,
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
    offscreen_texture: RefCell<Option<metal::Texture>>,
    /// v1.0 P0-c: Dimensions of the current offscreen texture (w, h) in
    /// physical pixels. Used to detect resize → recreate offscreen.
    offscreen_dims: Cell<(f32, f32)>,
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
