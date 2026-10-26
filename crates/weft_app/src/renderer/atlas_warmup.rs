//! Per-frame glyph-atlas warm-up: collect all on-screen characters not yet
//! rasterized and add them to the atlas before vertex building runs.
//!
//! v1.13.11 (PLAN_v1.13.11): the block-view scan moved from warm_atlas's
//! `.rev().take(64)` × 200-line sweep to [`MetalRenderer::
//! warm_block_view_glyphs`] — a visible-window, once-ever, identity-keyed
//! scan with a per-frame byte budget. warm_atlas keeps the grid/panel/
//! overlay warmup plus the block-view cwd/branch/diagnose strings.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

use crate::glyph::GlyphAtlas;
use crate::overlay::{OverlayContent, OverlayWarmup, PaletteDrawParams, SettingsDrawParams};
use crate::paint::block_view::layout_pass::bounds::{lower_bound_height, upper_bound_height};
use crate::paint::overlays::{FindDrawState, NoteEditorDrawState};
use crate::paint::panel::PanelDrawParams;
use crate::paint::preedit::TuiPreeditDrawParams;
use crate::paint::prompt::PromptDrawParams;
use crate::paint::tab_bar::TabBarDrawState;
use crate::paint::ui_helpers::{
    block_duration_str, live_context_label, live_elapsed_label, panel_display,
};
use crate::renderer::MetalRenderer;
use unicode_segmentation::UnicodeSegmentation;
use weft_core::blocks::{Block, BlockId};
use weft_core::complete::Match;
use weft_core::grid::Grid;
use weft_core::vt::Terminal;

/// v1.13.11 N1.5: per-pane, per-frame scan-byte budget for the block-view
/// warmup. Block capture caps at 1 MiB (blocks.rs `DEFAULT_OUTPUT_CAP`
/// territory), so an unbounded first sweep costs milliseconds of HashSet
/// inserts in one frame; this splits a max-size block's first scan across
/// ≤ 8 frames, each well under 1 ms. The budget stops at line boundaries
/// and every windowed block always scans ≥ 1 line (progress guarantee).
pub(crate) const BLOCK_WARM_BUDGET_BYTES: usize = 128 * 1024;

/// v1.13.11 N1.2: the newest K blocks are ALWAYS part of the warm window.
/// Streaming output and finalize-created blocks land at the newest end, and
/// the previous frame's prefix sum / live-height bound can be stale — this
/// constant union keeps the window a superset where it matters (plan N1.2
/// 兜底).
pub(crate) const BLOCK_WARM_NEWEST_BLOCKS: usize = 16;

/// v1.13.11 N1.2: rows of extra margin (as pixels: rows × pitch) around the
/// binary-searched visible band. Absorbs the live segment's header/
/// separator chrome (~4 rows) and f32 wobble; over-scanning is amortized by
/// once-ever.
pub(crate) const BLOCK_WARM_MARGIN_ROWS: usize = 64;

/// v1.13.11 N1.2: extra blocks included on each side of the searched band
/// (layout_pass keeps 1 for the sticky header; the warm window adds 2).
pub(crate) const BLOCK_WARM_OVERSCAN_BLOCKS: usize = 2;

/// v1.12.2 B3-3: the in-flight block has no stable [`BlockId`] of its own —
/// there is at most one per terminal, so a per-namespace sentinel key carries
/// its watermark.
///
/// v1.13.11 P1-1 (rust-reviewer): the sentinel's scan identity is the
/// tracker's per-command generation (`BlockTracker::command_generation`),
/// not a constant — chained commands (`a; b`) can cross the D→A/B boundary
/// inside one PTY batch with no intervening frame, so the
/// forget-when-idle path alone cannot stop command 2 inheriting command 1's
/// watermark. A generation mismatch restarts the live scan at 0.
const IN_FLIGHT_KEY: BlockId = BlockId(u64::MAX);

