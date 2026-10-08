//! Per-pane base-content rendering for split tabs.

use crate::glyph::GlyphAtlas;
use crate::paint::block_view_model::BlockViewPaintModel;
use crate::paint::overlays::FindDrawState;
use crate::paint::prompt::PromptDrawParams;
use crate::paint::tab_bar::TabBarDrawState;
use crate::renderer::bg_block_cache;
use crate::renderer::MetalRenderer;
use weft_core::input::InputMode;
use weft_core::selection::SelectionHandler;
use weft_core::vt::Terminal;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PaneBaseView {
    Block,
    Grid,
}

fn pane_base_view(terminal: &Terminal) -> PaneBaseView {
    if terminal.show_block_view() {
        PaneBaseView::Block
    } else {
        PaneBaseView::Grid
    }
}

fn background_prompt<'a>(pane: &'a PaneRenderInfo<'a>) -> Option<PromptDrawParams<'a>> {
    (pane.terminal.effective_input_mode() == InputMode::Editor).then(|| PromptDrawParams {
        focused: false,
        cwd: pane.terminal.cwd(),
        lines: &pane.terminal.editor().buffer.lines,
        cursor: pane.terminal.editor().buffer.cursor,
        preedit: None,
        preedit_cursor: None,
        search: pane.terminal.editor().search_view(),
        selection: None,
        scroll_offset: pane.terminal.editor().buffer.scroll_offset,
        submit_on_ctrl_enter: pane.submit_on_ctrl_enter,
    })
}

