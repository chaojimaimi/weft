//! Renderer construction: production `new()` + the window-independent
//! `build_paint_core` core + the headless golden-test constructor.
//!
//! v1.11.6 (PLAN_v1116 M3/D-b): split out of metal_backend/mod.rs so the
//! architecture gate (800-line budget, no allowlist entry) stays green —
//! the device/pipeline init core grew past the budget once the headless
//! constructor joined it.

use core_graphics_types::geometry::CGSize;
use metal::{
    CompileOptions, Device, MTLPixelFormat, MTLResourceOptions, MTLVertexFormat, MetalLayer,
    RenderPipelineDescriptor, SamplerDescriptor, VertexDescriptor,
};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use tracing::info;
use winit::window::Window;

use crate::glyph::GlyphAtlas;
use crate::macos_window::{attach_layer_to_nsview, set_layer_opaque};
use crate::paint::grid_cache::BlockLayoutCache;
use crate::paint::metal_backend::LowUsageCounter;
use crate::renderer::MetalRenderer;
use weft_core::config::{FontConfig, Theme};

impl MetalRenderer {
    #[allow(clippy::too_many_arguments)] // mirrors build_paint_core's arity (B2 config gate)
    pub fn new(
        window: &Window,
        font_config: FontConfig,
        theme: Theme,
        minimum_contrast: f32,
        semantic_output_enabled: bool,
        padding_logical: (u32, u32),
        opacity: f32,
        // v1.12.2 B2 (PLAN_S2_render): `[window]
        // presents_with_transaction_live_resize`, injected once — flipping it
        // at runtime requires an app restart (documented on the config key).
        live_resize_flip_enabled: bool,
    ) -> Self {
        // v1.11.6 (PLAN_v1116 M3/D-b): the window-independent construction
        // core was extracted to `build_paint_core`; production `new()` is
        // exactly that core plus the NSView attachment. Only three window
        // dependencies remain here — scale_factor, inner_size and the layer
        // attachment itself (architect-verified list).
        let device = Device::system_default().expect("No Metal device found");
        // v1.11.12 (PLAN_v11112 M-B): metal-device phase boundary. Inert in
        // headless golden tests — the probe's STARTUP_BEGIN OnceLock is only
        // set when WEFT_GUI_PERF_PROBE=1.
        crate::performance_probe::report_phase(crate::performance_probe::StartupPhase::MetalDevice);
        let scale = window.scale_factor();
        let size = window.inner_size();
        let renderer = Self::build_paint_core(
            device,
            font_config,
            theme,
            minimum_contrast,
            semantic_output_enabled,
            padding_logical,
            opacity,
            scale,
            (size.width as f32, size.height as f32),
            live_resize_flip_enabled,
        );
        unsafe {
            attach_layer_to_nsview(&renderer.layer, window, scale);
            // Layer opacity: a non-opaque layer lets the transparent window
            // show the desktop through the (alpha-scaled) cell backgrounds.
            set_layer_opaque(&renderer.layer, renderer.opacity >= 1.0);
        }
        // PLAN_zoom Z-f (Appendix E-2): displayLayer pull wiring runs AFTER
        // the layer attach -- delegate + redraw policy need the live layer.
        // (app_runtime's install_zoom_sequence_hook runs pre-attach and only
        // does the class-level setFrameSize add.)
        crate::macos_zoom::install_display_layer_hook(window);
        // PLAN_zoom appendix I: `zoom:` on the winit window class is
        // replaced by the self-managed animation IMP (220ms, event-loop
        // stepped). Runs here because the NSWindow must exist (same timing
        // constraint as the displayLayer wiring above); class-level and
        // idempotent inside macos_zoom.
        crate::macos_zoom::install_zoom_override_hook(window);
        renderer
    }