/// v1.13.11 P2-1 (rust-reviewer): output identity = (ptr, len) folded into
/// one `usize` — a bare `Arc::as_ptr` is ABA-vulnerable: a continuation
/// merge frees the old Arc and a fresh allocation can land on the same
/// address, silently matching the stale watermark. Folding `len` (rotated
/// into the high half, disjoint from macOS's low-half heap pointers) makes
/// any merge that changes the byte length a mismatch. Residual 备案 (plan
/// 风险表): a same-address AND equal-length replacement still aliases —
/// accepted, since the replacement content then spans the same extent the
/// watermark already covers.
fn output_identity(output: &str) -> usize {
    output.as_ptr() as usize ^ output.len().rotate_left(32)
}

/// v1.12.2 B3-3 (PLAN_S2_render): per-namespace, per-block output-line
/// watermarks for the incremental block-view warmup. Namespaces are
/// per-pane session ids — BlockIds are allocated per-Terminal, so a shared
/// constant namespace would leak watermarks across tab/focus switches.
///
/// v1.13.11 (N1.3): once-ever semantics for BOTH drag and idle frames —
/// a block's output lines are scanned at most once per (namespace, output
/// identity). Block output is append-only (finished blocks are immutable,
/// in-flight output grows at the tail), so lines below the watermark cannot
/// produce new glyphs. The watermark entry carries the block's output
/// identity (`output_identity`: (ptr, len) of `block.output`, P2-1 ABA
/// hardening): a continuation merge replaces a block's output while KEEPING
/// its BlockId (continuation.rs finalize), so an identity mismatch
/// invalidates the entry and the next frame rescans from line 0 — merged-in
/// lines are never skipped.
///
/// **Invalidation**: cleared by `update_scale` and `rebuild_atlas` — both
/// swap in a fresh empty atlas, and stale "already scanned" marks would then
/// skip glyphs the new atlas still needs (the fragmented-output bug class).
#[derive(Default)]
pub(crate) struct BlockScanWatermarks {
    /// (namespace, BlockId) → (scanned output lines, output identity at
    /// scan time).
    scanned_lines: HashMap<(u64, BlockId), (usize, usize)>,
    /// (namespace, BlockId) → (output line count, output identity).
    /// Identity-matched counts are permanently valid (finished output is
    /// immutable); caching them keeps every frame from re-walking a giant
    /// block just to bound the scan range. The in-flight scan skips this
    /// cache (no stable identity — it scans to EOF under the budget).
    line_counts: HashMap<(u64, BlockId), (usize, usize)>,
}

impl BlockScanWatermarks {
    /// Output line count for `(namespace, id)`, counted from `output` on
    /// first sight of a given identity. An identity mismatch (continuation
    /// merge replaced the output) forces a recount. P1-2a: the expanded
    /// scan no longer needs a total (it walks to EOF under the budget), so
    /// this remains only as the collapsed "{n} lines" fallback.
    pub(crate) fn line_count(
        &mut self,
        namespace: u64,
        id: BlockId,
        identity: usize,
        output: &str,
    ) -> usize {
        match self.line_counts.get(&(namespace, id)) {
            Some((count, idty)) if *idty == identity => *count,
            _ => {
                let count = output.lines().count();
                self.line_counts.insert((namespace, id), (count, identity));
                count
            }
        }
    }

    /// Range `[start, end)` of output lines to scan this frame for one
    /// finished block: `[watermark, line_count)`. The per-frame byte budget
    /// caps the span at the call site. A recorded watermark whose output
    /// identity no longer matches is invalidated (continuation merge)
    /// → restart at 0. Pure resume semantics (unit-tested, no device).
    /// P1-2a: callers that scan to EOF under the budget (no up-front total
    /// walk) pass `usize::MAX` — the end clamp degenerates to the watermark
    /// alone and the budget bounds the span.
    pub(crate) fn output_scan_range(
        &mut self,
        namespace: u64,
        id: BlockId,
        identity: usize,
        line_count: usize,
    ) -> (usize, usize) {
        let start = match self.scanned_lines.get(&(namespace, id)) {
            Some((lines, idty)) if *idty == identity => (*lines).min(line_count),
            _ => {
                // First sight or identity mismatch → rescan from line 0.
                self.scanned_lines.remove(&(namespace, id));
                0
            }
        };
        (start, line_count)
    }

