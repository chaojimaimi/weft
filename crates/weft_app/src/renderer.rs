//! Metal GPU renderer for the terminal Grid.

use core_graphics_types::geometry::CGSize;
use metal::{
    CompileOptions, Device, MTLClearColor, MTLLoadAction, MTLPixelFormat, MTLPrimitiveType,
    MTLResourceOptions, MTLStoreAction, MTLVertexFormat, MetalLayer, RenderPassDescriptor,
    RenderPipelineDescriptor, SamplerDescriptor, VertexDescriptor,
};
use objc2::msg_send;
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

/// Count how many visual rows a text line occupies when wrapped at `cols`
/// columns. Wide characters consume 2 columns.
fn wrapped_row_count(text: &str, cols: usize) -> usize {
    if cols == 0 {
        return 1;
    }
    let mut rows = 1;
    let mut col = 0usize;
    for c in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w == 0 {
            continue;
        }
        if col + w > cols {
            rows += 1;
            col = 0;
        }
        col += w;
    }
    rows.max(1)
}

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
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
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
    pub context_menu_target: Option<(f32, f32, weft_core::blocks::BlockId)>,
    /// Last-rendered popup rectangles (completion + palette), for border
    /// drag-resize hot-zone detection. None when the popup wasn't drawn.
    pub completion_popup_rect: Option<[f32; 4]>, // [x0, y0, x1, y1]
    pub palette_popup_rect: Option<[f32; 4]>,
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
            block_view_rows: Vec::new(),
            layout_ctx: None,
        }
    }

    /// Swap the active theme. Recolors the whole screen on the next draw
    /// (colors are resolved per-frame from cells' color-origins + this theme +
    /// the terminal palette, so no rebuild is needed).
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
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

    pub fn resize(&mut self, window: &Window, size: winit::dpi::PhysicalSize<u32>) {
        // Use physical pixels for viewport to match drawable_size and grid dimensions
        let vp_w = size.width as f32;
        let vp_h = size.height as f32;
        self.viewport = (vp_w, vp_h);
        // IMPORTANT: drawable_size must be in PHYSICAL PIXELS
        self.layer
            .set_drawable_size(CGSize::new(size.width as f64, size.height as f64));
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

    /// Draw the terminal Grid (and optional overlays) to screen.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        terminal: &Terminal,
        selection: &SelectionHandler,
        cursor_blink_on: bool,
        cursor_blink_phase: f32,
        overlays: &crate::overlay::OverlayStack<'_>,
        block_scroll: usize,
        // v0.8 U6: block-content metrics (total_rows, visible_rows,
        // max_scroll) for the dynamic scrollbar thumb. None in grid view.
        scroll_metrics: Option<(usize, usize, usize)>,
    ) {
        let drawable = match self.layer.next_drawable() {
            Some(d) => d,
            None => return,
        };

        // Build this frame's LayoutCtx: the single source of truth for
        // coordinate math in every overlay builder (v0.8 stage 1). Stored on
        // self so methods that don't receive it directly can still access it
        // during this draw; rebuilt every frame so resizes/padding changes
        // take effect immediately.
        let ctx = crate::layout::LayoutCtx::new(
            self.viewport,
            self.cell_width() as f32,
            self.cell_height() as f32,
            self.padding_x,
            self.padding_y,
        );
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
        let mut pending_hit_regions: Vec<crate::overlay::HitRegion> = Vec::new();
        // Reset popup rects — will be set by build_completion/palette_vertices.
        self.completion_popup_rect = None;
        self.palette_popup_rect = None;
        let mut vertices = if show_blocks {
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
            // hit-testing falls back to Grid coordinates.
            self.block_view_rows.clear();
            self.build_grid_vertices(
                grid,
                terminal.palette(),
                cursor,
                selection,
                terminal.cursor_visible && cursor_blink_on && prompt.is_none(),
                terminal.cursor_style,
            )
        };

        // Overlay the history panel on top of the grid (drawn after, so it
        // composites over terminal cells via the enabled alpha blend).
        if let Some(p) = panel {
            vertices.extend_from_slice(&self.build_panel_vertices(p));
        }

        // v0.8 U6 scrollbar: dynamic thumb position + height proportional to
        // visible/total content. The thumb sits in a track spanning the block
        // region; its vertical position reflects block_scroll (scrolled up →
        // thumb near top). Color uses theme.accent_dim (Quiet — barely visible
        // until you scroll). Only drawn when content overflows the viewport.
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
                    let thumb_color = color_to_normalized(self.theme.accent_dim);
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
            vertices.extend_from_slice(&self.build_prompt_vertices(p, cursor_blink_phase));
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
                let prompt_cols = prompt
                    .map(|p| {
                        let prompt_indent = if p.cursor.0 == 0 { 2 } else { 0 };
                        (prompt_indent + p.cursor.1) as f32 * cw_f + self.padding_x
                    })
                    .unwrap_or(self.padding_x);
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

        // Context menu overlay (F7) — drawn at mouse position.
        if let Some((x, y, _block_id)) = &self.context_menu_target {
            vertices.extend_from_slice(&self.build_context_menu_vertices(*x, *y));
        }

        // Debug: log first row characters and verify vertex data
        if vertices.is_empty() {
            let pass_desc = RenderPassDescriptor::new();
            let color_att = pass_desc.color_attachments().object_at(0).unwrap();
            color_att.set_texture(Some(drawable.texture()));
            color_att.set_load_action(MTLLoadAction::Clear);
            color_att.set_store_action(MTLStoreAction::Store);
            color_att.set_clear_color(MTLClearColor::new(bg_r, bg_g, bg_b, clear_a));

            let command_buffer = self.queue.new_command_buffer();
            let encoder = command_buffer.new_render_command_encoder(pass_desc);
            encoder.end_encoding();
            command_buffer.present_drawable(drawable);
            command_buffer.commit();
            self.hit_regions = pending_hit_regions;
            return;
        }

        // Upload vertex buffer
        let vertex_data_size = vertices.len() * std::mem::size_of::<f32>();
        let vertex_buffer = self.device.new_buffer_with_data(
            vertices.as_ptr() as *const _,
            vertex_data_size as u64,
            MTLResourceOptions::CPUCacheModeWriteCombined,
        );

        // Viewport uniform
        let vp_data: [f32; 2] = [self.viewport.0, self.viewport.1];
        let vp_buffer = self.device.new_buffer_with_data(
            vp_data.as_ptr() as *const _,
            8,
            MTLResourceOptions::CPUCacheModeWriteCombined,
        );

        // Render pass
        let pass_desc = RenderPassDescriptor::new();
        let color_att = pass_desc.color_attachments().object_at(0).unwrap();
        color_att.set_texture(Some(drawable.texture()));
        color_att.set_load_action(MTLLoadAction::Clear);
        color_att.set_store_action(MTLStoreAction::Store);
        color_att.set_clear_color(MTLClearColor::new(bg_r, bg_g, bg_b, clear_a));

        let command_buffer = self.queue.new_command_buffer();
        let encoder = command_buffer.new_render_command_encoder(pass_desc);

        encoder.set_render_pipeline_state(&self.pipeline);
        encoder.set_vertex_buffer(0, Some(&vertex_buffer), 0);
        encoder.set_vertex_buffer(1, Some(&vp_buffer), 0);

        let tex = self.atlas.texture();
        encoder.set_fragment_texture(0, Some(tex));
        encoder.set_fragment_sampler_state(0, Some(&self.sampler));

        let vertex_count = vertices.len() / 12;
        if vertex_count > 0 {
            encoder.draw_primitives(MTLPrimitiveType::Triangle, 0, vertex_count as u64);
        }
        encoder.end_encoding();

        command_buffer.present_drawable(drawable);
        command_buffer.commit();
        self.hit_regions = pending_hit_regions;
    }

    /// Build vertex buffer from the terminal Grid.
    fn build_grid_vertices(
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

        let mut vertices = Vec::with_capacity(num_rows * num_cols * 72);

        // Theme-derived colors (resolved per-frame from the active theme).
        let default_fg = color_to_normalized(self.theme.foreground);
        let default_bg = color_to_normalized(self.theme.background);
        let cursor_color = color_to_normalized(self.theme.cursor);
        let selection_bg = color_to_normalized(self.theme.selection);

        for row in 0..num_rows {
            for col in 0..num_cols {
                let cell = grid.cell(row, col);

                // Skip wide char spacers (rendered as part of the preceding cell)
                if cell.flags.contains(CellFlags::WIDE_SPACER) {
                    continue;
                }

                let x = self.padding_x + col as f32 * cw;
                let y = self.padding_y + row as f32 * ch;

                // Determine cell colors (resolve the cell's color-origin against
                // the palette / theme defaults).
                let fg = resolve_cell_color(cell.fg, default_fg, palette);
                let bg = resolve_cell_color(cell.bg, default_bg, palette);
                // Scale the plain background alpha by window opacity so empty
                // cells show the desktop through them. Text/selection/cursor
                // pick their own colors with alpha 1.0 in `final_bg` below, so
                // they stay fully opaque regardless of this scaling.
                let mut bg = bg;
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

                // Two triangles: TL-BL-BR, TL-BR-TR
                let quad: [[f32; 12]; 6] = [
                    [
                        x0,
                        y0,
                        u0,
                        v0,
                        final_fg[0],
                        final_fg[1],
                        final_fg[2],
                        final_fg[3],
                        final_bg[0],
                        final_bg[1],
                        final_bg[2],
                        final_bg[3],
                    ],
                    [
                        x0,
                        y1,
                        u0,
                        v1,
                        final_fg[0],
                        final_fg[1],
                        final_fg[2],
                        final_fg[3],
                        final_bg[0],
                        final_bg[1],
                        final_bg[2],
                        final_bg[3],
                    ],
                    [
                        x1,
                        y1,
                        u1,
                        v1,
                        final_fg[0],
                        final_fg[1],
                        final_fg[2],
                        final_fg[3],
                        final_bg[0],
                        final_bg[1],
                        final_bg[2],
                        final_bg[3],
                    ],
                    [
                        x0,
                        y0,
                        u0,
                        v0,
                        final_fg[0],
                        final_fg[1],
                        final_fg[2],
                        final_fg[3],
                        final_bg[0],
                        final_bg[1],
                        final_bg[2],
                        final_bg[3],
                    ],
                    [
                        x1,
                        y1,
                        u1,
                        v1,
                        final_fg[0],
                        final_fg[1],
                        final_fg[2],
                        final_fg[3],
                        final_bg[0],
                        final_bg[1],
                        final_bg[2],
                        final_bg[3],
                    ],
                    [
                        x1,
                        y0,
                        u1,
                        v0,
                        final_fg[0],
                        final_fg[1],
                        final_fg[2],
                        final_fg[3],
                        final_bg[0],
                        final_bg[1],
                        final_bg[2],
                        final_bg[3],
                    ],
                ];

                for vertex in &quad {
                    vertices.extend_from_slice(vertex);
                }

                // Draw bar/underline cursor overlay
                if is_cursor && show_cursor {
                    match cursor_style {
                        CursorStyle::Bar | CursorStyle::BlinkingBar => {
                            let bar_w = 2.0 * (self.viewport.0 / grid.num_cols as f32 / cw);
                            let bar_w = bar_w.max(1.0).min(cw * 0.15);
                            let quad: [[f32; 12]; 6] = [
                                [
                                    x0,
                                    y0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    cursor_color[0],
                                    cursor_color[1],
                                    cursor_color[2],
                                    cursor_color[3],
                                ],
                                [
                                    x0,
                                    y1,
                                    0.0,
                                    1.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    cursor_color[0],
                                    cursor_color[1],
                                    cursor_color[2],
                                    cursor_color[3],
                                ],
                                [
                                    x0 + bar_w,
                                    y1,
                                    0.0,
                                    1.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    cursor_color[0],
                                    cursor_color[1],
                                    cursor_color[2],
                                    cursor_color[3],
                                ],
                                [
                                    x0,
                                    y0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    cursor_color[0],
                                    cursor_color[1],
                                    cursor_color[2],
                                    cursor_color[3],
                                ],
                                [
                                    x0 + bar_w,
                                    y1,
                                    0.0,
                                    1.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    cursor_color[0],
                                    cursor_color[1],
                                    cursor_color[2],
                                    cursor_color[3],
                                ],
                                [
                                    x0 + bar_w,
                                    y0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    cursor_color[0],
                                    cursor_color[1],
                                    cursor_color[2],
                                    cursor_color[3],
                                ],
                            ];
                            for vertex in &quad {
                                vertices.extend_from_slice(vertex);
                            }
                        }
                        CursorStyle::Underline | CursorStyle::BlinkingUnderline => {
                            let line_h = 2.0;
                            let quad: [[f32; 12]; 6] = [
                                [
                                    x0,
                                    y1 - line_h,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    cursor_color[0],
                                    cursor_color[1],
                                    cursor_color[2],
                                    cursor_color[3],
                                ],
                                [
                                    x0,
                                    y1,
                                    0.0,
                                    1.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    cursor_color[0],
                                    cursor_color[1],
                                    cursor_color[2],
                                    cursor_color[3],
                                ],
                                [
                                    x1,
                                    y1,
                                    0.0,
                                    1.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    cursor_color[0],
                                    cursor_color[1],
                                    cursor_color[2],
                                    cursor_color[3],
                                ],
                                [
                                    x0,
                                    y1 - line_h,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    cursor_color[0],
                                    cursor_color[1],
                                    cursor_color[2],
                                    cursor_color[3],
                                ],
                                [
                                    x1,
                                    y1,
                                    0.0,
                                    1.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    cursor_color[0],
                                    cursor_color[1],
                                    cursor_color[2],
                                    cursor_color[3],
                                ],
                                [
                                    x1,
                                    y1 - line_h,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    0.0,
                                    cursor_color[0],
                                    cursor_color[1],
                                    cursor_color[2],
                                    cursor_color[3],
                                ],
                            ];
                            for vertex in &quad {
                                vertices.extend_from_slice(vertex);
                            }
                        }
                        _ => {} // Block cursor handled above
                    }
                }
            }
        }

        vertices
    }

    /// Build vertices for the right-side history panel overlay: a translucent
    /// background, a search box, and one row per finished block (newest first,
    /// filtered by the query), color-coded by exit code. The selected row is
    /// highlighted and, if expanded, its output is shown beneath. Drawn after
    /// the grid so it composites on top via the enabled alpha blend.
    fn build_panel_vertices(&self, p: &PanelDrawParams) -> Vec<f32> {
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let vp_w = self.viewport.0;
        let vp_h = self.viewport.1;
        let width_px = p.width_px;
        if width_px <= 0.0 || cw <= 0.0 || ch <= 0.0 {
            return Vec::new();
        }
        let panel_x = (vp_w - width_px).max(0.0);
        let panel_cols = ((width_px / cw) as usize).max(1);
        let mut vertices = Vec::new();

        // Translucent panel background: a darkened theme bg drawn as a bg-only
        // quad sampling the space glyph (mask 0 → shader emits pure bg color).
        let theme_bg = color_to_normalized(self.theme.background);
        let panel_bg = [
            theme_bg[0] * 0.45,
            theme_bg[1] * 0.45,
            theme_bg[2] * 0.45,
            0.94,
        ];
        let sel_bg = [
            theme_bg[0] + (1.0 - theme_bg[0]) * 0.18,
            theme_bg[1] + (1.0 - theme_bg[1]) * 0.18,
            theme_bg[2] + (1.0 - theme_bg[2]) * 0.18,
            0.95,
        ];
        let (su, sv, suw, svh) = self.space_uv();
        // V-swap to match grid rendering (CAMetalLayer flip compensation).
        let bg_uv = [su, sv + svh, su + suw, sv];
        push_quad(
            &mut vertices,
            [panel_x, 0.0, panel_x + width_px, vp_h],
            bg_uv,
            [0.0; 4],
            panel_bg,
        );

        let fg = color_to_normalized(self.theme.foreground);
        let dim = [fg[0] * 0.6, fg[1] * 0.6, fg[2] * 0.6, 1.0];
        let green = [0.53, 0.80, 0.36, 1.0];
        let red = [0.85, 0.36, 0.36, 1.0];

        // Search box row: "Search:" label + the live query text.
        self.push_text(
            &mut vertices,
            panel_x + cw * 0.5,
            ch * 0.4,
            "Search:",
            dim,
            panel_cols,
        );
        let query_x = panel_x + cw * 0.5 + "Search: ".chars().count() as f32 * cw;
        self.push_text(&mut vertices, query_x, ch * 0.4, p.query, fg, panel_cols);

        // Display list: newest-first, filtered by query (capped to fit).
        let max_rows = visible_panel_rows(vp_h, self.cell_height());
        let display = panel_display(p.blocks, p.query, max_rows);
        let row_h = ch * 1.1;
        let mut y = ch * 1.9;
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
            let label = truncate_str(&block.command, cmd_cols);
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
    fn build_prompt_vertices(&self, p: &PromptDrawParams, cursor_blink_phase: f32) -> Vec<f32> {
        let mut verts = Vec::new();
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let vp_w = self.viewport.0;
        let vp_h = self.viewport.1;
        if cw <= 0.0 || ch <= 0.0 || vp_w <= 0.0 || vp_h <= 0.0 {
            return verts;
        }

        let n_lines = p.lines.len().max(1);
        // Box height matches what `input_box_height_px` subtracted from the
        // grid (one pad row + N text lines + one pad row) so they never gap.
        let box_h = ch * (n_lines as f32 + 2.0);
        let box_y1 = (vp_h - self.padding_y).max(0.0);
        let box_y0 = (box_y1 - box_h).max(0.0);
        let box_x0 = self.padding_x;
        let box_x1 = (vp_w - self.padding_x).max(box_x0);

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
        push_quad(
            &mut verts,
            [box_x0, box_y0, box_x1, box_y1],
            bg_uv,
            [0.0; 4],
            box_bg,
        );

        let text_y0 = box_y0 + ch; // first text row (below the top pad row)
        let left = box_x0; // flush to the box edge (matches the block view)
        let box_cols = (((box_x1 - left) / cw).max(1.0)) as usize;

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
        // v0.8 Quiet: prompt marker ❯ uses accent_dim (warm gray) so the
        // UI skeleton recedes — was hardcoded cold blue [0.42, 0.85, 1.0].
        let prompt_str = "❯ ";
        let prompt_chars = 2;
        let prompt_c = color_to_normalized(self.theme.accent_dim);
        self.push_text(
            &mut verts,
            left,
            text_y0,
            prompt_str,
            prompt_c,
            prompt_chars,
        );

        // Editor buffer lines (line 0 starts after the prompt).
        for (i, line) in p.lines.iter().enumerate() {
            let y = text_y0 + i as f32 * ch;
            let (start_x, max_chars) = if i == 0 {
                let sx = left + prompt_chars as f32 * cw;
                let avail = box_cols.saturating_sub(prompt_chars).max(1);
                (sx, avail)
            } else {
                (left, box_cols)
            };
            self.push_line_tokenized(&mut verts, start_x, y, line, max_chars);
        }

        // Cursor bar at (line, col).
        let (cl, cc) = p.cursor;
        let cy = text_y0 + cl as f32 * ch;
        let text_start_x = if cl == 0 {
            left + prompt_chars as f32 * cw
        } else {
            left
        };
        // v0.8: cursor X must use the DISPLAY width of chars before the cursor,
        // not the char count — CJK chars occupy 2 columns each, so `cc × cw`
        // leaves the caret stranded mid-cell for input like "Weft项目设计.md".
        // Sum the actual rendered columns of the first `cc` chars on this line.
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
        let cx = text_start_x + cursor_offset_cols as f32 * cw;
        let bar_w = (cw * 0.12).max(2.0);

        // ── v0.8 signature: warm cursor breath + amber glow ─────────────
        // Smooth sin() alpha over a 2400ms period (phase in radians).
        // sin maps [0, 2π) → [-1, 1]; we remap to [0.25, 1.0] so the caret
        // never fully disappears (calmer than hard on/off). The glow halo
        // is a wider, very-low-alpha amber quad behind the caret that
        // breathes in sync (peaks at ~0.25 alpha).
        let s = cursor_blink_phase.sin(); // [-1, 1]
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
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let vp_w = self.viewport.0;
        let theme_bg = color_to_normalized(self.theme.background);
        let fg = color_to_normalized(self.theme.foreground);
        // v0.8 Quiet: accent_dim for the selected-row highlight tint.
        let prompt_c = color_to_normalized(self.theme.accent_dim);
        let (su, sv, suw, svh) = self.space_uv();
        let bg_uv = [su, sv + svh, su + suw, sv];

        let label_color = [fg[0] * 0.85, fg[1] * 0.85, fg[2] * 0.85, 1.0];
        let sel_label_color = fg;
        let suffix_color = [fg[0] * 0.40, fg[1] * 0.40, fg[2] * 0.40, 1.0];

        let popup_bottom = anchor_y;
        let avail_rows = ((popup_bottom / ch).ceil() as usize).saturating_sub(1);
        let max_rows = self.popup_max_rows.min(avail_rows.max(1));
        let start = selected.saturating_sub(max_rows - 1);
        let end = (start + max_rows).min(matches.len());
        let shown = end - start;

        // Dynamic width from the longest visible label.
        let max_label_cols = matches[start..end]
            .iter()
            .map(|m| Self::text_col_width(&m.label))
            .max()
            .unwrap_or(10);
        let suffix_cols = 10usize;
        let gap_cols = 2usize;
        let popup_cols = 1 + 2 + max_label_cols + gap_cols + suffix_cols + 1;
        let popup_max_cols = ((vp_w * self.popup_width_scale) / cw) as usize;
        let popup_cols = popup_cols.clamp(25, popup_max_cols.max(25));
        let popup_w = popup_cols as f32 * cw;
        let popup_x0 = box_x0;
        let popup_x1 = (popup_x0 + popup_w).min(vp_w - self.padding_x);

        let pad = cw * 0.5;
        let icon_w = 2.0 * cw;
        let label_x = popup_x0 + pad + icon_w;
        let label_cols = popup_cols
            .saturating_sub(1 + 2 + gap_cols + suffix_cols + 1)
            .max(5);

        // Popup height: N rows + top padding (0.5ch for visual breathing room).
        // The bottom edge is flush with the input box top (anchor_y).
        // Each row occupies exactly `ch` pixels, starting from the bottom up.
        let top_pad = ch * 0.5;
        let popup_h = shown as f32 * ch + top_pad;
        let popup_top = popup_bottom - popup_h;
        let border_c = [0.5, 0.5, 0.5, 0.35];
        let popup_bg = [
            theme_bg[0] + (1.0 - theme_bg[0]) * 0.05,
            theme_bg[1] + (1.0 - theme_bg[1]) * 0.05,
            theme_bg[2] + (1.0 - theme_bg[2]) * 0.05,
            1.0,
        ];

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

        // Each row occupies exactly `ch` pixels. The bottom-most row starts at
        // `popup_bottom - ch` and extends to `popup_bottom` — fully inside the
        // popup. Subsequent rows step upward by `ch`.
        let mut y = popup_bottom - ch;
        for i in (start..end).rev() {
            if y < popup_top {
                break;
            }
            let is_sel = i == selected;
            let lcolor = if is_sel { sel_label_color } else { label_color };
            if is_sel {
                push_quad(
                    &mut verts,
                    [popup_x0 + 1.0, y, popup_x1 - 1.0, y + ch],
                    bg_uv,
                    [0.0; 4],
                    [prompt_c[0], prompt_c[1], prompt_c[2], 0.20],
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
            self.push_text(&mut verts, popup_x0 + pad, y, icon, icon_color, 3);
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
            // stagger when labels had different widths.
            let suffix_x = label_x + (max_label_cols.min(label_cols) + gap_cols) as f32 * cw;
            self.push_text(&mut verts, suffix_x, y, suffix, suffix_color, suffix_cols);
            y -= ch;
        }

        // Return the popup rect for border drag-resize hot-zone detection.
        let popup_rect = Some([popup_x0, popup_top, popup_x1, popup_bottom]);

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
        // v0.8 Quiet direction: chevrons/prompt marks use accent_dim (warm gray)
        // so the UI skeleton recedes — was hardcoded cold blue [0.42, 0.85, 1.0].
        let prompt_c = color_to_normalized(self.theme.accent_dim);
        // Header / cwd dim text: also accent_dim (Quiet — uniform dim chrome).
        let dim = color_to_normalized(self.theme.accent_dim);
        // Block separator: theme.separator (barely-visible warm dark).
        let separator = color_to_normalized(self.theme.separator);
        let (su, sv, suw, svh) = self.space_uv();
        let bg_uv = [su, sv + svh, su + suw, sv];
        let border_c = [0.5, 0.5, 0.5, 0.35];
        let popup_bg = [
            theme_bg[0] + (1.0 - theme_bg[0]) * 0.05,
            theme_bg[1] + (1.0 - theme_bg[1]) * 0.05,
            theme_bg[2] + (1.0 - theme_bg[2]) * 0.05,
            1.0,
        ];

        let pad_x = self.padding_x;
        let left = pad_x;
        let right = vp_w - pad_x;
        let cols = (((right - left) / cw).max(1.0)) as usize;

        // Layout: centered window in the upper portion of the viewport.
        let popup_w = vp_w * self.popup_width_scale;
        let popup_x0 = (vp_w - popup_w) / 2.0;
        let popup_x1 = popup_x0 + popup_w;

        // If we're in form mode, render the form instead of the search list.
        if let Some(form) = p.form {
            let form_top = vp_h * 0.15;
            let form_h = (form.fields.len() as f32 + 3.0) * ch + ch * 0.5;
            let rect = Some([popup_x0, form_top, popup_x1, form_top + form_h]);
            let v = self.build_palette_form_vertices(form, popup_x0, popup_x1, vp_h);
            return (v, rect);
        }

        // Search mode: query box + results list.
        let max_results = self.popup_max_rows.min(p.entries.len().max(1));
        let shown = max_results.min(p.entries.len());
        let popup_h = (shown as f32 + 2.0) * ch + ch * 0.5; // +2 for header + padding
        let popup_top = vp_h * 0.15;
        let popup_bottom = popup_top + popup_h;

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
        let query_y = popup_top + ch * 0.5;
        if !p.banner.is_empty() {
            // Sub-mode: show banner + input buffer.
            self.push_text(
                &mut verts,
                popup_x0 + cw * 0.5,
                query_y,
                p.banner,
                prompt_c,
                cols,
            );
            let banner_cols = Self::text_col_width(p.banner);
            let input_x = popup_x0 + cw * 0.5 + (banner_cols + 1) as f32 * cw;
            let avail = (((popup_x1 - input_x) / cw).max(1.0)) as usize;
            self.push_text(&mut verts, input_x, query_y, p.submode_input, fg, avail);
        } else {
            // Normal search mode.
            let query_label = "> ";
            self.push_text(
                &mut verts,
                popup_x0 + cw * 0.5,
                query_y,
                query_label,
                prompt_c,
                cols,
            );
            let qx = popup_x0 + cw * 0.5 + query_label.chars().count() as f32 * cw;
            let avail = (((popup_x1 - qx) / cw).max(1.0)) as usize;
            self.push_text(&mut verts, qx, query_y, p.query, fg, avail);
        }

        // Separator below query.
        let sep_y = query_y + ch;
        push_quad(
            &mut verts,
            [popup_x0, sep_y, popup_x1, sep_y + 1.0],
            bg_uv,
            [0.0; 4],
            separator,
        );

        // Results rows.
        let start = p.selection.saturating_sub(max_results.saturating_sub(1));
        let end = (start + max_results).min(p.entries.len());
        let mut y = sep_y + ch;
        for i in start..end {
            if y + ch > popup_bottom {
                break;
            }
            let is_sel = i == p.selection;
            if is_sel {
                push_quad(
                    &mut verts,
                    [popup_x0 + 1.0, y, popup_x1 - 1.0, y + ch],
                    bg_uv,
                    [0.0; 4],
                    [prompt_c[0], prompt_c[1], prompt_c[2], 0.20],
                );
            }
            let entry = &p.entries[i];
            let lcolor = if is_sel { fg } else { dim };
            let suffix_color = [fg[0] * 0.40, fg[1] * 0.40, fg[2] * 0.40, 1.0];

            // Label + description.
            let label_x = popup_x0 + cw * 0.5;
            let label_avail = (((popup_x1 - label_x) / cw) as usize)
                .saturating_sub(12)
                .max(1);
            self.push_text(&mut verts, label_x, y, entry.label, lcolor, label_avail);

            // Kind suffix (right-aligned area).
            let suffix_x = popup_x1 - cw * 0.5 - 10.0 * cw;
            self.push_text(&mut verts, suffix_x, y, entry.kind_label, suffix_color, 10);
            y += ch;
        }

        // Return the popup rect for border drag-resize hot-zone detection.
        let popup_rect = Some([popup_x0, popup_top, popup_x1, popup_bottom]);

        (verts, popup_rect)
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
        // v0.8 Quiet: accent_dim for highlight tint + dim text.
        let prompt_c = color_to_normalized(self.theme.accent_dim);
        let dim = color_to_normalized(self.theme.accent_dim);
        let (su, sv, suw, svh) = self.space_uv();
        let bg_uv = [su, sv + svh, su + suw, sv];
        let border_c = [0.5, 0.5, 0.5, 0.35];
        let popup_bg = [
            theme_bg[0] + (1.0 - theme_bg[0]) * 0.05,
            theme_bg[1] + (1.0 - theme_bg[1]) * 0.05,
            theme_bg[2] + (1.0 - theme_bg[2]) * 0.05,
            1.0,
        ];

        let n_fields = form.fields.len();
        let popup_h = (n_fields as f32 + 3.0) * ch + ch * 0.5;
        let popup_top = vp_h * 0.15;
        let popup_bottom = popup_top + popup_h;

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
                // Highlight the current field's value area.
                push_quad(
                    &mut verts,
                    [val_x, y, popup_x1 - cw * 0.5, y + ch],
                    bg_uv,
                    [0.0; 4],
                    [prompt_c[0], prompt_c[1], prompt_c[2], 0.15],
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

    /// Build the right-click context menu (F7) as a small popup at (x, y).
    fn build_context_menu_vertices(&self, x: f32, y: f32) -> Vec<f32> {
        let mut verts = Vec::new();
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let vp_w = self.viewport.0;
        let theme_bg = color_to_normalized(self.theme.background);
        let fg = color_to_normalized(self.theme.foreground);
        // v0.8 Quiet: accent_dim for the selected-row highlight tint.
        let prompt_c = color_to_normalized(self.theme.accent_dim);
        let separator = color_to_normalized(self.theme.separator);
        let (su, sv, suw, svh) = self.space_uv();
        let bg_uv = [su, sv + svh, su + suw, sv];

        let items = ["Copy Command", "Copy Output", "Toggle Fold"];
        let item_h = ch * 1.2;
        let menu_w = 180.0 * (self.scale as f32);
        let menu_h = items.len() as f32 * item_h + ch * 0.4;

        // Clamp to viewport.
        let menu_x0 = x.min(vp_w - menu_w - 4.0);
        let menu_y0 = y;
        let menu_x1 = menu_x0 + menu_w;
        let menu_y1 = menu_y0 + menu_h;

        let popup_bg = [
            theme_bg[0] + (1.0 - theme_bg[0]) * 0.08,
            theme_bg[1] + (1.0 - theme_bg[1]) * 0.08,
            theme_bg[2] + (1.0 - theme_bg[2]) * 0.08,
            1.0,
        ];
        let border_c = [0.5, 0.5, 0.5, 0.35];

        // Background.
        push_quad(
            &mut verts,
            [menu_x0, menu_y0, menu_x1, menu_y1],
            bg_uv,
            [0.0; 4],
            popup_bg,
        );
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
            let item_y = menu_y0 + ch * 0.2 + i as f32 * item_h;
            let color = if i == items.len() - 1 {
                prompt_c // "Toggle Fold" in accent
            } else {
                fg
            };
            self.push_text(
                &mut verts,
                menu_x0 + cw * 0.4,
                item_y,
                label,
                color,
                (menu_w / cw * 0.9) as usize,
            );
            // Separator between items (except last).
            if i + 1 < items.len() {
                let sep_y = item_y + item_h;
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
        selection: &weft_core::selection::SelectionHandler,
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

        let pad_x = self.padding_x;
        let pad_y = self.padding_y;
        let left = pad_x;
        let right = vp_w - pad_x;
        let cols = (((right - left) / cw).max(1.0)) as usize;
        let pitch = ch * 1.1;

        let theme_bg = color_to_normalized(self.theme.background);
        let fg = color_to_normalized(self.theme.foreground);
        // v0.8 Quiet direction: chevrons/prompt marks use accent_dim (warm gray)
        // so the UI skeleton recedes — was hardcoded cold blue [0.42, 0.85, 1.0].
        let prompt_c = color_to_normalized(self.theme.accent_dim);
        // Header / cwd dim text: also accent_dim (Quiet — uniform dim chrome).
        let dim = color_to_normalized(self.theme.accent_dim);
        // Block separator: theme.separator (barely-visible warm dark).
        let separator = color_to_normalized(self.theme.separator);
        let (su, sv, suw, svh) = self.space_uv();
        let bg_uv = [su, sv + svh, su + suw, sv];

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
        let content_bottom_y;
        if let Some(cwd) = cwd {
            if live.is_none() {
                let fixed_y = region_bottom_y - pitch;
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
                // Scrollable content starts above the CWD line.
                content_bottom_y = region_bottom_y - 2.0 * pitch;
            } else {
                content_bottom_y = region_bottom_y;
            }
        } else {
            content_bottom_y = region_bottom_y;
        }

        // ── Phase 1: Pre-layout scrollable content rows ──────────────────
        //
        // Flatten every history row into a list with its y-distance from
        // `content_bottom_y`. Scrolling applies a pixel offset so the entire
        // content slides as a unit (matching Warp). Only rows within the clip
        // region are rendered — no blank space.

        enum LaidRow<'a> {
            Output(&'a str),
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
        }

        let mut rows: Vec<f32> = Vec::new();
        let mut row_data: Vec<LaidRow> = Vec::new();
        let mut cursor_dist = 0.0;

        // Bottom of scrollable content: live block (CommandExecuting) or
        // nothing (Editor mode — CWD is already handled above).
        if let Some(live) = live {
            for line in live.output.lines().rev() {
                let vis_rows = wrapped_row_count(line, cols);
                cursor_dist += vis_rows as f32 * pitch;
                rows.push(cursor_dist);
                row_data.push(LaidRow::Output(line));
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

        for b in blocks.iter().rev() {
            let foldable = b.output.lines().any(|l| !l.trim().is_empty());
            if !b.collapsed {
                let mut out_lines: Vec<&str> = b.output.lines().collect();
                while out_lines
                    .last()
                    .is_some_and(|l| matches!(l.trim(), "%" | "$" | "#"))
                {
                    out_lines.pop();
                }
                for line in out_lines.iter().rev() {
                    let vis_rows = wrapped_row_count(line, cols);
                    cursor_dist += vis_rows as f32 * pitch;
                    rows.push(cursor_dist);
                    row_data.push(LaidRow::Output(line));
                }
            }
            cursor_dist += pitch;
            rows.push(cursor_dist);
            row_data.push(LaidRow::Command {
                command: &b.command,
                collapsed: b.collapsed,
                foldable,
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
        }

        // ── Phase 2: Render scrollable content with offset ───────────────

        let scroll_px = (block_scroll as f32) * pitch;
        let clip_top = pad_y;
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
        let sel_bv = selection.block_view_selection.as_ref();
        let selection_bg = color_to_normalized(self.theme.selection);
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
                let anchor = if s.start.row_index >= s.end.row_index {
                    s.start.char_index
                } else {
                    s.end.char_index
                };
                (0, anchor.min(max_char))
            } else if snap_idx == bottom {
                let anchor = if s.start.row_index >= s.end.row_index {
                    s.end.char_index
                } else {
                    s.start.char_index
                };
                (anchor.min(max_char), max_char)
            } else {
                (0, max_char)
            };
            (c_end > c_start).then_some((c_start, c_end))
        };

        for (i, &dist) in rows.iter().enumerate() {
            let row_top_y = content_bottom_y - dist + scroll_px;
            let row_bottom_y = row_top_y + pitch;

            if row_bottom_y < clip_top || row_top_y > clip_bottom {
                continue;
            }

            let y = row_top_y;

            match &row_data[i] {
                LaidRow::Output(text) => {
                    let chunks: Vec<String> = wrap_line_chunks(text, cols).collect();
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
                        self.push_text(&mut verts, left, y, text, fg, cols);
                        // Record the row band for selection hit-testing.
                        bv_rows.push(weft_core::selection::BlockViewRow {
                            kind: weft_core::selection::BlockViewRowKind::Output,
                            text: text.to_string(),
                            block_id: None,
                            y_top: y,
                            y_bottom: y + pitch,
                        });
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
                                self.push_text(&mut verts, left, cy, chunk, fg, cols);
                            }
                            // Each wrapped sub-line is its own selectable band.
                            bv_rows.push(weft_core::selection::BlockViewRow {
                                kind: weft_core::selection::BlockViewRowKind::Output,
                                text: chunk.clone(),
                                block_id: None,
                                y_top: cy,
                                y_bottom: cy + pitch,
                            });
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
                    self.push_line_tokenized(&mut verts, cmd_x, y, command, avail);
                    if *foldable {
                        hit_regions.push(crate::overlay::HitRegion {
                            x0: 0.0,
                            y0: y,
                            x1: self.viewport.0,
                            y1: y + pitch,
                            target: crate::overlay::HitTarget::BlockFold(*block_id),
                        });
                    }
                    // Command row is selectable (copies the command text).
                    bv_rows.push(weft_core::selection::BlockViewRow {
                        kind: weft_core::selection::BlockViewRowKind::Command,
                        text: command.to_string(),
                        block_id: Some(*block_id),
                        y_top: y,
                        y_bottom: y + pitch,
                    });
                }
                LaidRow::Header { text } => {
                    self.push_text(&mut verts, left, y, text, dim, cols);
                    // Header is metadata, not selectable — but we still record
                    // the band so hit-testing can return None cleanly instead
                    // of falling through to a neighbouring row.
                    bv_rows.push(weft_core::selection::BlockViewRow {
                        kind: weft_core::selection::BlockViewRowKind::Header,
                        text: String::new(),
                        block_id: None,
                        y_top: y,
                        y_bottom: y + pitch,
                    });
                }
                LaidRow::Separator => {
                    let ly = y + pitch * 0.5;
                    push_quad(
                        &mut verts,
                        [left, ly, right, ly + 1.5],
                        bg_uv,
                        [0.0; 4],
                        separator,
                    );
                    bv_rows.push(weft_core::selection::BlockViewRow {
                        kind: weft_core::selection::BlockViewRowKind::Separator,
                        text: String::new(),
                        block_id: None,
                        y_top: y,
                        y_bottom: y + pitch,
                    });
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
                    bv_rows.push(weft_core::selection::BlockViewRow {
                        kind: weft_core::selection::BlockViewRowKind::LiveCommand,
                        text: command.to_string(),
                        block_id: None,
                        y_top: y,
                        y_bottom: y + pitch,
                    });
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
                let sticky_y = pad_y;
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

        (verts, hit_regions, bv_rows)
    }

    /// Column width of a character (0 for zero-width combining marks,
    /// 1 for ASCII/narrow, 2 for CJK full-width).
    fn char_col_width(c: char) -> usize {
        unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)
    }

    /// Total column width of a string — sum of each char's display width.
    /// Use this instead of `chars().count()` whenever a width/position
    /// calculation must match what `push_text` actually renders (CJK chars
    /// occupy 2 columns each, not 1).
    fn text_col_width(s: &str) -> usize {
        s.chars()
            .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
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
            let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
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
}

/// Whether a block matches the panel search query (empty query = match all).
/// Shared by the renderer (list layout) and the app (selection clamping).
pub fn block_matches_query(block: &Block, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let q = query.to_lowercase();
    block.command.to_lowercase().contains(&q) || block.output.to_lowercase().contains(&q)
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
fn visible_panel_rows(viewport_h: f32, cell_h: u32) -> usize {
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
}
