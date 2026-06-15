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
use weft_core::config::{FontConfig, Theme};
use weft_core::grid::{CellColor, CellFlags, CellWidth, Color, CursorStyle};
use weft_core::selection::SelectionHandler;
use weft_core::vt::Terminal;

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
}

impl MetalRenderer {
    pub fn new(window: &Window, font_config: FontConfig, theme: Theme) -> Self {
        let device = Device::system_default().expect("No Metal device found");
        let queue = device.new_command_queue();

        info!("Metal device: {}", device.name());

        let scale = window.scale_factor();
        let size = window.inner_size();

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
    color.a = 1.0;
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
        }
    }

    /// Swap the active theme. Recolors the whole screen on the next draw
    /// (colors are resolved per-frame from cells' color-origins + this theme +
    /// the terminal palette, so no rebuild is needed).
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
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

    /// Rebuild the glyph atlas from a (possibly changed) font config — used on
    /// live config reload when font family/size/line-height changes. Returns
    /// the new cell dimensions so the caller can recompute grid rows/cols and
    /// PTY size.
    pub fn rebuild_atlas(&mut self, font_config: FontConfig) -> (u32, u32) {
        self.font_config = font_config;
        self.atlas = GlyphAtlas::new(&self.device, &self.font_config, self.scale);
        (self.atlas.cell_width, self.atlas.cell_height)
    }

    /// Draw the terminal Grid to screen.
    pub fn draw(
        &mut self,
        terminal: &Terminal,
        selection: &SelectionHandler,
        cursor_blink_on: bool,
    ) {
        let drawable = match self.layer.next_drawable() {
            Some(d) => d,
            None => return,
        };

        let grid = terminal.grid();
        let cursor = &grid.cursor;

        // Clear color from the theme background.
        let bg = self.theme.background;
        let (bg_r, bg_g, bg_b, bg_a) = (
            bg.r as f64 / 255.0,
            bg.g as f64 / 255.0,
            bg.b as f64 / 255.0,
            bg.a as f64 / 255.0,
        );

        // Collect unique on-screen characters not yet in the atlas, then
        // rasterize each exactly once. This avoids O(R*C) hash lookups per
        // frame — after warm-up the set is almost always empty.
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
            for ch in &missing {
                self.atlas.get_or_rasterize(*ch);
            }
        }

        // Build vertex data from Grid cells
        let vertices = self.build_grid_vertices(
            grid,
            terminal.palette(),
            cursor,
            selection,
            terminal.cursor_visible && cursor_blink_on,
            terminal.cursor_style,
        );

        // Debug: log first row characters and verify vertex data
        if vertices.is_empty() {
            let pass_desc = RenderPassDescriptor::new();
            let color_att = pass_desc.color_attachments().object_at(0).unwrap();
            color_att.set_texture(Some(drawable.texture()));
            color_att.set_load_action(MTLLoadAction::Clear);
            color_att.set_store_action(MTLStoreAction::Store);
            color_att.set_clear_color(MTLClearColor::new(bg_r, bg_g, bg_b, bg_a));

            let command_buffer = self.queue.new_command_buffer();
            let encoder = command_buffer.new_render_command_encoder(pass_desc);
            encoder.end_encoding();
            command_buffer.present_drawable(drawable);
            command_buffer.commit();
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
        color_att.set_clear_color(MTLClearColor::new(0.0, 0.0, 0.0, 1.0));

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

                let x = col as f32 * cw;
                let y = row as f32 * ch;

                // Determine cell colors (resolve the cell's color-origin against
                // the palette / theme defaults).
                let fg = resolve_cell_color(cell.fg, default_fg, palette);
                let bg = resolve_cell_color(cell.bg, default_bg, palette);

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