    /// Record that `lines_total` output lines of this block have been scanned
    /// (monotonic — an append-only contract makes the count a high-water
    /// mark; `max` also guards against a transient count regression). The
    /// entry is stamped with the output identity the scan saw.
    pub(crate) fn record_scanned(
        &mut self,
        namespace: u64,
        id: BlockId,
        lines_total: usize,
        identity: usize,
    ) {
        let slot = self
            .scanned_lines
            .entry((namespace, id))
            .or_insert((0, identity));
        slot.0 = slot.0.max(lines_total);
        slot.1 = identity;
    }

    /// Atlas rebuild/scale-change invalidation: every scanned-line watermark
    /// is dropped so the next frame re-scans everything the fresh, empty
    /// atlas needs. P1-2b (rust-reviewer): line_counts SURVIVE — they are
    /// identity-keyed, a block's output is immutable per identity, and a
    /// line count has nothing to do with atlas contents; dropping them here
    /// would re-walk every windowed block's full output for nothing.
    pub(crate) fn clear(&mut self) {
        self.scanned_lines.clear();
    }

    /// P3 fix (rust-reviewer): drop one block's watermark. The in-flight
    /// sentinel key ([`IN_FLIGHT_KEY`]) is REUSED across commands — without
    /// forgetting it when no block is in flight, the next command's early
    /// output lines would inherit the previous command's watermark and be
    /// skipped (missing glyphs until the next full rescan).
    pub(crate) fn forget(&mut self, namespace: u64, id: BlockId) {
        self.scanned_lines.remove(&(namespace, id));
        self.line_counts.remove(&(namespace, id));
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
    /// v1.13.11: the block-output scan moved to
    /// [`MetalRenderer::warm_block_view_glyphs`] (visible-window, once-ever,
    /// byte-budgeted). This function keeps the grid cells, panel, prompt,
    /// overlays and the block-view cwd/branch/diagnose strings.
    ///
    /// Draw-path contract (unchanged): this is a *warmup* — the paint path
    /// itself stays lookup-only (a missing glyph paints as blank/space), so
    /// skipping the warmup would resurrect the fragmented-output bug class.
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
        completions: Option<(&[Match], usize, Option<usize>)>,
        palette: Option<&PaletteDrawParams<'_>>,
        settings: Option<&SettingsDrawParams<'_>>,
        tab_bar: &TabBarDrawState,
        diagnose_texts: &[&str],
    ) {
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
            // v1.14.4/v1.15.0: panel-only strings (footer label, ▲/▼, and
            // the three one-shot footer feedback messages) have no other
            // warm source — push_text silently skips unwarmed glyphs.
            missing.extend(crate::panel_component::FOOTER_LABEL.chars());
            missing.extend(crate::panel_component::FOOTER_FEEDBACK_LOADED_PREFIX.chars());
            missing.extend(crate::panel_component::FOOTER_FEEDBACK_LOADED_SUFFIX.chars());
            missing.extend(crate::panel_component::FOOTER_FEEDBACK_EXHAUSTED.chars());
            missing.extend(crate::panel_component::FOOTER_FEEDBACK_ERROR.chars());
            missing.extend(crate::panel_component::FOOTER_FEEDBACK_DIGITS.chars());
            missing.extend(['▲', '▼']);
            missing.extend(p.query.chars());
            // v1.15.1: warm the panel IME preedit — the paint path renders it
            // inline after the query (panel.rs), and an unwarmed CJK
            // composition's first frame would paint blank cells.
            if !p.panel_ime_preedit.is_empty() {
                missing.extend(p.panel_ime_preedit.chars());
            }
            // v1.13.18 P4: the warm-up row estimate reads the shared budget
            // function. This associated fn has no renderer state, so
            // chrome_top reads 0.0 — the WIDEST (superset) window, exactly
            // what a warm-up wants. Warm-up estimate only, NOT the viewport
            // fact source: paint / scroll clamp / hit-test / a11y read the
            // budget from the real geometry.
            let ch = atlas.cell_height as f32;
            let layout = crate::layout::layout_panel(0.0, 0.0, ch, 0.0, viewport_h);
            let rows_bottom =
                crate::layout::panel_rows_bottom(&layout, viewport_h, !p.blocks.is_empty(), ch);
            let max_blocks = crate::layout::panel_visible_row_budget(
                layout.list_top,
                layout.row_height,
                ch,
                rows_bottom,
            );
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
        if let Some((completions, _, _)) = completions {
            // Emoji icons used by the popup (📁📄 via CoreText color path).
            missing.extend(['📁', '📄', '»']);
            for m in completions {
                missing.extend(m.label.chars());
            }
        }
        // Block-view shared header strings (cwd / git branch) — block outputs
        // themselves are warmed by [`MetalRenderer::warm_block_view_glyphs`]
        // (v1.13.11 visible-window scan).
        if terminal.show_block_view() {
            if let Some(cwd) = terminal.cwd() {
                missing.extend(cwd.chars());
            }
            if let Some(branch) = terminal.git_branch() {
                missing.extend(" git:()".chars());
                missing.extend(branch.chars());
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
            // v1.15.1: warm the IME preedit (palette precedent) so the first
            // frame of a CJK composition doesn't leave blank cells.
            if !find.ime_preedit.is_empty() {
                missing.extend(find.ime_preedit.chars());
            }
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
        // v0.9 fix: warm up the command palette (Cmd+P) so CJK / other
        // non-ASCII chars typed via IME render instead of leaving blank
        // cells (same rationale as the find bar above).
        // v1.15.1: single source — query/entries/form/banner/submode/preedit
        // charsets now come from `OverlayContent::CommandPalette::warm_chars`
        // (the Settings precedent), so warmup and paint can never drift.
        // The "> " input prefix is rendered by the paint path
        // (paint/palette.rs) but not covered by warm_chars, so it stays an
        // explicit warm here.
        if let Some(p) = palette {
            missing.extend("> ".chars());
            OverlayContent::CommandPalette(*p).warm_chars(&mut missing);
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
    }

    /// v1.13.11 N1: block-view glyph warmup — the visible-window successor
    /// of the B3-3 `.rev().take(64)` × 200-line sweep (now deleted).
    ///
    /// Window (N1.2): the finished blocks intersecting a conservative
    /// SUPERSET of the visible band, binary-searched with the LAYOUT side's
    /// authoritative bound functions (`lower_bound_height` /
    /// `upper_bound_height`: prefix_sum + header depth + clear spacers),
    /// widened by [`BLOCK_WARM_MARGIN_ROWS`] and ±
    /// [`BLOCK_WARM_OVERSCAN_BLOCKS`], and ALWAYS unioned with the newest
    /// [`BLOCK_WARM_NEWEST_BLOCKS`] blocks. Superset argument (plan N1.2
    /// 裕量论证):
    /// - the clip band is approximated by the whole viewport
    ///   (`threshold_high` uses `viewport_h` ≥ content_bottom_y − clip_top)
    ///   — only ever larger than the true band;
    /// - `live_height_px` is the PREVIOUS frame's live total, gated to the
    ///   pane that owns the live cache (`LiveLayoutCache::pane() == ns`) and
    ///   to frames with a block in flight — otherwise 0 (the "live ends →
    ///   window resets" contract). Streaming growth can push genuinely-new
    ///   bottom blocks out of the shifted band; the constant {newest 16}
    ///   union covers them (the live block itself has its own watermark
    ///   path below);
    /// - the prefix sum is the previous frame's sync (the active pane syncs
    ///   last in a frame, so the warmup reads its geometry); a stale entry
    ///   costs at most one frame of lateness under once-ever. A prefix sum
    ///   that doesn't cover this block set falls back to a full scan
    ///   (layout_pass.rs:294-295 同款).
    ///
    /// Background panes cannot derive per-pane geometry from the global
    /// block_layout_cache: they pass `visible_override` — the pane's
    /// previous rebuild's visible set (renderer `bg_visible_blocks`, N1.4) —
    /// which replaces the geometric window (still unioned with {newest 16}).
    ///
    /// Watermarks (N1.3): once-ever, identity-keyed — see
    /// [`BlockScanWatermarks`]. Collapsed blocks skip output scanning and
    /// record NO watermark (expanding must look like a first scan); command/
    /// duration/header strings always scan (the header row still renders
    /// them). The in-flight block keeps its append-only [`IN_FLIGHT_KEY`]
    /// watermark with the 200-line cap removed. Scan cost is bounded per
    /// pane per frame by [`BLOCK_WARM_BUDGET_BYTES`] (N1.5).
    ///
    /// Same partial-borrow contract as [`MetalRenderer::warm_atlas`]:
    /// associated function, `&mut atlas` disjoint from everything else.
    ///
    /// `cwd`/`git_branch` are the pane terminal's cwd/branch
    /// (`Terminal::cwd()` / `Terminal::git_branch()`, layout_pass 同参) —
    /// v1.13.13 FIX-E feeds them into the in-flight header label warm.
    ///
    /// Returns the number of output bytes actually scanned this frame (the
    /// budget's test observability hook).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn warm_block_view_glyphs(
        atlas: &mut GlyphAtlas,
        tracker: &weft_core::blocks::BlockTracker,
        show_block_view: bool,
        warm_namespace: u64,
        visible_override: Option<&[BlockId]>,
        cwd: Option<&str>,
        git_branch: Option<&str>,
        viewport_h: f32,
        block_scroll: f32,
        viewport_rows: usize,
        scale: f64,
        block_cache: &crate::paint::grid_cache::BlockLayoutCache,
        live_cache: &crate::paint::live_cache::LiveLayoutCache,
        watermarks: &RefCell<BlockScanWatermarks>,
    ) -> u64 {
        if !show_block_view {
            return 0;
        }
        let blocks = tracker.session_blocks();
        let n = blocks.len();
        // Row pitch = the atlas cell height in physical px (layout.pitch ==
        // ctx.cell_h, layout/terminal.rs:274).
        let pitch = atlas.cell_height as f32;
        // Same formula the paint path uses (block_view.rs:94).
        let header_height = crate::paint::block_view::block_header_band_height(pitch, scale);
        // N1.2 live gating: only the pane whose live cache this is pays the
        // live-height band shift.
        let live_height_px = if tracker.in_flight().is_some() && live_cache.pane() == warm_namespace
        {
            // Note: this is the window TOTAL only — the live segment adds
            // ~4 rows of blank/command/header chrome on top. The shortfall
            // shifts the band toward older blocks by <5 rows, well inside
            // BLOCK_WARM_MARGIN_ROWS, and the {newest 16} union covers the
            // newest end regardless.
            live_cache.total_display_rows() as f32 * pitch
        } else {
            0.0
        };

        let mut missing: HashSet<char> = HashSet::new();
        // v1.13.11: block output graphemes — multi-scalar clusters go to the
        // CoreText cluster atlas (get_or_rasterize_cluster) so an ℹ️/⚠️ in
        // block history renders like the live grid. Keyed by the cluster
        // string itself (Borrow<str> lookups avoid per-occurrence allocs).
        let mut missing_clusters: HashMap<String, (char, bool)> = HashMap::new();
        let mut wm = watermarks.borrow_mut();
        let mut budget_left = BLOCK_WARM_BUDGET_BYTES;
        let mut scanned_bytes = 0u64;

        // Oldest-first selection. Geometric window indices are newest-first
        // (0 = newest) and convert via `blocks[n - 1 - idx]`
        // (layout_pass.rs:340 同款).
        let newest_start = n.saturating_sub(BLOCK_WARM_NEWEST_BLOCKS);
        let selected: Vec<&Block> = match visible_override {
            Some(visible) => {
                let set: HashSet<u64> = visible.iter().map(|id| id.0).collect();
                blocks
                    .iter()
                    .enumerate()
                    .filter(|(i, b)| *i >= newest_start || set.contains(&b.id.0))
                    .map(|(_, b)| b)
                    .collect()
            }
            None => {
                let prefix_sum = block_cache.prefix_sum();
                let clear_ps = block_cache.clear_prefix_sum();
                let (win_start, win_end) = block_warm_window(
                    n,
                    prefix_sum,
                    clear_ps,
                    pitch,
                    header_height,
                    viewport_rows as f32 * pitch,
                    viewport_h,
                    block_scroll * pitch,
                    live_height_px,
                );
                let start = win_start.min(BLOCK_WARM_NEWEST_BLOCKS).min(n);
                let end = win_end.max(BLOCK_WARM_NEWEST_BLOCKS.min(n)).min(n);
                (start..end).map(|idx| &blocks[n - 1 - idx]).collect()
            }
        };

        // v1.13.13 FIX-C: the in-flight scan runs BEFORE the finished loop —
        // it gets first claim on the shared [`BLOCK_WARM_BUDGET_BYTES`]. The
        // old order let a megabyte finished block consume ≤ 8 frames of
        // budget while a live command's fresh rows starved (v1.13.11 备案
        // starvation order). Deliberate side effect (评审 P3 备案): the
        // untouched-budget first-line guarantee effectively hands over from
        // the finished head block to the live lines — a finished block's
        // first over-budget line defers one extra frame, by design, not a
        // regression.
        //
        // In-flight (live) block during CommandExecuting. Append-only
        // watermark, 200-line cap REMOVED (N1.3) — the budget bounds the
        // per-frame cost instead. No line-count cache: a live output has no
        // identity Arc; scanning from the watermark to EOF under the budget
        // needs no total. P1-1: the watermark's identity keys on the
        // tracker's COMMAND GENERATION — a chained command that starts
        // inside the same batch as its predecessor's finalize bumps the
        // generation, so its live scan restarts at 0 instead of inheriting
        // the previous command's watermark. v1.13.13 FIX-D: the epoch folds
        // in too — a screen-snapshot REWRITE (`replace_screen_snapshot` /
        // `split_screen_history`) replaces the live text wholesale,
        // invalidating the append-only watermark. Both counters are
        // monotonic; the fold is injective while both stay < 2^32.
        if let Some(live) = tracker.in_flight() {
            missing.extend("❯ ".chars());
            missing.extend(live.command.chars());
            // v1.13.13 FIX-E: the live header renders `live_context_label`
            // (layout_pass.rs 同参: abbreviated cwd · git branch · elapsed),
            // whose product charset (abbreviated path forms, branch name,
            // `…` ellipsis) does not coincide with the raw command/output
            // strings — warm the rendered artifact itself, same inputs as
            // the paint path (`live.cwd` first, terminal cwd fallback).
            let elapsed = live_elapsed_label(live.started_at, SystemTime::now());
            if let Some(label) = live_context_label(live.cwd.or(cwd), git_branch, Some(&elapsed)) {
                missing.extend(label.chars());
            }
            let live_identity = (tracker.command_generation()
                ^ tracker.screen_rewrite_epoch().rotate_left(32))
                as usize;
            let (start, end) =
                wm.output_scan_range(warm_namespace, IN_FLIGHT_KEY, live_identity, usize::MAX);
            let mut scanned_to = start;
            let mut bytes = 0usize;
            for line in live.output.lines().skip(start).take(end - start) {
                // P1-2c: same untouched-budget-only first-line guarantee.
                let first_line_guarantee = bytes == 0 && budget_left == BLOCK_WARM_BUDGET_BYTES;
                if bytes + line.len() > budget_left && !first_line_guarantee {
                    break;
                }
                bytes += line.len();
                scanned_to += 1;
                collect_line_glyphs(&mut missing, &mut missing_clusters, line);
            }
            wm.record_scanned(warm_namespace, IN_FLIGHT_KEY, scanned_to, live_identity);
            // FIX-C: first claim on the budget — the finished loop below
            // sees whatever the live scan left (the old trailing position
            // never needed this deduction).
            budget_left = budget_left.saturating_sub(bytes);
            scanned_bytes += bytes as u64;
        } else {
            // P3 fix: the sentinel watermark belongs to the *previous*
            // command once nothing is in flight. The generation key above
            // is the real invalidation since P1-1; the forget stays as the
            // table-bounds hygiene.
            wm.forget(warm_namespace, IN_FLIGHT_KEY);
        }

        for b in selected {
            let identity = output_identity(&b.output);
            // Command + duration strings render on every header, collapsed
            // or not — always scanned (one short line each).
            missing.extend("❯ ".chars());
            missing.extend(b.command.chars());
            missing.extend(block_duration_str(b).chars());
            // Per-block header cwd/status (layout_pass.rs:563-571 render
            // these; before v1.13.11 they were never warmed, so deep-scrolled
            // old blocks could miss their header glyphs). P1-2a: the
            // potentially-full line-count walk only feeds the COLLAPSED
            // "{n} lines" summary (block_component.rs:52-61 — expanded
            // blocks render the cwd instead), so it lives in this branch
            // and never bypasses the byte budget on the expanded path.
            let presentation = if b.collapsed {
                let header_lines = block_cache
                    .get_if_cached(b.id.0)
                    .map(|c| c.content.line_meta.len())
                    .unwrap_or_else(|| wm.line_count(warm_namespace, b.id, identity, &b.output));
                crate::block_component::block_presentation(b, header_lines)
            } else {
                crate::block_component::block_presentation(b, 0)
            };
            missing.extend(presentation.cwd.chars());
            missing.extend(presentation.status.chars());
            if b.collapsed {
                // Collapsed short-circuit: the output region is not
                // rendered, so its glyphs are not needed — and NO watermark
                // is recorded, so an expanded block's first frame rescans
                // from line 0 (评审 P2-2: a full watermark here would make
                // the expansion permanently blank).
                continue;
            }
            // P1-2a: scan to EOF under the budget — NO full line-count walk
            // up front. The identity-keyed watermark bounds `start`, and
            // `usize::MAX` removes the end clamp (same shape as the
            // in-flight path below).
            let (start, end) = wm.output_scan_range(warm_namespace, b.id, identity, usize::MAX);
            let mut scanned_to = start;
            let mut bytes = 0usize;
            for line in b.output.lines().skip(start).take(end - start) {
                // Budget stop at a line boundary. P1-2c: the first-line
                // progress guarantee only fires while this frame's budget
                // is UNTOUCHED — once anything was spent this frame, even
                // a block's first over-budget line defers to the next
                // frame instead of punching through the budget.
                let first_line_guarantee = bytes == 0 && budget_left == BLOCK_WARM_BUDGET_BYTES;
                if bytes + line.len() > budget_left && !first_line_guarantee {
                    break;
                }
                bytes += line.len();
                scanned_to += 1;
                collect_line_glyphs(&mut missing, &mut missing_clusters, line);
            }
            wm.record_scanned(warm_namespace, b.id, scanned_to, identity);
            budget_left = budget_left.saturating_sub(bytes);
            scanned_bytes += bytes as u64;
        }

        for ch in &missing {
            atlas.get_or_rasterize(*ch, crate::glyph::GlyphStyle::REGULAR);
        }
        for (cluster, (lead, is_wide)) in &missing_clusters {
            atlas.get_or_rasterize_cluster(cluster, *lead, *is_wide);
        }
        scanned_bytes
    }
}

/// v1.13.11 N1.2: the warm window's NEWEST-FIRST index range for `n` blocks.
/// Pure geometry over the shared height formula
/// `prefix_sum[j]*pitch + j*header_height + clear_ps[j]*clear_pitch` —
/// reuses the layout side's bound functions instead of re-deriving (the
/// formula drift risk is the v1 draft's P0). Returns `(0, n)` (full scan)
/// when the prefix sum doesn't cover the block set. Unit-tested in
/// `atlas_warmup_tests.rs` (window-superset suite).
#[allow(clippy::too_many_arguments)] // pure geometry kernel; mirrors layout_pass's bound fns
fn block_warm_window(
    n: usize,
    prefix_sum: &[usize],
    clear_ps: &[usize],
    pitch: f32,
    header_height: f32,
    clear_pitch: f32,
    viewport_h: f32,
    scroll_px: f32,
    live_height_px: f32,
) -> (usize, usize) {
    if n == 0 || prefix_sum.len() != n + 1 {
        return (0, n);
    }
    let margin_px = BLOCK_WARM_MARGIN_ROWS as f32 * pitch;
    // Same shape as layout_pass.rs:299-301 with conservative clip constants:
    // content_bottom == clip_bottom makes the low threshold scroll-only, and
    // the whole viewport stands in for (content_bottom − clip_top).
    let threshold_low = scroll_px - margin_px - live_height_px;
    let threshold_high = viewport_h + scroll_px + margin_px - live_height_px;
    let j_low = lower_bound_height(
        prefix_sum,
        clear_ps,
        pitch,
        header_height,
        clear_pitch,
        threshold_low,
    );
    let j_high = upper_bound_height(
        prefix_sum,
        clear_ps,
        pitch,
        header_height,
        clear_pitch,
        threshold_high,
    );
    // first_visible −1 (layout) then the warm-side ±2-block overscan.
    (
        j_low.saturating_sub(1 + BLOCK_WARM_OVERSCAN_BLOCKS),
        (j_high + BLOCK_WARM_OVERSCAN_BLOCKS).min(n),
    )
}

/// Collect one output line's glyphs into the warm sets: single-scalar
/// graphemes as chars, multi-scalar graphemes (ℹ️, ⚠️, combining-mark runs)
/// as cluster-atlas entries with their lead scalar and width class.
fn collect_line_glyphs(
    missing: &mut HashSet<char>,
    missing_clusters: &mut HashMap<String, (char, bool)>,
    line: &str,
) {
    for grapheme in line.graphemes(true) {
        let mut chars = grapheme.chars();
        let lead = chars.next().unwrap_or(' ');
        if chars.next().is_some() {
            if !missing_clusters.contains_key(grapheme) {
                let is_wide = weft_core::grid::terminal_text_width(grapheme) > 1;
                missing_clusters.insert(grapheme.to_string(), (lead, is_wide));
            }
        } else if lead != ' ' {
            missing.insert(lead);
        }
    }
}

// v1.13.11: the watermark + warm-mechanism test suites moved to
// `atlas_warmup_tests.rs` (#[path] companion file) — this file sits close to
// the 800-line commit gate and N1's rewritten suites are sizeable.
// v1.13.13: the FIX-C/D/E suites went to a second companion
// (`atlas_warmup_v11313_tests.rs`) — the first companion was itself near
// the gate, so the new suites got their own file instead of pushing it over.
#[cfg(test)]
#[path = "atlas_warmup_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "atlas_warmup_v11313_tests.rs"]
mod v11313_tests;
