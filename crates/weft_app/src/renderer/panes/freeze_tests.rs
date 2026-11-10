//! F7-2 (PLAN_v1184 §2-F7-2) background-pane freeze tests, split out of
//! panes.rs's inline `mod tests` (commit-gate `*/tests.rs` budget
//! exemption — panes.rs sat 15 lines over the 800 gate with the test
//! inline). The freeze-bits globals are process-global, so this module is
//! self-contained: own renderer helper, own lock discipline.

use crate::renderer::panes::PaneRenderInfo;
use crate::renderer::MetalRenderer;

/// Mirror the golden skip precedent: no Metal device (CI without GPU)
/// skips instead of failing (panes.rs tests-module twin).
fn headless_renderer_or_skip() -> Option<MetalRenderer> {
    metal::Device::system_default()?;
    Some(crate::renderer::MetalRenderer::new_headless_paint(
        weft_core::config::Theme::weft_warm(),
    ))
}

/// F7-2 truth table ③ (PLAN_v1184 §2-F7-2): while the rewrap freeze is
/// armed, the background BLOCK pane rebuild is skipped WHOLESALE — a
/// cold pane creates no cache entry (nothing to serve), a warm pane
/// keeps serving its previous (stale-rect) vertices without a rebuild.
/// The wholesale skip (not a fingerprint skip) is what keeps the shared
/// `block_layout_cache` free of per-frame rebuild churn: a rebuild's
/// internal `sync_blocks` would pay band-Both costs (and, over the table
/// budget, budget-gated foreign evictions — F7-1d) at ≤15fps during the
/// animation. Since F7-1d the sync no longer evicts cross-pane entries
/// (budget retention — see the grid_cache retention test), so the skip is
/// a pure performance lock, not a correctness one.
/// GLOBAL_ZOOM_STATE_LOCK is mandatory — the freeze bits are
/// process-global and shared with the zoom_render tests.
#[test]
fn background_block_pane_rebuild_is_wholesale_skipped_while_rewrap_frozen() {
    let Some(renderer) = headless_renderer_or_skip() else {
        eprintln!("skipping bg rewrap-freeze test: no Metal device available");
        return;
    };
    let _guard = crate::paint::zoom_render::GLOBAL_ZOOM_STATE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Clean slate for the process-global freeze bits.
    crate::paint::zoom_render::set_self_zoom_window(false);
    crate::paint::zoom_render::set_rewrap_freeze_live_resize(false);

    let mut terminal = weft_core::vt::Terminal::new(6, 40);
    terminal.process(b"\x1b]133;A\x07$ \x1b]133;B\x07seq 3\n\x1b]133;C\x07a\nb\n");
    let pane = PaneRenderInfo {
        rect: [0.0; 4],
        terminal: &terminal,
        block_scroll: 0.0,
        submit_on_ctrl_enter: true,
        pane_session_id: 9001,
    };

    // Frozen cold pane: no rebuild, no vertices, no cache entry — and
    // therefore no sync_blocks traffic into the shared layout cache.
    crate::paint::zoom_render::set_rewrap_freeze_live_resize(true);
    let mut frozen: Vec<f32> = Vec::new();
    assert!(
        !renderer.build_background_pane_content(
            &pane,
            &mut frozen,
            &mut Vec::new(),
            &mut Vec::new()
        ),
        "the frozen path must report no rebuild"
    );
    assert!(frozen.is_empty(), "a cold pane has nothing to serve");
    assert!(
        renderer.background_block_caches.borrow().is_empty(),
        "the freeze must not create a cache entry"
    );

    // Unfreeze: the rebuild runs and warms the cache (settle path).
    crate::paint::zoom_render::set_rewrap_freeze_live_resize(false);
    let mut warm: Vec<f32> = Vec::new();
    assert!(
        renderer.build_background_pane_content(&pane, &mut warm, &mut Vec::new(), &mut Vec::new()),
        "unfrozen rebuild must run"
    );
    assert!(!warm.is_empty());
    assert_eq!(renderer.background_block_caches.borrow().len(), 1);

    // Frozen warm pane: the previous build keeps serving (stale bytes,
    // no rebuild — the cache entry is untouched).
    crate::paint::zoom_render::set_rewrap_freeze_live_resize(true);
    let mut stale_served: Vec<f32> = Vec::new();
    assert!(
        !renderer.build_background_pane_content(
            &pane,
            &mut stale_served,
            &mut Vec::new(),
            &mut Vec::new()
        ),
        "the frozen warm path must report no rebuild"
    );
    assert_eq!(
        stale_served, warm,
        "the frozen pane serves the previous build byte-for-byte"
    );

    // Clean slate for the other tests under the shared lock.
    crate::paint::zoom_render::set_rewrap_freeze_live_resize(false);
}

