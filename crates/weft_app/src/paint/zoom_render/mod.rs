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

use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::MutexGuard;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use metal::{
    MTLClearColor, MTLLoadAction, MTLPrimitiveType, MTLResourceOptions, MTLScissorRect,
    MTLStoreAction, MTLViewport, RenderPassDescriptor,
};

use crate::glyph::GlyphAtlas;
use crate::layout::Rect;
use crate::paint::metal_backend::PaneInstanceSegment;
use crate::renderer::MetalRenderer;

// Appendix F-5 split rule ("over budget -> split, no ceiling raise"): the
// pure zoom decision layer (suppression gate / expiry decision / degrade
// verdict) lives in policy.rs with its truth tables; the zoom-window
// watch/verdict state lives in verdict.rs (this module: Metal/cache plumbing).
mod policy;
pub(crate) use policy::{
    cascade_force_commit, defers_main, pull_can_freshen, pull_degrade_verdict,
    pull_present_allowed, zoom_diag_anomalous, zoom_flush_action, ZoomFlushAction,
};
mod verdict;
pub(crate) use verdict::{note_zoom_step, zoom_window_finished};

/// `presentsWithTransaction` staleness bound (E-2): the bind state is reset
/// when the IMP exits; this deadline is the fallback for a bind left over
/// from a callback that skipped its unbind.
const TX_BIND_TIMEOUT: Duration = Duration::from_millis(300);

/// Everything the lean encode needs, handed off frame-by-frame.
pub(crate) struct FrameCache {
    vertices: Vec<f32>,
    bg_stream: Vec<f32>,
    glyph_stream: Vec<f32>,
    clear_color: (f64, f64, f64, f64),
    /// Viewport (physical px) the cached streams were built for. The lean
    /// encode reuses it verbatim as the NDC space: content stays 1:1 anchored
    /// at the content origin (no stretch); area beyond it (zoom-in) shows the
    /// theme background -- the accepted Z-f visual semantics (E-1).
    viewport: (f32, f32),
    pane_instance_ranges: Vec<PaneInstanceSegment>,
    pane_vertex_ranges: Vec<(Rect, Range<usize>)>,
    /// False until the first non-empty frame was handed off. An empty cache
    /// must never pull: presenting empty streams is a bare background flash,
    /// the 1.12.9 whitewash symptom (E-3).
    populated: bool,
    /// Drawable size of the last presented frame (dedup watermark, E-3).
    last_presented: Option<(f32, f32)>,
    /// Instant of the last PULL present (present-rate limiter,
    /// PLAN_zoom_drawable_stall A): the pull may acquire a drawable at most
    /// once per PULL_MIN_INTERVAL. In-memory only; nothing persists it.
    last_present_at: Option<Instant>,
    // Registered handles (retained; main-thread-exclusive use, see module doc).
    device: Option<metal::Device>,
    queue: Option<metal::CommandQueue>,
    layer: Option<metal::MetalLayer>,
    legacy_pipeline: Option<metal::RenderPipelineState>,
    instanced_pipeline: Option<metal::RenderPipelineState>,
    bg_pipeline: Option<metal::RenderPipelineState>,
    sampler: Option<metal::SamplerState>,
    atlas_texture: Option<metal::Texture>,
    color_texture: Option<metal::Texture>,
    index_buffer: Option<metal::Buffer>,
    /// `presentsWithTransaction` bind state (E-2): `Some(bind instant)` while
    /// the cached layer is bound to the current CA transaction.
    tx_bound_since: Option<Instant>,
}

impl FrameCache {
    fn empty() -> Self {
        Self {
            vertices: Vec::new(),
            bg_stream: Vec::new(),
            glyph_stream: Vec::new(),
            clear_color: (0.0, 0.0, 0.0, 1.0),
            viewport: (0.0, 0.0),
            pane_instance_ranges: Vec::new(),
            pane_vertex_ranges: Vec::new(),
            populated: false,
            last_presented: None,
            last_present_at: None,
            device: None,
            queue: None,
            layer: None,
            legacy_pipeline: None,
            instanced_pipeline: None,
            bg_pipeline: None,
            sampler: None,
            atlas_texture: None,
            color_texture: None,
            index_buffer: None,
            tx_bound_since: None,
        }
    }

    /// A pull may only re-present over a real prior frame.
    fn pullable(&self) -> bool {
        self.populated
            && !(self.vertices.is_empty()
                && self.bg_stream.is_empty()
                && self.glyph_stream.is_empty())
    }

    /// Stash rule (E-3): all-empty streams = the idle early-exit path ran --
    /// nothing was re-encoded, so the cache keeps its previous content but
    /// the watermark still advances (the nesting rule: a flush-nested
    /// displayLayer then sees requested == watermark and skips). Any
    /// non-empty stream = a full frame: replace everything. The main path
    /// always has at least one non-empty stream (the idle branch is keyed on
    /// all three being empty), so the two cases are disjoint.
    ///
    /// Returns whether a full frame was stashed (drives the H-1 atlas handle
    /// refresh in `stash_frame`: idle frames must keep the textures paired
    /// with the kept streams).
    #[allow(clippy::too_many_arguments)]
    fn apply_stash(
        &mut self,
        vertices: Vec<f32>,
        bg_stream: Vec<f32>,
        glyph_stream: Vec<f32>,
        clear_color: (f64, f64, f64, f64),
        viewport: (f32, f32),
        pane_instance_ranges: Vec<PaneInstanceSegment>,
        pane_vertex_ranges: Vec<(Rect, Range<usize>)>,
        drawable_tex_size: (f32, f32),
    ) -> bool {
        if vertices.is_empty() && bg_stream.is_empty() && glyph_stream.is_empty() {
            self.last_presented = Some(drawable_tex_size);
            return false;
        }
        self.vertices = vertices;
        self.bg_stream = bg_stream;
        self.glyph_stream = glyph_stream;
        self.clear_color = clear_color;
        self.viewport = viewport;
        self.pane_instance_ranges = pane_instance_ranges;
        self.pane_vertex_ranges = pane_vertex_ranges;
        self.populated = true;
        self.last_presented = Some(drawable_tex_size);
        true
    }
}

