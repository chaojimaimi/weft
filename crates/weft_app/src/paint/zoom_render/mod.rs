//! Zoom frame cache: pull-mode content supply for the `displayLayer:` path
//! (PLAN_zoom_sequence_ownership Appendix E, phase A -- Z-f).
//!
//! During the system zoom animation the compositor stretches the LAST
//! presented frame (push model) into every interpolated bounds -- that
//! stretching is the residual font distortion no main-thread render order can
//! reach (E-0). The fix is Warp's pull model: a `displayLayer:` callback
//! renders CURRENT content at the requested size synchronously. The callback
//! can never reach `App` / `MetalRenderer` (both are owned by `run_app`'s
//! `&mut`; aliasing them is UB -- the tab.rs raw-pointer bridge lesson), so
//! everything the lean encode needs is handed off into this global cache
//! every frame from `encode_and_present`:
//!
//! - content: the three vertex/instance streams + clear color + the frame's
//!   viewport + per-pane ranges (moved with `mem::take`, zero copy);
//! - handles: the Metal objects the lean encode binds. Session-invariant ones
//!   (layer/queue/device/pipelines/sampler/index buffer) are retained once at
//!   registration; the atlas textures refresh on EVERY stashed frame (H-1:
//!   `update_scale` / `rebuild_atlas` replace the whole GlyphAtlas, so a
//!   once-retained texture would pair NEW stream UVs with the OLD atlas --
//!   garbled glyphs for the rest of the session);
//! - watermark: the drawable size of the last presented frame (dedup).
//!
//! Threading: every writer and reader runs on the main thread (the encode
//! epilogue and AppKit's CA flow are the same thread). The `Mutex` exists to
//! cross Rust's static/borrow boundaries, not for contention. metal-rs
//! declares its owned handle types `Send + Sync` at the crate level
//! (metal-0.29.0 lib.rs `foreign_obj_type!` -> `foreign_type! ... Sync +
//! Send`), so the handles are stored directly -- no manual Send wrapper -- and
//! *usage* stays main-thread-exclusive by construction (same discipline as
//! the rest of the renderer).

use std::sync::atomic::{AtomicBool, Ordering};
// (cache.rs owns the FRAME_CACHE Mutex now; this module's only Mutex use is
// the test-only global-state lock below.)
#[cfg(test)]
use std::sync::Mutex;
use std::time::{Duration, Instant};

use metal::{
    MTLClearColor, MTLLoadAction, MTLPrimitiveType, MTLResourceOptions, MTLScissorRect,
    MTLStoreAction, MTLViewport, RenderPassDescriptor,
};

use crate::renderer::MetalRenderer;

// Appendix F-5 split rule ("over budget -> split, no ceiling raise"): the
// pure zoom decision layer (suppression gate / expiry decision) lives in
// policy.rs with its truth tables; the zoom-window watch/verdict state
// lives in verdict.rs; the FrameCache storage + stash/tx-bind plumbing
// lives in cache.rs (v1.15.0 T4 governance move). This module: Metal/zoom
// control plumbing.
mod policy;
pub(crate) use policy::{
    cascade_force_commit, pull_can_freshen, pull_present_allowed, set_zoom_step_wait,
    zoom_control_flow, zoom_flush_action, zoom_step_wait, ZoomControlFlow, ZoomFlushAction,
};
mod verdict;
pub(crate) use verdict::{note_zoom_step, zoom_window_finished};
mod cache;
// Zero-rewrite re-imports: the rest of this module (and the tests) read the
// moved items under their original names, and `pub(crate) use` keeps the
// `zoom_render::stash_frame` path stable for the metal_backend callers.
pub(crate) use cache::stash_frame;
use cache::{expire_stale_tx_bind, lock, set_layer_transaction, FrameCache};

/// `presentsWithTransaction` staleness bound (E-2): the bind state is reset
/// when the IMP exits; this deadline is the fallback for a bind left over
/// from a callback that skipped its unbind.
const TX_BIND_TIMEOUT: Duration = Duration::from_millis(300);

