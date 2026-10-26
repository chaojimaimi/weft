//! Zoom frame-cache storage, moved VERBATIM out of `mod.rs` (v1.15.0 T4
//! governance move; the F-5 "over budget -> split, no ceiling raise" rule
//! applied a second time to this module — policy.rs/verdict.rs went first).
//! Zero behavior change: the parent re-imports every item so the
//! `metal_backend` call sites keep the `zoom_render::stash_frame` path.
//! Only mechanical `pub(super)` visibility markers were added — Rust module
//! privacy gives the PARENT no access to a child's private items, and the
//! parent's encode/present plumbing reads FrameCache fields directly.

use std::ops::Range;
use std::sync::MutexGuard;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use crate::glyph::GlyphAtlas;
use crate::layout::Rect;
use crate::paint::metal_backend::PaneInstanceSegment;

/// Everything the lean encode needs, handed off frame-by-frame.
pub(crate) struct FrameCache {
    pub(super) vertices: Vec<f32>,
    pub(super) bg_stream: Vec<f32>,
    pub(super) glyph_stream: Vec<f32>,
    pub(super) clear_color: (f64, f64, f64, f64),
    /// Viewport (physical px) the cached streams were built for. The lean
    /// encode reuses it verbatim as the NDC space: content stays 1:1 anchored
    /// at the content origin (no stretch); area beyond it (zoom-in) shows the
    /// theme background -- the accepted Z-f visual semantics (E-1).
    pub(super) viewport: (f32, f32),
    pub(super) pane_instance_ranges: Vec<PaneInstanceSegment>,
    pub(super) pane_vertex_ranges: Vec<(Rect, Range<usize>)>,
    /// False until the first non-empty frame was handed off. An empty cache
    /// must never pull: presenting empty streams is a bare background flash,
    /// the 1.12.9 whitewash symptom (E-3).
    pub(super) populated: bool,
    /// Drawable size of the last presented frame (dedup watermark, E-3).
    pub(super) last_presented: Option<(f32, f32)>,
    /// Instant of the last PULL present (present-rate limiter,
    /// PLAN_zoom_drawable_stall A): the pull may acquire a drawable at most
    /// once per PULL_MIN_INTERVAL. In-memory only; nothing persists it.
    pub(super) last_present_at: Option<Instant>,
    // Registered handles (retained; main-thread-exclusive use, see module doc).
    pub(super) device: Option<metal::Device>,
    pub(super) queue: Option<metal::CommandQueue>,
    pub(super) layer: Option<metal::MetalLayer>,
    pub(super) legacy_pipeline: Option<metal::RenderPipelineState>,
    pub(super) instanced_pipeline: Option<metal::RenderPipelineState>,
    pub(super) bg_pipeline: Option<metal::RenderPipelineState>,
    pub(super) sampler: Option<metal::SamplerState>,
    pub(super) atlas_texture: Option<metal::Texture>,
    pub(super) color_texture: Option<metal::Texture>,
    pub(super) index_buffer: Option<metal::Buffer>,
    /// `presentsWithTransaction` bind state (E-2): `Some(bind instant)` while
    /// the cached layer is bound to the current CA transaction.
    pub(super) tx_bound_since: Option<Instant>,
}

impl FrameCache {
    pub(super) fn empty() -> Self {
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
    pub(super) fn pullable(&self) -> bool {
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
    pub(super) fn apply_stash(
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

pub(super) fn cache() -> &'static Mutex<FrameCache> {
    FRAME_CACHE.get_or_init(|| Mutex::new(FrameCache::empty()))
}

/// The cache is main-thread-exclusive (see module doc); a poisoned lock still
/// holds valid frame data (stash never panics mid-mutation), so recovery is
/// preferred over pulling the whole path down.
pub(super) fn lock() -> MutexGuard<'static, FrameCache> {
    cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// M-2 leak bound: a bind whose IMP panicked between bind and unbind would
/// otherwise leave the layer on `presentsWithTransaction = true` forever.
/// Swept here (every stashed frame) AND at every re-bind, so the stale bind
/// cannot outlive TX_BIND_TIMEOUT past the last frame.
pub(super) fn expire_stale_tx_bind(cache: &mut FrameCache) {
    if cache
        .tx_bound_since
        .is_some_and(|since| since.elapsed() > super::TX_BIND_TIMEOUT)
    {
        set_layer_transaction(cache, false);
        cache.tx_bound_since = None;
    }
}

/// Typed layer call, unwind-guarded (house rule: every ObjC boundary degrades
/// instead of aborting -- VULN-005). Returns whether a live handle was there.
pub(super) fn set_layer_transaction(cache: &FrameCache, bound: bool) -> bool {
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