impl MetalRenderer {
    pub(super) fn warm_background_pane_atlases(
        atlas: &mut GlyphAtlas,
        viewport_h: f32,
        panes: &[PaneRenderInfo<'_>],
        tab_bar: &TabBarDrawState,
        // v1.12.2 B3-3 (PLAN_S2_render): drag-time incremental block scan —
        // each background pane warms under its own namespace (session id;
        // P2: namespaces must be per-pane, never a shared constant).
        live_resize: bool,
        watermarks: &std::cell::RefCell<crate::renderer::atlas_warmup::BlockScanWatermarks>,
    ) {
        let no_find: Option<FindDrawState> = None;
        let no_note: Option<crate::paint::overlays::NoteEditorDrawState> = None;
        for pane in panes {
            let prompt = background_prompt(pane);
            Self::warm_atlas(
                atlas,
                viewport_h,
                &no_find,
                &no_note,
                pane.terminal,
                pane.terminal.grid(),
                None,
                prompt.as_ref(),
                None,
                None,
                None,
                None,
                tab_bar,
                // Background panes don't render diagnose panels.
                &[],
                live_resize,
                watermarks,
                pane.pane_session_id,
            );
        }
    }

    /// Build one background pane's base content. Block-view panes emit legacy
    /// vertices bounded by LayoutCtx.clip; grid panes emit a dual-stream
    /// `GridInstanceBatch` whose bg/glyph float offsets are appended to the
    /// caller's flat buffers and recorded as a `PaneInstanceRanges` entry
    /// (paired with the pane's scissor rect). Interactive state stays
    /// active-only.
    ///
    /// v1.13.5 T16b (PLAN_v11217 §3.11): the block branch is gated by the
    /// per-pane vertex cache (`renderer/bg_block_cache.rs`) — composite
    /// fingerprint + 60ms throttle. Vertices are appended into
    /// `background_vertices` (legacy stream, like the grid branch's dual
    /// streams); returns whether the cache REBUILT this frame (test /
    /// observability; false for grid panes).
    pub(super) fn build_background_pane_content(
        &self,
        pane: &PaneRenderInfo<'_>,
        background_vertices: &mut Vec<f32>,
        bg_stream: &mut Vec<f32>,
        glyph_stream: &mut Vec<f32>,
    ) -> bool {
        match pane_base_view(pane.terminal) {
            PaneBaseView::Grid => {
                let bg_start = bg_stream.len();
                let glyph_start = glyph_stream.len();
                // v1.12.2 B3-2: per-pane incremental rebuild — the returned
                // count reports how many rows were dirty this frame.
                let (pane_batch, rebuilt_rows) = self
                    .build_grid_instances_for_background_pane(pane.terminal, pane.pane_session_id);
                bg_stream.extend(pane_batch.bg_stream);
                glyph_stream.extend(pane_batch.glyph_stream);
                let bg_end = bg_stream.len();
                let glyph_end = glyph_stream.len();
                self.pane_instance_ranges.borrow_mut().push((
                    pane.rect,
                    crate::paint::grid_instances::PaneInstanceRanges {
                        bg_range: (bg_start, bg_end),
                        glyph_range: (glyph_start, glyph_end),
                    },
                ));
                tracing::debug!(
                    bg_rect = ?pane.rect,
                    bg_run_count = (bg_end - bg_start) / 8,
                    glyph_instance_count = (glyph_end - glyph_start) / 16,
                    grid_rows = pane.terminal.grid().num_rows,
                    grid_cols = pane.terminal.grid().num_cols,
                    rebuilt_rows,
                    "built background grid pane"
                );
                false
            }
            PaneBaseView::Block => {
                let editor_mode = pane.terminal.effective_input_mode() == InputMode::Editor;
                let prompt = background_prompt(pane);
                // v1.13.5 T16b: composite gate key — every input that bakes
                // into the vertices (see BgBlockFingerprint's field docs).
                let fingerprint = bg_block_cache::BgBlockFingerprint {
                    live_output_version: pane.terminal.block_tracker().live_output_version(),
                    screen_head_lines: pane.terminal.screen_head_lines(),
                    cwd: pane.terminal.cwd().map(str::to_string),
                    git_branch: pane.terminal.git_branch().map(str::to_string),
                    theme_generation: self.background_grid_generation.get(),
                    cell_dims: (self.cell_width(), self.cell_height()),
                    rect: pane.rect,
                    block_scroll: pane.block_scroll,
                    editor_mode,
                };
                let now = std::time::Instant::now();
                let action = {
                    let caches = self.background_block_caches.borrow();
                    caches.get(&pane.pane_session_id).map_or(
                        bg_block_cache::BgBlockCacheAction::RebuildNow,
                        |entry| {
                            bg_block_cache::bg_block_cache_action(
                                Some((&entry.fingerprint, entry.last_rebuild)),
                                &fingerprint,
                                now,
                                bg_block_cache::BG_BLOCK_CACHE_THROTTLE,
                            )
                        },
                    )
                };
                if matches!(
                    action,
                    bg_block_cache::BgBlockCacheAction::Reuse
                        | bg_block_cache::BgBlockCacheAction::Throttled
                ) {
                    // Reuse (or throttle-hold): serve the previous build.
                    let caches = self.background_block_caches.borrow();
                    let entry = caches
                        .get(&pane.pane_session_id)
                        .expect("Reuse/Throttled imply a cache entry");
                    background_vertices.extend_from_slice(&entry.vertices);
                    tracing::debug!(
                        bg_rect = ?pane.rect,
                        ?action,
                        "reused background block pane vertices"
                    );
                    return false;
                }
                let ctx = self.layout_ctx.expect("background pane LayoutCtx");
                let region_bottom_y = prompt.as_ref().map_or_else(
                    || ctx.bottom(),
                    |p| {
                        crate::paint::prompt::prompt_layout_for_buffer(&ctx, p.lines, p.cursor)
                            .1
                            .box_rect[1]
                    },
                );
                let mut selection = SelectionHandler::new();
                let (mut vertices, _, _) = self.build_block_view_vertices(
                    BlockViewPaintModel {
                        blocks: pane.terminal.block_tracker().session_blocks(),
                        live_head_lines: pane.terminal.screen_head_lines(),
                        region_bottom_y,
                        cwd: pane.terminal.cwd(),
                        git_branch: pane.terminal.git_branch(),
                        live: (!editor_mode)
                            .then(|| pane.terminal.block_tracker().in_flight())
                            .flatten(),
                        block_scroll: pane.block_scroll,
                        viewport_rows: pane.terminal.grid().num_rows,
                        block_hovered: None,
                        block_selected: None,
                        block_action_hovered: None,
                        spinner_phase: -1.0,
                        find_block_highlight: None,
                        palette: pane.terminal.palette(),
                        cache_namespace: pane.pane_session_id,
                        block_diagnose_state: &std::collections::HashMap::new(),
                        ai_configured: false,
                        tui_cursor: None,
                        tui_preedit: None,
                        cursor_blink_on: false,
                        is_alt: pane.terminal.is_alt_screen_active(),
                        now: std::time::SystemTime::now(),
                    },
                    &mut selection,
                );
                if let Some(prompt) = prompt {
                    vertices.extend(self.build_prompt_vertices(&prompt, 0.0, false));
                }
                // Store, then append the stored copy (one authoritative blob).
                {
                    let mut caches = self.background_block_caches.borrow_mut();
                    caches.insert(
                        pane.pane_session_id,
                        bg_block_cache::BgBlockCache {
                            vertices,
                            fingerprint,
                            last_rebuild: now,
                        },
                    );
                    let entry = caches.get(&pane.pane_session_id).expect("just inserted");
                    background_vertices.extend_from_slice(&entry.vertices);
                }
                tracing::debug!(bg_rect = ?pane.rect, "built background block pane");
                true
            }
        }
    }

    /// v1.13.5 T16b test/observability hook: prune background block-vertex
    /// caches against the live pane list (the draw() B3-2 retain pattern —
    /// session ids are monotonic, so a plain retain keeps the map bounded).
    #[cfg(test)]
    pub(super) fn retain_background_block_caches(&self, live_ids: &std::collections::HashSet<u64>) {
        self.background_block_caches
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
    #[test]
    fn background_block_pane_cache_gates_rebuilds() {
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
    #[test]
    fn theme_and_atlas_hooks_clear_background_block_caches() {
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
    #[test]
    fn background_block_caches_retain_live_panes_only() {
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
