//! Per-pane base-content rendering for split tabs.

use crate::renderer::MetalRenderer;
use weft_core::vt::Terminal;

// v1.20.3 WP1 1A: background-pane warm/build moved to a #[path] submodule
// (line-count debt; the #[path] is relative to renderer/ because panes.rs
// is a non-mod.rs file). The glob re-imports the three free items so the
// tests below and external callers see unchanged paths.
#[path = "panes/background.rs"]
mod background;

#[cfg(test)]
use background::*;

/// Immutable draw inputs for one non-active pane.
#[derive(Clone, Copy)]
pub struct PaneRenderInfo<'a> {
    pub rect: crate::layout::Rect,
    pub terminal: &'a Terminal,
    pub block_scroll: f32,
    pub submit_on_ctrl_enter: bool,
    /// v1.4.1: pane-scoped namespace for the styled-line vertex cache.
    pub pane_session_id: u64,
}

impl MetalRenderer {
    /// v1.13.5 T16b test/observability hook: prune background block-vertex
    /// caches against the live pane list (the draw() B3-2 retain pattern —
    /// session ids are monotonic, so a plain retain keeps the map bounded).
    #[cfg(test)]
    pub(super) fn retain_background_block_caches(&self, live_ids: &std::collections::HashSet<u64>) {
        self.background_block_caches
            .borrow_mut()
            .retain(|id, _| live_ids.contains(id));
    }

    /// v1.13.11 N1.4 test/observability hook: prune the per-pane visible
    /// block windows (the draw() retain runs the same `.retain` inline next
    /// to `background_block_caches`).
    #[cfg(test)]
    pub(super) fn retain_bg_visible_blocks(&self, live_ids: &std::collections::HashSet<u64>) {
        self.bg_visible_blocks
            .borrow_mut()
            .retain(|id, _| live_ids.contains(id));
    }

    pub(crate) fn draw_legacy_vertex_ranges(
        &self,
        encoder: &metal::RenderCommandEncoderRef,
        vertices_len: usize,
    ) {
        let ranges = self.pane_vertex_ranges.borrow();
        let mut pane_end = 0usize;
        for (rect, range) in ranges.iter() {
            debug_assert_eq!(range.start, pane_end, "pane vertex ranges form a prefix");
            let [x0, y0, x1, y1] = *rect;
            let sx = x0.max(0.0).min(self.viewport.0) as u64;
            let sy = y0.max(0.0).min(self.viewport.1) as u64;
            let sw = (x1 - x0).max(0.0).min(self.viewport.0 - sx as f32) as u64;
            let sh = (y1 - y0).max(0.0).min(self.viewport.1 - sy as f32) as u64;
            pane_end = range.end;
            if sw == 0 || sh == 0 || range.end <= range.start {
                continue;
            }
            encoder.set_scissor_rect(metal::MTLScissorRect {
                x: sx,
                y: sy,
                width: sw,
                height: sh,
            });
            encoder.draw_primitives(
                metal::MTLPrimitiveType::Triangle,
                (range.start / 12) as u64,
                ((range.end - range.start) / 12) as u64,
            );
        }

        let vertex_count = vertices_len / 12;
        let tail_start = pane_end / 12;
        if tail_start < vertex_count {
            encoder.set_scissor_rect(metal::MTLScissorRect {
                x: 0,
                y: 0,
                width: self.viewport.0 as u64,
                height: self.viewport.1 as u64,
            });
            encoder.draw_primitives(
                metal::MTLPrimitiveType::Triangle,
                tail_start as u64,
                (vertex_count - tail_start) as u64,
            );
        }
    }
}

