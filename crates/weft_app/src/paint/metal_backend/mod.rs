//! Metal backend: frame encoding and per-pane pipeline state.
//!
//! Extracted from renderer.rs (M4 step 3). Contains buffer ring management
//! and the render pass encoding epilogue. No business types (Block/Settings/etc).
//! v1.11.6 (PLAN_v1116 M3/D-b): renderer construction (device/shaders/
//! pipelines/atlas + the headless golden constructor) moved to
//! `constructor.rs` to stay under the 800-line gate; the live-resize
//! setter test lives there too.

use metal::{MTLClearColor, MTLLoadAction, MTLPixelFormat, MTLStoreAction, RenderPassDescriptor};
use std::cell::{Cell, RefCell};

use crate::renderer::MetalRenderer;

mod buffer_capacity;
mod constructor;
mod grid_instances;
pub(crate) use buffer_capacity::LowUsageCounter;

/// v1.4.2 Phase B3: Per-pane dual-stream ranges paired with the pane's
/// scissor rect. The Metal backend iterates this list to issue per-pane
/// scissor + bg draw + glyph draw without re-deriving counts via `/16`/`/8`.
/// Reuses B2's `PaneInstanceRanges` for the float offsets (Copy + Default
/// via `(usize, usize)` tuples — `Range<usize>` is not `Default`).
pub(crate) type PaneInstanceSegment = (
    crate::layout::Rect,
    crate::paint::grid_instances::PaneInstanceRanges,
);

/// v1.4.2 Phase B3: Background-stream Metal state. Groups the bg pipeline
/// (no atlas sampling) + triple-buffered instance ring. Kept as a single
/// field on `MetalRenderer` to avoid bloating the struct with 4 separate
/// fields (renderer.rs is at the architecture-gate ceiling).
pub(crate) struct BgStream {
    pub pipeline: metal::RenderPipelineState,
    pub ring: RefCell<Vec<metal::Buffer>>,
    pub ring_idx: Cell<usize>,
    pub capacity: Cell<u64>,
    pub low_usage_frames: LowUsageCounter,
}

