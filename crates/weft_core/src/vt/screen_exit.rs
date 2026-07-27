//! Primary-screen exit handling and command document lifecycle.
//!
//! # R1-3: Command Document Lifecycle Phases
//!
//! The WARP optimization plan defines three document lifecycle phases for
//! primary-screen TUI sessions (Claude Code, OpenCode, etc.). They map to
//! existing code entities as follows:
//!
//! | Plan term            | Code entity |
//! |----------------------|-------------|
//! | live grid snapshot   | [`Terminal::primary_screen_app_active()`] + viewport
//! |                      | ownership mask (`primary_screen_viewport_ownership`)
//! | settling tail        | [`SettleState::PendingDeferred`] / [`SettleState::Settling`]
//! |                      | + [`PRIMARY_SCREEN_EXIT_SETTLE_DELAY`] (200ms window)
//! | frozen block         | Terminal `Block` (finalized by
//! |                      | `finish_deferred_screen_command` after settle)
//!
//! OSC 133 (A/B/C/D) drives `ShellPhase` in `BlockTracker`; the 200ms settle
//! window drives `SettleState` in `CapabilityFlags`. The two state machines
//! are orthogonal and bridged by `settle_primary_screen_exit()`, which calls
//! `finish_deferred_screen_command()` when the settle window elapses.
//!
//! Naming note: "frozen" appears in three contexts — `freeze_primary_screen_`
//! `document_candidate` (freezes the document *boundary*), `PrimaryScreen`
//! `InterruptCapture.frozen_text` (freezes an interrupt-instant snapshot),
//! and the terminal `Block` (the final "frozen block" state). All three are
//! intentionally named for their distinct roles; this module comment exists
//! to prevent confusion when mapping plan terminology to code.

use super::Terminal;
use crate::blocks::{OutputCapture, ShellPhase, StyledOutput, MAX_OUTPUT_BYTES};
use std::time::{Duration, Instant};

mod freeze;
mod ownership;

pub(in crate::vt) use ownership::PrimaryScreenOwnership;

pub const PRIMARY_SCREEN_EXIT_SETTLE_DELAY: Duration = Duration::from_millis(200);
pub const PRIMARY_HISTORY_SNAPSHOT_INTERVAL: Duration = Duration::from_millis(50);

pub(in crate::vt) struct PendingPrimaryScreenExit {
    pub(in crate::vt) exit_code: Option<i32>,
    pub(in crate::vt) last_activity: Instant,
}

pub(in crate::vt) struct PrimaryScreenInterruptCapture {
    pub(in crate::vt) frozen_text: String,
    pub(in crate::vt) frozen_styled: StyledOutput,
    pub(in crate::vt) tail: OutputCapture,
    pub(in crate::vt) origin_row: Option<usize>,
}

impl Terminal {
    /// Primary-screen TUIs such as Claude Code do not enter DEC 1049, but
    /// repeatedly use absolute cursor addressing to own the whole viewport.
    pub fn primary_screen_app_active(&self) -> bool {
        !self.capabilities.alt_active
            && self.block_tracker.phase() == ShellPhase::CommandExecuting
            && self.capabilities.primary_screen_cursor_ops >= 2
    }

    /// Whether this primary-screen owner has demonstrated atomic full-frame repainting.
    pub fn primary_screen_repaint_capable(&self) -> bool {
        self.primary_screen_app_active() && self.capabilities.primary_screen_synchronized_frame_seen
    }

    pub(super) fn begin_primary_screen_synchronized_frame(&mut self) {
        self.synchronized_frame_cleared_rows = 0;
    }

    pub(super) fn finish_primary_screen_synchronized_frame(&mut self) {
        if self.synchronized_output_started.is_some() {
            let complete_primary_frame = self.primary_screen_app_active()
                && self.synchronized_frame_cleared_rows >= self.grid.num_rows;
            if complete_primary_frame {
                self.discard_superseded_primary_screen_frame();
            }
            self.capabilities.primary_screen_synchronized_frame_seen |= complete_primary_frame;
        }
    }

    pub(super) fn reset_primary_screen_synchronized_frame(&mut self) {
        self.synchronized_output_started = None;
        self.synchronized_frame_cleared_rows = 0;
        self.capabilities.primary_screen_synchronized_frame_seen = false;
    }