static FRAME_CACHE: OnceLock<Mutex<FrameCache>> = OnceLock::new();

fn cache() -> &'static Mutex<FrameCache> {
    FRAME_CACHE.get_or_init(|| Mutex::new(FrameCache::empty()))
}

/// The cache is main-thread-exclusive (see module doc); a poisoned lock still
/// holds valid frame data (stash never panics mid-mutation), so recovery is
/// preferred over pulling the whole path down.
fn lock() -> MutexGuard<'static, FrameCache> {
    cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Degradation switch (E-5 risk 1): the injected `displayLayer:` IMP cannot
/// be unloaded, so the kill lever for the pull lives here -- off returns the
/// window to the 1.12.10 push-only form.
static PULL_ENABLED: AtomicBool = AtomicBool::new(true);
/// Re-entry guard for the pull path: a CA callback nested inside a present
/// must not re-encode (E-2 step 1).
static PULL_ACTIVE: AtomicBool = AtomicBool::new(false);
/// Registration is once-per-session; flipping this back on is `teardown`.
static HANDLES_REGISTERED: AtomicBool = AtomicBool::new(false);

/// Degradation kill lever (E-5 risk 1): the injected IMP cannot be unloaded,
/// so the degrade form (1.12.10 push-only) is one call away. Not wired to
/// config this batch -- carried for the field-run A/B and degrade paths
/// (E-5's "全局 feature 开关"); reads go through `pull_enabled`.
#[allow(dead_code)]
pub(crate) fn set_pull_enabled(enabled: bool) {
    PULL_ENABLED.store(enabled, Ordering::Release);
}

pub(crate) fn pull_enabled() -> bool {
    PULL_ENABLED.load(Ordering::Acquire)
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

/// M-2 leak bound: a bind whose IMP panicked between bind and unbind would
/// otherwise leave the layer on `presentsWithTransaction = true` forever.
/// Swept here (every stashed frame) AND at every re-bind, so the stale bind
/// cannot outlive TX_BIND_TIMEOUT past the last frame.
fn expire_stale_tx_bind(cache: &mut FrameCache) {
    if cache
        .tx_bound_since
        .is_some_and(|since| since.elapsed() > TX_BIND_TIMEOUT)
    {
        set_layer_transaction(cache, false);
        cache.tx_bound_since = None;
    }
}

/// Unbind (E-2 step 5): reset `presentsWithTransaction` when the IMP exits.
pub(crate) fn unbind_pull_transaction() {
    let mut cache = lock();
    if cache.tx_bound_since.take().is_some() {
        set_layer_transaction(&cache, false);
    }
}

/// Typed layer call, unwind-guarded (house rule: every ObjC boundary degrades
/// instead of aborting -- VULN-005). Returns whether a live handle was there.
fn set_layer_transaction(cache: &FrameCache, bound: bool) -> bool {
    let Some(layer) = cache.layer.as_ref() else {
        return false;
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        layer.set_presents_with_transaction(bound);
    }));
    if result.is_err() {
        tracing::error!("setPresentsWithTransaction panicked; pull present degrades to async");
    }
    result.is_ok()
}

/// Hand one encoded frame to the cache (the two `encode_and_present`
/// present->flush gaps). See `FrameCache::apply_stash` for the idle/full
/// frame rule. On a full frame the atlas texture handles are refreshed from
/// the live renderer's atlas (H-1); on an idle frame they stay paired with
/// the kept streams. Also sweeps a stale transaction bind (M-2).
#[allow(clippy::too_many_arguments)]
pub(crate) fn stash_frame(
    vertices: Vec<f32>,
    bg_stream: Vec<f32>,
    glyph_stream: Vec<f32>,
    clear_color: (f64, f64, f64, f64),
    viewport: (f32, f32),
    pane_instance_ranges: Vec<PaneInstanceSegment>,
    pane_vertex_ranges: Vec<(Rect, Range<usize>)>,
    atlas: &GlyphAtlas,
    drawable_tex_size: (f32, f32),
) {
    let mut cache = lock();
    let replaced = cache.apply_stash(
        vertices,
        bg_stream,
        glyph_stream,
        clear_color,
        viewport,
        pane_instance_ranges,
        pane_vertex_ranges,
        drawable_tex_size,
    );
    if replaced {
        // H-1: two refcount bumps per real frame -- negligible against a
        // full stream upload, and the only way the cached textures can never
        // outlive the atlas the streams' UVs were built for.
        cache.atlas_texture = Some(atlas.texture().clone());
        cache.color_texture = Some(atlas.color_texture().clone());
    }
    expire_stale_tx_bind(&mut cache);
}

/// Pull entry (E-1 / phase B consumer): re-encode the cached frame at the
/// requested drawable size and present it. Returns whether a frame was
/// presented. Never touches `App` / `MetalRenderer`.
pub(crate) fn redraw_cached_frame(width: f64, height: f64) -> bool {
    if !pull_enabled() || !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0
    {
        return false;
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
