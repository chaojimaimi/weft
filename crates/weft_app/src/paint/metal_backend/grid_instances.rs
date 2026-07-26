//! Grid instance ring upload + instanced draw, extracted from
//! `metal_backend/mod.rs` for v1.4.2 B1 (headroom for B2/B3 background-run
//! merge).
//!
//! Both methods are pure extractions from `encode_and_present` — no behavior
//! change. The ring upload returns `(ring_idx, data_size)` so the caller can
//! pass them to the draw method without recomputing.

use metal::{MTLIndexType, MTLPrimitiveType, MTLResourceOptions};

impl super::MetalRenderer {
    /// v1.0 P1.5-B1: Upload instance buffer via triple-buffered ring.
    /// Same pattern as the vertex ring: 3 rotating buffers, grown on
    /// demand, written via copy_nonoverlapping + did_modify_range.
    /// Each instance is 16 floats (64 bytes) — for a 80×30 grid that's
    /// ~150KB/frame vs the old ~540KB vertex buffer.
    ///
    /// Returns `(instance_ring_idx, instance_data_size)`. When
    /// `instance_data_size == 0` (no instances to upload), returns `(0, 0)`
    /// and skips the ring entirely.
    pub(crate) fn upload_instance_ring(&self, instances: &[f32]) -> (usize, u64) {
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
                // Grow: recreate ALL buffers (see vertex ring comment in
                // metal_backend/mod.rs — only replacing ring[ring_idx] would
                // leave the other two at the old capacity, causing a buffer
                // overflow when the ring rotates to them on subsequent frames).
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
        (instance_ring_idx, instance_data_size)
    }

    /// v1.0 P1.5-B1: Draw 1 — grid instances (instanced pipeline). Each
    /// instance is one cell (or cursor/hyperlink decoration); the static
    /// index buffer + shader-side corner derivation mean only the
    /// instance buffer changes per frame.
    ///
    /// v1.3 Batch 5: when multiple panes are present, instances are split
    /// into per-pane segments (recorded in `pane_instance_ranges`). Each
    /// segment is drawn with its own scissor rect so panes don't bleed
    /// into each other. Single-pane tabs still take the fast path: one
    /// scissor covering the full viewport, one draw call.
    ///
    /// No-op when `instance_data_size == 0` (matches the original guard in
    /// `encode_and_present`).
    pub(crate) fn draw_grid_instances(
        &self,
        encoder: &metal::RenderCommandEncoderRef,
        instances: &[f32],
        instance_data_size: u64,
        instance_ring_idx: usize,
        vp_data: &[f32; 2],
    ) {
        if instance_data_size == 0 {
            return;
        }
        let tex = self.atlas.texture();
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

        // v1.3 Batch 5: per-pane scissor + draw. When the renderer is
        // in single-pane mode (no background panes, block view, or
        // idle frame with no instances built), `pane_instance_ranges`
        // is empty — fall back to the legacy single draw call with no
        // scissor (Metal defaults to the full viewport).
        let ranges = self.pane_instance_ranges.borrow();
        if ranges.is_empty() {
            let instance_count = instances.len() / 16;
            encoder.draw_indexed_primitives_instanced(
                MTLPrimitiveType::Triangle,
                6, // index count (two triangles)
                MTLIndexType::UInt16,
                &self.index_buffer,
                0, // index buffer offset
                instance_count as u64,
            );
        } else {
            for (i, (rect, range)) in ranges.iter().enumerate() {
                let [x0, y0, x1, y1] = *rect;
                // Clamp to drawable bounds — Metal panics if scissor
                // rect extends past the attachment.
                let sx = x0.max(0.0).min(self.viewport.0) as u64;
                let sy = y0.max(0.0).min(self.viewport.1) as u64;
                let sw = (x1 - x0).max(0.0).min(self.viewport.0 - sx as f32) as u64;
                let sh = (y1 - y0).max(0.0).min(self.viewport.1 - sy as f32) as u64;
                if sw == 0 || sh == 0 {
                    tracing::info!(segment = i, rect = ?rect, "skipping zero-area pane segment");
                    continue;
                }
                let start_instance = range.start / 16;
                let instance_count = (range.end - range.start) / 16;
                tracing::info!(
                    segment = i,
                    rect = ?rect,
                    scissor = ?[sx, sy, sw, sh],
                    start_instance,
                    instance_count,
                    "draw pane segment"
                );
                encoder.set_scissor_rect(metal::MTLScissorRect {
                    x: sx,
                    y: sy,
                    width: sw,
                    height: sh,
                });
                if instance_count == 0 {
                    continue;
                }
                encoder.draw_indexed_primitives_instanced_base_instance(
                    MTLPrimitiveType::Triangle,
                    6, // index count (two triangles)
                    MTLIndexType::UInt16,
                    &self.index_buffer,
                    0, // index buffer offset
                    instance_count as u64,
                    0, // base vertex
                    start_instance as u64,
                );
            }
            // Reset scissor to full viewport so subsequent overlay /
            // block-view draws aren't clipped to the last pane's rect.
            encoder.set_scissor_rect(metal::MTLScissorRect {
                x: 0,
                y: 0,
                width: self.viewport.0 as u64,
                height: self.viewport.1 as u64,
            });
        }
    }
}
