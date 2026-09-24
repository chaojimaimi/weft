//! v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL): incremental primary-screen TUI
//! history capture — scroll-out prefix extraction, 1MiB block chunking, and
//! the BlockTracker plumbing that drives them.
//!
//! The screen-owned TUI transcript is composed from three parts in document
//! order (see `compose_screen_history`): the preserved-frame history (Phase
//! 2), the captured scroll-out prefix (this module), and the current viewport
//! snapshot. The prefix is append-only — rows pushed out of the viewport are
//! captured into the in-flight block BEFORE the scrollback ring can evict
//! them, so ring eviction is decoupled from TUI history. Past
//! `DEFAULT_OUTPUT_CAP` the head is settled as a finished block and the tail
//! continues in a new in-flight block (contiguous ids, byte-seamless text);
//! ordinary command-output capture keeps truncating.

use super::style::StyledLine;
use super::{Block, BlockId, BlockTracker, StyledOutput};
use std::sync::Arc;
use std::time::SystemTime;

/// The byte offset of the last line boundary at or before `max` bytes —
/// `max` itself when the first line alone exceeds the budget (the head then
/// cuts mid-line and the continuation resumes at the cut, keeping the text
/// byte-seamless). The boundary INCLUDES the terminating `\n` in the head.
pub(crate) fn line_boundary_at_or_before(text: &str, max: usize) -> usize {
    let max = max.min(text.len());
    match text[..max].rfind('\n') {
        Some(index) => index + 1,
        None => max,
    }
}

