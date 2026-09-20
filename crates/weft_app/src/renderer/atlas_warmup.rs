//! Per-frame glyph-atlas warm-up: collect all on-screen characters not yet
//! rasterized and add them to the atlas before vertex building runs.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use crate::glyph::GlyphAtlas;
use crate::overlay::{OverlayContent, OverlayWarmup, PaletteDrawParams, SettingsDrawParams};
use crate::paint::overlays::{FindDrawState, NoteEditorDrawState};
use crate::paint::panel::PanelDrawParams;
use crate::paint::preedit::TuiPreeditDrawParams;
use crate::paint::prompt::PromptDrawParams;
use crate::paint::tab_bar::TabBarDrawState;
use crate::paint::ui_helpers::{block_duration_str, panel_display, visible_panel_rows};
use crate::renderer::MetalRenderer;
use weft_core::blocks::BlockId;
use weft_core::complete::Match;
use weft_core::grid::Grid;
use weft_core::vt::Terminal;

/// v1.12.2 B3-3 (PLAN_S2_render): per-frame cap on block output lines fed to
/// the warmup scan — the pre-B3-3 hardcoded `take(200)` per block, kept as a
/// named constant for the watermark math (both the finished-block scan and
/// the in-flight scan used 200).
const BLOCK_OUTPUT_SCAN_LIMIT: usize = 200;

/// v1.12.2 B3-3: the in-flight block has no stable [`BlockId`] of its own —
/// there is at most one per terminal, so a per-namespace sentinel key carries
/// its watermark.
const IN_FLIGHT_KEY: BlockId = BlockId(u64::MAX);

/// v1.12.2 B3-3 (PLAN_S2_render): per-namespace, per-block output-line
/// watermarks for the drag-time incremental atlas warmup. Namespaces are
/// per-pane session ids — BlockIds are allocated per-Terminal, so a shared
/// constant namespace would leak watermarks across tab/focus switches.
///
/// The warmup scans block outputs (finished + in-flight) every frame; with
/// 64 blocks × up to 200 lines each this dominates the drag frame budget.
/// During a live resize the scan starts at the per-block watermark (how many
/// lines were already scanned) instead of line 0. Block output is
/// append-only (finished blocks are immutable, in-flight output grows at the
/// tail), so lines below the watermark cannot produce new glyphs.
///
/// **Invalidation**: cleared by `update_scale` and `rebuild_atlas` — both
/// swap in a fresh empty atlas, and stale "already scanned" marks would then
/// skip glyphs the new atlas still needs (the fragmented-output bug class).
#[derive(Default)]
pub(crate) struct BlockScanWatermarks {
    scanned_lines: HashMap<(u64, BlockId), usize>,
}

impl BlockScanWatermarks {
    /// Range `[start, end)` of output lines to scan this frame for one
    /// block. `line_count` is the block's current output line count and
    /// `limit` the per-frame cap (see [`BLOCK_OUTPUT_SCAN_LIMIT`]).
    ///
    /// Non-live frames always scan from 0 — outside a drag the full scan is
    /// the pre-B3-3 behavior and its cost is irrelevant next to the frame
    /// budget. Pure function (unit-tested, no device needed).
    pub(crate) fn output_scan_range(
        &self,
        namespace: u64,
        id: BlockId,
        live_resize: bool,
        line_count: usize,
        limit: usize,
    ) -> (usize, usize) {
        let end = line_count.min(limit);
        if !live_resize {
            return (0, end);
        }
        let start = self
            .scanned_lines
            .get(&(namespace, id))
            .copied()
            .unwrap_or(0)
            .min(end);
        (start, end)
    }

    /// Record that `lines_total` output lines of this block have been scanned
    /// (monotonic — an append-only contract makes the count a high-water
    /// mark; `max` also guards against a transient count regression).
    pub(crate) fn record_scanned(&mut self, namespace: u64, id: BlockId, lines_total: usize) {
        let slot = self.scanned_lines.entry((namespace, id)).or_default();
        *slot = (*slot).max(lines_total);
    }

    /// Atlas rebuild invalidation: every watermark is dropped so the next
    /// frame re-scans everything the fresh, empty atlas needs.
    pub(crate) fn clear(&mut self) {
        self.scanned_lines.clear();
    }