/// T15b (PLAN_v11217 §3.10): identity truth table for the active-pane full
/// rebuild in `draw()`'s multi-pane branch. Force exactly when the pane the
/// global grid row cache held changed since the last multi-pane frame
/// (`prev == None` = first frame). Tab switches always differ because pane
/// session ids are globally monotonic, so the new tab's active pane id is
/// fresh; a same-pane layout move intentionally returns false — that case
/// is owned by the origin fingerprint inside `build_grid_instances`.
pub(super) fn should_force_full_redraw(
    prev: Option<weft_core::pane_layout::PaneId>,
    current: weft_core::pane_layout::PaneId,
) -> bool {
    prev != Some(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use weft_core::pane_layout::PaneId;

    /// Mirror the golden skip precedent: no Metal device (CI without GPU)
    /// skips instead of failing.
    fn headless_renderer_or_skip() -> Option<crate::renderer::MetalRenderer> {
        metal::Device::system_default()?;
        Some(crate::renderer::MetalRenderer::new_headless_paint(
            weft_core::config::Theme::weft_warm(),
        ))
    }

    /// T15b truth table: first multi-pane frame, same-pane steady state,
    /// in-split pane switch, and tab switch (fresh monotonic id).
    #[test]
    fn should_force_full_redraw_truth_table() {
        let a = PaneId(1);
        let b = PaneId(2);
        assert!(
            should_force_full_redraw(None, a),
            "first multi-pane frame: no recorded pane → force"
        );
        assert!(
            !should_force_full_redraw(Some(a), a),
            "same pane as last frame → incremental, no force"
        );
        assert!(
            should_force_full_redraw(Some(a), b),
            "active pane switch inside the split → force"
        );
        assert!(
            should_force_full_redraw(Some(b), a),
            "different pane (tab switch lands on a fresh monotonic id) → force"
        );
    }

    #[test]
    fn background_pane_uses_its_own_terminal_view_mode() {
        let mut terminal = Terminal::new(24, 80);
        assert_eq!(pane_base_view(&terminal), PaneBaseView::Grid);

        terminal.process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
        assert!(terminal.show_block_view());
        assert_eq!(pane_base_view(&terminal), PaneBaseView::Block);

        terminal.process(b"\x1b[?1049h");
        assert!(!terminal.show_block_view());
        assert_eq!(pane_base_view(&terminal), PaneBaseView::Grid);
    }

    /// T16b headless double-frame stub: version unchanged → the second
    /// frame performs NO rebuild (build count 0); a content bump inside the
    /// throttle window holds the old vertices; after the window it rebuilds;
    /// a rect-only delta rebuilds immediately.
    // F7-2: this test OBSERVES the rewrap freeze flag (build_background_pane_content
    // early-returns while armed), so it must hold the SHARED
    // GLOBAL_ZOOM_STATE_LOCK — the zoom_render/macos_zoom flag tests flip it
    // concurrently (rust-reviewer round 7's cross-module interleave shape).
    #[test]
    fn background_block_pane_cache_gates_rebuilds() {
        let _zoom_guard = crate::paint::zoom_render::GLOBAL_ZOOM_STATE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(renderer) = headless_renderer_or_skip() else {
            eprintln!("skipping bg block cache test: no Metal device available");
            return;
        };
        let mut terminal = weft_core::vt::Terminal::new(6, 40);
        terminal.process(b"\x1b]133;A\x07$ \x1b]133;B\x07seq 10\n\x1b]133;C\x07one\ntwo\n");
        // Per-frame pane builder (a closure would hold the &terminal across
        // the process() calls below).
        fn make_pane<'a>(
            terminal: &'a weft_core::vt::Terminal,
            rect: crate::layout::Rect,
        ) -> PaneRenderInfo<'a> {
            PaneRenderInfo {
                rect,
                terminal,
                block_scroll: 0.0,
                submit_on_ctrl_enter: true,
                pane_session_id: 4242,
            }
        }

        let mut stream: Vec<f32> = Vec::new();
        assert!(
            renderer.build_background_pane_content(
                &make_pane(&terminal, [0.0; 4]),
                &mut stream,
                &mut Vec::new(),
                &mut Vec::new()
            ),
            "first frame must rebuild (cold cache)"
        );
        let first_len = stream.len();
        assert!(first_len > 0, "cold build must produce vertices");

        // Second frame, byte-identical inputs: no rebuild, same bytes.
        let mut stream2: Vec<f32> = Vec::new();
        assert!(
            !renderer.build_background_pane_content(
                &make_pane(&terminal, [0.0; 4]),
                &mut stream2,
                &mut Vec::new(),
                &mut Vec::new()
            ),
            "second frame with an unchanged fingerprint must NOT rebuild"
        );
        assert_eq!(stream, stream2, "reused vertices must be byte-equal");

        // Content bump (streaming) inside the 60ms window: throttled —
        // no rebuild, but the (stale) vertices still serve.
        terminal.process(b"three\n");
        let mut stream3: Vec<f32> = Vec::new();
        assert!(
            !renderer.build_background_pane_content(
                &make_pane(&terminal, [0.0; 4]),
                &mut stream3,
                &mut Vec::new(),
                &mut Vec::new()
            ),
            "content bump inside the throttle window must be held"
        );
        assert_eq!(
            stream3.len(),
            first_len,
            "throttled frame still serves vertices"
        );

        // After the window: rebuild with the new content.
        std::thread::sleep(
            crate::renderer::bg_block_cache::BG_BLOCK_CACHE_THROTTLE
                + std::time::Duration::from_millis(5),
        );
        let mut stream4: Vec<f32> = Vec::new();
        assert!(
            renderer.build_background_pane_content(
                &make_pane(&terminal, [0.0; 4]),
                &mut stream4,
                &mut Vec::new(),
                &mut Vec::new()
            ),
            "content bump after the throttle window must rebuild"
        );

        // rect-only delta: immediate rebuild, no throttle wait.
        let mut stream5: Vec<f32> = Vec::new();
        assert!(
            renderer.build_background_pane_content(
                &make_pane(&terminal, [0.0, 0.0, 200.0, 100.0]),
                &mut stream5,
                &mut Vec::new(),
                &mut Vec::new()
            ),
            "rect-only delta must rebuild immediately"
        );
    }

    /// T16b: the color/font invalidation hooks clear the whole map.
    // F7-2: observes the freeze flag via build_background_pane_content —
    // hold the SHARED GLOBAL_ZOOM_STATE_LOCK (zoom-flag tests flip it).
    #[test]
    fn theme_and_atlas_hooks_clear_background_block_caches() {
        let _zoom_guard = crate::paint::zoom_render::GLOBAL_ZOOM_STATE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(mut renderer) = headless_renderer_or_skip() else {
            eprintln!("skipping bg block cache hook test: no Metal device available");
            return;
        };
        let mut terminal = weft_core::vt::Terminal::new(6, 40);
        terminal.process(b"\x1b]133;A\x07$ \x1b]133;B\x07seq 2\n\x1b]133;C\x07a\n");
        let pane = PaneRenderInfo {
            rect: [0.0; 4],
            terminal: &terminal,
            block_scroll: 0.0,
            submit_on_ctrl_enter: true,
            pane_session_id: 7,
        };
        let mut sink: Vec<f32> = Vec::new();
        assert!(renderer.build_background_pane_content(
            &pane,
            &mut sink,
            &mut Vec::new(),
            &mut Vec::new()
        ));
        assert_eq!(renderer.background_block_caches.borrow().len(), 1);

        renderer.set_theme(weft_core::config::Theme::weft_warm());
        assert!(
            renderer.background_block_caches.borrow().is_empty(),
            "set_theme must clear the background block caches"
        );

        // Rebuild, then the atlas/font hook.
        let mut sink: Vec<f32> = Vec::new();
        assert!(renderer.build_background_pane_content(
            &pane,
            &mut sink,
            &mut Vec::new(),
            &mut Vec::new()
        ));
        renderer.rebuild_atlas(weft_core::config::FontConfig::default());
        assert!(
            renderer.background_block_caches.borrow().is_empty(),
            "rebuild_atlas must clear the background block caches"
        );
    }

    /// T16b: the draw()-side retain prunes closed/active panes' entries.
    // F7-2: observes the freeze flag via build_background_pane_content —
    // hold the SHARED GLOBAL_ZOOM_STATE_LOCK (zoom-flag tests flip it).
    #[test]
    fn background_block_caches_retain_live_panes_only() {
        let _zoom_guard = crate::paint::zoom_render::GLOBAL_ZOOM_STATE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(renderer) = headless_renderer_or_skip() else {
            eprintln!("skipping bg block cache retain test: no Metal device available");
            return;
        };
        let mut terminal = weft_core::vt::Terminal::new(6, 40);
        terminal.process(b"\x1b]133;A\x07$ \x1b]133;B\x07seq 2\n\x1b]133;C\x07a\n");
        for id in [1u64, 2u64, 3u64] {
            let pane = PaneRenderInfo {
                rect: [0.0; 4],
                terminal: &terminal,
                block_scroll: 0.0,
                submit_on_ctrl_enter: true,
                pane_session_id: id,
            };
            let mut sink: Vec<f32> = Vec::new();
            assert!(renderer.build_background_pane_content(
                &pane,
                &mut sink,
                &mut Vec::new(),
                &mut Vec::new()
            ));
        }
        assert_eq!(renderer.background_block_caches.borrow().len(), 3);
        let live: std::collections::HashSet<u64> = [2u64, 3u64].into_iter().collect();
        renderer.retain_background_block_caches(&live);
        assert_eq!(renderer.background_block_caches.borrow().len(), 2);
        assert!(renderer.background_block_caches.borrow().contains_key(&2));
        assert!(!renderer.background_block_caches.borrow().contains_key(&1));
    }

    /// v1.13.11 N1.4 (PLAN_v1.13.11 测试 5): a background block-pane REBUILD
    /// collects its visible finished-block ids into `bg_visible_blocks`
    /// (consumed by `warm_background_pane_atlases` as that pane's warm
    /// window), and the draw()-side retain prunes closed/active panes'
    /// entries (a closed pane's entry would otherwise pin its last window
    /// forever). Collection happens only on rebuild — a T16b cache-hit frame
    /// keeps the previous frame's set, matching the plan's "预热照跑" note.
    // F7-2: observes the freeze flag via build_background_pane_content —
    // hold the SHARED GLOBAL_ZOOM_STATE_LOCK (zoom-flag tests flip it).
    #[test]
    fn bg_visible_blocks_collected_on_rebuild_and_retained_live_panes_only() {
        let _zoom_guard = crate::paint::zoom_render::GLOBAL_ZOOM_STATE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(renderer) = headless_renderer_or_skip() else {
            eprintln!("skipping bg visible-blocks test: no Metal device available");
            return;
        };
        let mut terminal = weft_core::vt::Terminal::new(6, 40);
        terminal.process(
            b"\x1b]133;A\x07$ \x1b]133;B\x07seq 7\nwarm me\nplease\n\x1b]133;D;0;weft-shell\x07",
        );
        let pane = PaneRenderInfo {
            rect: [0.0; 4],
            terminal: &terminal,
            block_scroll: 0.0,
            submit_on_ctrl_enter: true,
            pane_session_id: 7,
        };
        let mut sink: Vec<f32> = Vec::new();
        assert!(renderer.build_background_pane_content(
            &pane,
            &mut sink,
            &mut Vec::new(),
            &mut Vec::new()
        ));
        let collected = renderer
            .bg_visible_blocks
            .borrow()
            .get(&7)
            .cloned()
            .unwrap_or_default();
        assert!(
            !collected.is_empty(),
            "a rebuild with visible output rows must collect finished-block ids"
        );

        // Pane close/promotion: the live-id retain drops the entry.
        let live: std::collections::HashSet<u64> = [8u64].into_iter().collect();
        renderer.retain_bg_visible_blocks(&live);
        assert!(
            !renderer.bg_visible_blocks.borrow().contains_key(&7),
            "a closed pane's visible-block window must be pruned"
        );
    }

    #[test]
    fn background_prompt_keeps_editor_content_without_focus() {
        let mut terminal = Terminal::new(24, 80);
        terminal.process(b"\x1b]133;A\x07");
        terminal.editor_mut().buffer.set_text("draft command");
        let pane = PaneRenderInfo {
            rect: [0.0, 0.0, 400.0, 300.0],
            terminal: &terminal,
            block_scroll: 0.0,
            submit_on_ctrl_enter: true,
            pane_session_id: 1,
        };

        let prompt = background_prompt(&pane).expect("integrated prompt");
        assert!(!prompt.focused);
        assert_eq!(prompt.lines, &["draft command"]);
        assert!(prompt.submit_on_ctrl_enter);
    }
}

// F7-2 freeze tests live in the gate-exempt sibling module (repo
// test-module convention; grid_cache/budget_tests.rs precedent — the
// #[path] is relative to renderer/ because panes.rs is a non-mod.rs file,
// and the inline tests module above already sat near the 800-line gate).
#[cfg(test)]
#[path = "panes/freeze_tests.rs"]
mod freeze_tests;

// v1.18.10 (PLAN_v11810_FROZEN_BG_VERTEX_AFFINE): frozen-rect affine —
// pure position map + cached-serve helper (identity short-circuit); the
// truth table lives in the gate-counted affine_tests.rs.
#[path = "panes/affine.rs"]
mod affine;

#[cfg(test)]
#[path = "panes/affine_tests.rs"]
mod affine_tests;