/// Filter pushed grid rows by ownership, returning the owned rows' text
/// joined in push order plus their styled lines (line indices local to the
/// text). Unowned and empty rows are skipped, matching the snapshot walk
/// semantics (unowned rows never belong to the TUI document, empty rows are
/// leading/interior blanks the snapshot also omits).
pub(crate) fn extract_owned_pushed_rows(
    rows: &[(String, Option<StyledLine>)],
    owned: &[bool],
) -> (String, Option<StyledOutput>) {
    let mut text = String::new();
    let mut lines: Vec<StyledLine> = Vec::new();
    let mut text_lines = 0u32;
    for (index, (row_text, row_styled)) in rows.iter().enumerate() {
        if !owned.get(index).copied().unwrap_or(false) || row_text.is_empty() {
            continue;
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(row_text);
        if let Some(mut styled) = row_styled.clone() {
            // The styled line index = the row's line index in the extracted
            // text (owned rows in push order, unstyled rows included) — the
            // append path offsets by the text line count.
            styled.line = text_lines;
            lines.push(styled);
        }
        text_lines += 1;
    }
    let styled = if lines.is_empty() {
        None
    } else {
        Some(StyledOutput { lines })
    };
    (text, styled)
}

/// The composed styled lines whose line index is in `[from, from + count)`,
/// re-indexed to start at 0. `None` when the range has no styled lines.
pub(crate) fn styled_lines_in(
    styled: &StyledOutput,
    from: usize,
    count: usize,
) -> Option<StyledOutput> {
    let to = from.saturating_add(count);
    let lines: Vec<StyledLine> = styled
        .lines
        .iter()
        .filter(|line| (line.line as usize) >= from && (line.line as usize) < to)
        .map(|line| StyledLine {
            line: line.line.saturating_sub(from as u32),
            ..line.clone()
        })
        .collect();
    if lines.is_empty() {
        None
    } else {
        Some(StyledOutput { lines })
    }
}

/// The composed styled lines at line index `>= from`, re-indexed to 0.
pub(crate) fn styled_lines_from(styled: &StyledOutput, from: usize) -> Option<StyledOutput> {
    styled_lines_in(styled, from, usize::MAX)
}

impl BlockTracker {
    /// Append owned rows scrolled out of the viewport to the in-flight
    /// block's screen prefix. Screen-owned sessions only (print capture is
    /// off while screen-owned, so no double-capture risk).
    pub(crate) fn append_screen_prefix(&mut self, text: &str, styled: Option<StyledOutput>) {
        if self.screen_document_start.is_none() {
            return;
        }
        self.output.append_screen_prefix(text, styled);
    }

    pub(crate) fn screen_prefix_len(&self) -> usize {
        self.output.screen_prefix_len()
    }

    pub(crate) fn screen_prefix_text(&self) -> &str {
        self.output.screen_prefix_text()
    }

    pub(crate) fn screen_prefix_styled(&self) -> Option<&StyledOutput> {
        self.output.screen_prefix_styled()
    }

    pub(crate) fn screen_prefix_line_count(&self) -> usize {
        self.output.screen_prefix_line_count()
    }

    /// v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL): settle 1MiB head chunks of a
    /// screen-owned TUI session as finished blocks and continue the tail in
    /// the in-flight block. The session keeps its pending command, document
    /// start and cwd — the continuation block gets the next contiguous id at
    /// the real command end. Split blocks record no exit code (the command
    /// is still running).
    pub(crate) fn split_screen_history(
        &mut self,
        heads: Vec<(String, Option<StyledOutput>)>,
        tail: String,
        tail_styled: Option<StyledOutput>,
        consumed_prefix: usize,
    ) {
        let command = self.pending_command.clone().unwrap_or_default();
        let cwd = self.pending_cwd.clone();
        let started_at = self.pending_started.unwrap_or_else(SystemTime::now);
        let now = SystemTime::now();
        for (head, head_styled) in heads {
            let block = Block {
                id: BlockId(self.ids.allocate()),
                command: command.clone(),
                cwd: cwd.clone(),
                output: head.into(),
                styled_output: head_styled.map(Arc::new),
                exit_code: None,
                started_at,
                finished_at: Some(now),
                collapsed: false,
                // v1.10.26 B-1: a settled 1MiB split-head block is a chunk of
                // the TUI frame document — screen-origin, clip-not-wrap.
                screen_origin: self.screen_document_start.is_some(),
            };
            if self.screen_document_start.is_some() {
                self.screen_owned_blocks.insert(block.id.0);
            }
            self.dirty_blocks.insert(block.id.0);
            self.blocks.push(block.clone());
            self.unpersisted.push(block);
        }
        // PLAN_v11217 §3.5: originally classified B ("chunk granularity,
        // not retention" — the 1MiB chunking loop upstream keeps tail ≤ 1MiB,
        // so this bound never binds), but the round-7 upgrade of the
        // freeze.rs fallback (:412) to cap-aware means the tail handed here
        // can be up to the CONFIGURED cap (fallback truncates at the cap).
        // With the old constant this site would then re-truncate that legit
        // tail back to 1 MiB — the same P0 shape as the style.rs replace
        // paths. Reads the configured field per the review's own linkage
        // note ("freeze 若升 A 此处须联动重审").
        self.output.replace(&tail, self.output_cap);
        self.output.drain_screen_prefix(consumed_prefix);
        self.styled_output = tail_styled.map(Arc::new);
        self.live_output_version = self.live_output_version.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::{CapturedStyle, DEFAULT_OUTPUT_CAP};
    use crate::grid::CellColor;

    fn styled_line(line: u32, color: CellColor) -> StyledLine {
        StyledLine {
            line,
            foregrounds: vec![crate::blocks::ForegroundSpan {
                start: 0,
                end: 3,
                color,
            }],
            ..StyledLine::default()
        }
    }

    #[test]
    fn line_boundary_includes_the_terminating_newline() {
        let text = "aa\nbb\ncc";
        assert_eq!(line_boundary_at_or_before(text, 100), 6); // "aa\nbb\n"
        assert_eq!(line_boundary_at_or_before(text, 5), 3); // "aa\n"
        assert_eq!(line_boundary_at_or_before(text, 3), 3);
        assert_eq!(line_boundary_at_or_before(text, 2), 2); // no '\n' → mid-line cut
        assert_eq!(line_boundary_at_or_before(text, 0), 0);
        assert_eq!(line_boundary_at_or_before("", 10), 0);
    }

    #[test]
    fn extracted_rows_keep_push_order_and_skip_unowned() {
        let rows = vec![
            (
                "first".to_string(),
                Some(styled_line(0, CellColor::Palette(1))),
            ),
            ("second".to_string(), None),
            (
                "third".to_string(),
                Some(styled_line(0, CellColor::Palette(3))),
            ),
            ("fourth".to_string(), None),
        ];
        let (text, styled) = extract_owned_pushed_rows(&rows, &[true, false, true, true]);
        assert_eq!(text, "first\nthird\nfourth");
        let styled = styled.expect("styled");
        assert_eq!(styled.lines.len(), 2);
        assert_eq!(styled.lines[0].line, 0);
        assert_eq!(
            styled.lines[0].foreground_at(0),
            Some(CellColor::Palette(1))
        );
        assert_eq!(
            styled.lines[1].line, 1,
            "line index = text-line index among owned rows"
        );
        assert_eq!(
            styled.lines[1].foreground_at(0),
            Some(CellColor::Palette(3))
        );
    }

    #[test]
    fn extracted_rows_skip_empty_rows_like_the_snapshot() {
        let rows = vec![
            ("a".to_string(), None),
            (String::new(), None),
            ("b".to_string(), None),
        ];
        let (text, styled) = extract_owned_pushed_rows(&rows, &[true, true, true]);
        assert_eq!(text, "a\nb");
        assert!(styled.is_none());
    }

    #[test]
    fn styled_line_indices_track_text_lines_across_unstyled_rows() {
        // A styled row after an owned-but-unstyled row must carry the TEXT
        // line index (the append-path offset), not a compacted styled index.
        let rows = vec![
            (
                "first".to_string(),
                Some(styled_line(0, CellColor::Palette(1))),
            ),
            ("plain".to_string(), None),
            (
                "third".to_string(),
                Some(styled_line(0, CellColor::Palette(3))),
            ),
        ];
        let (text, styled) = extract_owned_pushed_rows(&rows, &[true, true, true]);
        assert_eq!(text, "first\nplain\nthird");
        let styled = styled.expect("styled");
        assert_eq!(styled.lines[0].line, 0);
        assert_eq!(
            styled.lines[1].line, 2,
            "skip the unstyled row in the index"
        );
    }

    #[test]
    fn extraction_with_no_owned_rows_is_empty() {
        let rows = vec![("a".to_string(), None), ("b".to_string(), None)];
        let (text, styled) = extract_owned_pushed_rows(&rows, &[false, false]);
        assert!(text.is_empty());
        assert!(styled.is_none());
    }

    #[test]
    fn styled_lines_in_partitions_and_reindexes() {
        let styled = StyledOutput {
            lines: vec![
                styled_line(0, CellColor::Palette(1)),
                styled_line(2, CellColor::Palette(2)),
                styled_line(5, CellColor::Palette(3)),
            ],
        };
        let head = styled_lines_in(&styled, 0, 3).expect("head");
        assert_eq!(head.lines.len(), 2);
        assert_eq!(head.lines[0].line, 0);
        assert_eq!(head.lines[1].line, 2);
        let tail = styled_lines_from(&styled, 3).expect("tail");
        assert_eq!(tail.lines.len(), 1);
        assert_eq!(tail.lines[0].line, 2, "re-indexed by -from");
        assert!(
            styled_lines_in(&styled, 3, 1).is_none(),
            "empty range → None"
        );
    }

    // ── Split plumbing ─────────────────────────────────────────────────

    fn tracker_with_session(command: &str) -> BlockTracker {
        let mut tracker = BlockTracker::new();
        tracker.on_prompt_start();
        tracker.on_command_start(command.to_string());
        tracker.begin_screen_owned_output(0);
        tracker
    }

    #[test]
    fn append_screen_prefix_requires_screen_ownership() {
        let mut tracker = BlockTracker::new();
        tracker.on_prompt_start();
        tracker.on_command_start("echo".to_string());
        tracker.append_screen_prefix("sneaky", None); // not screen-owned → no-op
        assert_eq!(tracker.screen_prefix_len(), 0);
        tracker.begin_screen_owned_output(5);
        tracker.append_screen_prefix("line one", None);
        tracker.append_screen_prefix("line two", None);
        assert_eq!(tracker.screen_prefix_text(), "line one\nline two");
        assert_eq!(tracker.screen_prefix_line_count(), 2);
    }

    #[test]
    fn split_settles_heads_and_continues_the_tail_seamlessly() {
        let mut tracker = tracker_with_session("omp");
        tracker.append_screen_prefix("line one", None);
        tracker.append_screen_prefix("line two", None);
        tracker
            .output
            .replace("viewport", crate::blocks::DEFAULT_OUTPUT_CAP);

        tracker.split_screen_history(
            vec![("line one\n".to_string(), None)],
            "line two\nviewport".to_string(),
            None,
            "line one\n".len(),
        );

        assert_eq!(tracker.blocks().len(), 1);
        let head = &tracker.blocks()[0];
        assert_eq!(head.command, "omp");
        assert_eq!(head.output.as_ref(), "line one\n");
        assert_eq!(head.exit_code, None, "split block is settled mid-command");
        // The tail continues in the in-flight block; the consumed prefix part
        // was drained so the next snapshot does not re-emit it.
        assert_eq!(
            tracker.in_flight().map(|live| live.output.to_string()),
            Some("line two\nviewport".to_string())
        );
        assert_eq!(tracker.screen_prefix_text(), "line two");
        assert_eq!(tracker.screen_prefix_line_count(), 1);
    }

    #[test]
    fn split_keeps_the_session_state_for_the_continuation_id() {
        let mut tracker = tracker_with_session("omp");
        tracker.append_screen_prefix("head", None);
        tracker.split_screen_history(
            vec![("head".to_string(), None)],
            "tail".to_string(),
            None,
            4,
        );
        // The session is still in flight: the real end finalizes the
        // continuation with the NEXT contiguous id.
        assert_eq!(tracker.blocks()[0].id, BlockId(1));
        tracker.on_command_end(0);
        assert_eq!(tracker.blocks().len(), 2);
        assert_eq!(tracker.blocks()[1].id, BlockId(2));
        assert_eq!(tracker.blocks()[1].output.as_ref(), "tail");
        assert_eq!(tracker.blocks()[1].exit_code, Some(0));
    }

    #[test]
    fn split_does_not_affect_ordinary_capture_truncation() {
        // The print-path truncation semantics are untouched: a non-screen
        // capture still truncates at DEFAULT_OUTPUT_CAP with the marker.
        let mut tracker = BlockTracker::new();
        tracker.on_prompt_start();
        tracker.on_command_start("cat huge".to_string());
        for _ in 0..(DEFAULT_OUTPUT_CAP + 1024) {
            tracker.on_print('a', CapturedStyle::default());
        }
        tracker.on_command_end(0);
        let block = tracker.blocks().last().unwrap();
        assert!(block.output.contains("block excerpt truncated at 1 MiB"));
        assert!(
            block.output.contains("full output remains in scrollback"),
            "marker must clarify the data is not lost"
        );
        assert!(block.output.len() <= DEFAULT_OUTPUT_CAP + 96);
    }

    #[test]
    fn split_blocks_are_screen_owned_and_unpersisted() {
        let mut tracker = tracker_with_session("omp");
        tracker.split_screen_history(
            vec![("head".to_string(), None)],
            "tail".to_string(),
            None,
            0,
        );
        assert!(tracker.session_produced_block_ids().contains(&1));
        assert_eq!(tracker.drain_unpersisted().len(), 1);
        assert!(
            tracker.blocks()[0].screen_origin,
            "v1.10.26 B-1: a settled split head block is screen-origin (TUI frame content)"
        );
    }

    /// The full chunking loop (as driven by the Terminal's split path):
    /// composed text past DEFAULT_OUTPUT_CAP peels 1MiB heads at line
    /// boundaries; head + tail concatenates back to the original (seamless).
    #[test]
    fn chunking_is_byte_seamless_and_bounded() {
        let mut composed = String::new();
        for index in 0..20_000 {
            if !composed.is_empty() {
                composed.push('\n');
            }
            composed.push_str(&format!("line {index:05} {}", "x".repeat(180)));
        }
        assert!(composed.len() > DEFAULT_OUTPUT_CAP * 3);

        let mut heads = Vec::new();
        let mut rest = composed.clone();
        while rest.len() > DEFAULT_OUTPUT_CAP {
            let boundary = line_boundary_at_or_before(&rest, DEFAULT_OUTPUT_CAP);
            assert!(boundary > 0, "chunking must always make progress");
            heads.push(rest[..boundary].to_string());
            rest = rest[boundary..].to_string();
        }
        assert!(rest.len() <= DEFAULT_OUTPUT_CAP);
        let reassembled = heads
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("")
            + &rest;
        assert_eq!(reassembled, composed, "chunking must be byte-seamless");
        assert!(heads.iter().all(|head| head.len() <= DEFAULT_OUTPUT_CAP));
    }
}