    /// P3 fix (rust-reviewer): drop one block's watermark. The in-flight
    /// sentinel key ([`IN_FLIGHT_KEY`]) is REUSED across commands — without
    /// forgetting it when no block is in flight, the next command's early
    /// output lines would inherit the previous command's watermark and be
    /// skipped during a drag (missing glyphs until the drag ends).
    pub(crate) fn forget(&mut self, namespace: u64, id: BlockId) {
        self.scanned_lines.remove(&(namespace, id));
    }
}

impl MetalRenderer {
    /// Collect every on-screen character not yet in the glyph atlas and
    /// rasterize each exactly once before vertex building runs.
    ///
    /// This is an associated function (not a `&mut self` method) so the
    /// caller in `draw()` can pass `&mut self.atlas` as a partial borrow
    /// disjoint from the immutable `self.layer` borrow held by `drawable`
    /// for the whole frame. Taking `&mut self` here would conflict with
    /// that borrow.
    ///
    /// v1.12.2 B3-3 (PLAN_S2_render): the block-output scan is incremental
    /// while a live resize is running — each block scans only the output
    /// lines after its per-block watermark (see [`BlockScanWatermarks`]).
    /// Outside a drag the full scan runs unchanged. Returns the number of
    /// block output lines actually scanned this frame (test observability).
    ///
    /// Draw-path contract (unchanged): this is a *warmup* — the paint path
    /// itself stays lookup-only (a missing glyph paints as blank/space), so
    /// skipping the warmup would resurrect the fragmented-output bug class;
    /// that is why B3-3 narrows the scan instead of gating it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn warm_atlas(
        atlas: &mut GlyphAtlas,
        viewport_h: f32,
        find_state: &Option<FindDrawState>,
        note_editor_state: &Option<NoteEditorDrawState>,
        terminal: &Terminal,
        grid: &Grid,
        panel: Option<&PanelDrawParams<'_>>,
        prompt: Option<&PromptDrawParams<'_>>,
        tui_preedit: Option<TuiPreeditDrawParams<'_>>,
        completions: Option<(&[Match], usize)>,
        palette: Option<&PaletteDrawParams<'_>>,
        settings: Option<&SettingsDrawParams<'_>>,
        tab_bar: &TabBarDrawState,
        diagnose_texts: &[&str],
        live_resize: bool,
        watermarks: &RefCell<BlockScanWatermarks>,
        warm_namespace: u64,
    ) -> u64 {
        // Collect unique on-screen GRID + panel characters not yet in the
        // atlas, then rasterize each exactly once.
        let mut missing = HashSet::new();
        // v1.10.12: styled (bold/italic) grid characters — the atlas rasterizes
        // styled faces on demand; `get_or_rasterize` degrades CJK/emoji/missing
        // variants to the regular face internally.
        let mut missing_styled: HashSet<(char, crate::glyph::GlyphStyle)> = HashSet::new();
        // v1.6.0: also collect multi-scalar grapheme cluster strings from
        // cells with CellFlags::EXTRA. These are rasterized via the
        // cluster atlas path (CoreText CTLine shaping) rather than the
        // single-scalar fast path. The tuple is (cluster, base_char, is_wide).
        let mut missing_clusters: HashSet<(String, char, bool)> = HashSet::new();
        for row in 0..grid.num_rows {
            for col in 0..grid.num_cols {
                let cell = grid.cell(row, col);
                let ch = cell.character;
                if ch != '\0' && ch != ' ' {
                    let style = crate::glyph::GlyphStyle::from_flags(cell.flags);
                    if atlas.get_style(ch, style).is_none() {
                        if style == crate::glyph::GlyphStyle::REGULAR {
                            missing.insert(ch);
                        } else {
                            missing_styled.insert((ch, style));
                        }
                    }
                }
                // v1.6.0: collect cluster strings for multi-scalar graphemes.
                if cell.flags.contains(weft_core::grid::CellFlags::EXTRA) {
                    if let Some(cluster) = grid.grapheme_at(row, col) {
                        if atlas.get_cluster(cluster).is_none() {
                            let is_wide = cell.width == weft_core::grid::CellWidth::Full;
                            missing_clusters.insert((cluster.to_string(), ch, is_wide));
                        }
                    }
                }
            }
        }
        if let Some(p) = panel {
            missing.extend("Search:".chars());
            missing.extend(p.query.chars());
            let max_blocks = visible_panel_rows(viewport_h, atlas.cell_height);
            for b in panel_display(p.blocks, p.query, p.scroll_offset, max_blocks) {
                missing.extend(b.command.chars());
                missing.extend(block_duration_str(b).chars());
                if Some(b.id) == p.expanded_id {
                    for line in b.output.lines().take(8) {
                        missing.extend(line.chars());
                    }
                }
            }
        }
        if let Some(p) = prompt {
            missing.extend("❯ ".chars());
            if let Some(cwd) = p.cwd {
                missing.extend(cwd.chars());
            }
            for line in p.lines {
                missing.extend(line.chars());
            }
            if let Some(preedit) = p.preedit {
                missing.extend(preedit.chars());
            }
            if let Some((q, sel)) = p.search {
                missing.extend("search: ".chars());
                missing.extend(q.chars());
                if let Some(m) = sel {
                    missing.extend(m.chars());
                }
            }
        }
        if let Some(preedit) = tui_preedit {
            missing.extend(preedit.text.chars());
        }
        // Completion popup warm-up: scan emoji icons + match labels.
        if let Some((completions, _)) = completions {
            // Emoji icons used by the popup (📁📄 via CoreText color path).
            missing.extend(['📁', '📄', '»']);
            for m in completions {
                missing.extend(m.label.chars());
            }
        }
        // Block-view (Editor mode + CommandExecuting overlay): commands,
        // outputs, durations. Session blocks only — hydrated history stays
        // in the panel.
        //
        // v1.12.2 B3-3: output lines scan from the per-block watermark during
        // a live resize (finished blocks are immutable, in-flight output is
        // append-only — lines below the watermark cannot produce new glyphs).
        // Command + duration strings stay scanned every frame (one short
        // line each — the watermark targets the dominant per-line cost).
        let mut scanned_output_lines: u64 = 0;
        if terminal.show_block_view() {
            let mut wm = watermarks.borrow_mut();
            if let Some(cwd) = terminal.cwd() {
                missing.extend(cwd.chars());
            }
            if let Some(branch) = terminal.git_branch() {
                missing.extend(" git:()".chars());
                missing.extend(branch.chars());
            }
            for b in terminal
                .block_tracker()
                .session_blocks()
                .iter()
                .rev()
                .take(64)
            {
                missing.extend("❯ ".chars());
                missing.extend(b.command.chars());
                missing.extend(block_duration_str(b).chars());
                // P1-2 fix (rust-reviewer): count only up to the per-frame
                // scan limit — a full `lines().count()` iterates up to ~1MB
                // of output per block per non-live frame (the old `.take(200)`
                // iterated exactly 200 lines). min(line_count, LIMIT) is all
                // `output_scan_range` consumes.
                let line_count = b.output.lines().take(BLOCK_OUTPUT_SCAN_LIMIT).count();
                let (start, end) = wm.output_scan_range(
                    warm_namespace,
                    b.id,
                    live_resize,
                    line_count,
                    BLOCK_OUTPUT_SCAN_LIMIT,
                );
                for line in b.output.lines().skip(start).take(end - start) {
                    missing.extend(line.chars());
                }
                wm.record_scanned(warm_namespace, b.id, end);
                scanned_output_lines += (end - start) as u64;
            }
            // In-flight (live) block during CommandExecuting.
            if let Some(live) = terminal.block_tracker().in_flight() {
                missing.extend("❯ ".chars());
                missing.extend(live.command.chars());
                let line_count = live.output.lines().take(BLOCK_OUTPUT_SCAN_LIMIT).count();
                let (start, end) = wm.output_scan_range(
                    warm_namespace,
                    IN_FLIGHT_KEY,
                    live_resize,
                    line_count,
                    BLOCK_OUTPUT_SCAN_LIMIT,
                );
                for line in live.output.lines().skip(start).take(end - start) {
                    missing.extend(line.chars());
                }
                wm.record_scanned(warm_namespace, IN_FLIGHT_KEY, end);
                scanned_output_lines += (end - start) as u64;
            } else {
                // P3 fix: the sentinel watermark belongs to the *previous*
                // command once nothing is in flight.
                wm.forget(warm_namespace, IN_FLIGHT_KEY);
            }
            // v1.8.9 fix: warm up AI diagnose panel text. The diagnose
            // result is dynamically produced by the local Ollama backend
            // and contains CJK characters that aren't in any static warmup
            // list. Without this, the atlas has no glyphs for them and
            // `push_text` silently skips each missing grapheme, leaving
            // only ASCII punctuation visible (the "fragmented output" bug).
            for text in diagnose_texts {
                missing.extend(text.chars());
            }
        }
        // Find bar (Cmd+F): warm up the query + status text + button
        // glyphs so CJK / other non-ASCII chars typed via IME render
        // instead of leaving blank cells (the atlas only auto-warms
        // grid/panel content; the find query is independent).
        if let Some(find) = find_state {
            missing.extend("Find: ".chars());
            missing.extend(find.query.chars());
            // Button labels + status fragments.
            missing.extend(['↑', '↓', 'A', 'a', '.', '*', '…']);
            if let Some(err) = &find.regex_error {
                missing.extend(err.chars());
            }
        }
        // v1.7.3-C: Note editor — warm up "Note: " label, hint footer
        // (⏎ save  ⎋ cancel) and the buffer text so CJK/IME input renders.
        if let Some(note) = note_editor_state {
            missing.extend("Note: ".chars());
            missing.extend(note.buffer.chars());
            missing.extend(['\u{23ce}', '\u{238b}']); // ⏎ ⎋
        }
        // v0.9 fix: warm up the command palette (Cmd+P) query + banner
        // + submode input so CJK / other non-ASCII chars typed via IME
        // render instead of leaving blank cells (same rationale as the
        // find bar above).
        if let Some(p) = palette {
            missing.extend("> ".chars());
            missing.extend(p.query.chars());
            if !p.banner.is_empty() {
                missing.extend(p.banner.chars());
            }
            missing.extend(p.submode_input.chars());
            // v1.8.4: warm up IME preedit chars for the palette input.
            if !p.ime_preedit.is_empty() {
                missing.extend(p.ime_preedit.chars());
            }
        }
        // v1.0 S1: warm up the Settings panel (Cmd+,) — tab labels,
        // status text, theme names, font family, keybinding strings.
        if let Some(s) = settings {
            OverlayContent::Settings(*s).warm_chars(&mut missing);
        }
        // v0.9 fix: warm up the tab bar close button "×" and separator
        // chars so they render instead of being silently skipped by
        // push_text (which drops chars not in the atlas).
        missing.extend(['×', '·', '•', '…']);
        for text in tab_bar.labels.iter().chain(&tab_bar.tooltips) {
            missing.extend(text.chars());
        }
        // F2 P0-2: warm up the status hint badge glyphs (▾ + label text).
        missing.extend("\u{25be} passthrough running".chars());
        // v1.10.21: warm up the alt-screen history-peek pill glyphs (↺ · CJK
        // label) exactly when the pill can be on screen. `push_text` silently
        // skips glyphs missing from the atlas, so without this the CJK label
        // would render as bare punctuation (the "fragmented output" failure).
        if terminal.is_alt_screen_history_peek() {
            missing.extend(crate::paint::alt_peek_pill::PILL_TEXT.chars());
        }
        // v1.7.3-C: warm up the bookmark star (★) used in block headers.
        missing.extend(['\u{2605}']);
        // F3-2: warm up the braille spinner glyphs (animated activity
        // indicator) and the static ● used under Reduce Motion.
        missing.extend(['●', '⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏']);
        for ch in &missing {
            atlas.get_or_rasterize(*ch, crate::glyph::GlyphStyle::REGULAR);
        }
        // v1.10.12: rasterize styled faces. `get_or_rasterize` degrades
        // CJK/emoji/symbol chars and missing variant faces to regular.
        for (ch, style) in &missing_styled {
            atlas.get_or_rasterize(*ch, *style);
        }
        // v1.6.0: rasterize multi-scalar grapheme clusters via the CoreText
        // cluster path. Each unique cluster string is shaped once and cached
        // in `cluster_cache`. The paint path's UV resolver will then find
        // them via `get_cluster`.
        for (cluster, base_char, is_wide) in &missing_clusters {
            atlas.get_or_rasterize_cluster(cluster, *base_char, *is_wide);
        }
        // B3-3: scanned block-output line count — the observability hook the
        // watermark tests assert on (0 on an incremental frame with no new
        // lines, >0 only for appended/new lines).
        scanned_output_lines
    }
}