/// Degradation switch (E-5 risk 1): the injected `displayLayer:` IMP cannot
/// be unloaded, so the kill lever for the pull lives here -- off returns the
/// window to the 1.12.10 push-only form.
static PULL_ENABLED: AtomicBool = AtomicBool::new(true);
/// Re-entry guard for the pull path: a CA callback nested inside a present
/// must not re-encode (E-2 step 1).
static PULL_ACTIVE: AtomicBool = AtomicBool::new(false);
/// Live-resize stand-down for the pull (Phase E, PLAN_zoom_drawable_stall):
/// while a drag gesture is active the pull STANDS DOWN — Warp presents once
/// per tick and a second stale-layout pull present is a jitter source. Fed
/// unconditionally by `set_live_resize` (orthogonal to the config gate; a
/// cheap store). Supersedes Phase D's limiter bypass.
static PULL_LIVE_RESIZE_STAND_DOWN: AtomicBool = AtomicBool::new(false);
/// Self-zoom drawing-protection window (Phase F, PLAN_zoom_drawable_stall):
/// true from animation ARM until the flush verdict — the animation AND the
/// ≤300 ms hot tail. Gates the C1 suppression and the C2 pull stand-down;
/// `zoom_anim_active()` alone drops one step early (the F.0 defect).
static SELF_ZOOM_WINDOW: AtomicBool = AtomicBool::new(false);
/// Registration is once-per-session; flipping this back on is `teardown`.
static HANDLES_REGISTERED: AtomicBool = AtomicBool::new(false);
/// Serializes process-global zoom-state tests across MODULE boundaries
/// (rust-reviewer round 7, reproduced flake): both zoom_render's and
/// macos_zoom's global-state tests touch SELF_ZOOM_WINDOW and the anim
/// state, so they must share ONE lock — two module-private locks interleave.
#[cfg(test)]
pub(crate) static GLOBAL_ZOOM_STATE_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn pull_enabled() -> bool {
    PULL_ENABLED.load(Ordering::Acquire)
}

/// Phase E live-resize stand-down: true for the whole live-resize drag
/// gesture (fed from `MetalRenderer::set_live_resize`, unconditionally).
pub(crate) fn set_pull_live_resize_stand_down(active: bool) {
    PULL_LIVE_RESIZE_STAND_DOWN.store(active, Ordering::Release);
}

fn pull_live_resize_stand_down() -> bool {
    PULL_LIVE_RESIZE_STAND_DOWN.load(Ordering::Acquire)
}

/// Phase F window writers/readers (same shape as the PULL_ENABLED pair):
/// opened by `start_zoom_anim` (macos_zoom), closed by `zoom_anim_cancel`
/// and by the flush verdict (`zoom_window_finished`).
pub(crate) fn set_self_zoom_window(active: bool) {
    SELF_ZOOM_WINDOW.store(active, Ordering::Release);
}

pub(crate) fn self_zoom_window() -> bool {
    SELF_ZOOM_WINDOW.load(Ordering::Acquire)
}

/// RAII re-entry guard (truth-table tested): the second `try_enter` while
/// held fails, and dropping the guard releases the slot even on early return.
pub(crate) struct ReentryGuard<'flag>(&'flag AtomicBool);

impl<'flag> ReentryGuard<'flag> {
    pub(crate) fn try_enter(flag: &'flag AtomicBool) -> Option<ReentryGuard<'flag>> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(ReentryGuard(flag))
    }
}

impl Drop for ReentryGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Watermark dedup (E-3 truth table): pull only when the requested drawable
/// size differs from the last presented frame. `None` (never presented)
/// pulls. Difference tolerance is 0.5 physical px, the same sub-pixel slack
/// as the main path's `vp_mismatch` check (LOW-1: sub-px drawable jitter
/// between a stash and the CA callback must not double-present).
pub(crate) fn watermark_requests_pull(last: Option<(f32, f32)>, requested: (f32, f32)) -> bool {
    match last {
        None => true,
        Some((lw, lh)) => (lw - requested.0).abs() > 0.5 || (lh - requested.1).abs() > 0.5,
    }
}

/// True while a pull present is in flight. The `displayLayer:` IMP must check
/// this BEFORE touching the cache (M-1): the Mutex is not reentrant and the
/// lean encode holds it across ObjC calls (`next_drawable`, buffer creation)
/// that can pump a nested CA callback -- a nested `pull_requests_frame` lock
/// would deadlock the main thread.
pub(crate) fn pull_in_progress() -> bool {
    PULL_ACTIVE.load(Ordering::Acquire)
}

/// Resized-branch forced-draw policy after Z-f (E-3): only a true live-resize
/// gesture (a drag) keeps the same-tick synchronous draw. Zoom animation
/// steps and non-gesture nudges are served by the displayLayer pull -- the
/// v4 per-step forced draw was itself what stretched the distortion window
/// (Appendix D-0: a 5-9 ms synchronous draw per animation step).
pub(crate) fn forced_sync_draw_for_resize(in_live_resize: bool) -> bool {
    in_live_resize
}

