//! Metal GPU renderer for the terminal Grid.

use core_graphics_types::geometry::CGSize;
use metal::{
    CompileOptions, Device, MTLClearColor, MTLLoadAction, MTLPixelFormat, MTLPrimitiveType,
    MTLResourceOptions, MTLStoreAction, MetalLayer, RenderPassDescriptor, RenderPipelineDescriptor,
};
use objc2::msg_send;
use tracing::info;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

use crate::glyph::GlyphAtlas;
use weft_core::grid::CellFlags;
use weft_core::vt::Terminal;

/// Metal GPU renderer: draws the terminal Grid to screen.
pub struct MetalRenderer {
    #[allow(dead_code)]
    device: Device,
    queue: metal::CommandQueue,
    layer: MetalLayer,
    pipeline: metal::RenderPipelineState,
    atlas: GlyphAtlas,
    viewport: (f32, f32),
}

impl MetalRenderer {
    pub fn new(window: &Window) -> Self {
        let device = Device::system_default().expect("No Metal device found");
        let queue = device.new_command_queue();

        info!("Metal device: {}", device.name());

        let scale = window.scale_factor();
        let size = window.inner_size();
        let vp_w = size.width as f32 * scale as f32;
        let vp_h = size.height as f32 * scale as f32;

        // Build glyph atlas
        let atlas = GlyphAtlas::new(&device, 14.0, scale);

        // Compile shaders
        let source = r#"#include <metal_stdlib>
using namespace metal;

struct TextVertex {
    float2 position;
    float2 tex_coord;
    float4 fg_color;
    float4 bg_color;
};

struct TextVertexOut {
    float4 position [[position]];
    float2 tex_coord;
    float4 fg_color;
    float4 bg_color;
    float is_bg;
};

vertex TextVertexOut text_vertex(
    constant TextVertex* vertices [[buffer(0)]],
    uint vid [[vertex_id]],
    constant float2& viewport_size [[buffer(1)]]
) {
    TextVertexOut out;
    TextVertex in = vertices[vid];

    float2 clip = (in.position / viewport_size) * 2.0 - 1.0;
    clip.y = -clip.y;

    out.position = float4(clip, 0.0, 1.0);
    out.tex_coord = in.tex_coord;
    out.fg_color = in.fg_color;
    out.bg_color = in.bg_color;
    out.is_bg = 0.0;
    return out;
}

fragment float4 text_fragment(
    TextVertexOut in [[stage_in]],
    texture2d<float> atlas [[texture(0)]],
    sampler atlas_sampler [[sampler(0)]]
) {
    float mask = atlas.sample(atlas_sampler, in.tex_coord).r;
    float4 fg = in.fg_color * mask;
    float4 bg = in.bg_color * (1.0 - mask);
    return float4(fg.rgb + bg.rgb, fg.a + bg.a);
}
"#;

        let compile_opts = CompileOptions::new();
        let library = device
            .new_library_with_source(source, &compile_opts)
            .expect("Failed to compile Metal shader");
        let vertex_fn = library.get_function("text_vertex", None).unwrap();
        let fragment_fn = library.get_function("text_fragment", None).unwrap();

        let pipeline_desc = RenderPipelineDescriptor::new();
        pipeline_desc.set_vertex_function(Some(&vertex_fn));
        pipeline_desc.set_fragment_function(Some(&fragment_fn));
        let color_att = pipeline_desc.color_attachments().object_at(0).unwrap();
        color_att.set_pixel_format(MTLPixelFormat::BGRA8Unorm);

        let pipeline = device
            .new_render_pipeline_state(&pipeline_desc)
            .expect("Failed to create render pipeline");

        // Create and configure Metal layer
        let layer = MetalLayer::new();
        layer.set_device(&device);
        layer.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        layer.set_presents_with_transaction(false);
        layer.set_maximum_drawable_count(3);
        layer.set_drawable_size(CGSize::new(vp_w as f64, vp_h as f64));

        unsafe {
            attach_layer_to_nsview(&layer, window);
        }

        info!(
            "Renderer initialized: {}x{} @ {}x scale, {}x{} cells",
            size.width,
            size.height,
            scale,
            atlas.cell_width,
            atlas.cell_height
        );

        Self {
            device,
            queue,
            layer,
            pipeline,
            atlas,
            viewport: (vp_w, vp_h),
        }
    }

