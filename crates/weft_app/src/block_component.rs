//! Pure presentation model for BlockView metadata.

use crate::paint::grid_cache::block_line_chunks;
use crate::paint::ui_helpers::{abbreviate_path, block_duration_str};
use weft_core::blocks::{Block, BlockId};
use weft_core::selection::BlockViewRowKind;
use weft_core::vt::Terminal;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BlockHeaderAction {
    Copy(BlockId),
    ToggleFold(BlockId),
}

/// Resolve the finalized block that owns a rendered row for hover purposes.
/// Header rows must retain hover because that is where inline actions are
/// painted; otherwise moving from the command text onto an action makes the
/// action disappear before it can be clicked.
pub(crate) fn hovered_block_for_row(
    kind: &BlockViewRowKind,
    block_id: Option<BlockId>,
) -> Option<BlockId> {
    match kind {
        BlockViewRowKind::Header | BlockViewRowKind::Command | BlockViewRowKind::Output => block_id,
        BlockViewRowKind::Separator | BlockViewRowKind::LiveCommand => None,
    }
}

/// Resolve only the inline header actions. Half-open bounds ensure a point on
/// an adjacent row belongs to exactly one block and cannot fall through to
/// terminal text selection.
pub(crate) fn block_header_action_at(
    regions: &[crate::overlay::HitRegion],
    x: f32,
    y: f32,
) -> Option<BlockHeaderAction> {
    regions.iter().find_map(|region| {
        if !region.contains_half_open(x, y) {
            return None;
        }
        match region.target {
            crate::overlay::HitTarget::BlockActionCopy(id) => Some(BlockHeaderAction::Copy(id)),
            crate::overlay::HitTarget::BlockActionFold(id) => {
                Some(BlockHeaderAction::ToggleFold(id))
            }
            _ => None,
        }
    })
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
    use weft_core::blocks::ShellPhase;

    let mut total = 0;
    let viewport_rows = terminal.grid().num_rows.max(1);
    for block in terminal.block_tracker().session_blocks() {
        total += completed_block_layout_rows(block, cols, header_rows, viewport_rows);
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

    #[test]
    fn block_header_actions_use_half_open_row_boundaries() {
        use crate::overlay::{HitRegion, HitTarget};

        let regions = vec![
            HitRegion {
                x0: 80.0,
                y0: 0.0,
                x1: 100.0,
                y1: 10.0,
                target: HitTarget::BlockActionCopy(BlockId(1)),
            },
            HitRegion {
                x0: 80.0,
                y0: 10.0,
                x1: 100.0,
                y1: 20.0,
                target: HitTarget::BlockActionCopy(BlockId(2)),
            },
        ];

        assert_eq!(
            block_header_action_at(&regions, 90.0, 10.0),
            Some(BlockHeaderAction::Copy(BlockId(2)))
        );
        assert_eq!(block_header_action_at(&regions, 100.0, 10.0), None);
    }

    #[test]
    fn block_header_row_retains_hover_for_inline_actions() {
        let id = BlockId(7);
        assert_eq!(
            hovered_block_for_row(&BlockViewRowKind::Header, Some(id)),
            Some(id)
        );
        assert_eq!(
            hovered_block_for_row(&BlockViewRowKind::Command, Some(id)),
            Some(id)
        );
        assert_eq!(
            hovered_block_for_row(&BlockViewRowKind::Output, Some(id)),
            Some(id)
        );
        assert_eq!(
            hovered_block_for_row(&BlockViewRowKind::Separator, Some(id)),
            None
        );
        assert_eq!(
            hovered_block_for_row(&BlockViewRowKind::LiveCommand, Some(id)),
            None
        );
    }

    #[test]
    fn block_header_action_resolver_ignores_non_actions() {
        use crate::overlay::{HitRegion, HitTarget};

        let regions = vec![HitRegion {
            x0: 0.0,
            y0: 0.0,
            x1: 20.0,
            y1: 20.0,
            target: HitTarget::BlockFold(BlockId(1)),
        }];
        assert_eq!(block_header_action_at(&regions, 10.0, 10.0), None);
    }
}