#[cfg(test)]
mod watermark_tests {
    use super::{BlockScanWatermarks, BLOCK_OUTPUT_SCAN_LIMIT, IN_FLIGHT_KEY};
    use weft_core::blocks::BlockId;

    #[test]
    fn non_live_frames_always_scan_from_zero() {
        let wm = BlockScanWatermarks::default();
        let id = BlockId(7);
        // Even with a recorded watermark, a non-live frame scans everything.
        let mut wm = wm;
        wm.record_scanned(0, id, 50);
        assert_eq!(
            wm.output_scan_range(0, id, false, 120, BLOCK_OUTPUT_SCAN_LIMIT),
            (0, 120),
            "outside a drag the full scan runs (pre-B3-3 behavior)"
        );
    }

    #[test]
    fn live_frames_scan_only_past_the_watermark() {
        let mut wm = BlockScanWatermarks::default();
        let id = BlockId(7);
        // Nothing recorded yet → a live frame still scans from 0.
        assert_eq!(
            wm.output_scan_range(0, id, true, 30, BLOCK_OUTPUT_SCAN_LIMIT),
            (0, 30)
        );
        wm.record_scanned(0, id, 30);
        assert_eq!(
            wm.output_scan_range(0, id, true, 30, BLOCK_OUTPUT_SCAN_LIMIT),
            (30, 30),
            "no new lines → empty range"
        );
        // Appended lines only.
        assert_eq!(
            wm.output_scan_range(0, id, true, 35, BLOCK_OUTPUT_SCAN_LIMIT),
            (30, 35)
        );
    }