    pub(super) fn note_primary_screen_full_erase(&mut self) {
        if !self.capabilities.alt_active {
            self.include_primary_screen_viewport_row(0);
        }
        if self.synchronized_output_started.is_some() && !self.capabilities.alt_active {
            self.synchronized_frame_cleared_rows = self.grid.num_rows;
            if self.primary_screen_app_active() {
                self.discard_superseded_primary_screen_frame();
            }
        }
    }

    pub(super) fn note_primary_screen_line_erase(&mut self) {
        if !self.capabilities.alt_active {
            self.include_primary_screen_viewport_row(self.grid.cursor.row);
        }
        if self.synchronized_output_started.is_some()
            && !self.capabilities.alt_active
            && self.grid.cursor.row == self.synchronized_frame_cleared_rows
        {
            self.synchronized_frame_cleared_rows += 1;
        }
    }

    pub(super) fn note_primary_screen_cursor_addressing(&mut self) {
        if !self.capabilities.alt_active
            && self.block_tracker.phase() == ShellPhase::CommandExecuting
        {
            self.capabilities.primary_screen_cursor_ops = self
                .capabilities
                .primary_screen_cursor_ops
                .saturating_add(1);
            if self.primary_screen_app_active() {
                self.begin_primary_screen_output_capture();
            }
        }
    }

    pub fn begin_primary_screen_interrupt_capture(&mut self) {
        if !self.primary_screen_app_active()
            || self.capabilities.primary_screen_interrupt_capture.is_some()
        {
            return;
        }
        let Some(document_start) = self.block_tracker.screen_document_start() else {
            return;
        };
        let (frozen_text, frozen_styled) = self.primary_screen_document_snapshot(document_start);
        self.capabilities.primary_screen_interrupt_capture = Some(PrimaryScreenInterruptCapture {
            frozen_text,
            frozen_styled,
            tail: OutputCapture::default(),
            origin_row: None,
        });
        tracing::info!("froze primary-screen transcript before interrupt");
    }

    pub fn cancel_primary_screen_interrupt_capture(&mut self) {
        self.capabilities.primary_screen_interrupt_capture = None;
    }

    /// Mouse protocol bytes are meaningful only while the TUI still owns the
    /// PTY. During Ctrl-C settlement they can race behind the exit marker and
    /// become literal `48;x;yM` shell input, so suspend reporting until the
    /// application either continues or the command is finalized.
    pub fn accepts_mouse_reporting_input(&self) -> bool {
        self.capabilities.mouse_protocol != crate::input::MouseProtocol::Off
            && (self.capabilities.alt_active
                || self.block_tracker.phase() == ShellPhase::CommandExecuting)
            && self.capabilities.primary_screen_interrupt_capture.is_none()
            && self.capabilities.primary_screen_exit.is_none()
    }

    pub(super) fn capture_primary_screen_interrupt_print(&mut self, c: char) {
        if let Some(capture) = &mut self.capabilities.primary_screen_interrupt_capture {
            capture.tail.print(c, MAX_OUTPUT_BYTES);
        }
    }

    pub(super) fn capture_primary_screen_interrupt_ascii(&mut self, bytes: &[u8]) {
        if let Some(capture) = &mut self.capabilities.primary_screen_interrupt_capture {
            capture.tail.print_ascii(bytes, MAX_OUTPUT_BYTES);
        }
    }

    pub(super) fn capture_primary_screen_interrupt_newline(&mut self) {
        if let Some(capture) = &mut self.capabilities.primary_screen_interrupt_capture {
            capture.tail.newline(MAX_OUTPUT_BYTES);
        }
    }

    pub(super) fn capture_primary_screen_interrupt_carriage_return(&mut self) {
        if let Some(capture) = &mut self.capabilities.primary_screen_interrupt_capture {
            capture.tail.carriage_return();
        }
    }

    pub(super) fn capture_primary_screen_interrupt_backspace(&mut self) {
        if let Some(capture) = &mut self.capabilities.primary_screen_interrupt_capture {
            capture.tail.backspace();
        }
    }

