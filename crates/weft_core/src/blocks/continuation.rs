use super::{Block, BlockId, BlockTracker, OutputCapture};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::SystemTime;

const LINK_SCAN_LINES: usize = 12;
const MIN_REPLAY_LINES: usize = 6;
const MIN_REPLAY_BYTES: usize = 160;
const MIN_REPLAY_PERCENT: usize = 55;

impl BlockTracker {
    pub(super) fn prepare_screen_continuation(&mut self, command: &str) {
        self.continuation_candidate = self
            .blocks
            .last()
            // v1.7.5: session_start 不再可靠（load_blocks 不再重置它），
            // 改用 screen_owned_blocks 判断是否为本会话产生的 block。
            // 加载的历史 block 不会进入 screen_owned_blocks，所以这里等价。
            .filter(|block| self.screen_owned_blocks.contains(&block.id.0))
            .filter(|block| output_links_resume_command(&block.output, command))
            .map(|block| block.id);
        if let Some(block_id) = self.continuation_candidate {
            tracing::info!(
                block_id = block_id.0,
                "detected primary-screen session continuation"
            );
        }
    }

    pub(super) fn activate_screen_continuation(&mut self) {
        let Some(candidate) = self.continuation_candidate.take() else {
            return;
        };
        if self.blocks.last().map(|block| block.id) != Some(candidate) {
            return;
        }
        self.continuation_base = self.blocks.pop();
        self.dirty_blocks.insert(candidate.0);
        tracing::info!(
            block_id = candidate.0,
            "activated primary-screen continuation"
        );
    }

    /// FIX_ORPHAN_PARSE_ERROR_OUTPUT: synthesize a finished block for a
    /// command whose `133;D` arrived WITHOUT any preceding `133;B` — zsh
    /// reported a parse error before preexec could fire, so the error output
    /// was staged on the `Terminal` (never captured in-flight) and the plain
    /// [`Self::on_command_end`] path would have been a noop.
    ///
    /// Replays the missing `on_command_start` bookkeeping (pending metadata +
    /// swapping the staged capture in as the block output), then delegates to
    /// the SAME [`Self::finalize`] path as a normal command: identical Block
    /// construction, dirty marking, and unpersisted queue, so persistence /
    /// history stay uniform. `phase` is never touched — the caller arrives at
    /// AtPrompt (B never flipped it) and stays there, matching
    /// `on_command_end`'s end state.
    ///
    /// `pub(crate)` (not `pub`) because the signature carries the
    /// crate-private [`OutputCapture`]; the only caller lives in
    /// `crate::vt::perform`.
    pub(crate) fn on_orphan_command_end(
        &mut self,
        exit_code: i32,
        command: String,
        mut staged: OutputCapture,
    ) {
        self.pending_command = Some(command);
        self.pending_started = Some(SystemTime::now());
        self.pending_cwd = self.current_cwd.clone();
        self.styled_output = None;
        self.screen_document_start = None;
        // After the swap `staged` only holds the previous in-flight capture
        // (empty on every real path — finalize always drained it); the local
        // drops at function end, so no explicit clear is needed.
        std::mem::swap(&mut self.output, &mut staged);
        self.finalize(Some(exit_code));
    }