/// Zoom-jump arming policy (v4, kept unchanged): arm on any size change
/// outside a live-resize gesture. Z-f keeps the arming for observation /
/// degrade marking only -- no per-step draw follows from it anymore.
pub(crate) fn zoom_jump_should_arm(size_changed: bool, in_live_resize: bool) -> bool {
    size_changed && !in_live_resize
}

/// Retain the SESSION-INVARIANT Metal objects the lean encode binds. Called
/// from the `encode_and_present` entry (a cheap flag check afterwards). The
/// atlas textures are deliberately NOT here (H-1): `update_scale` and
/// `rebuild_atlas` replace the whole GlyphAtlas mid-session, so they refresh
/// per stashed frame in `stash_frame` instead. The retained layer outlives a
/// renderer swap; `teardown` (renderer `Drop`) clears everything so no handle
/// dangles (E-1 handle-lifecycle rule).
#[allow(clippy::too_many_arguments)]
pub(crate) fn ensure_handles_registered(
    device: &metal::Device,
    queue: &metal::CommandQueue,
    layer: &metal::MetalLayer,
    legacy_pipeline: &metal::RenderPipelineState,
    instanced_pipeline: &metal::RenderPipelineState,
    bg_pipeline: &metal::RenderPipelineState,
    sampler: &metal::SamplerState,
    index_buffer: &metal::Buffer,
) {
    if HANDLES_REGISTERED.load(Ordering::Acquire) {
        return;
    }
    let mut cache = lock();
    cache.device = Some(device.clone());
    cache.queue = Some(queue.clone());
    cache.layer = Some(layer.clone());
    cache.legacy_pipeline = Some(legacy_pipeline.clone());
    cache.instanced_pipeline = Some(instanced_pipeline.clone());
    cache.bg_pipeline = Some(bg_pipeline.clone());
    cache.sampler = Some(sampler.clone());
    cache.index_buffer = Some(index_buffer.clone());
    HANDLES_REGISTERED.store(true, Ordering::Release);
}

/// Clear the cache and drop the retained handles (renderer teardown). The
/// atlas texture handles do not need special teardown care for correctness:
/// they are refreshed from the live renderer's atlas on every stashed frame
/// (H-1), so they are never older than the last real frame.
pub(crate) fn teardown() {
    *lock() = FrameCache::empty();
    HANDLES_REGISTERED.store(false, Ordering::Release);
}

// SAFETY: `MetalRenderer` owns the CAMetalLayer and every GPU handle the
// cache retains; once it drops, a pull must never touch them again. The impl
// lives here (not renderer.rs) because it belongs to the zoom-render domain
// and renderer.rs sits at its architecture-gate ceiling.
impl Drop for MetalRenderer {
    fn drop(&mut self) {
        teardown();
    }
}

/// Watermark read for the `displayLayer:` IMP (dedup step, E-2 step 2).
pub(crate) fn pull_requests_frame(width: f64, height: f64) -> bool {
    watermark_requests_pull(lock().last_presented, (width as f32, height as f32))
}

/// Bind the cached layer to the CURRENT Core Animation transaction
/// (`setPresentsWithTransaction: YES`, E-2 step 3). Without this the lean
/// present commits independently of the transaction that carries the new
/// bounds and every zoom step keeps one stretched frame (E-2 "pull present
/// semantics"). Typed metal-rs call on the retained layer -- no raw msg_send.
pub(crate) fn bind_pull_transaction() {
    let mut cache = lock();
    expire_stale_tx_bind(&mut cache);
    if set_layer_transaction(&cache, true) {
        cache.tx_bound_since = Some(Instant::now());
    }
}

/// Unbind (E-2 step 5): reset `presentsWithTransaction` when the IMP exits.
pub(crate) fn unbind_pull_transaction() {
    let mut cache = lock();
    if cache.tx_bound_since.take().is_some() {
        set_layer_transaction(&cache, false);
    }
}

