//! Metal backend: device/pipeline initialization and frame encoding.
//!
//! Extracted from renderer.rs (M4 step 3). Contains Metal device setup,
//! shader compilation, pipeline creation, buffer ring management, and
//! the render pass encoding epilogue. No business types (Block/Settings/etc).

use core_graphics_types::geometry::CGSize;
use metal::{
    CompileOptions, Device, MTLClearColor, MTLIndexType, MTLLoadAction, MTLPixelFormat,
    MTLPrimitiveType, MTLResourceOptions, MTLStoreAction, MTLVertexFormat, MetalLayer,
    RenderPassDescriptor, RenderPipelineDescriptor, SamplerDescriptor, VertexDescriptor,
};
use std::cell::{Cell, RefCell};
use tracing::info;
use winit::window::Window;

use crate::glyph::GlyphAtlas;
use crate::macos_window::{attach_layer_to_nsview, set_layer_opaque};
use crate::paint::grid_cache::BlockLayoutCache;
use crate::renderer::MetalRenderer;
use weft_core::config::{FontConfig, Theme};

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
        let font_config = crate::settings_validation::runtime_font_config(&font_config);

        info!("Metal device: {}", device.name());

        let scale = window.scale_factor();
        let size = window.inner_size();

        // Logical padding (points) → physical pixels for rendering/layout.
        let padding_x = padding_logical.0 as f32 * scale as f32;
        let padding_y = padding_logical.1 as f32 * scale as f32;
        let opacity = crate::settings_validation::runtime_opacity(opacity);

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
        let source = include_str!("text_shader.metal");

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
            block_hovered: None,
            spinner_phase: -1.0,
            reduce_motion: false,
            increase_contrast: false,
            sidebar_width_override: None,
            cursor_blink_on: true,
            block_layout_cache: RefCell::new(BlockLayoutCache::default()),
            cached_scroll_metrics: Cell::new(None),
            cached_panel_scroll_metrics: Cell::new(None),
            styled_lookup_counter: Cell::new(0),
            styled_paint_us_counter: Cell::new(0),
            last_expanded_block_count: Cell::new(0),
            grid_row_cache: RefCell::new(Vec::new()),
            force_full_grid: Cell::new(true),
            prev_cursor_row: Cell::new(None),
            prev_cursor_col: Cell::new(None),
            // Start true so the first frame forces a rebuild (no previous
            // cursor state to compare against).
            prev_show_cursor: Cell::new(true),
            instances_unchanged: Cell::new(false),
            prev_show_blocks: Cell::new(false),
            prev_primary_screen_row_start: Cell::new(None),
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
            frame_trace: RefCell::new(crate::frame_trace::FrameTraceRecorder::disabled()),
            frame_id: Cell::new(0),
        }
    }

    /// v1.0 P0-c: Ensure the offscreen texture exists and matches the current
    /// viewport size. Recreates the texture on resize. Returns true if the
    /// texture is usable (false on first frame or after a failed allocation).
    pub(crate) fn ensure_offscreen_texture(&self) -> bool {
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

    /// v1.0 P1.5-B1: Render-pass encoding epilogue extracted from `draw()`.
    /// Handles the early-exit idle path, vertex/instance buffer ring uploads,
    /// GPU scroll blit, render pass setup, draw calls, offscreen → drawable
    /// blit, and present. Called at the end of `draw()` with the accumulated
    /// vertex/instance buffers and frame parameters.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn encode_and_present(
        &self,
        drawable: &metal::MetalDrawableRef,
        vertices: &[f32],
        instances: &[f32],
        clear_color: (f64, f64, f64, f64),
        drawable_tex_size: (f32, f32),
        vp_mismatch: bool,
        view_switched: bool,
    ) {
        let (bg_r, bg_g, bg_b, clear_a) = clear_color;

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
            // R3 task 6: stamp frame id + mark ENCODE end on the idle path too.
            // Always set_label (even when frame_id == 0): metal 0.29's
            // CommandBufferRef::label() calls nsstring_as_str which panics on
            // a null NSString via slice::from_raw_parts precondition. Without
            // set_label the label is null, and register_gpu_completion_handler
            // below reads .label() to parse the frame id.
            command_buffer.set_label(&format!("weft-frame-{}", self.frame_id.get()));
            self.frame_trace.borrow_mut().encode_end();
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
            register_gpu_completion_handler(command_buffer);
            command_buffer.commit();
            return;
        }

        // v1.0 P1.5-B0: Upload vertex buffer via triple-buffered ring.
        // Avoids per-frame `new_buffer_with_data` allocation (~540KB/frame).
        // Reuses buffers across frames; only allocates when capacity is
        // exceeded (e.g. on first frame or after resize to a larger grid).
        // v1.0 P1.5-B1: skip upload when `vertices` is empty (grid view with
        // no overlays); only the instance buffer is uploaded in that case.
        let vertex_data_size = std::mem::size_of_val(vertices) as u64;
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
        let instance_data_size = std::mem::size_of_val(instances) as u64;
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
        // R3 task 6: stamp the frame id on the command buffer label so the
        // async add_completed_handler can correlate GPU completion. Always
        // set_label (even when frame_id == 0): metal 0.29's label() panics on
        // null NSString via slice::from_raw_parts precondition, and
        // register_gpu_completion_handler below reads .label() unconditionally
        // when the gpu probe is installed (main.rs installs it at startup).
        command_buffer.set_label(&format!("weft-frame-{}", self.frame_id.get()));

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
        // R3 task 6: ENCODE segment ends here (just before commit). Register a
        // GPU-completion handler so we can correlate the async GPU finish back
        // to this frame's id. The handler captures the submit timestamp and
        // posts FrameGpuComplete to the global channel drained on the main
        // thread next frame. Triple-buffering means this lands 1–2 frames late.
        self.frame_trace.borrow_mut().encode_end();
        register_gpu_completion_handler(command_buffer);
        command_buffer.commit();
    }
}

/// Register an `add_completed_handler` on the command buffer that measures
/// GPU elapsed time and posts it to the global frame-trace channel. No-op when
/// the channel was never installed (probe disabled — the tx is `None`).
///
/// The `block::Block` must be `'static + Send` because Metal invokes it on an
/// internal thread; we therefore capture only the submit `Instant` and the
/// `frame_id`, never `&self`.
fn register_gpu_completion_handler(command_buffer: &metal::CommandBufferRef) {
    let Some(tx) = crate::frame_trace::gpu_completion_tx() else {
        return;
    };
    let submitted = std::time::Instant::now();
    let frame_id = command_buffer
        .label()
        .strip_prefix("weft-frame-")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    // ConcreteBlock::new wraps the closure; .copy() moves it to the heap as an
    // RcBlock that derefs to Block<...>, which is what add_completed_handler
    // expects. The block must outlive this call site — Metal retains it until
    // the GPU invokes it, and the closure is 'static (captures only owned data).
    let block = block::ConcreteBlock::new(move |_cmd_buf: &metal::CommandBufferRef| {
        let gpu_us = submitted.elapsed().as_micros() as u64;
        // Channel send errors (receiver dropped during shutdown) are ignored —
        // the trace is best-effort and must never panic the GPU thread.
        let _ = tx.send(crate::frame_trace::FrameGpuComplete { frame_id, gpu_us });
    })
    .copy();
    command_buffer.add_completed_handler(&block);
}