    #[test]
    fn watermark_is_monotonic_and_namespaced() {
        let mut wm = BlockScanWatermarks::default();
        let id = BlockId(1);
        wm.record_scanned(0, id, 40);
        wm.record_scanned(0, id, 20); // transient regression → max wins
        assert_eq!(
            wm.output_scan_range(0, id, true, 60, BLOCK_OUTPUT_SCAN_LIMIT),
            (40, 60)
        );
        // Namespaces are independent (active pane vs background panes).
        assert_eq!(
            wm.output_scan_range(9, id, true, 10, BLOCK_OUTPUT_SCAN_LIMIT),
            (0, 10)
        );
    }

    #[test]
    fn scan_range_clamps_to_the_per_frame_limit() {
        let mut wm = BlockScanWatermarks::default();
        let id = BlockId(2);
        // 500-line output capped at the 200-line per-frame scan.
        assert_eq!(
            wm.output_scan_range(0, id, false, 500, BLOCK_OUTPUT_SCAN_LIMIT),
            (0, 200)
        );
        wm.record_scanned(0, id, 200);
        assert_eq!(
            wm.output_scan_range(0, id, true, 500, BLOCK_OUTPUT_SCAN_LIMIT),
            (200, 200)
        );
    }

    #[test]
    fn in_flight_sentinel_is_forgotten_between_commands() {
        // P3 fix (rust-reviewer): the sentinel key is reused across commands —
        // forgetting it when nothing is in flight stops the next command's
        // early output lines inheriting the previous command's watermark.
        let mut wm = BlockScanWatermarks::default();
        wm.record_scanned(0, IN_FLIGHT_KEY, 30);
        wm.forget(0, IN_FLIGHT_KEY);
        assert_eq!(
            wm.output_scan_range(0, IN_FLIGHT_KEY, true, 5, BLOCK_OUTPUT_SCAN_LIMIT),
            (0, 5),
            "next command rescans from zero"
        );
    }

