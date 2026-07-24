//! Pure presentation model for BlockView metadata.

use crate::paint::grid_cache::block_line_chunks;
use crate::paint::ui_helpers::{abbreviate_path, block_duration_str};
use weft_core::blocks::Block;
use weft_core::vt::Terminal;

mod hover;
pub(crate) use hover::{block_header_action_at, hovered_block_for_row, BlockHeaderAction};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BlockTone {
    Success,
    Error,
    Warning,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BlockPresentation {
    pub(crate) label: String,
    pub(crate) tone: BlockTone,
}

pub(crate) fn block_presentation(block: &Block, output_lines: usize) -> BlockPresentation {
    let duration = block_duration_str(block);
    let status = match block.exit_code {
        Some(code) => format!("exit {code}"),
        None => "interrupted".to_string(),
    };
    let tone = match block.exit_code {
        Some(0) => BlockTone::Success,
        Some(_) => BlockTone::Error,
        None => BlockTone::Warning,
    };

    let mut parts = if block.collapsed {
        let unit = if output_lines == 1 { "line" } else { "lines" };
        vec![format!("{output_lines} {unit}")]
    } else {
        vec![block
            .cwd
            .as_deref()
            .map(abbreviate_path)
            .unwrap_or_else(|| "~".to_string())]
    };
    if !duration.is_empty() {
        parts.push(duration);
    }
    parts.push(status);

    BlockPresentation {
        label: parts.join(" · "),
        tone,
    }
}

/// OpenCode 1.18.x clears its TUI but emits no session card on exit (verified
/// from the raw PTY stream). Surface stable CLI recovery commands without
/// inventing a session id or coupling Weft to OpenCode's private database.
pub(crate) fn command_resume_hints(block: &Block) -> &'static [&'static str] {
    const OPENCODE_HINTS: &[&str] = &[
        "Continue last session: opencode -c",
        "Choose a session: opencode session list; opencode -s <session-id>",
    ];
    let executable = block
        .command
        .split_whitespace()
        .next()
        .and_then(|part| part.rsplit('/').next());
    if block.exit_code != Some(0) && executable == Some("opencode") {
        OPENCODE_HINTS
    } else {
        &[]
    }
}

pub(crate) fn live_context_label(cwd: Option<&str>, git_branch: Option<&str>) -> Option<String> {
    let cwd = cwd.map(abbreviate_path).filter(|cwd| !cwd.is_empty())?;
    Some(match git_branch {
        Some(branch) if !branch.is_empty() => format!("{cwd} git:({branch})"),
        _ => cwd,
    })
}

/// Prompt height belongs only to the shell editor. A running full-screen
/// application paints BlockView to the bottom of the viewport.
pub(crate) fn block_prompt_lines(terminal: &Terminal) -> Option<usize> {
    (terminal.effective_input_mode() == weft_core::input::InputMode::Editor)
        .then_some(terminal.editor().buffer.lines.len())
}

/// Total/visible BlockView rows used by scrollbar and input geometry.
pub(crate) fn completed_block_row_count(output_lines: usize, header_rows: usize) -> usize {
    output_lines + header_rows.max(1) + 2 // command + accessible header band + gap
}

/// A successful shell `clear` leaves one terminal-sized blank screen between
/// the blocks on either side of it. Keep this semantic in rows so painting,
/// scrolling, find navigation and history-panel jumps cannot drift apart.
pub(crate) fn clear_block_spacer_rows(command: &str, viewport_rows: usize) -> usize {
    usize::from(command.split_whitespace().next() == Some("clear")) * viewport_rows.max(1)
}

fn completed_block_visible_lines(block: &Block) -> Vec<&str> {
    if block.collapsed {
        return Vec::new();
    }
    let lines: Vec<&str> = block.output.lines().collect();
    let len = crate::paint::grid_cache::trimmed_output_line_count(&lines);
    lines[..len].to_vec()
}

pub(crate) fn completed_block_output_rows(block: &Block, cols: usize) -> usize {
    if block.collapsed {
        return 0;
    }
    command_resume_hints(block)
        .iter()
        .copied()
        .chain(completed_block_visible_lines(block))
        .map(|line| block_line_chunks(line, cols).count())
        .sum()
}