/// One 6-vertex quad (12 floats per vertex: pos + tex + fg + bg) with
/// per-corner tex sentinels so the UV bit-equality assertions bite.
fn push_quad(out: &mut Vec<f32>, x0: f32, y0: f32, x1: f32, y1: f32) {
    for (i, (x, y)) in [(x0, y0), (x1, y0), (x1, y1), (x0, y0), (x1, y1), (x0, y1)]
        .into_iter()
        .enumerate()
    {
        out.extend_from_slice(&[
            x,
            y,
            i as f32 * 0.25,
            1.0 - i as f32 * 0.125,
            0.25,
            0.5,
            0.75,
            1.0,
            0.1,
            0.2,
            0.3,
            0.4,
        ]);
    }
}

/// v1.18.10 (PLAN_v11810_FROZEN_BG_VERTEX_AFFINE §2.1): rendering-level
/// freeze assertions. The cache entry is CONSTRUCTED directly (a header
/// band + a content quad hand-baked under rect A) so the served geometry is
/// exact by construction and independent of build_block_view internals;
/// the code under test is the frozen-serve path (entry bytes + rect pair in
/// → stream out). GLOBAL_ZOOM_STATE_LOCK is mandatory — the freeze bits are
/// process-global and shared with the zoom_render tests.
#[test]
fn frozen_serve_follows_pane_rect_and_identity_stays_byte_exact() {
    let Some(renderer) = headless_renderer_or_skip() else {
        eprintln!("skipping frozen affine serve test: no Metal device available");
        return;
    };
    let _guard = crate::paint::zoom_render::GLOBAL_ZOOM_STATE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    crate::paint::zoom_render::set_self_zoom_window(false);
    crate::paint::zoom_render::set_rewrap_freeze_live_resize(false);

    const VERTEX_FLOATS: usize = 12;
    const QUAD_FLOATS: usize = 6 * VERTEX_FLOATS;

    // Rect A: the bake. Rect B: 0.5x re-base onto an offset origin — every
    // mapped coordinate below is exactly representable, so the spot checks
    // use `==` (stronger than the plan's ±1e-3 windows).
    let rect_a: crate::layout::Rect = [0.0, 0.0, 400.0, 300.0];
    let rect_b: crate::layout::Rect = [40.0, 30.0, 240.0, 180.0];
    let mut baked: Vec<f32> = Vec::new();
    push_quad(&mut baked, 0.0, 0.0, 400.0, 24.0); // header band: A's full width
    push_quad(&mut baked, 8.0, 40.0, 392.0, 64.0); // content quad inside A
    renderer.background_block_caches.borrow_mut().insert(
        9100,
        crate::renderer::bg_block_cache::BgBlockCache {
            vertices: baked.clone(),
            fingerprint: crate::renderer::bg_block_cache::BgBlockFingerprint {
                live_output_version: 1,
                screen_head_lines: 0,
                cwd: None,
                git_branch: None,
                theme_generation: 0,
                cell_dims: (8, 16),
                rect: rect_a,
                block_scroll: 0.0,
                editor_mode: false,
            },
            last_rebuild: std::time::Instant::now(),
        },
    );

    let mut terminal = weft_core::vt::Terminal::new(6, 40);
    terminal.process(b"\x1b]133;A\x07$ \x1b]133;B\x07seq 3\n\x1b]133;C\x07a\nb\n");
    let pane = |rect: crate::layout::Rect| PaneRenderInfo {
        rect,
        terminal: &terminal,
        block_scroll: 0.0,
        submit_on_ctrl_enter: true,
        pane_session_id: 9100,
    };

    crate::paint::zoom_render::set_rewrap_freeze_live_resize(true);

    // ① Identity rect (pane.rect == fingerprint.rect): the serve must take
    // the untouched extend path — byte-for-byte the baked entry (the
    // stale-rect-free twin of the :97-100 sentinel above).
    let mut served: Vec<f32> = Vec::new();
    assert!(
        !renderer.build_background_pane_content(
            &pane(rect_a),
            &mut served,
            &mut Vec::new(),
            &mut Vec::new()
        ),
        "the frozen path must report no rebuild"
    );
    assert_eq!(
        served, baked,
        "identity rect: the frozen serve must be byte-for-byte the baked entry"
    );

    // ② Re-based rect (pane.rect = B ≠ A): every vertex moves inside B
    // (x ∈ [B.x, B.x+B.w], y likewise — ±1e-3), every non-position float
    // stays bit-identical, and the cache entry itself is untouched.
    let mut mapped: Vec<f32> = Vec::new();
    assert!(
        !renderer.build_background_pane_content(
            &pane(rect_b),
            &mut mapped,
            &mut Vec::new(),
            &mut Vec::new()
        ),
        "the frozen path must report no rebuild"
    );
    assert_eq!(
        mapped.len(),
        baked.len(),
        "the affine must not change the stream length"
    );
    let span = |stream: &[f32], quad: usize, axis: usize| {
        let mut lo = f32::INFINITY;
        let mut hi = f32::NEG_INFINITY;
        for v in 0..6 {
            let a = stream[quad * QUAD_FLOATS + v * VERTEX_FLOATS + axis];
            lo = lo.min(a);
            hi = hi.max(a);
        }
        (lo, hi)
    };
    // Exact re-base expectations: A→B is origin (40, 30) at exactly 0.5x.
    assert_eq!(span(&mapped, 0, 0), (40.0, 240.0), "band quad x span");
    assert_eq!(span(&mapped, 0, 1), (30.0, 42.0), "band quad y span");
    assert_eq!(span(&mapped, 1, 0), (44.0, 236.0), "content quad x span");
    assert_eq!(span(&mapped, 1, 1), (50.0, 62.0), "content quad y span");
    for (qi, quad) in mapped.chunks_exact(QUAD_FLOATS).enumerate() {
        let baked_quad = &baked[qi * QUAD_FLOATS..(qi + 1) * QUAD_FLOATS];
        for v in 0..6 {
            let x = quad[v * VERTEX_FLOATS];
            let y = quad[v * VERTEX_FLOATS + 1];
            assert!(
                x >= rect_b[0] - 1e-3 && x <= rect_b[2] + 1e-3,
                "quad {qi} vertex {v}: x {x} outside B"
            );
            assert!(
                y >= rect_b[1] - 1e-3 && y <= rect_b[3] + 1e-3,
                "quad {qi} vertex {v}: y {y} outside B"
            );
            assert_eq!(
                quad[v * VERTEX_FLOATS + 2..(v + 1) * VERTEX_FLOATS],
                baked_quad[v * VERTEX_FLOATS + 2..(v + 1) * VERTEX_FLOATS],
                "quad {qi} vertex {v}: the 10 non-position floats must be bit-identical"
            );
        }
    }
    {
        let caches = renderer.background_block_caches.borrow();
        let entry = caches.get(&9100).expect("entry must survive the serves");
        assert_eq!(
            entry.fingerprint.rect, rect_a,
            "the freeze must not mutate the entry"
        );
        assert_eq!(
            entry.vertices, baked,
            "the freeze must not mutate the baked bytes"
        );
    }

    // Clean slate for the other tests under the shared lock.
    crate::paint::zoom_render::set_rewrap_freeze_live_resize(false);
}