    #[test]
    fn atlas_rebuild_clears_all_watermarks_and_in_flight_key_exists() {
        let mut wm = BlockScanWatermarks::default();
        assert_ne!(
            IN_FLIGHT_KEY,
            BlockId(0),
            "sentinel must not collide with real ids"
        );
        wm.record_scanned(0, IN_FLIGHT_KEY, 12);
        wm.record_scanned(0, BlockId(3), 44);
        wm.clear();
        assert_eq!(
            wm.output_scan_range(0, IN_FLIGHT_KEY, true, 20, BLOCK_OUTPUT_SCAN_LIMIT),
            (0, 20),
            "cleared watermark → live frame rescans from zero"
        );
        assert_eq!(
            wm.output_scan_range(0, BlockId(3), true, 50, BLOCK_OUTPUT_SCAN_LIMIT),
            (0, 50)
        );
    }
}

#[cfg(test)]
mod warm_atlas_incremental_tests {
    use super::{BlockScanWatermarks, MetalRenderer};
    use crate::glyph::GlyphAtlas;
    use crate::paint::overlays::{FindDrawState, NoteEditorDrawState};
    use crate::paint::tab_bar::TabBarDrawState;
    use weft_core::vt::Terminal;

    /// Mirror the golden skip precedent: no Metal device (CI without GPU)
    /// skips instead of failing (the atlas rasterizer needs a device).
    fn headless_atlas_or_skip() -> Option<GlyphAtlas> {
        let device = metal::Device::system_default()?;
        Some(GlyphAtlas::new(
            &device,
            &weft_core::config::FontConfig::default(),
            2.0,
        ))
    }