    /// v1.11.6 (PLAN_v1116 M3/D-b): window-independent construction core
    /// shared by the production `new()` and the headless golden-test
    /// constructor. Everything that needs the `&Window` (scale factor, inner
    /// size, layer attachment, layer opacity flag) stays at the call sites;
    /// the viewport is passed in physical pixels.
    #[allow(clippy::too_many_arguments)]
    fn build_paint_core(
        device: Device,
        font_config: FontConfig,
        theme: Theme,
        minimum_contrast: f32,
        semantic_output_enabled: bool,
        padding_logical: (u32, u32),
        opacity: f32,
        scale: f64,
        viewport: (f32, f32),
        // v1.12.2 B2 (PLAN_S2_render): config gate for the live-resize
        // present mode (see `live_resize_flip_enabled`).
        live_resize_flip_enabled: bool,
    ) -> Self {
        let queue = device.new_command_queue();
        let font_config = crate::settings_validation::runtime_font_config(&font_config);

        info!("Metal device: {}", device.name());

        // Logical padding (points) → physical pixels for rendering/layout.
        let padding_x = padding_logical.0 as f32 * scale as f32;
        let padding_y = padding_logical.1 as f32 * scale as f32;
        let opacity = crate::settings_validation::runtime_opacity(opacity);

        // Use physical pixels for viewport to stay consistent with drawable_size
        // and grid dimensions (which are calculated from physical cell sizes).
        let vp_w = viewport.0;
        let vp_h = viewport.1;

        // Build glyph atlas with CJK support
        let atlas = GlyphAtlas::new(&device, &font_config, scale);
        // v1.11.12 (PLAN_v11112 M-B): atlas phase boundary.
        crate::performance_probe::report_phase(crate::performance_probe::StartupPhase::Atlas);

        info!(
            "Window: {}x{} physical ({}x scale), viewport: {}x{} physical, atlas cells: {}x{}",
            vp_w, vp_h, scale, vp_w, vp_h, atlas.cell_width, atlas.cell_height
        );

        // Compile shaders with DEBUGGING MODE
        // Change DEBUG_MODE to 0-5 to test different rendering paths
        // 0 = normal texture rendering
        // 1 = solid red (test if fragment shader runs)
        // 2 = texture alpha as white (test if texture sampling works)
        // 3 = position as color (test if vertices pass position)
        // 4 = UV as color (test if vertices pass UVs)
        // 5 = background color only
        let source = include_str!("../text_shader.metal");

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

        // v1.4.2 Phase B3: Background-stream pipeline. Same instanced quad
        // topology (corner derived from vertex_id, instance buffer at slot 2)
        // but uses `bg_vertex`/`bg_fragment` — no atlas sampling, just solid
        // color quads. 8-float instances (origin+size+bg = 32B). Drawn before
        // the glyph stream so text renders on top.
        let bg_vertex_fn = library.get_function("bg_vertex", None).unwrap();
        let bg_fragment_fn = library.get_function("bg_fragment", None).unwrap();
        let bg_desc = RenderPipelineDescriptor::new();
        bg_desc.set_vertex_function(Some(&bg_vertex_fn));
        bg_desc.set_fragment_function(Some(&bg_fragment_fn));
        let bg_color_att = bg_desc.color_attachments().object_at(0).unwrap();
        bg_color_att.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        bg_color_att.set_blending_enabled(true);
        bg_color_att.set_source_rgb_blend_factor(metal::MTLBlendFactor::SourceAlpha);
        bg_color_att.set_destination_rgb_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);
        bg_color_att.set_source_alpha_blend_factor(metal::MTLBlendFactor::SourceAlpha);
        bg_color_att.set_destination_alpha_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);
        let bg_pipeline = device
            .new_render_pipeline_state(&bg_desc)
            .expect("Failed to create bg render pipeline");

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
        // Metal uses the actual backing store size for rendering.
        // v1.11.6 (PLAN_v1116 M3/D-b): the layer is created UNATTACHED here.
        // `new()` attaches it to the NSView afterwards; the headless golden
        // constructor keeps it unattached (vertex builds never present).
        let layer = MetalLayer::new();
        layer.set_device(&device);
        layer.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        layer.set_presents_with_transaction(false);
        layer.set_maximum_drawable_count(3);
        layer.set_drawable_size(CGSize::new(vp_w as f64, vp_h as f64));

        info!(
            "Renderer initialized: {}x{} @ {}x scale, {}x{} cells",
            vp_w, vp_h, scale, atlas.cell_width, atlas.cell_height
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
            minimum_contrast: crate::settings_validation::runtime_minimum_contrast(
                minimum_contrast,
            ),
            semantic_output_enabled,
            // v1.11.3 (PLAN_v1113 §3.3): default false; the setter is wired
            // through config_controller.apply_config on config/profile change.
            bold_is_bright: false,
            padding_x,
            padding_y,
            opacity,
            hit_regions: Vec::new(),
            popup_width_scale: 0.6,
            popup_max_rows: 16, // B1: see InteractionState::new

            context_menu_target: None,
            layout_ctx: None,
            find_state: None,
            note_editor_state: None,
            panel_highlight: None,
            block_hovered: None,
            block_selected: None,
            block_action_hovered: None,
            bookmarked_blocks: std::sync::Arc::new(std::collections::HashSet::new()),
            block_diagnose_state: std::collections::HashMap::new(),
            ai_configured: false,
            spinner_phase: -1.0,
            reduce_motion: false,
            increase_contrast: false,
            sidebar_width_override: None,
            config_status_hint: None,
            paste_toast: None,
            cursor_blink_on: true,
            block_layout_cache: RefCell::new(BlockLayoutCache::default()),
            live_layout_cache: RefCell::new(crate::paint::live_cache::LiveLayoutCache::default()),
            resize_present_probe: std::cell::Cell::new(None),
            // v1.11.6 (PLAN_v1116 M2): polled per-frame by the redraw
            // controller; flipped through `set_live_resize` (runtime.rs).
            live_resize_active: false,
            // v1.12.2 B2 (PLAN_S2_render): config-gated rollback carrier +
            // its flush observability counter (tests assert the gate).
            live_resize_flip_enabled,
            core_animation_flushes: Cell::new(0),
            // v1.12.2 B3-2/B3-3 (PLAN_S2_render): background-pane row caches
            // + the drag-time warmup watermark table (cleared on atlas
            // rebuilds by update_scale/rebuild_atlas).
            background_grid_row_caches: RefCell::new(HashMap::new()),
            background_grid_generation: Cell::new(1),
            block_scan_watermarks: RefCell::new(
                crate::renderer::atlas_warmup::BlockScanWatermarks::default(),
            ),
            scroll_metrics_memo: Cell::new(None),
            cached_scroll_metrics: Cell::new(None),
            cached_panel_scroll_metrics: Cell::new(None),
            styled_lookup_counter: Cell::new(0),
            styled_paint_us_counter: Cell::new(0),
            grid_build_us_counter: Cell::new(0),
            styled_line_cache: RefCell::new(crate::paint::styled_line_cache::StyledLineCache::new()),
            last_expanded_block_count: Cell::new(0),
            block_view_tui_caret_area: Cell::new(None),
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
            upload_low_usage_frames: [LowUsageCounter::new(), LowUsageCounter::new()],
            instanced_pipeline,
            index_buffer,
            instance_ring: RefCell::new(Vec::new()),
            instance_ring_idx: Cell::new(0),
            instance_capacity: Cell::new(0),
            // v1.4.2 Phase B3: background-stream pipeline + ring.
            bg_stream: crate::paint::metal_backend::BgStream {
                pipeline: bg_pipeline,
                ring: RefCell::new(Vec::new()),
                ring_idx: Cell::new(0),
                capacity: Cell::new(0),
                low_usage_frames: LowUsageCounter::new(),
            },
            frame_trace: RefCell::new(crate::frame_trace::FrameTraceRecorder::disabled()),
            frame_id: Cell::new(0),
            pane_instance_ranges: RefCell::new(Vec::new()),
            pane_vertex_ranges: RefCell::new(Vec::new()),
        }
    }

    /// v1.11.6 (PLAN_v1116 M3/D-b): headless renderer for vertex golden
    /// tests. Shares `build_paint_core` with production `new()` — the layer
    /// stays an unattached standalone CAMetalLayer (golden tests only build
    /// CPU vertices, never present), scale is fixed at 1.0 and the viewport
    /// at 840x600 so captured bytes are stable across running machines.
    ///
    /// `layout_ctx` is filled through the SAME production draw()-entry chain
    /// (`terminal_layout_for_renderer(...).layout_ctx()`) — `build_block_view_vertices`
    /// panics on a missing layout_ctx (block_view.rs:87), and re-deriving
    /// the geometry in the test would silently fork the pixel math.
    #[cfg(test)]
    pub(crate) fn new_headless_paint(theme: Theme) -> Self {
        let device = Device::system_default().expect("No Metal device found");
        let mut renderer = Self::build_paint_core(
            device,
            weft_core::config::FontConfig::default(),
            theme,
            4.5,
            false,
            (8, 8),
            1.0,
            1.0,
            (840.0, 600.0),
            // B2 (PLAN_S2_render): headless tests exercise the ROLLBACK
            // CARRIER (flip + flush) side of the config gate — the disabled
            // side is covered by setting `live_resize_flip_enabled = false`
            // on a headless renderer (the `true` below mirrors a config with the key explicitly set; the default/absent key mirrors `false`).
            true,
        );
        // v1.11.6 (PLAN_v1116 M3 / architect P0-2): the production draw()
        // entry builds LayoutCtx via terminal_layout_for_renderer +
        // layout_ctx(); golden builds must resolve the same geometry (no
        // sidebar → chrome_left 0, exactly the closed-sidebar production
        // case). Fixes are intentionally literal: a changed viewport or
        // cell size above simply re-captures the goldens.
        let layout = crate::terminal_geometry::terminal_layout_for_renderer(
            &renderer,
            winit::dpi::PhysicalSize::new(840u32, 600u32),
            0.0,
        );
        renderer.layout_ctx = Some(layout.layout_ctx());
        renderer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// v1.11.6 (PLAN_v1116 M2 §7): `set_live_resize` flips the renderer flag
    /// and the layer's `presentsWithTransaction` property in lockstep, and
    /// early-returns when the value is unchanged (the renderer polls it every
    /// frame). Uses the M3 headless constructor (same `build_paint_core` as
    /// production; the layer needs no NSView for the property getter).
    #[test]
    fn live_resize_setter_flips_layer_transaction_mode() {
        // Mirror the golden skip precedent: no Metal device (CI without GPU)
        // skips instead of failing.
        let Some(_device) = Device::system_default() else {
            eprintln!("skipping live-resize setter test: no Metal device available");
            return;
        };
        let mut renderer = MetalRenderer::new_headless_paint(weft_core::config::Theme::weft_dark());
        assert!(!renderer.live_resize_active, "init false");
        assert!(
            !renderer.layer.presents_with_transaction(),
            "layer starts in async-present mode"
        );

        renderer.set_live_resize(true);
        assert!(renderer.live_resize_active);
        assert!(
            renderer.layer.presents_with_transaction(),
            "active live resize must flip the layer property"
        );

        // Same value → early return; neither the flag nor the property flips.
        renderer.set_live_resize(true);
        assert!(renderer.live_resize_active);
        assert!(renderer.layer.presents_with_transaction());

        renderer.set_live_resize(false);
        assert!(!renderer.live_resize_active);
        assert!(
            !renderer.layer.presents_with_transaction(),
            "resize end must restore async present"
        );

        // Early-return on the way down too.
        renderer.set_live_resize(false);
        assert!(!renderer.live_resize_active);
        assert!(!renderer.layer.presents_with_transaction());
    }

    // ── v1.12.2 B2 (PLAN_S2_render): config-gated live-resize present mode ──
    //
    // G-B bidirectional switch tests. The headless constructor injects
    // `live_resize_flip_enabled = true` (the v1.11.6 rollback carrier) so the
    // flip test above keeps covering the restored path; the OFF side (the
    // new default) mirrors a config without the key by clearing the field.

    /// Config OFF (default): `set_live_resize(true)` must NOT engage present
    /// mode — the flag stays false, the layer keeps
    /// `presentsWithTransaction = NO`, and the transaction flush is skipped
    /// (observable via `core_animation_flushes` staying at zero even when
    /// the encode paths call the flush hook every frame).
    #[test]
    fn live_resize_config_gate_off_keeps_layer_no_and_never_flushes() {
        let Some(_device) = Device::system_default() else {
            eprintln!("skipping live-resize gate-off test: no Metal device available");
            return;
        };
        let mut renderer = MetalRenderer::new_headless_paint(weft_core::config::Theme::weft_dark());
        renderer.live_resize_flip_enabled = false; // mirrors default config

        renderer.set_live_resize(true);
        assert!(
            !renderer.live_resize_active,
            "gate off: present mode must never engage"
        );
        assert!(
            !renderer.layer.presents_with_transaction(),
            "gate off: layer must stay in async-present mode"
        );

        // Simulate the two per-frame flush call sites (metal_backend
        // mod.rs:187/:452): both must be no-ops under the gate.
        renderer.flush_core_animation_if_live_resize();
        renderer.flush_core_animation_if_live_resize();
        assert_eq!(
            renderer.core_animation_flushes.get(),
            0,
            "gate off: CATransaction::flush must never run"
        );

        // Unchanged-value early return stays safe with the gate off.
        renderer.set_live_resize(true);
        assert!(!renderer.live_resize_active);
        assert!(!renderer.layer.presents_with_transaction());
    }

    /// Drag semantics with the switch ON are unchanged by the zoom channel:
    /// enabled && active still flushes, and channels-off still does not.
    #[test]
    fn drag_flush_with_switch_on_survives_the_zoom_channel() {
        let Some(_device) = Device::system_default() else {
            eprintln!("skipping drag-flush survival test: no Metal device available");
            return;
        };
        let mut renderer = MetalRenderer::new_headless_paint(weft_core::config::Theme::weft_dark());
        assert!(renderer.live_resize_flip_enabled);
        assert_eq!(renderer.core_animation_flushes.get(), 0);

        renderer.set_live_resize(true);
        renderer.flush_core_animation_if_live_resize();
        assert_eq!(
            renderer.core_animation_flushes.get(),
            1,
            "drag with the switch on: flush unchanged"
        );
    }

    /// Config ON (`presents_with_transaction_live_resize = true`): the
    /// v1.11.6 rollback carrier is restored — present mode engages, the
    /// layer flips, and the transaction flush runs while active (counter
    /// moves), stopping again when the resize ends.
    #[test]
    fn live_resize_config_gate_on_restores_flip_and_flush() {
        let Some(_device) = Device::system_default() else {
            eprintln!("skipping live-resize gate-on test: no Metal device available");
            return;
        };
        let mut renderer = MetalRenderer::new_headless_paint(weft_core::config::Theme::weft_dark());
        assert!(
            renderer.live_resize_flip_enabled,
            "headless constructor injects the rollback-carrier side"
        );
        assert_eq!(renderer.core_animation_flushes.get(), 0);

        renderer.set_live_resize(true);
        assert!(renderer.live_resize_active);
        assert!(renderer.layer.presents_with_transaction());

        renderer.flush_core_animation_if_live_resize();
        assert_eq!(
            renderer.core_animation_flushes.get(),
            1,
            "gate on + active: flush must run"
        );

        // Resize ended: present mode disengages and the flush no-ops again.
        renderer.set_live_resize(false);
        renderer.flush_core_animation_if_live_resize();
        assert_eq!(
            renderer.core_animation_flushes.get(),
            1,
            "no flush once live resize ended"
        );
    }
}