    pub(super) fn capture_primary_screen_interrupt_erase_line(&mut self, mode: u16) {
        if let Some(capture) = &mut self.capabilities.primary_screen_interrupt_capture {
            capture.tail.erase_line(mode);
        }
    }

    pub(super) fn capture_primary_screen_interrupt_cursor_position(&mut self, clear_line: bool) {
        if let Some(capture) = &mut self.capabilities.primary_screen_interrupt_capture {
            let origin_row = *capture.origin_row.get_or_insert(self.grid.cursor.row);
            let row = self.grid.cursor.row.saturating_sub(origin_row);
            let col = self.grid.cursor.col;
            capture.tail.goto(row, col, MAX_OUTPUT_BYTES);
            if clear_line && col == 0 {
                capture.tail.erase_line(2);
                capture.tail.goto(row, col, MAX_OUTPUT_BYTES);
            }
        }
    }

    /// Scroll the grid and keep screen-document and viewport-relative side
    /// state synchronized with the same row rotation.
    pub(super) fn scroll_grid_up(&mut self, count: usize) {
        self.scroll_grid_rows(count, false);
    }

    pub(super) fn scroll_grid_down(&mut self, count: usize) {
        self.scroll_grid_rows(count, true);
    }

    fn scroll_grid_rows(&mut self, count: usize, down: bool) {
        let origin_before = self.grid.scrollback.position();
        let (top, bottom) = self.grid.scroll_region();
        if down {
            self.grid.scroll_down(count);
        } else {
            self.grid.scroll_up(count);
        }
        self.transform_primary_screen_rows(
            origin_before,
            self.grid.scrollback.position(),
            top,
            bottom,
            count,
            down,
        );
        if self.capabilities.alt_active {
            self.grid.discard_scroll_and_dirty_all();
        }
        if !self.hyperlinks.cell_map_is_empty() {
            self.hyperlinks.clear_cell_map();
        }
    }

    pub(super) fn begin_primary_screen_output_capture(&mut self) {
        self.block_tracker
            .begin_screen_owned_output(self.capabilities.primary_screen_document_candidate);
    }

    fn primary_screen_document_snapshot(&self, document_start: u64) -> (String, StyledOutput) {
        // v1.6.1: resolve hyperlink ids to URLs via the Terminal's registry
        // so captured Block output preserves OSC 8 links. The closure borrows
        // `&self.hyperlinks` immutably, which coexists with `&self.grid`.
        let url_resolver = |id: u32| -> Option<std::sync::Arc<str>> {
            self.hyperlinks.url(id).map(std::sync::Arc::<str>::from)
        };
        self.capabilities
            .primary_screen_ownership
            .viewport
            .as_ref()
            .map_or_else(
                || {
                    self.grid
                        .document_snapshot_from_position_with_resolver(document_start, url_resolver)
                },
                |owned| {
                    self.grid
                        .document_snapshot_from_position_with_ownership_masks_and_resolver(
                            document_start,
                            &self.capabilities.primary_screen_ownership.scrollback,
                            owned,
                            url_resolver,
                        )
                },
            )
    }

    /// Apply a runtime scrollback limit to the primary grid and its ownership
    /// mask as one transaction. The primary grid is hidden in `alt_grid`
    /// while an alternate-screen application is active.
    pub fn set_scrollback_max_lines(&mut self, max_lines: usize) {
        let primary = if self.capabilities.alt_active {
            &mut self.alt_grid
        } else {
            &mut self.grid
        };
        let cols = primary.num_cols;
        primary.scrollback.set_max_lines(max_lines, cols);
        self.capabilities
            .primary_screen_ownership
            .retain_scrollback_suffix(primary.scrollback.len());
    }

    pub(super) fn index_primary_screen(&mut self) -> bool {
        let origin = self.grid.scrollback.position();
        let (top, bottom) = self.grid.scroll_region();
        let scrolled = self.grid.index();
        if scrolled {
            self.transform_primary_screen_rows(
                origin,
                self.grid.scrollback.position(),
                top,
                bottom,
                1,
                false,
            );
        }
        scrolled
    }

    pub(super) fn reverse_index_primary_screen(&mut self) -> bool {
        let origin = self.grid.scrollback.position();
        let (top, bottom) = self.grid.scroll_region();
        let scrolled = self.grid.reverse_index();
        if scrolled {
            self.transform_primary_screen_rows(
                origin,
                self.grid.scrollback.position(),
                top,
                bottom,
                1,
                true,
            );
        }
        scrolled
    }