/// Pull entry (E-1 / phase B consumer): re-encode the cached frame at the
/// requested drawable size and present it. Returns whether a frame was
/// presented. Never touches `App` / `MetalRenderer`.
pub(crate) fn redraw_cached_frame(width: f64, height: f64) -> bool {
    if !pull_enabled() || !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0
    {
        return false;
    }
    if crate::macos_zoom::zoom_anim_active() || self_zoom_window() || pull_live_resize_stand_down()
    {
        return false; // self-zoom (Phase C/F): the main path is the sole
                      // supplier — the window spans arm→flush, so the ≤300 ms
                      // flush tail stays protected too (Phase F); live resize
                      // (Phase E): Warp presents once per tick — a
                      // stale-layout pull is a jitter source.
    }
    let Some(guard) = ReentryGuard::try_enter(&PULL_ACTIVE) else {
        // Nested CA callback while a present is already in flight: skip.
        return false;
    };
    let presented = present_cached_frame();
    drop(guard);
    if presented {
        verdict::record_presented_pull();
    }
    presented
}

/// The lean encode (E-2 construct list): bind registered pipeline/sampler ->
/// vertex bytes (cached streams) -> viewport/scissor (current drawable) ->
/// draw -> present. Skips the offscreen path and the styled rebuilds -- the
/// cache IS the most recent frame's complete presentation data. The render
/// target is whatever drawable the layer hands out; its texture size is the
/// watermark source of truth (same authority as `drawable_tex_size` in the
/// main path).
fn present_cached_frame() -> bool {
    let mut cache = lock();
    if !cache.pullable() {
        return false;
    }
    if !pull_present_allowed(cache.last_present_at, std::time::Instant::now()) {
        tracing::debug!("inline pull skipped: present-rate throttle");
        return false;
    }
    // Optimistic stamp: the pool debt starts at acquisition, not at encode
    // completion. If the rare post-stamp nil path hits (pool momentarily
    // empty), the next pull waits one interval — a skipped beat, not a
    // regression.
    cache.last_present_at = Some(std::time::Instant::now());
    let Some(handles) = LeanHandles::borrow(&cache) else {
        return false;
    };
    let Some(drawable) = handles.layer.next_drawable() else {
        return false;
    };
    let texture = drawable.texture();
    let target = (texture.width() as f32, texture.height() as f32);
    if target.0 <= 0.0 || target.1 <= 0.0 {
        return false;
    }

    let command_buffer = handles.queue.new_command_buffer();
    let pass_desc = RenderPassDescriptor::new();
    let color_att = pass_desc.color_attachments().object_at(0).unwrap();
    color_att.set_texture(Some(texture));
    color_att.set_load_action(MTLLoadAction::Clear);
    color_att.set_store_action(MTLStoreAction::Store);
    let (bg_r, bg_g, bg_b, clear_a) = cache.clear_color;
    color_att.set_clear_color(MTLClearColor::new(bg_r, bg_g, bg_b, clear_a));

    let encoder = command_buffer.new_render_command_encoder(pass_desc);
    // 1:1 anchoring (E-1): the NDC space is the CACHED viewport, so vertex
    // coordinates map to their original physical positions -- content keeps
    // its cell layout at any window size. Metal clips fragments outside the
    // drawable; the scissor bounds the pass to the drawable.
    let vp_data: [f32; 2] = [cache.viewport.0, cache.viewport.1];
    encoder.set_viewport(MTLViewport {
        originX: 0.0,
        originY: 0.0,
        width: cache.viewport.0 as f64,
        height: cache.viewport.1 as f64,
        znear: 0.0,
        zfar: 1.0,
    });
    encoder.set_scissor_rect(MTLScissorRect {
        x: 0,
        y: 0,
        width: texture.width(),
        height: texture.height(),
    });

    encode_grid_streams(&cache, &handles, encoder, &vp_data, target);
    encode_legacy_vertices(&cache, &handles, encoder, &vp_data, target);
    encoder.end_encoding();

    command_buffer.present_drawable(drawable);
    command_buffer.commit();
    cache.last_presented = Some(target);
    true
}

/// One-shot Metal buffer from cached bytes (the ring buffers' write-combined
/// storage mode; pull frames are rare enough that per-pull allocation is the
/// lean option).
fn bytes_buffer(device: &metal::Device, data: &[f32]) -> Option<metal::Buffer> {
    if data.is_empty() {
        return None;
    }
    Some(device.new_buffer_with_data(
        data.as_ptr() as *const std::os::raw::c_void,
        std::mem::size_of_val(data) as u64,
        MTLResourceOptions::CPUCacheModeWriteCombined,
    ))
}

/// Registered handles borrowed out of the cache for one lean encode (the
/// cache guard outlives the encode, so no refcount churn is needed).
struct LeanHandles<'a> {
    layer: &'a metal::MetalLayer,
    queue: &'a metal::CommandQueue,
    device: &'a metal::Device,
    legacy_pipeline: &'a metal::RenderPipelineState,
    instanced_pipeline: &'a metal::RenderPipelineState,
    bg_pipeline: &'a metal::RenderPipelineState,
    sampler: &'a metal::SamplerState,
    atlas_texture: &'a metal::Texture,
    color_texture: &'a metal::Texture,
    index_buffer: &'a metal::Buffer,
}

