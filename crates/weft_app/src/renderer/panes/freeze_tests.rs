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