pub(crate) fn completed_block_layout_rows(
    block: &Block,
    cols: usize,
    header_rows: usize,
    viewport_rows: usize,
) -> usize {
    completed_block_output_rows(block, cols)
        + completed_block_row_count(0, header_rows)
        + clear_block_spacer_rows(&block.command, viewport_rows)
}

/// Distance from the bottom of a completed block to the visual row that owns
/// a Find hit. This mirrors the renderer's bottom-to-top order, including
/// wrapped output and recovery hints.
pub(crate) fn completed_block_match_row_from_bottom(
    block: &Block,
    hit: &weft_core::find::BlockMatch,
    cols: usize,
) -> usize {
    if block.collapsed {
        return 1;
    }
    let hints = command_resume_hints(block)
        .iter()
        .map(|hint| block_line_chunks(hint, cols).count())
        .sum::<usize>();
    let lines = completed_block_visible_lines(block);
    if hit.is_command {
        return hints
            + lines
                .iter()
                .map(|line| block_line_chunks(line, cols).count())
                .sum::<usize>()
            + 1;
    }

    let line_index = hit.line.min(lines.len().saturating_sub(1));
    let rows_after = lines
        .iter()
        .skip(line_index.saturating_add(1))
        .map(|line| block_line_chunks(line, cols).count())
        .sum::<usize>();
    let chunks = lines
        .get(line_index)
        .map(|line| block_line_chunks(line, cols).collect::<Vec<_>>())
        .unwrap_or_default();
    let mut chars_before = 0usize;
    let chunk_index = chunks
        .iter()
        .position(|chunk| {
            let end = chars_before + chunk.chars().count();
            let owns = hit.col < end || end == chars_before;
            chars_before = end;
            owns
        })
        .unwrap_or_else(|| chunks.len().saturating_sub(1));
    hints + rows_after + chunks.len().saturating_sub(chunk_index + 1) + 1
}

pub(crate) fn block_content_metrics(
    terminal: &Terminal,
    cols: usize,
    header_rows: usize,
) -> (usize, usize) {
    block_content_metrics_with_cache(terminal, cols, header_rows, None)
}

/// R2-2: cached variant of [`block_content_metrics`]. When `cache` is `Some`
/// and a block's layout is cached, reads `output_rows` in O(1) instead of
/// re-wrapping every output line in O(m) via [`completed_block_output_rows`].
/// Falls back to the direct path for uncached blocks (e.g. a block finalized
/// after the last paint, or callers without renderer access).
pub(crate) fn block_content_metrics_with_cache(
    terminal: &Terminal,
    cols: usize,
    header_rows: usize,
    cache: Option<&crate::paint::grid_cache::BlockLayoutCache>,
) -> (usize, usize) {
    use weft_core::blocks::ShellPhase;

    let mut total = 0;
    let viewport_rows = terminal.grid().num_rows.max(1);
    for block in terminal.block_tracker().session_blocks() {
        // R2-2: read cached output_rows (O(1)) instead of re-wrapping every
        // block's output every frame (O(m) per block). The cache is keyed by
        // BlockId.0; ensure_cached was already called by the paint path, so
        // the entry exists. If somehow it doesn't (e.g. block finalized after
        // the last paint), fall back to the direct computation.
        let output_rows = cache
            .and_then(|c| c.get_if_cached(block.id.0))
            .map(|c| {
                if c.cols != cols || c.collapsed != block.collapsed {
                    // Stale cache entry — fall back. ensure_cached will fix it
                    // on the next paint frame. This is rare (resize between
                    // paint and metrics) and correct, just not O(1).
                    completed_block_output_rows(block, cols)
                } else {
                    c.output_rows
                }
            })
            .unwrap_or_else(|| completed_block_output_rows(block, cols));
        total += output_rows
            + completed_block_row_count(0, header_rows)
            + clear_block_spacer_rows(&block.command, viewport_rows);
    }
    if terminal.block_tracker().phase() == ShellPhase::CommandExecuting {
        if let Some(live) = terminal.block_tracker().in_flight() {
            let lines: Vec<&str> = live.output.lines().collect();
            let start = lines
                .len()
                .saturating_sub(crate::paint::grid_cache::MAX_LAYOUT_LINES_LIVE);
            total += lines[start..]
                .iter()
                .map(|line| block_line_chunks(line, cols).count())
                .sum::<usize>();
            total += 2; // command + gap
            total += usize::from(live.cwd.or(terminal.cwd()).is_some());
        }
    }
    (total, terminal.grid().num_rows.max(1))
}