impl<'a> LeanHandles<'a> {
    fn borrow(cache: &'a FrameCache) -> Option<Self> {
        Some(Self {
            layer: cache.layer.as_ref()?,
            queue: cache.queue.as_ref()?,
            device: cache.device.as_ref()?,
            legacy_pipeline: cache.legacy_pipeline.as_ref()?,
            instanced_pipeline: cache.instanced_pipeline.as_ref()?,
            bg_pipeline: cache.bg_pipeline.as_ref()?,
            sampler: cache.sampler.as_ref()?,
            atlas_texture: cache.atlas_texture.as_ref()?,
            color_texture: cache.color_texture.as_ref()?,
            index_buffer: cache.index_buffer.as_ref()?,
        })
    }
}

/// Per-pane scissor for the lean encode, clamped to BOTH the cached viewport
/// (the rect coordinate space) AND the current drawable (H-2: zoom-out makes
/// the render target smaller than the cached viewport -- a scissor past the
/// render attachment is Metal validation UB). Same clamp semantics as the
/// main path's `offscreen_blit_size` (metal_backend/mod.rs). Returns
/// `(x, y, width, height)`.
fn lean_pane_scissor(
    rect: [f32; 4],
    viewport: (f32, f32),
    target: (f32, f32),
) -> (u64, u64, u64, u64) {
    let [x0, y0, x1, y1] = rect;
    let sx = x0.max(0.0).min(viewport.0).min(target.0) as u64;
    let sy = y0.max(0.0).min(viewport.1).min(target.1) as u64;
    let sw = (x1 - x0)
        .max(0.0)
        .min(viewport.0 - sx as f32)
        .min(target.0 - sx as f32) as u64;
    let sh = (y1 - y0)
        .max(0.0)
        .min(viewport.1 - sy as f32)
        .min(target.1 - sy as f32) as u64;
    (sx, sy, sw, sh)
}