    pub fn resize(&mut self, window: &Window, size: winit::dpi::PhysicalSize<u32>) {
        let scale = window.scale_factor();
        let vp_w = size.width as f32 * scale as f32;
        let vp_h = size.height as f32 * scale as f32;
        self.viewport = (vp_w, vp_h);
        self.layer
            .set_drawable_size(CGSize::new(vp_w as f64, vp_h as f64));
        window.request_redraw();
    }

    /// Cell width in logical pixels (for terminal size calculation).
    pub fn cell_width(&self) -> u32 {
        self.atlas.cell_width
    }

    /// Cell height in logical pixels (for terminal size calculation).
    pub fn cell_height(&self) -> u32 {
        self.atlas.cell_height
    }

    /// Draw the terminal Grid to screen.
    pub fn draw(&self, terminal: &Terminal) {
        let drawable = match self.layer.next_drawable() {
            Some(d) => d,
            None => return,
        };

        let grid = terminal.grid();
        let cursor = &grid.cursor;

        // Build vertex data from Grid cells
        let vertices = self.build_grid_vertices(grid, cursor);

        if vertices.is_empty() {
            // Nothing to draw, just clear
            let pass_desc = RenderPassDescriptor::new();
            let color_att = pass_desc.color_attachments().object_at(0).unwrap();
            color_att.set_texture(Some(drawable.texture()));
            color_att.set_load_action(MTLLoadAction::Clear);
            color_att.set_store_action(MTLStoreAction::Store);
            color_att.set_clear_color(MTLClearColor::new(0.12, 0.12, 0.14, 1.0));

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
        color_att.set_clear_color(MTLClearColor::new(0.12, 0.12, 0.14, 1.0));

        let command_buffer = self.queue.new_command_buffer();
        let encoder = command_buffer.new_render_command_encoder(pass_desc);

        encoder.set_render_pipeline_state(&self.pipeline);
        encoder.set_vertex_buffer(0, Some(&vertex_buffer), 0);
        encoder.set_vertex_buffer(1, Some(&vp_buffer), 0);
        encoder.set_fragment_texture(0, Some(self.atlas.texture()));

        let vertex_count = vertices.len() / 12; // 12 floats per vertex
        encoder.draw_primitives(MTLPrimitiveType::Triangle, 0, vertex_count as u64);
        encoder.end_encoding();

        command_buffer.present_drawable(drawable);
        command_buffer.commit();
    }

    /// Build vertex buffer from the terminal Grid.
    ///
    /// Each cell = 1 quad = 6 vertices. Each vertex has:
    ///   position(2) + tex_coord(2) + fg_color(4) + bg_color(4) = 12 floats
    /// Wait, the shader uses 10 floats (pos2 + uv2 + fg4 + bg4) but we have
    /// fg and bg separate. Let me use 10 floats: pos(2) + uv(2) + fg(4) + bg(4).
    fn build_grid_vertices(
        &self,
        grid: &weft_core::grid::Grid,
        cursor: &weft_core::grid::Cursor,
    ) -> Vec<f32> {
        let cw = self.atlas.cell_width as f32;
        let ch = self.atlas.cell_height as f32;
        let num_rows = grid.num_rows;
        let num_cols = grid.num_cols;

        let mut vertices = Vec::with_capacity(num_rows * num_cols * 72); // 6 verts * 12 floats

        // Default colors
        let default_fg = [0.9, 0.9, 0.9, 1.0];
        let default_bg = [0.12, 0.12, 0.14, 1.0];

        // Cursor color (bright green)
        let cursor_color = [0.2, 0.8, 0.4, 1.0];

        for row in 0..num_rows {
            for col in 0..num_cols {
                let cell = grid.cell(row, col);

                // Skip wide char spacers (rendered as part of the preceding cell)
                if cell.flags.contains(CellFlags::WIDE_SPACER) {
                    continue;
                }

                let x = col as f32 * cw;
                let y = row as f32 * ch;

                // Determine cell colors
                let fg = color_to_normalized(cell.fg, default_fg);
                let bg = color_to_normalized(cell.bg, default_bg);

                // Check if this is the cursor position
                let is_cursor = row == cursor.row && col == cursor.col;

                // Look up glyph UV
                let ch_char = if cell.character == '\0' || cell.flags.contains(CellFlags::WIDE_SPACER) {
                    ' '
                } else {
                    cell.character
                };

                let (u0, v0, u1, v1) = if let Some(glyph) = self.atlas.get(ch_char) {
                    let (u, v) = glyph.uv_origin;
                    let (uw, vh) = glyph.uv_size;
                    (u, v, u + uw, v + vh)
                } else {
                    // Space: use a blank region in the atlas (character 32)
                    let (u, v) = self.atlas.get(' ').map(|g| g.uv_origin).unwrap_or((0.0, 0.0));
                    let (uw, vh) = self.atlas.get(' ').map(|g| g.uv_size).unwrap_or((0.0, 0.0));
                    (u, v, u + uw, v + vh)
                };

                // Override fg for cursor cell
                let final_fg = if is_cursor {
                    cursor_color
                } else {
                    fg
                };

                // Draw cursor as colored block when on cursor position and cell is empty
                let final_bg = if is_cursor && ch_char == ' ' {
                    cursor_color
                } else {
                    bg
                };

                let x0 = x;
                let y0 = y;
                let x1 = x + cw;
                let y1 = y + ch;

                // Two triangles: TL-BL-BR, TL-BR-TR
                // Each vertex: pos(2) + uv(2) + fg_color(4) + bg_color(4) = 12 floats
                let quad: [[f32; 12]; 6] = [
                    [x0, y0, u0, v0, final_fg[0], final_fg[1], final_fg[2], final_fg[3], final_bg[0], final_bg[1], final_bg[2], final_bg[3]],
                    [x0, y1, u0, v1, final_fg[0], final_fg[1], final_fg[2], final_fg[3], final_bg[0], final_bg[1], final_bg[2], final_bg[3]],
                    [x1, y1, u1, v1, final_fg[0], final_fg[1], final_fg[2], final_fg[3], final_bg[0], final_bg[1], final_bg[2], final_bg[3]],
                    [x0, y0, u0, v0, final_fg[0], final_fg[1], final_fg[2], final_fg[3], final_bg[0], final_bg[1], final_bg[2], final_bg[3]],
                    [x1, y1, u1, v1, final_fg[0], final_fg[1], final_fg[2], final_fg[3], final_bg[0], final_bg[1], final_bg[2], final_bg[3]],
                    [x1, y0, u1, v0, final_fg[0], final_fg[1], final_fg[2], final_fg[3], final_bg[0], final_bg[1], final_bg[2], final_bg[3]],
                ];

                for vertex in &quad {
                    vertices.extend_from_slice(vertex);
                }
            }
        }

        vertices
    }
}

/// Convert a grid Color to normalized RGBA floats.
fn color_to_normalized(color: weft_core::grid::Color, default: [f32; 4]) -> [f32; 4] {
    // Check if it's the default color (meaning "use theme default")
    if color.r == 0 && color.g == 0 && color.b == 0 && color.a == 0 {
        return default;
    }
    [
        color.r as f32 / 255.0,
        color.g as f32 / 255.0,
        color.b as f32 / 255.0,
        color.a as f32 / 255.0,
    ]
}

/// Attach a Metal layer to the winit window's NSView.
///
/// # Safety
/// Caller must ensure `window` is a valid macOS window with an AppKit NSView.
unsafe fn attach_layer_to_nsview(layer: &MetalLayer, window: &Window) {
    let handle = window.window_handle().expect("Failed to get window handle");
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        panic!("Weft requires macOS (AppKit)");
    };

    let ns_view: *mut objc2::runtime::AnyObject = appkit.ns_view.as_ptr().cast();
    let layer_ptr: *mut objc2::runtime::AnyObject =
        (&**layer) as *const _ as *mut objc2::runtime::AnyObject;

    let _: () = msg_send![ns_view, setWantsLayer: true];
    let _: () = msg_send![ns_view, setLayer: layer_ptr];
}