    /// Finalize the in-flight command. Screen sessions that explicitly link
    /// to the next command reuse their existing block only after the new TUI
    /// proves it replayed a substantial portion of that block. A failed or
    /// unrelated launch restores the old block and records a separate command.
    pub(super) fn finalize(&mut self, exit_code: Option<i32>) {
        let Some(command) = self.pending_command.take() else {
            self.clear_pending_capture();
            return;
        };
        let started_at = self.pending_started.take().unwrap_or_else(SystemTime::now);
        let cwd = self.pending_cwd.take();
        let (raw_output, captured_styled) = self.output.take_styled();
        let screen_owned = self.screen_document_start.take().is_some();
        self.continuation_candidate = None;

        let output = crate::secrets::mask(&raw_output);
        // v1.7.0-A: prefer the freshly-captured styled output from the RLE.
        // Fall back to the screen-snapshot styled_output (primary-screen TUI
        // blocks) only when the capture path didn't produce one.
        let styled_output = if output == raw_output {
            captured_styled
                .map(Arc::new)
                .or_else(|| self.styled_output.take())
        } else {
            self.styled_output = None;
            None
        };

        let continuation = self.continuation_base.take();
        let replayed = continuation
            .as_ref()
            .is_some_and(|base| replayed_screen_document(&base.output, &output));
        let block = if replayed {
            let base = continuation.expect("checked continuation");
            tracing::info!(
                block_id = base.id.0,
                "merged replayed primary-screen continuation"
            );
            Block {
                id: base.id,
                command: base.command,
                cwd: base.cwd.or(cwd),
                output: output.into(),
                styled_output,
                exit_code,
                started_at: base.started_at,
                finished_at: Some(SystemTime::now()),
                collapsed: base.collapsed,
                // v1.10.26 B-1: a merged continuation is the same TUI document,
                // so it keeps the screen-origin clip-not-wrap semantics.
                screen_origin: screen_owned,
            }
        } else {
            if let Some(base) = continuation {
                tracing::info!(
                    block_id = base.id.0,
                    "restored unproven screen continuation"
                );
                self.blocks.push(base);
            }
            Block {
                id: BlockId(self.ids.allocate()),
                command,
                cwd,
                output: output.into(),
                styled_output,
                exit_code,
                started_at,
                finished_at: Some(SystemTime::now()),
                collapsed: false,
                screen_origin: screen_owned,
            }
        };

        if screen_owned {
            self.screen_owned_blocks.insert(block.id.0);
        }
        self.dirty_blocks.insert(block.id.0);
        self.blocks.push(block.clone());
        self.unpersisted.push(block);
    }

    fn clear_pending_capture(&mut self) {
        self.pending_started = None;
        self.pending_cwd = None;
        self.output.clear();
        self.styled_output = None;
        self.screen_document_start = None;
        self.continuation_candidate = None;
        if let Some(base) = self.continuation_base.take() {
            self.blocks.push(base);
        }
    }
}

fn output_links_resume_command(output: &str, command: &str) -> bool {
    let command = command.trim();
    if command.is_empty() {
        return false;
    }
    let lines = output.lines().collect::<Vec<_>>();
    let start = lines.len().saturating_sub(LINK_SCAN_LINES);
    (start..lines.len()).any(|index| {
        let line = lines[index].trim();
        let follows_resume_label = index > 0
            && lines[index - 1]
                .trim()
                .to_ascii_lowercase()
                .contains("resume");
        let continue_card = line
            .strip_prefix("Continue")
            .is_some_and(|rest| rest.trim() == command);
        (line == command && follows_resume_label) || continue_card
    })
}

fn replayed_screen_document(previous: &str, current: &str) -> bool {
    let previous_lines = meaningful_lines(previous);
    if previous_lines.len() < MIN_REPLAY_LINES {
        return false;
    }
    let previous_bytes = previous_lines.iter().map(|line| line.len()).sum::<usize>();
    if previous_bytes < MIN_REPLAY_BYTES {
        return false;
    }
    let current_lines = meaningful_lines(current)
        .into_iter()
        .collect::<HashSet<_>>();
    let matched = previous_lines
        .iter()
        .filter(|line| current_lines.contains(**line))
        .collect::<HashSet<_>>();
    let matched_bytes = matched.iter().map(|line| line.len()).sum::<usize>();
    matched.len() >= MIN_REPLAY_LINES
        && matched_bytes >= MIN_REPLAY_BYTES
        && matched_bytes.saturating_mul(100) >= previous_bytes.saturating_mul(MIN_REPLAY_PERCENT)
}