/// v1.19.0 (PLAN_v1190 §4 P1-4/O2/N6): with the grid-render flag ON the
/// fingerprint's `live_output_version` component switches its value source
/// to `grid.revision` (`region_cache::live_output_version_source` — no new
/// fingerprint field). A hand-baked cache entry whose fingerprint pins the
/// CURRENT revision must be reused; any grid mutation (revision bump) must
/// invalidate it. Mirror of the hand-baked-entry precedent above
/// (frozen_serve_follows_pane_rect_and_identity_stays_byte_exact).
#[test]
fn bg_fingerprint_tracks_grid_revision_while_grid_render_flag_on() {
    let Some(renderer) = headless_renderer_or_skip() else {
        eprintln!("skipping grid-revision fingerprint test: no Metal device available");
        return;
    };
    let _guard = crate::paint::zoom_render::GLOBAL_ZOOM_STATE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    crate::paint::zoom_render::set_self_zoom_window(false);
    crate::paint::zoom_render::set_rewrap_freeze_live_resize(false);

    let mut terminal = weft_core::vt::Terminal::new(6, 40);
    terminal.set_live_block_grid_render(true);
    terminal.process(b"\x1b]133;A\x07$ \x1b]133;B\x07seq 3\n");
    let revision = terminal.grid().revision;
    // Sanity: under the flag the version source really is the grid revision.
    assert_eq!(
        crate::paint::region_cache::live_output_version_source(&terminal),
        revision,
        "flag ON must ride grid.revision, not the capture ledger version"
    );

    // Hand-bake an entry whose fingerprint pins the current revision and
    // matches every other composite-key input of the pane below.
    let rect_a: crate::layout::Rect = [0.0, 0.0, 400.0, 300.0];
    let baked: Vec<f32> = vec![1.0; 24];
    renderer.background_block_caches.borrow_mut().insert(
        9200,
        crate::renderer::bg_block_cache::BgBlockCache {
            vertices: baked.clone(),
            fingerprint: crate::renderer::bg_block_cache::BgBlockFingerprint {
                live_output_version: revision,
                screen_head_lines: terminal.screen_head_lines(),
                cwd: None,
                git_branch: None,
                theme_generation: renderer.background_grid_generation.get(),
                cell_dims: (renderer.cell_width(), renderer.cell_height()),
                rect: rect_a,
                block_scroll: 0.0,
                editor_mode: false,
            },
            // Backdate past the 60ms content throttle: after the revision
            // bump below the gate must reach RebuildNow, not the Throttled
            // serve window.
            last_rebuild: std::time::Instant::now() - std::time::Duration::from_secs(1),
        },
    );

    // Same revision → fingerprint hit → cached serve, no rebuild. The pane
    // holds `&terminal`, so each serve phase gets its own scoped value and
    // the borrow ends before `terminal.process` below needs `&mut`.
    let served: Vec<f32> = {
        let pane = PaneRenderInfo {
            rect: rect_a,
            terminal: &terminal,
            block_scroll: 0.0,
            submit_on_ctrl_enter: true,
            pane_session_id: 9200,
        };
        let mut served: Vec<f32> = Vec::new();
        assert!(
            !renderer.build_background_pane_content(
                &pane,
                &mut served,
                &mut Vec::new(),
                &mut Vec::new()
            ),
            "same-revision fingerprint must reuse the hand-baked entry"
        );
        served
    };
    assert_eq!(
        served, baked,
        "identity rect: the reuse serve must be byte-for-byte the baked entry"
    );

    // A grid mutation bumps the revision → fingerprint miss → rebuild.
    terminal.process(b"more output\n");
    assert_ne!(
        terminal.grid().revision,
        revision,
        "content writes must bump grid.revision"
    );
    let rebuilt: Vec<f32> = {
        let pane = PaneRenderInfo {
            rect: rect_a,
            terminal: &terminal,
            block_scroll: 0.0,
            submit_on_ctrl_enter: true,
            pane_session_id: 9200,
        };
        let mut rebuilt: Vec<f32> = Vec::new();
        assert!(
            renderer.build_background_pane_content(
                &pane,
                &mut rebuilt,
                &mut Vec::new(),
                &mut Vec::new()
            ),
            "bumped revision must invalidate the entry and rebuild"
        );
        rebuilt
    };
    assert_ne!(
        rebuilt, baked,
        "the rebuild must replace the stale baked bytes, not re-serve them"
    );

    // Clean slate for the other tests under the shared lock.
    renderer.background_block_caches.borrow_mut().remove(&9200);
    crate::paint::zoom_render::set_rewrap_freeze_live_resize(false);
}
