//! Per-frame glyph-atlas warm-up: collect all on-screen characters not yet
//! rasterized and add them to the atlas before vertex building runs.

use std::collections::HashSet;

use crate::glyph::GlyphAtlas;
use crate::overlay::{OverlayContent, OverlayWarmup, PaletteDrawParams, SettingsDrawParams};
use crate::paint::overlays::{FindDrawState, NoteEditorDrawState};
use crate::paint::panel::PanelDrawParams;
use crate::paint::preedit::TuiPreeditDrawParams;
use crate::paint::prompt::PromptDrawParams;
use crate::paint::tab_bar::TabBarDrawState;
use crate::paint::ui_helpers::{block_duration_str, panel_display, visible_panel_rows};
use crate::renderer::MetalRenderer;
use weft_core::complete::Match;
use weft_core::grid::Grid;
use weft_core::vt::Terminal;

impl MetalRenderer {
    /// Collect every on-screen character not yet in the glyph atlas and
    /// rasterize each exactly once before vertex building runs.
    ///
    /// This is an associated function (not a `&mut self` method) so the
    /// caller in `draw()` can pass `&mut self.atlas` as a partial borrow
    /// disjoint from the immutable `self.layer` borrow held by `drawable`
    /// for the whole frame. Taking `&mut self` here would conflict with
    /// that borrow.
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
        if terminal.show_block_view() {
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
                for line in b.output.lines().take(200) {
                    missing.extend(line.chars());
                }
            }
            // In-flight (live) block during CommandExecuting.
            if let Some(live) = terminal.block_tracker().in_flight() {
                missing.extend("❯ ".chars());
                missing.extend(live.command.chars());
                for line in live.output.lines().take(200) {
                    missing.extend(line.chars());
                }
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
}