/// Cached bg + glyph streams, mirroring `draw_grid_instances` (single-pane
/// fast path vs the per-pane scissor loop) against freshly built buffers.
fn encode_grid_streams(
    cache: &FrameCache,
    handles: &LeanHandles<'_>,
    encoder: &metal::RenderCommandEncoderRef,
    vp_data: &[f32; 2],
    target: (f32, f32),
) {
    // Build each stream's buffer only when that stream has content -- the
    // two streams are independent (a grid view frame carries no overlay
    // vertices; a block-view frame may carry no instances).
    let bg_buf = if !cache.bg_stream.is_empty() {
        bytes_buffer(handles.device, &cache.bg_stream)
    } else {
        None
    };
    let glyph_buf = if !cache.glyph_stream.is_empty() {
        bytes_buffer(handles.device, &cache.glyph_stream)
    } else {
        None
    };
    if bg_buf.is_none() && glyph_buf.is_none() {
        return;
    }
    if cache.pane_instance_ranges.is_empty() {
        if let Some(bg_buf) = bg_buf.as_ref() {
            encoder.set_render_pipeline_state(handles.bg_pipeline);
            encoder.set_vertex_buffer(2, Some(bg_buf), 0);
            encoder.set_vertex_bytes(1, 8, vp_data.as_ptr() as *const _);
            encoder.draw_indexed_primitives_instanced(
                MTLPrimitiveType::Triangle,
                6,
                metal::MTLIndexType::UInt16,
                handles.index_buffer,
                0,
                (cache.bg_stream.len() / 8) as u64,
            );
        }
        if let Some(glyph_buf) = glyph_buf.as_ref() {
            encoder.set_render_pipeline_state(handles.instanced_pipeline);
            encoder.set_vertex_buffer(2, Some(glyph_buf), 0);
            encoder.set_vertex_bytes(1, 8, vp_data.as_ptr() as *const _);
            encoder.set_fragment_texture(0, Some(handles.atlas_texture));
            encoder.set_fragment_texture(1, Some(handles.color_texture));
            encoder.set_fragment_sampler_state(0, Some(handles.sampler));
            encoder.draw_indexed_primitives_instanced(
                MTLPrimitiveType::Triangle,
                6,
                metal::MTLIndexType::UInt16,
                handles.index_buffer,
                0,
                (cache.glyph_stream.len() / 16) as u64,
            );
        }
        return;
    }
    for (rect, seg) in &cache.pane_instance_ranges {
        let (sx, sy, sw, sh) = lean_pane_scissor(*rect, cache.viewport, target);
        if sw == 0 || sh == 0 {
            continue;
        }
        encoder.set_scissor_rect(MTLScissorRect {
            x: sx,
            y: sy,
            width: sw,
            height: sh,
        });
        let (bg_start, bg_end) = seg.bg_range;
        if bg_end > bg_start {
            if let Some(bg_buf) = bg_buf.as_ref() {
                encoder.set_render_pipeline_state(handles.bg_pipeline);
                encoder.set_vertex_buffer(2, Some(bg_buf), 0);
                encoder.set_vertex_bytes(1, 8, vp_data.as_ptr() as *const _);
                encoder.draw_indexed_primitives_instanced_base_instance(
                    MTLPrimitiveType::Triangle,
                    6,
                    metal::MTLIndexType::UInt16,
                    handles.index_buffer,
                    0,
                    ((bg_end - bg_start) / 8) as u64,
                    0,
                    (bg_start / 8) as u64,
                );
            }
        }
        let (glyph_start, glyph_end) = seg.glyph_range;
        if glyph_end > glyph_start {
            if let Some(glyph_buf) = glyph_buf.as_ref() {
                encoder.set_render_pipeline_state(handles.instanced_pipeline);
                encoder.set_vertex_buffer(2, Some(glyph_buf), 0);
                encoder.set_vertex_bytes(1, 8, vp_data.as_ptr() as *const _);
                encoder.set_fragment_texture(0, Some(handles.atlas_texture));
                encoder.set_fragment_texture(1, Some(handles.color_texture));
                encoder.set_fragment_sampler_state(0, Some(handles.sampler));
                encoder.draw_indexed_primitives_instanced_base_instance(
                    MTLPrimitiveType::Triangle,
                    6,
                    metal::MTLIndexType::UInt16,
                    handles.index_buffer,
                    0,
                    ((glyph_end - glyph_start) / 16) as u64,
                    0,
                    (glyph_start / 16) as u64,
                );
            }
        }
    }
    // Reset the scissor so the legacy pass is not clipped to the last pane.
    encoder.set_scissor_rect(MTLScissorRect {
        x: 0,
        y: 0,
        width: target.0 as u64,
        height: target.1 as u64,
    });
}

/// Cached overlay/block-view vertex stream, mirroring
/// `draw_legacy_vertex_ranges` (pane ranges + tail).
fn encode_legacy_vertices(
    cache: &FrameCache,
    handles: &LeanHandles<'_>,
    encoder: &metal::RenderCommandEncoderRef,
    vp_data: &[f32; 2],
    target: (f32, f32),
) {
    let Some(vertex_buf) = bytes_buffer(handles.device, &cache.vertices) else {
        return;
    };
    encoder.set_render_pipeline_state(handles.legacy_pipeline);
    encoder.set_vertex_buffer(0, Some(&vertex_buf), 0);
    encoder.set_vertex_bytes(1, 8, vp_data.as_ptr() as *const _);
    encoder.set_fragment_texture(0, Some(handles.atlas_texture));
    encoder.set_fragment_texture(1, Some(handles.color_texture));
    encoder.set_fragment_sampler_state(0, Some(handles.sampler));
    let mut pane_end = 0usize;
    for (rect, range) in &cache.pane_vertex_ranges {
        let (sx, sy, sw, sh) = lean_pane_scissor(*rect, cache.viewport, target);
        pane_end = range.end;
        if sw == 0 || sh == 0 || range.end <= range.start {
            continue;
        }
        encoder.set_scissor_rect(MTLScissorRect {
            x: sx,
            y: sy,
            width: sw,
            height: sh,
        });
        encoder.draw_primitives(
            MTLPrimitiveType::Triangle,
            (range.start / 12) as u64,
            ((range.end - range.start) / 12) as u64,
        );
    }
    let vertex_count = cache.vertices.len() / 12;
    let tail_start = pane_end / 12;
    if tail_start < vertex_count {
        encoder.set_scissor_rect(MTLScissorRect {
            x: 0,
            y: 0,
            width: target.0 as u64,
            height: target.1 as u64,
        });
        encoder.draw_primitives(
            MTLPrimitiveType::Triangle,
            tail_start as u64,
            (vertex_count - tail_start) as u64,
        );
    }
}

#[cfg(test)]
mod tests;