    pub(super) fn insert_primary_screen_lines(&mut self, count: usize) {
        let origin = self.grid.scrollback.position();
        let row = self.grid.cursor.row;
        let (top, bottom) = self.grid.scroll_region();
        self.grid.insert_blank_lines(count);
        if (top..=bottom).contains(&row) {
            self.transform_primary_screen_rows(origin, origin, row, bottom, count, true);
        }
    }

    pub(super) fn delete_primary_screen_lines(&mut self, count: usize) {
        let origin = self.grid.scrollback.position();
        let row = self.grid.cursor.row;
        let (top, bottom) = self.grid.scroll_region();
        self.grid.delete_lines(count);
        if (top..=bottom).contains(&row) {
            self.transform_primary_screen_rows(origin, origin, row, bottom, count, false);
        }
    }

    pub fn show_block_view(&self) -> bool {
        self.block_tracker.bootstrap_ready()
            && !self.capabilities.alt_active
            && !self.primary_screen_exit_pending()
            && (!self.primary_screen_app_active() || self.capabilities.primary_history_view)
    }

    pub fn primary_screen_exit_pending(&self) -> bool {
        self.capabilities.primary_screen_exit.is_some()
    }

    /// First viewport row owned by the active primary-screen application.
    ///
    /// Shell rows can remain physically present above a TUI that paints below
    /// the current cursor. They stay in the Grid for terminal correctness and
    /// detached history, but the live renderer must not expose them as part of
    /// the application's frame.
    pub fn primary_screen_visible_row_start(&self) -> Option<usize> {
        let owns_live_view = self.primary_screen_app_active() || self.primary_screen_exit_pending();
        if self.capabilities.alt_active || !owns_live_view || self.grid.scroll_offset > 0 {
            return None;
        }
        self.block_tracker.screen_document_start().map(|start| {
            viewport_row_for_document_start(
                start,
                self.grid.scrollback.position(),
                self.grid.num_rows,
            )
        })
    }

    /// Rows currently owned by a primary-screen application for live paint.
    ///
    /// This is a rendering policy only: unowned shell rows remain in the Grid
    /// so a sparse, multi-stage TUI repaint cannot destroy data needed by a
    /// later stage or by detached history capture.
    pub fn primary_screen_viewport_ownership(&self) -> Option<&[bool]> {
        let owns_live_view = self.primary_screen_app_active() || self.primary_screen_exit_pending();
        (!self.capabilities.alt_active && owns_live_view && self.grid.scroll_offset == 0)
            .then_some(
                self.capabilities
                    .primary_screen_ownership
                    .viewport
                    .as_deref(),
            )
            .flatten()
    }

    pub fn primary_history_view(&self) -> bool {
        self.capabilities.primary_history_view
    }

    pub fn set_primary_history_view(&mut self, active: bool) {
        let entering = active && !self.capabilities.primary_history_view;
        self.capabilities.primary_history_view = active;
        if active {
            self.grid.scroll_offset = 0;
        } else {
            self.capabilities.primary_history_snapshot_at = None;
        }
        if entering && self.primary_screen_app_active() {
            self.snapshot_primary_screen_output();
            self.capabilities.primary_history_snapshot_at = Some(Instant::now());
            tracing::debug!(
                bytes = self
                    .block_tracker
                    .in_flight()
                    .map_or(0, |live| live.output.len()),
                "snapshotted primary-screen TUI for history browsing"
            );
        }
    }

    /// Coalesced by the app after it drains the current frame's PTY batches,
    /// then rate-limited here so a high-frequency TUI cannot rescan the capped
    /// document on every display frame.
    pub fn refresh_primary_history_snapshot(&mut self) -> bool {
        self.refresh_primary_history_snapshot_at(Instant::now())
    }