/// Whether fresh PTY output should keep the BlockView pinned to the live tail.
///
/// A primary-screen application also emits output when it repaints after
/// SIGWINCH. Once the user has explicitly entered its detached history view,
/// that repaint must not steal the viewport and switch back to the live Grid.
pub(crate) fn should_follow_running_output(
    phase: weft_core::blocks::ShellPhase,
    primary_history_view: bool,
) -> bool {
    phase == weft_core::blocks::ShellPhase::CommandExecuting && !primary_history_view
}

/// Clamp a bottom-relative BlockView offset to the range produced by the
/// current viewport and wrapping width.
///
/// Width, height, font, sidebar and prompt changes can all shrink the range;
/// retaining an offset from the previous layout makes the newest rows
/// unreachable or paints blank space.
pub(crate) fn reconciled_block_scroll(
    scroll: usize,
    total_rows: usize,
    visible_rows: usize,
) -> usize {
    scroll.min(total_rows.saturating_sub(visible_rows))
}

/// Reconcile one tab's detached transcript after any geometry change.
///
/// Window resize events are not the only source of new terminal dimensions:
/// opening a panel, changing font metrics and restoring a window all call the
/// shared layout recomputation path too. Keeping the full calculation here
/// prevents those paths from drifting apart.
pub(crate) fn reconciled_terminal_block_scroll(
    terminal: &Terminal,
    layout_ctx: &crate::layout::LayoutCtx,
    header_rows: usize,
    current: usize,
) -> Option<(usize, usize, usize)> {
    if !terminal.show_block_view() {
        return None;
    }
    let (total, _) = block_content_metrics(terminal, terminal.grid().num_cols, header_rows);
    let cwd_header = crate::layout::block_cwd_header_active(
        terminal.effective_input_mode() == weft_core::input::InputMode::Editor,
        terminal.cwd().is_some(),
    );
    let visible =
        crate::layout::block_visible_rows(layout_ctx, block_prompt_lines(terminal), cwd_header);
    Some((
        reconciled_block_scroll(current, total, visible),
        total,
        visible,
    ))
}

