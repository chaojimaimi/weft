//! Grid instance ring upload + instanced draw, extracted from
//! `metal_backend/mod.rs` for v1.4.2 B1 (headroom for B2/B3 background-run
//! merge).
//!
//! v1.4.2 Phase B3: now uploads + draws TWO streams per frame:
//! - **bg stream**: 8-float instances (origin+size+color) drawn via
//!   `bg_pipeline` (no atlas sampling, solid color quads).
//! - **glyph stream**: 16-float instances (origin/size/uv/fg/bg) drawn via
//!   `instanced_pipeline` (atlas sampling + alpha blend).
//!
//! Per-pane scissor + draw is performed by iterating `pane_instance_ranges`
//! (a `Vec<(Rect, PaneInstanceRanges)>`). For each pane: set scissor, draw
//! bg range via `draw_indexed_primitives_instanced_base_instance`, then
//! draw glyph range. Single-pane tabs take the fast path: one scissor (full
//! viewport), two draw calls (bg + glyph).

use metal::{MTLIndexType, MTLPrimitiveType};

impl super::MetalRenderer {
    /// v1.0 P1.5-B1: Upload glyph-stream instance buffer via triple-buffered
    /// ring. Same pattern as the vertex ring: 3 rotating buffers, grown on
    /// demand, written via copy_nonoverlapping + did_modify_range.
    /// Each instance is 16 floats (64 bytes).
    ///
    /// Returns `(ring_idx, data_size)`. When `data_size == 0`, returns
    /// `(0, 0)` and skips the ring entirely.
    pub(crate) fn upload_instance_ring(&self, instances: &[f32]) -> (usize, u64) {
        let instance_data_size = std::mem::size_of_val(instances) as u64;
        let mut instance_ring_idx: usize = 0;
        {
            let mut ring = self.instance_ring.borrow_mut();
            super::buffer_capacity::resize_ring_for_usage(
                &self.device,
                &mut ring,
                &self.instance_capacity,
                &self.upload_low_usage_frames[1],
                instance_data_size,
            );
        }
        if instance_data_size > 0 {
            let ring = self.instance_ring.borrow_mut();
            instance_ring_idx = self.instance_ring_idx.get();

            let buffer = &ring[instance_ring_idx];
            {
                let ptr = buffer.contents() as *mut u8;
                // SAFETY: two invariants hold.
                // (1) The ring slot being written is not in flight on the
                // GPU: the ring index advances once per upload, so this slot
                // was last handed to the GPU ring.len() uploads ago (frames
                // in flight ≤ ring length), and `did_modify_range` below —
                // not this write — is what publishes the new contents.
                // (2) `resize_ring_for_usage` above guarantees the buffer is
                // at least `instance_data_size` bytes, so the copy stays in
                // bounds of the storage `contents()` points into.
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
        (instance_ring_idx, instance_data_size)
    }

    /// v1.4.2 Phase B3: Upload bg-stream instance buffer via triple-buffered
    /// ring (mirror of `upload_instance_ring` but for 8-float bg instances).
    /// Each instance is 8 floats (32 bytes) — origin(2) + size(2) + bg(4).
    pub(crate) fn upload_bg_ring(&self, instances: &[f32]) -> (usize, u64) {
        let data_size = std::mem::size_of_val(instances) as u64;
        let mut ring_idx: usize = 0;
        {
            let mut ring = self.bg_stream.ring.borrow_mut();
            super::buffer_capacity::resize_ring_for_usage(
                &self.device,
                &mut ring,
                &self.bg_stream.capacity,
                &self.bg_stream.low_usage_frames,
                data_size,
            );
        }
        if data_size > 0 {
            let ring = self.bg_stream.ring.borrow_mut();
            ring_idx = self.bg_stream.ring_idx.get();

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
                // at least `data_size` bytes, so the copy stays in bounds of
                // the storage `contents()` points into.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        instances.as_ptr() as *const u8,
                        ptr,
                        data_size as usize,
                    );
                }
                buffer.did_modify_range(metal::NSRange {
                    location: 0,
                    length: data_size,
                });
            }

            self.bg_stream.ring_idx.set((ring_idx + 1) % ring.len());
        }
        (ring_idx, data_size)
    }

    /// v1.4.2 Phase B3: Draw dual-stream grid instances (bg then glyph).
    /// For each pane segment, sets the scissor rect and issues two
    /// `draw_indexed_primitives_instanced_base_instance` calls — one for
    /// the bg range (8-float stride) using `bg_stream.pipeline`, one for
    /// the glyph range (16-float stride) using `instanced_pipeline`.
    ///
    /// Single-pane fast path: `pane_instance_ranges` is empty → one
    /// scissor (full viewport), two draw calls covering all instances.
    ///
    /// No-op when both `bg_data_size` and `glyph_data_size` are 0 (matches
    /// the original guard in `encode_and_present`).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn draw_grid_instances(
        &self,
        encoder: &metal::RenderCommandEncoderRef,
        bg_instances: &[f32],
        bg_data_size: u64,
        bg_ring_idx: usize,
        glyph_instances: &[f32],
        glyph_data_size: u64,
        glyph_ring_idx: usize,
        vp_data: &[f32; 2],
    ) {
        if bg_data_size == 0 && glyph_data_size == 0 {
            return;
        }

        let ranges = self.pane_instance_ranges.borrow();
        let atlas_tex = self.atlas.texture();
        // v1.10.4: RGBA color atlas for color emoji, sampled at texture(1)
        // when a glyph instance carries the fg.a=2.0 sentinel.
        let color_tex = self.atlas.color_texture();
        let glyph_ring = self.instance_ring.borrow();
        let bg_ring = self.bg_stream.ring.borrow();

        if ranges.is_empty() {
            // Single-pane fast path: no scissor (Metal defaults to full
            // viewport). Two draw calls — bg then glyph.
            if bg_data_size > 0 {
                encoder.set_render_pipeline_state(&self.bg_stream.pipeline);
                encoder.set_vertex_buffer(2, Some(&bg_ring[bg_ring_idx]), 0);
                encoder.set_vertex_bytes(1, 8, vp_data.as_ptr() as *const _);
                let bg_count = bg_instances.len() / 8;
                encoder.draw_indexed_primitives_instanced(
                    MTLPrimitiveType::Triangle,
                    6,
                    MTLIndexType::UInt16,
                    &self.index_buffer,
                    0,
                    bg_count as u64,
                );
            }
            if glyph_data_size > 0 {
                encoder.set_render_pipeline_state(&self.instanced_pipeline);
                encoder.set_vertex_buffer(2, Some(&glyph_ring[glyph_ring_idx]), 0);
                encoder.set_vertex_bytes(1, 8, vp_data.as_ptr() as *const _);
                encoder.set_fragment_texture(0, Some(atlas_tex));
                encoder.set_fragment_texture(1, Some(color_tex));
                encoder.set_fragment_sampler_state(0, Some(&self.sampler));
                let glyph_count = glyph_instances.len() / 16;
                encoder.draw_indexed_primitives_instanced(
                    MTLPrimitiveType::Triangle,
                    6,
                    MTLIndexType::UInt16,
                    &self.index_buffer,
                    0,
                    glyph_count as u64,
                );
            }
        } else {
            for (i, (rect, seg)) in ranges.iter().enumerate() {
                let [x0, y0, x1, y1] = *rect;
                let sx = x0.max(0.0).min(self.viewport.0) as u64;
                let sy = y0.max(0.0).min(self.viewport.1) as u64;
                let sw = (x1 - x0).max(0.0).min(self.viewport.0 - sx as f32) as u64;
                let sh = (y1 - y0).max(0.0).min(self.viewport.1 - sy as f32) as u64;
                if sw == 0 || sh == 0 {
                    tracing::trace!(segment = i, rect = ?rect, "skipping zero-area pane segment");
                    continue;
                }
                encoder.set_scissor_rect(metal::MTLScissorRect {
                    x: sx,
                    y: sy,
                    width: sw,
                    height: sh,
                });

                let bg_start = seg.bg_range.0;
                let bg_end = seg.bg_range.1;
                if bg_end > bg_start {
                    encoder.set_render_pipeline_state(&self.bg_stream.pipeline);
                    encoder.set_vertex_buffer(2, Some(&bg_ring[bg_ring_idx]), 0);
                    encoder.set_vertex_bytes(1, 8, vp_data.as_ptr() as *const _);
                    let bg_start_instance = bg_start / 8;
                    let bg_instance_count = (bg_end - bg_start) / 8;
                    encoder.draw_indexed_primitives_instanced_base_instance(
                        MTLPrimitiveType::Triangle,
                        6,
                        MTLIndexType::UInt16,
                        &self.index_buffer,
                        0,
                        bg_instance_count as u64,
                        0,
                        bg_start_instance as u64,
                    );
                }

                let glyph_start = seg.glyph_range.0;
                let glyph_end = seg.glyph_range.1;
                if glyph_end > glyph_start {
                    encoder.set_render_pipeline_state(&self.instanced_pipeline);
                    encoder.set_vertex_buffer(2, Some(&glyph_ring[glyph_ring_idx]), 0);
                    encoder.set_vertex_bytes(1, 8, vp_data.as_ptr() as *const _);
                    encoder.set_fragment_texture(0, Some(atlas_tex));
                    encoder.set_fragment_texture(1, Some(color_tex));
                    encoder.set_fragment_sampler_state(0, Some(&self.sampler));
                    let glyph_start_instance = glyph_start / 16;
                    let glyph_instance_count = (glyph_end - glyph_start) / 16;
                    encoder.draw_indexed_primitives_instanced_base_instance(
                        MTLPrimitiveType::Triangle,
                        6,
                        MTLIndexType::UInt16,
                        &self.index_buffer,
                        0,
                        glyph_instance_count as u64,
                        0,
                        glyph_start_instance as u64,
                    );
                }

                tracing::trace!(
                    segment = i,
                    rect = ?rect,
                    scissor = ?[sx, sy, sw, sh],
                    bg_instances = (bg_end - bg_start) / 8,
                    glyph_instances = (glyph_end - glyph_start) / 16,
                    "draw dual-stream pane segment"
                );
            }
            // Reset scissor to full viewport so subsequent overlay / block-view
            // draws aren't clipped to the last pane's rect.
            encoder.set_scissor_rect(metal::MTLScissorRect {
                x: 0,
                y: 0,
                width: self.viewport.0 as u64,
                height: self.viewport.1 as u64,
            });
        }
    }
}