fn meaningful_lines(text: &str) -> Vec<&str> {
    text.lines()
        .map(str::trim)
        .filter(|line| line.len() >= 4)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::{CapturedStyle, ForegroundSpan, StyledLine, StyledOutput};
    use crate::grid::CellColor;

    const REPLAY: &str = "banner line with enough content\nmodel and account information\nworking directory /tmp/project\nprompt asking for a performance report\nanswer line one with useful detail\nanswer line two with useful detail";

    fn finish_screen(tracker: &mut BlockTracker, command: &str, output: &str) {
        tracker.on_prompt_start();
        tracker.on_command_start(command.to_string());
        tracker.begin_screen_owned_output(0);
        tracker.replace_screen_output(output);
        tracker.on_command_end(0);
    }

    #[test]
    fn explicit_resume_reuses_the_previous_screen_block() {
        let mut tracker = BlockTracker::new();
        let original = format!("{REPLAY}\nResume this session with:\ntool --resume session-id");
        finish_screen(&mut tracker, "tool", &original);
        let original_id = tracker.blocks()[0].id;
        assert!(
            tracker.blocks()[0].screen_origin,
            "v1.10.26 B-1: a screen-owned block is marked screen-origin"
        );
        tracker.drain_unpersisted();

        tracker.on_command_start("tool --resume session-id".to_string());
        tracker.begin_screen_owned_output(10);
        assert!(tracker.blocks().is_empty());
        assert_eq!(tracker.in_flight().map(|live| live.command), Some("tool"));
        tracker.replace_screen_output(&format!("{original}\nnew answer after resuming"));
        tracker.on_command_end(0);

        assert_eq!(tracker.blocks().len(), 1);
        assert_eq!(tracker.blocks()[0].id, original_id);
        assert_eq!(tracker.blocks()[0].command, "tool");
        assert!(
            tracker.blocks()[0].screen_origin,
            "v1.10.26 B-1: the merged continuation keeps the screen-origin marker (same TUI document)"
        );
        assert!(tracker.blocks()[0]
            .output
            .ends_with("new answer after resuming"));
        assert_eq!(tracker.drain_unpersisted()[0].id, original_id);
    }

    #[test]
    fn continue_session_card_uses_the_same_generic_path() {
        let mut tracker = BlockTracker::new();
        let original = format!("{REPLAY}\nSession project\nContinue  agent -s session-id");
        finish_screen(&mut tracker, "agent", &original);
        tracker.drain_unpersisted();

        tracker.on_command_start("agent -s session-id".to_string());
        tracker.begin_screen_owned_output(10);
        tracker.replace_screen_output(&original);
        tracker.on_command_end(130);

        assert_eq!(tracker.blocks().len(), 1);
        assert_eq!(tracker.blocks()[0].command, "agent");
    }

    #[test]
    fn failed_resume_restores_history_and_keeps_its_own_block() {
        let mut tracker = BlockTracker::new();
        let original = format!("{REPLAY}\nResume this session with:\ntool --resume missing");
        finish_screen(&mut tracker, "tool", &original);
        tracker.drain_unpersisted();

        tracker.on_command_start("tool --resume missing".to_string());
        tracker.begin_screen_owned_output(10);
        tracker.replace_screen_output("banner only\nresume failed: session not found");
        tracker.on_command_end(1);

        assert_eq!(tracker.blocks().len(), 2);
        assert_eq!(tracker.blocks()[0].command, "tool");
        assert_eq!(tracker.blocks()[1].command, "tool --resume missing");
    }

    #[test]
    fn resume_that_never_takes_the_screen_cannot_remove_history() {
        let mut tracker = BlockTracker::new();
        let original = format!("{REPLAY}\nResume this session with:\ntool --resume unavailable");
        finish_screen(&mut tracker, "tool", &original);
        let original_id = tracker.blocks()[0].id;
        tracker.drain_unpersisted();

        tracker.on_command_start("tool --resume unavailable".to_string());
        tracker.on_print_ascii_run(b"session service is unavailable", CapturedStyle::default());
        tracker.on_command_end(1);

        assert_eq!(tracker.blocks().len(), 2);
        assert_eq!(tracker.blocks()[0].id, original_id);
        assert_eq!(tracker.blocks()[1].command, "tool --resume unavailable");
        assert_eq!(
            tracker.blocks()[1].output.as_ref(),
            "session service is unavailable"
        );
    }

    #[test]
    fn ordinary_output_that_mentions_a_command_never_merges() {
        let mut tracker = BlockTracker::new();
        tracker.on_prompt_start();
        tracker.on_command_start("printf help".to_string());
        tracker.on_print_ascii_run(
            b"Resume this session with:\ntool --resume id",
            CapturedStyle::default(),
        );
        tracker.on_command_end(0);

        tracker.on_command_start("tool --resume id".to_string());
        tracker.begin_screen_owned_output(10);
        tracker.replace_screen_output(REPLAY);
        tracker.on_command_end(0);
        assert_eq!(tracker.blocks().len(), 2);
    }

    #[test]
    fn merged_continuation_keeps_current_snapshot_styles() {
        let mut tracker = BlockTracker::new();
        let original = format!("{REPLAY}\nResume this session with:\ntool --resume styled");
        finish_screen(&mut tracker, "tool", &original);
        tracker.drain_unpersisted();
        tracker.on_command_start("tool --resume styled".to_string());
        tracker.begin_screen_owned_output(10);
        tracker.replace_screen_snapshot(
            &original,
            StyledOutput {
                lines: vec![StyledLine {
                    line: 0,
                    foregrounds: vec![ForegroundSpan {
                        start: 0,
                        end: 6,
                        color: CellColor::Palette(2),
                    }],
                    backgrounds: Vec::new(),
                    links: Vec::new(),
                    attributes: Vec::new(),
                }],
            },
        );
        tracker.on_command_end(0);

        assert_eq!(
            tracker.blocks()[0]
                .styled_output
                .as_deref()
                .and_then(|styled| styled.line(0))
                .and_then(|line| line.foreground_at(1)),
            Some(CellColor::Palette(2))
        );
    }

    /// v1.7.5 regression: a block loaded from SQLite on startup/Restore must
    /// NOT be picked as a continuation candidate, even when its output
    /// contains a "Resume this session with: tool --resume id" link that
    /// matches the command the user runs this session. Only screen-owned
    /// blocks (produced THIS session) are eligible. Otherwise the loaded
    /// history would be popped/merged and the restored history would silently
    /// disappear.
    #[test]
    fn loaded_history_block_is_not_picked_as_continuation() {
        let mut tracker = BlockTracker::new();
        let original = format!("{REPLAY}\nResume this session with:\ntool --resume id");
        let loaded = Block {
            id: BlockId(1),
            command: "tool".to_string(),
            cwd: None,
            output: std::sync::Arc::from(original.as_str()),
            styled_output: None,
            exit_code: Some(0),
            started_at: std::time::SystemTime::now(),
            finished_at: Some(std::time::SystemTime::now()),
            collapsed: false,
            screen_origin: false,
        };
        tracker.load_blocks(vec![loaded]);
        let loaded_id = tracker.blocks()[0].id;
        assert_eq!(tracker.blocks().len(), 1);

        // Running the resume command this session must not consume the loaded
        // block: continuation_candidate stays None.
        tracker.on_command_start("tool --resume id".to_string());
        assert_eq!(tracker.continuation_candidate, None);
        tracker.begin_screen_owned_output(10);
        tracker.replace_screen_output(&original);
        tracker.on_command_end(0);

        // Loaded history is preserved; a separate new block was appended.
        assert_eq!(tracker.blocks().len(), 2);
        assert_eq!(tracker.blocks()[0].id, loaded_id);
        assert_eq!(tracker.blocks()[1].command, "tool --resume id");
    }
}