// v1.11.6: the constructor impl (new/build_paint_core/new_headless_paint)
// moved to `constructor.rs`; this impl holds the frame-encoding methods.
impl MetalRenderer {
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
        bg_instances: &[f32],
        glyph_instances: &[f32],
        clear_color: (f64, f64, f64, f64),
        drawable_tex_size: (f32, f32),
        vp_mismatch: bool,
        view_switched: bool,
    ) {
        let (bg_r, bg_g, bg_b, clear_a) = clear_color;

        // v1.0 P1.5-B1: early-exit only when BOTH instances (grid) and
        // vertices (overlays / block view) are empty. In grid view with no
        // overlays, `vertices` is empty but `instances` carries the cells.
        // v1.4.2 Phase B3: dual-stream — check both bg + glyph streams.
        if vertices.is_empty() && bg_instances.is_empty() && glyph_instances.is_empty() {
            // Count fully idle frames toward low-water recovery too. Without
            // this, switching away from a one-off huge BlockView frame could
            // retain its upload rings forever because this fast path returns
            // before the normal upload maintenance below.
            {
                let mut ring = self.vertex_buffer_ring.borrow_mut();
                buffer_capacity::resize_ring_for_usage(
                    &self.device,
                    &mut ring,
                    &self.vertex_buffer_capacity,
                    &self.upload_low_usage_frames[0],
                    0,
                );
            }
            {
                let mut ring = self.instance_ring.borrow_mut();
                buffer_capacity::resize_ring_for_usage(
                    &self.device,
                    &mut ring,
                    &self.instance_capacity,
                    &self.upload_low_usage_frames[1],
                    0,
                );
            }
            {
                let mut ring = self.bg_stream.ring.borrow_mut();
                buffer_capacity::resize_ring_for_usage(
                    &self.device,
                    &mut ring,
                    &self.bg_stream.capacity,
                    &self.bg_stream.low_usage_frames,
                    0,
                );
            }
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
            self.flush_core_animation_if_live_resize();
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
        {
            let mut ring = self.vertex_buffer_ring.borrow_mut();
            buffer_capacity::resize_ring_for_usage(
                &self.device,
                &mut ring,
                &self.vertex_buffer_capacity,
                &self.upload_low_usage_frames[0],
                vertex_data_size,
            );
        }
        if vertex_data_size > 0 {
            let ring = self.vertex_buffer_ring.borrow_mut();
            ring_idx = self.vertex_buffer_ring_idx.get();

            // Write vertex data into the current ring buffer.
            let buffer = &ring[ring_idx];
            {
                let ptr = buffer.contents() as *mut u8;
                // SAFETY: two invariants hold.
                // (1) The ring slot being written is not in flight on the
                // GPU: the ring index advances once per upload, so this slot
                // was last handed to the GPU ring.len() uploads ago (frames
                // in flight ≤ ring length), and `did_modify_range` below —
                // not this write — is what publishes the new contents.
                // (2) `resize_ring_for_usage` above guarantees the buffer is
                // at least `vertex_data_size` bytes, so the copy stays in
                // bounds of the storage `contents()` points into.
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
        // Extracted to grid_instances.rs (v1.4.2 B1).
        // v1.4.2 Phase B3: dual-stream — upload bg runs (8 floats each) and
        // glyph instances (16 floats each) to separate rings. The glyph ring
        // reuses `instance_ring` (same 16-float stride as the legacy path).
        let (bg_ring_idx, bg_data_size) = self.upload_bg_ring(bg_instances);
        let (glyph_ring_idx, glyph_data_size) = self.upload_instance_ring(glyph_instances);

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

        // v1.0 P1.5-B1: Draw 1 — grid instances (instanced pipeline).
        // Extracted to grid_instances.rs (v1.4.2 B1).
        // v1.4.2 Phase B3: dual-stream — bg runs (8 floats) drawn first
        // with the bg pipeline (no atlas), then glyph instances (16 floats)
        // with the instanced pipeline (atlas-sampled). Per-pane scissor is
        // applied when `pane_instance_ranges` is non-empty (multi-pane).
        self.draw_grid_instances(
            encoder,
            bg_instances,
            bg_data_size,
            bg_ring_idx,
            glyph_instances,
            glyph_data_size,
            glyph_ring_idx,
            &vp_data,
        );

        // v1.0 P1.5-B1: Draw 2 — overlays / block view (legacy pipeline).
        // Uses the per-vertex descriptor (3×float4, stride 48) + B0 ring.
        if vertex_data_size > 0 {
            let tex = self.atlas.texture();
            // v1.10.4: RGBA color atlas for color emoji (fg.a=2.0 sentinel).
            let color_tex = self.atlas.color_texture();
            encoder.set_render_pipeline_state(&self.pipeline);
            let ring = self.vertex_buffer_ring.borrow();
            // v1.0 P1.5-B0 fix: render the buffer we wrote just this frame
            // (ring[ring_idx]), NOT ring_idx-1 (which is last frame's buffer
            // and caused content-change flicker in block view).
            let cur = ring_idx;
            encoder.set_vertex_buffer(0, Some(&ring[cur]), 0);
            // v1.0 P1.5-B0: set_vertex_bytes for viewport (8 bytes << 4KB limit).
            encoder.set_vertex_bytes(1, 8, vp_data.as_ptr() as *const _);
            // Atlas + sampler may already be bound from the instance draw;
            // rebind defensively in case only this path runs (block view).
            encoder.set_fragment_texture(0, Some(tex));
            encoder.set_fragment_texture(1, Some(color_tex));
            encoder.set_fragment_sampler_state(0, Some(&self.sampler));

            self.draw_legacy_vertex_ranges(encoder, vertices.len());
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
        self.flush_core_animation_if_live_resize();
    }

    /// v1.11.6 (PLAN_v1116 M2 step 4 / architect P1-6): while live-resize
    /// present mode is active, `presentsWithTransaction` commits the drawable
    /// synchronously with the enclosing Core Animation transaction — but the
    /// resized layer bounds and the new pixels only land together if that
    /// transaction is actually flushed this frame. Without the explicit
    /// flush the stretched-frame artifact survives (bounds change and frame
    /// land in different transactions). No-op when no live resize is active.
    /// Typed `objc2-quartz-core` API, unwind-guarded per project rule.
    ///
    /// v1.12.2 B2 (PLAN_S2_render): also gated by the
    /// `presents_with_transaction_live_resize` config switch (default off) —
    /// the synchronous flush waits on the WindowServer for 0-33ms, which
    /// stopped paying for itself once resize commits fell to ~3ms. Every
    /// flush that passes the gate bumps `core_animation_flushes` so tests
    /// can observe the gate end-to-end.
    fn flush_core_animation_if_live_resize(&self) {
        if !self.live_resize_flip_enabled || !self.live_resize_active {
            return;
        }
        self.core_animation_flushes
            .set(self.core_animation_flushes.get().saturating_add(1));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            objc2_quartz_core::CATransaction::flush();
        }));
        // P2-1 (rust-reviewer): a swallowed unwind would silently disable the
        // atomic live-resize commit — leave a trail instead.
        if result.is_err() {
            tracing::warn!(
                "CATransaction::flush panicked; live-resize frame may not commit atomically"
            );
        }
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