    fn empty_tab_bar() -> TabBarDrawState {
        TabBarDrawState {
            tab_count: 1,
            active_tab: 0,
            labels: Vec::new(),
            tooltips: Vec::new(),
            hovered_tab: None,
            scroll_offset: 0.0,
            plus_hovered: false,
            arrow_left_hovered: false,
            arrow_right_hovered: false,
            chrome_left: 0.0,
            layout_right: 800.0,
            drag_index: None,
            drag_ghost_x: None,
            drag_insert_index: None,
        }
    }

    /// Warm with every overlay parameter at None — only the grid + block
    /// scan contribute, so the return value is exactly the block output
    /// lines scanned.
    fn warm(
        terminal: &Terminal,
        atlas: &mut GlyphAtlas,
        live_resize: bool,
        watermarks: &std::cell::RefCell<BlockScanWatermarks>,
        namespace: u64,
    ) -> u64 {
        let no_find: Option<FindDrawState> = None;
        let no_note: Option<NoteEditorDrawState> = None;
        let tab_bar = empty_tab_bar();
        MetalRenderer::warm_atlas(
            atlas,
            600.0,
            &no_find,
            &no_note,
            terminal,
            terminal.grid(),
            None,
            None,
            None,
            None,
            None,
            None,
            &tab_bar,
            &[],
            live_resize,
            watermarks,
            namespace,
        )
    }

    /// B3-3 acceptance: one finished block (2 output lines) + one in-flight
    /// block (3 → 4 lines). Non-live scans everything; live frames scan only
    /// appended lines; a cleared watermark table (atlas rebuild) rescans.
    #[test]
    fn warm_atlas_block_scan_is_incremental_under_live_resize() {
        let Some(mut atlas) = headless_atlas_or_skip() else {
            eprintln!("skipping warm_atlas incremental test: no Metal device available");
            return;
        };
        let mut terminal = Terminal::new(10, 40);
        // Finished block: command `ls`, 2 output lines, exited.
        terminal.process(b"\x1b]133;A\x07ls\x1b]133;B\x07out-1\nout-2\n\x1b]133;C\x07");
        // In-flight block: 3 lines of live output (OSC 133;B with no C/D).
        terminal.process(b"\x1b]133;A\x07sleep\x1b]133;B\x07");
        terminal.process(b"alpha\nbeta\ngamma\n");
        assert!(
            terminal.show_block_view(),
            "shell integration must select block view"
        );
        assert_eq!(
            terminal
                .block_tracker()
                .in_flight()
                .map(|l| l.output.lines().count()),
            Some(3),
            "test fixture: in-flight block must carry 3 output lines"
        );

        let wm = std::cell::RefCell::new(BlockScanWatermarks::default());
        // Normal frame: full scan (2 finished + 3 in-flight lines).
        assert_eq!(warm(&terminal, &mut atlas, false, &wm, 0), 5);
        // Live-resize frame, nothing appended → nothing scanned (the
        // append-only watermark is the whole point of B3-3).
        assert_eq!(warm(&terminal, &mut atlas, true, &wm, 0), 0);

        // One appended in-flight line → exactly one new line scanned.
        terminal.process(b"delta\n");
        assert_eq!(warm(&terminal, &mut atlas, true, &wm, 0), 1);

        // Non-live frames keep the full scan regardless of watermarks.
        assert_eq!(warm(&terminal, &mut atlas, false, &wm, 0), 6);

        // Atlas rebuild invalidation (update_scale/rebuild_atlas clear):
        // the next live frame rescans everything the fresh atlas needs.
        wm.borrow_mut().clear();
        assert_eq!(warm(&terminal, &mut atlas, true, &wm, 0), 6);

        // Namespaces are independent (background panes don't inherit the
        // active pane's watermarks).
        assert_eq!(warm(&terminal, &mut atlas, true, &wm, 77), 6);
    }
}