/// F3-2: Braille spinner glyphs for the running-command activity indicator.
/// Cycled left-to-right by `spinner_phase` (see `spinner_char_for_phase`).
pub(crate) const SPINNER_CHARS: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// F3-2: Map a normalized phase [0, 1) to a braille spinner glyph.
/// Returns `●` (static dot) when `reduce_motion` is true or the phase is
/// negative (disabled). Pure logic — unit-tested.
pub(crate) fn spinner_char_for_phase(phase: f32, reduce_motion: bool) -> char {
    if reduce_motion || phase < 0.0 {
        return '●';
    }
    let idx = ((phase * SPINNER_CHARS.len() as f32) as usize) % SPINNER_CHARS.len();
    SPINNER_CHARS[idx]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};
    use weft_core::blocks::{Block, BlockId};

    fn block(exit_code: Option<i32>, collapsed: bool) -> Block {
        let started_at = SystemTime::UNIX_EPOCH;
        Block {
            id: BlockId(1),
            command: "cargo test".into(),
            cwd: Some("/tmp/weft".into()),
            output: "one\ntwo\nthree\n".into(),
            styled_output: None,
            exit_code,
            started_at,
            finished_at: Some(started_at + Duration::from_millis(1200)),
            collapsed,
        }
    }

    #[test]
    fn collapsed_block_uses_one_line_summary() {
        let presentation = block_presentation(&block(Some(0), true), 3);
        assert_eq!(presentation.label, "3 lines · 1.2s · exit 0");
        assert_eq!(presentation.tone, BlockTone::Success);
    }

    #[test]
    fn expanded_block_keeps_context_and_surfaces_failure() {
        let presentation = block_presentation(&block(Some(7), false), 3);
        assert_eq!(presentation.label, "/tmp/weft · 1.2s · exit 7");
        assert_eq!(presentation.tone, BlockTone::Error);
    }

    #[test]
    fn interrupted_block_has_warning_tone() {
        let presentation = block_presentation(&block(None, true), 1);
        assert_eq!(presentation.label, "1 line · 1.2s · interrupted");
        assert_eq!(presentation.tone, BlockTone::Warning);
    }

    #[test]
    fn interrupted_opencode_block_surfaces_stable_resume_commands() {
        let mut interrupted = block(None, false);
        interrupted.command = "/Users/me/.opencode/bin/opencode".into();
        assert_eq!(
            command_resume_hints(&interrupted),
            [
                "Continue last session: opencode -c",
                "Choose a session: opencode session list; opencode -s <session-id>",
            ]
        );

        interrupted.exit_code = Some(130);
        assert!(!command_resume_hints(&interrupted).is_empty());
        interrupted.exit_code = Some(0);
        assert!(command_resume_hints(&interrupted).is_empty());
    }

    #[test]
    fn running_block_metrics_include_context_as_a_live_header() {
        fn running_terminal(cwd: bool) -> Terminal {
            let mut terminal = Terminal::new(24, 80);
            if cwd {
                terminal.process(b"\x1b]7;file://localhost/Users/me/.hermes\x07");
            }
            terminal.process(b"\x1b]133;A\x07hermes update\x1b]133;B\x07\x1b]133;C\x07");
            terminal
        }

        let mut late_cwd = running_terminal(false);
        assert_eq!(block_content_metrics(&late_cwd, 80, 1).0, 2);
        late_cwd.process(b"\x1b]7;file://localhost/Users/me/.hermes\x07");
        assert_eq!(block_content_metrics(&late_cwd, 80, 1).0, 3);
        assert_eq!(block_content_metrics(&running_terminal(true), 80, 1).0, 3);
        assert_eq!(
            live_context_label(Some("/Users/me/.hermes"), Some("main")),
            Some("/Users/me/.hermes git:(main)".into())
        );
    }

    #[test]
    fn primary_tui_document_exposes_positive_local_scroll_range() {
        let mut terminal = Terminal::new(4, 32);
        terminal.process(b"\x1b]133;A\x07claude\x1b]133;B\x07\x1b]133;C\x07");
        terminal.process(b"\x1b[H\x1b[2;1H\x1b[2J\x1b[Hone\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix");
        terminal.set_primary_history_view(true);

        let total = block_content_metrics(&terminal, 32, 2).0;
        assert!(total > terminal.grid().num_rows);
        assert!(total.saturating_sub(terminal.grid().num_rows) > 0);
    }

    #[test]
    fn primary_history_repaint_does_not_force_follow_live_tail() {
        use weft_core::blocks::ShellPhase;

        assert!(should_follow_running_output(
            ShellPhase::CommandExecuting,
            false
        ));
        assert!(!should_follow_running_output(
            ShellPhase::CommandExecuting,
            true
        ));
        assert!(!should_follow_running_output(ShellPhase::AtPrompt, false));
    }

    #[test]
    fn block_scroll_is_reconciled_when_a_larger_viewport_shrinks_the_range() {
        let compact = crate::layout::LayoutCtx::new((800.0, 400.0), 10.0, 20.0, 10.0, 10.0);
        let expanded = crate::layout::LayoutCtx::new((1200.0, 900.0), 10.0, 20.0, 10.0, 10.0);
        let compact_visible = crate::layout::block_visible_rows(&compact, None, false);
        let expanded_visible = crate::layout::block_visible_rows(&expanded, None, false);
        assert!(expanded_visible > compact_visible);

        let total = 100;
        let compact_top = total - compact_visible;
        let expanded_top = total - expanded_visible;
        assert_eq!(
            reconciled_block_scroll(compact_top, total, expanded_visible),
            expanded_top
        );
        assert_eq!(reconciled_block_scroll(12, 20, 30), 0);
    }

    #[test]
    fn terminal_scroll_reconciliation_uses_shared_layout_geometry() {
        let mut terminal = Terminal::new(8, 32);
        terminal.process(
            b"\x1b]133;A\x07long-command\x1b]133;B\x07\x1b]133;C\x07\x1b[H\x1b[2;1H\x1b[2J\x1b[Hone\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix",
        );
        terminal.set_primary_history_view(true);
        let layout = crate::layout::LayoutCtx::new((1200.0, 900.0), 10.0, 20.0, 10.0, 10.0);

        let (scroll, total, visible) =
            reconciled_terminal_block_scroll(&terminal, &layout, 2, usize::MAX).unwrap();
        assert_eq!(scroll, total.saturating_sub(visible));
    }

    #[test]
    fn block_metrics_include_wrapped_resume_hint_rows() {
        let mut terminal = Terminal::new(24, 80);
        terminal.process(b"\x1b]133;A\x07opencode\x1b]133;B\x07\x1b]133;C\x07\x1b]133;D;130\x07");

        assert_eq!(block_content_metrics(&terminal, 80, 1).0, 5);
        assert!(block_content_metrics(&terminal, 20, 1).0 > 5);
        assert_eq!(
            block_content_metrics(&terminal, 80, 2).0,
            block_content_metrics(&terminal, 80, 1).0 + 1
        );
    }

    #[test]
    fn clear_block_reserves_a_reachable_terminal_sized_history_band() {
        let mut terminal = Terminal::new(6, 80);
        terminal.process(
            b"\x1b]133;A\x07echo old\x1b]133;B\x07\x1b]133;C\x07old output\r\n\x1b]133;D;0\x07",
        );
        terminal.process(b"\x1b]133;A\x07clear\x1b]133;B\x07\x1b]133;C\x07\x1b]133;D;0\x07");

        assert_eq!(clear_block_spacer_rows("clear", 6), 6);
        assert_eq!(clear_block_spacer_rows("clear --keep-scrollback", 6), 6);
        assert_eq!(clear_block_spacer_rows("printf clear", 6), 0);

        let total = block_content_metrics(&terminal, 80, 1).0;
        assert_eq!(total, 13);
        assert!(
            total.saturating_sub(terminal.grid().num_rows) >= terminal.grid().num_rows,
            "the block above clear must remain reachable: total={total}"
        );
    }

    #[test]
    fn wrapped_rows_keep_emoji_graphemes_atomic() {
        assert_eq!(block_line_chunks("A👩‍🔬B", 4).count(), 1);
        assert_eq!(block_line_chunks("A👩‍🔬B", 3).count(), 2);
    }

    #[test]
    fn block_metrics_match_renderer_for_structural_rows() {
        let mut terminal = Terminal::new(24, 80);
        terminal.process(
            b"\x1b]133;A\x07report\x1b]133;B\x07\x1b]133;C\x07\
              \xe2\x94\x82 column one \xe2\x94\x82 column two \xe2\x94\x82\r\n\
              \xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\r\n\
              \x1b]133;D;0\x07",
        );

        // Both structural lines are clipped to one rendered row at 8 cols.
        // The block itself contributes command + header + gap.
        assert_eq!(block_content_metrics(&terminal, 8, 1).0, 5);
    }

    #[test]
    fn block_metrics_trim_the_same_finished_prompt_rows_as_renderer() {
        let mut terminal = Terminal::new(24, 80);
        terminal.process(
            b"\x1b]133;A\x07report\x1b]133;B\x07\x1b]133;C\x07output\r\n%\r\n$\r\n#\r\n\x1b]133;D;0\x07",
        );

        assert_eq!(block_content_metrics(&terminal, 80, 1).0, 4);
    }

    #[test]
    fn block_metrics_cap_live_rows_to_the_renderer_window() {
        let mut terminal = Terminal::new(24, 80);
        terminal.process(b"\x1b]133;A\x07stream\x1b]133;B\x07\x1b]133;C\x07");
        let output = (0..2005)
            .map(|index| format!("line {index}\r\n"))
            .collect::<String>();
        terminal.process(output.as_bytes());

        assert_eq!(block_content_metrics(&terminal, 80, 1).0, 2002);
    }

    #[test]
    fn spinner_char_returns_static_dot_for_reduce_motion() {
        assert_eq!(spinner_char_for_phase(0.0, true), '●');
        assert_eq!(spinner_char_for_phase(0.5, true), '●');
    }

    #[test]
    fn spinner_char_returns_static_dot_for_negative_phase() {
        assert_eq!(spinner_char_for_phase(-1.0, false), '●');
    }

    #[test]
    fn spinner_char_cycles_through_all_glyphs() {
        let n = SPINNER_CHARS.len();
        for (i, expected) in SPINNER_CHARS.iter().enumerate() {
            let phase = i as f32 / n as f32;
            assert_eq!(spinner_char_for_phase(phase, false), *expected);
        }
    }

    #[test]
    fn spinner_char_wraps_around_at_one() {
        // Phase exactly 1.0 should wrap to index 0.
        assert_eq!(spinner_char_for_phase(1.0, false), SPINNER_CHARS[0]);
        // Phase slightly less than 1.0 should be the last glyph.
        let last_idx = SPINNER_CHARS.len() - 1;
        let last = last_idx as f32 / SPINNER_CHARS.len() as f32;
        assert_eq!(
            spinner_char_for_phase(last + 0.001, false),
            SPINNER_CHARS[last_idx]
        );
    }

    // ---- R2-2 assumption falsification tests ----
    // These tests validate the 5 assumptions listed in BATCH3_IMPLEMENTATION_PLAN.md
    // before any prefix-sum optimization is applied. They pin the current behavior
    // so the optimization can be verified against them.

    fn r22_block(command: &str, output: &str) -> Block {
        Block {
            id: BlockId(1),
            command: command.into(),
            cwd: Some("/tmp/weft".into()),
            output: output.into(),
            styled_output: None,
            exit_code: Some(0),
            started_at: SystemTime::UNIX_EPOCH,
            finished_at: Some(SystemTime::UNIX_EPOCH + Duration::from_millis(100)),
            collapsed: false,
        }
    }

    /// Assumption 1 (CONFIRMED): block height depends on viewport_rows when
    /// the command is `clear` — `clear_block_spacer_rows` returns
    /// `viewport_rows` for clear, 0 otherwise. R2-2 must either include
    /// viewport_rows in the cache key or extract the spacer as a separate
    /// O(1) term.
    #[test]
    fn r22_clear_command_height_depends_on_viewport_rows() {
        let b = r22_block("clear", "");
        let cols = 80;
        let header_rows = 2;

        let h_30 = completed_block_layout_rows(&b, cols, header_rows, 30);
        let h_50 = completed_block_layout_rows(&b, cols, header_rows, 50);

        // clear command produces a viewport-sized spacer → height must differ.
        assert_ne!(
            h_30, h_50,
            "clear command height must depend on viewport_rows"
        );
        // The spacer equals viewport_rows; the rest (header + gap) is fixed.
        assert_eq!(h_50 - h_30, 20, "delta must equal viewport_rows delta");
    }

    /// Assumption 1 (negative case): non-clear commands do NOT depend on
    /// viewport_rows. This confirms the dependency is isolated to clear.
    #[test]
    fn r22_non_clear_command_height_independent_of_viewport_rows() {
        let b = r22_block("ls -la", "file1\nfile2\nfile3\n");
        let cols = 80;
        let header_rows = 2;

        let h_30 = completed_block_layout_rows(&b, cols, header_rows, 30);
        let h_50 = completed_block_layout_rows(&b, cols, header_rows, 50);

        assert_eq!(
            h_30, h_50,
            "non-clear command height must not depend on viewport_rows"
        );
    }

    /// Assumption 3: `block_content_metrics` is the single total source.
    /// This test pins its contract: returns (total, visible) where visible
    /// = grid num_rows. It does NOT test all 6 call sites (see grep in the
    /// batch 3 research), but pins the function signature/return shape.
    #[test]
    fn r22_block_content_metrics_returns_total_and_visible() {
        use weft_core::vt::Terminal;
        let mut terminal = Terminal::new(30, 80);
        // No blocks → total = 0, visible = num_rows.
        let (total, visible) = block_content_metrics(&terminal, 80, 2);
        assert_eq!(total, 0);
        assert_eq!(visible, 30);
        // block_content_metrics reads terminal.grid().num_rows as viewport_rows.
        // Resizing the terminal changes visible (and would change total if any
        // block had a clear command).
        terminal.resize(50, 80);
        let (total2, visible2) = block_content_metrics(&terminal, 80, 2);
        assert_eq!(total2, 0);
        assert_eq!(visible2, 50);
    }

    /// Assumption 5: `CachedBlockLayout.lines` already stores wrapped chunks,
    /// but `completed_block_output_rows` re-wraps via `block_line_chunks`.
    /// This test confirms the re-wrap is redundant (same result), validating
    /// that R2-2 can safely route through the cache.
    #[test]
    fn r22_cached_chunks_match_completed_block_output_rows() {
        use crate::paint::grid_cache::BlockLayoutCache;
        let b = r22_block("echo hi", "short line\na much longer line that surely wraps past eighty columns when rendered at eighty cols\n");
        let cols = 80;

        // Cache path: ensure_cached + sum chunks per line.
        let mut cache = BlockLayoutCache::default();
        cache.ensure_cached(&b, cols);
        let cached = cache.get(b.id.0);
        let cached_rows: usize = cached.lines.iter().map(|l| l.chunks.len()).sum();

        // Direct path: completed_block_output_rows re-wraps.
        let direct_rows = completed_block_output_rows(&b, cols);

        assert_eq!(
            cached_rows, direct_rows,
            "cache chunks must match direct wrap count"
        );
    }

    /// R2-2 regression: collapsed blocks must report `output_rows = 0` in the
    /// cache, matching `completed_block_output_rows`. Without the collapsed
    /// guard in `compute_block_layout`, the cache would store non-zero rows
    /// and the scrollbar thumb would be sized as if the output were visible.
    #[test]
    fn r22_collapsed_block_cache_reports_zero_output_rows() {
        use crate::paint::grid_cache::BlockLayoutCache;
        let mut b = r22_block("echo hi", "line one\nline two\nline three\n");
        let cols = 80;

        // Uncollapsed: cache should have non-zero output_rows.
        let mut cache = BlockLayoutCache::default();
        cache.ensure_cached(&b, cols);
        let uncollapsed_rows = cache.get(b.id.0).output_rows;
        assert_eq!(uncollapsed_rows, 3);

        // Collapse: cache must rebuild with output_rows = 0.
        b.collapsed = true;
        cache.ensure_cached(&b, cols);
        assert_eq!(cache.get(b.id.0).output_rows, 0);

        // The cached path must agree with the fallback path.
        let mut terminal = Terminal::new(24, 80);
        terminal.process(b"\x1b]133;A\x07echo hi\x1b]133;B\x07\x1b]133;C\x07line one\r\nline two\r\nline three\r\n\x1b]133;D;0\x07");
        // No direct API to collapse a finalized block; verify via the function
        // contract: block_content_metrics (None cache) and _with_cache (Some)
        // must agree.
        let (total_none, _) = block_content_metrics(&terminal, 80, 1);
        let cache = terminal.block_tracker(); // borrow to build a cache
        let mut blk_cache = BlockLayoutCache::default();
        for blk in cache.session_blocks() {
            blk_cache.ensure_cached(blk, 80);
        }
        let (total_some, _) = block_content_metrics_with_cache(&terminal, 80, 1, Some(&blk_cache));
        assert_eq!(
            total_none, total_some,
            "cached and uncached totals must match"
        );
    }
}