    pub(super) fn refresh_primary_history_snapshot_at(&mut self, now: Instant) -> bool {
        if !self.capabilities.primary_history_view || !self.primary_screen_app_active() {
            return false;
        }
        if self
            .capabilities
            .primary_history_snapshot_at
            .is_some_and(|previous| {
                now.saturating_duration_since(previous) < PRIMARY_HISTORY_SNAPSHOT_INTERVAL
            })
        {
            return false;
        }
        self.snapshot_primary_screen_output();
        self.capabilities.primary_history_snapshot_at = Some(now);
        true
    }

    pub(super) fn snapshot_primary_screen_output(&mut self) {
        if let Some(capture) = &self.capabilities.primary_screen_interrupt_capture {
            let (text, styled) = merge_primary_screen_interrupt_tail(
                capture.frozen_text.clone(),
                capture.frozen_styled.clone(),
                capture.tail.as_str(),
            );
            self.block_tracker.replace_screen_snapshot(&text, styled);
            return;
        }
        let Some(document_start) = self.block_tracker.screen_document_start() else {
            return;
        };
        let (text, styled) = self.primary_screen_document_snapshot(document_start);
        let (text, styled) = space_primary_screen_exit_tail(text, styled);
        self.block_tracker.replace_screen_snapshot(&text, styled);
    }

    pub(super) fn defer_primary_screen_exit(&mut self, exit_code: Option<i32>) {
        self.block_tracker.defer_screen_command_end();
        self.capabilities.primary_screen_exit = Some(PendingPrimaryScreenExit {
            exit_code,
            last_activity: Instant::now(),
        });
        // No block_id here: the deferred command's BlockId is not allocated
        // until `settle_primary_screen_exit` → `finish_deferred_screen_command`
        // runs. Logging the previous block's id would mislead log analysis.
        tracing::info!(
            ?exit_code,
            settle_delay_ms = PRIMARY_SCREEN_EXIT_SETTLE_DELAY.as_millis(),
            "deferred primary-screen command finalization"
        );
    }

    pub(super) fn note_primary_screen_exit_activity(&mut self) {
        if let Some(pending) = &mut self.capabilities.primary_screen_exit {
            pending.last_activity = Instant::now();
        }
    }

    /// A late primary-screen exit tail commonly rewrites rows from column 0
    /// without first issuing EL. Clear the old row before that first scalar so
    /// shorter status/resume lines cannot retain stale suffix cells.
    pub(super) fn prepare_primary_screen_exit_row_overwrite(&mut self) {
        if self.capabilities.primary_screen_exit.is_some()
            && !self.capabilities.alt_active
            && self.grid.cursor.col == 0
        {
            let row = self.grid.cursor.row;
            self.grid.clear_line_all();
            self.hyperlinks.unlink_row(row);
        }
    }

    pub fn settle_primary_screen_exit_if_idle(&mut self, now: Instant) -> bool {
        let ready = self
            .capabilities
            .primary_screen_exit
            .as_ref()
            .is_some_and(|pending| {
                now.saturating_duration_since(pending.last_activity)
                    >= PRIMARY_SCREEN_EXIT_SETTLE_DELAY
            });
        ready && self.settle_primary_screen_exit()
    }

    pub fn settle_primary_screen_exit(&mut self) -> bool {
        let Some(pending) = self.capabilities.primary_screen_exit.take() else {
            return false;
        };
        self.snapshot_primary_screen_output();
        self.block_tracker
            .finish_deferred_screen_command(pending.exit_code);
        self.capabilities.primary_screen_interrupt_capture = None;
        // A killed TUI is not guaranteed to emit DEC mouse-mode resets. Do
        // not let stale reporting state turn later shell clicks into literal
        // SGR mouse coordinates such as `48;62;25M`.
        self.capabilities.mouse_protocol = crate::input::MouseProtocol::Off;
        self.capabilities.sgr_mouse = false;
        let block_id = self
            .block_tracker
            .blocks()
            .last()
            .map(|b| b.id.0)
            .unwrap_or(0);
        tracing::info!(
            ?pending.exit_code,
            block_id,
            "settled primary-screen command finalization"
        );
        true
    }
}

fn merge_primary_screen_interrupt_tail(
    frozen_text: String,
    frozen_styled: StyledOutput,
    tail: &str,
) -> (String, StyledOutput) {
    let tail = semantic_exit_tail(tail).trim_matches('\n');
    if tail.is_empty() {
        return (frozen_text, frozen_styled);
    }
    let merged = format!("{}\n\n{}", frozen_text.trim_end_matches('\n'), tail);
    space_primary_screen_exit_tail(merged, frozen_styled)
}

fn semantic_exit_tail(tail: &str) -> &str {
    let explicit = ["Press Ctrl-C again to exit", "Resume this session with:"]
        .into_iter()
        .filter_map(|marker| line_marker_start(tail, marker))
        .min();
    let session_card = tail
        .match_indices("Session")
        .filter(|(start, _)| *start == 0 || tail.as_bytes().get(start - 1) == Some(&b'\n'))
        .find_map(|(start, _)| {
            tail[start..]
                .lines()
                .skip(1)
                .take(3)
                .any(|line| line.trim_start().starts_with("Continue"))
                .then_some(start)
        });
    explicit
        .into_iter()
        .chain(session_card)
        .min()
        .map_or(tail, |start| &tail[start..])
}

fn line_marker_start(text: &str, marker: &str) -> Option<usize> {
    text.match_indices(marker)
        .map(|(start, _)| start)
        .find(|&start| start == 0 || text.as_bytes().get(start - 1) == Some(&b'\n'))
}

fn viewport_row_for_document_start(start: u64, viewport_origin: u64, rows: usize) -> usize {
    start.saturating_sub(viewport_origin).min(rows as u64) as usize
}

fn space_primary_screen_exit_tail(
    text: String,
    mut styled: StyledOutput,
) -> (String, StyledOutput) {
    if !text.contains("Press Ctrl-C again to exit") && !text.contains("Resume this session with:") {
        return (text, styled);
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let insert_before: Vec<usize> = (1..lines.len())
        .filter(|&index| {
            let line = lines[index].trim();
            let semantic_tail =
                line == "Press Ctrl-C again to exit" || line == "Resume this session with:";
            semantic_tail && !lines[index - 1].trim().is_empty()
        })
        .collect();
    if insert_before.is_empty() {
        drop(lines);
        return (text, styled);
    }

    let mut spaced = Vec::with_capacity(lines.len() + insert_before.len());
    for (index, line) in lines.into_iter().enumerate() {
        if insert_before.binary_search(&index).is_ok() {
            spaced.push("");
        }
        spaced.push(line);
    }
    for line in &mut styled.lines {
        let original = line.line as usize;
        let shift = insert_before.partition_point(|&index| index <= original);
        line.line = line.line.saturating_add(shift as u32);
    }
    (spaced.join("\n"), styled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::StyledLine;

    #[test]
    fn exit_tail_spacing_shifts_parallel_style_line_indices() {
        let styled = StyledOutput {
            lines: (0..4)
                .map(|line| StyledLine {
                    line,
                    foregrounds: Vec::new(),
                    backgrounds: Vec::new(),
                    links: Vec::new(),
                })
                .collect(),
        };
        let (text, styled) = space_primary_screen_exit_tail(
            "answer\nPress Ctrl-C again to exit\nResume this session with:\nclaude --resume id"
                .to_string(),
            styled,
        );

        assert_eq!(
            text,
            "answer\n\nPress Ctrl-C again to exit\n\nResume this session with:\nclaude --resume id"
        );
        assert_eq!(
            styled
                .lines
                .iter()
                .map(|line| line.line)
                .collect::<Vec<_>>(),
            [0, 2, 4, 5]
        );
    }

    #[test]
    fn semantic_tail_discards_repainted_banners_for_claude_and_opencode() {
        assert_eq!(
            semantic_exit_tail("repainted answer\nPress Ctrl-C again to exit\nResume this session with:\nclaude --resume id"),
            "Press Ctrl-C again to exit\nResume this session with:\nclaude --resume id"
        );
        assert_eq!(
            semantic_exit_tail("opencode banner\nSession   project\nContinue  opencode -s id"),
            "Session   project\nContinue  opencode -s id"
        );
    }

    #[test]
    fn document_start_maps_to_a_clamped_viewport_row() {
        assert_eq!(viewport_row_for_document_start(12, 10, 8), 2);
        assert_eq!(viewport_row_for_document_start(8, 10, 8), 0);
        assert_eq!(viewport_row_for_document_start(30, 10, 8), 8);
    }
}
