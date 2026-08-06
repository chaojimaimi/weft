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
use crate::blocks::{CapturedStyle, OutputCapture, ShellPhase, StyledOutput, MAX_OUTPUT_BYTES};
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
        // v1.10.6: DEC 2026 synchronized output (`?2026h`) is strong TUI
        // evidence — a plain shell command never emits it. pi and other
        // modern TUIs use it on EVERY repaint; without counting it toward
        // TUI detection, cursor_ops stays < 2 (pi only sends one CUU on
        // startup + CHR per keystroke, neither reaching the threshold) and
        // `primary_screen_app_active()` is never true. Count it the same
        // way as cursor addressing, then start the output capture if the
        // threshold is crossed.
        if !self.capabilities.alt_active {
            self.capabilities.primary_screen_cursor_ops = self
                .capabilities
                .primary_screen_cursor_ops
                .saturating_add(1);
            if self.primary_screen_app_active() {
                self.begin_primary_screen_output_capture();
            }
        }
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

    pub(super) fn note_primary_screen_cursor_addressing(&mut self, absolute: bool) {
        // v1.10.4: count cursor addressing on the primary screen regardless
        // of shell phase. Previously gated on CommandExecuting, which missed
        // TUIs that run WITHOUT shell integration (phase stays NotIntegrated —
        // e.g. openclaw, or any app started before the shell hooks installed).
        // Such apps still own the viewport via CUU/CUD + EL/ED redraws and
        // need the TUI-safe scroll path; the 133;A/B markers reset the count
        // at each prompt, so the phase gate added no protection against
        // misclassification within a command.
        //
        // Round 4 review (MEDIUM-2): scope caveat — in an integrated shell
        // the count resets at every OSC 133 prompt marker, but a genuinely
        // non-integrated session (no 133 at all) never resets it, so
        // `tui_owned_scroll()` stays true for the rest of the session (a
        // permanent dirty-all rebuild; correctness unaffected). Note also
        // that this phase-free counting ONLY feeds the scroll/blit path —
        // `primary_screen_app_active()` still requires CommandExecuting, so
        // screen ownership, snapshots and the BlockView never engage for a
        // non-integrated app (fixed by `tui_scroll_discards_blit_...` tests
        // which drive the 133 sequence first).
        if !self.capabilities.alt_active {
            self.capabilities.primary_screen_cursor_ops = self
                .capabilities
                .primary_screen_cursor_ops
                .saturating_add(1);
            // v1.10.4: absolute addressing (CUP/VPA/CHR) marks a
            // full-viewport repainter (Claude Code) that needs the live
            // grid; relative-only TUIs (openclaw) keep the BlockView.
            self.capabilities.primary_screen_absolute_addressing |= absolute;
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
        let (frozen_text, frozen_styled, _) = self.primary_screen_document_snapshot(document_start);
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

    pub(super) fn capture_primary_screen_interrupt_print(&mut self, c: char, style: CapturedStyle) {
        if let Some(capture) = &mut self.capabilities.primary_screen_interrupt_capture {
            capture.tail.print(c, style, MAX_OUTPUT_BYTES);
        }
    }

    pub(super) fn capture_primary_screen_interrupt_ascii(
        &mut self,
        bytes: &[u8],
        style: CapturedStyle,
    ) {
        if let Some(capture) = &mut self.capabilities.primary_screen_interrupt_capture {
            capture.tail.print_ascii(bytes, style, MAX_OUTPUT_BYTES);
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

    /// v1.10.4: Whether the current viewport is owned by a TUI that repaints
    /// after scrolling (alt-screen apps, or primary-screen TUIs with >= 2
    /// cursor-addressing ops — the openclaw/Claude-Code pattern). Such apps
    /// must NOT use the GPU scroll-blit fast path (which assumes scrolled
    /// rows keep their content): they overwrite scrolled rows with their
    /// redraw, so blitting stale content under the redraw produced the
    /// "content squeezed together / overlapping" corruption. The print path
    /// also uses this to keep cursor-follow viewport scrolls alive across
    /// TUI repaints.
    pub(super) fn tui_owned_scroll(&self) -> bool {
        self.capabilities.alt_active || self.capabilities.primary_screen_cursor_ops >= 2
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
        if self.tui_owned_scroll() {
            self.grid.discard_scroll_and_dirty_all();
        }
        if !self.hyperlinks.cell_map_is_empty() {
            self.hyperlinks.clear_cell_map();
        }
    }

    pub(super) fn begin_primary_screen_output_capture(&mut self) {
        let was_owned = self.block_tracker.screen_document_start().is_some();
        self.block_tracker
            .begin_screen_owned_output(self.capabilities.primary_screen_document_candidate);
        // v1.10.7: first screen ownership of this command locks the render
        // mode. Sparse repainters (pi/openclaw) detected with relative-only
        // addressing keep the BlockView for the whole command, even when a
        // later repaint issues a full-viewport CUP (which would otherwise
        // flip the layout to the live grid mid-task and hide the history
        // blocks). Full-viewport CUP TUIs (Claude Code) lock to the grid.
        if !was_owned && self.block_tracker.screen_document_start().is_some() {
            self.capabilities.primary_screen_block_view_locked =
                !self.capabilities.primary_screen_absolute_addressing;
        }
    }

    fn primary_screen_document_snapshot(
        &self,
        document_start: u64,
    ) -> (String, StyledOutput, Option<usize>) {
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
            // v1.10.4: LF overflow is THE dominant scroll path for
            // primary-screen TUIs (content streaming past the bottom row).
            // `grid.index()` records a pending_scroll delta; for TUI-owned
            // viewports we discard it so the renderer rebuilds instead of
            // GPU-blitting stale content under the app's redraw.
            if self.tui_owned_scroll() {
                self.grid.discard_scroll_and_dirty_all();
            }
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
            if self.tui_owned_scroll() {
                self.grid.discard_scroll_and_dirty_all();
            }
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
            && (!self.primary_screen_app_active()
                || self.capabilities.primary_history_view
                || self.capabilities.primary_screen_block_view_locked)
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
    ///
    /// v1.10.6: only applies to full-viewport CUP TUIs (claude code). A
    /// sparse repainter (pi/openclaw) starts from the shell's 133;B boundary
    /// and writes content incrementally — the frozen `document_start` predates
    /// the app's own output, so hiding rows before it would hide the app's
    /// content. v1.10.7: gate on the per-command render-mode LOCK (set at
    /// first screen ownership) instead of the transient absolute flag, so a
    /// sparse repainter's occasional CUP cannot start hiding rows mid-task.
    pub fn primary_screen_visible_row_start(&self) -> Option<usize> {
        let owns_live_view = self.primary_screen_app_active() || self.primary_screen_exit_pending();
        if self.capabilities.alt_active
            || !owns_live_view
            || self.grid.scroll_offset > 0
            || self.capabilities.primary_screen_block_view_locked
        {
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
    ///
    /// v1.10.6: only return the ownership mask when the TUI has used absolute
    /// cursor addressing (CUP/VPA) — the full-viewport repaint pattern that
    /// touches every row. A sparse repainter like pi/openclaw only touches
    /// its input row per keystroke; applying the partial mask would hide the
    /// rest of the TUI's content (the "pi interface vanishes until touchpad
    /// scroll" symptom). v1.10.7: gate on the per-command render-mode LOCK
    /// (set at first screen ownership) instead of the transient absolute
    /// flag, so a sparse repainter's occasional CUP cannot start masking
    /// rows mid-task. `hidden_before_row` (from `screen_document_start`)
    /// still hides shell rows above the TUI boundary — that is independent
    /// of the ownership mask and always applies.
    pub fn primary_screen_viewport_ownership(&self) -> Option<&[bool]> {
        let owns_live_view = self.primary_screen_app_active() || self.primary_screen_exit_pending();
        if self.capabilities.alt_active
            || !owns_live_view
            || self.grid.scroll_offset > 0
            || self.capabilities.primary_screen_block_view_locked
        {
            return None;
        }
        self.capabilities
            .primary_screen_ownership
            .viewport
            .as_deref()
    }

    pub fn primary_history_view(&self) -> bool {
        self.capabilities.primary_history_view
    }

    /// v1.10.6: the cursor's line index in the most recent primary-screen
    /// snapshot. `None` until the first snapshot, or when the cursor sat on
    /// a row the snapshot skipped (empty / unowned).
    pub fn primary_screen_cursor_snapshot_line(&self) -> Option<usize> {
        self.capabilities.primary_screen_cursor_snapshot_line
    }

    /// v1.10.6: refresh just the cursor's snapshot line, without the
    /// rate-limit or `replace_screen_snapshot` side effects. Called on
    /// every keystroke so the caret/preedit have a precise row even when
    /// no PTY output has arrived yet (IME preedit, idle TUI).
    /// v1.10.7 (reviewer MEDIUM): skip the full document rebuild when the
    /// cursor position is unchanged since the last caret refresh — the
    /// tracked line only depends on the cursor's row, and this call has no
    /// rate limit.
    pub fn snapshot_primary_screen_output_for_caret(&mut self) {
        if self.block_tracker.screen_document_start().is_none() {
            return;
        }
        let cursor = (self.grid.cursor.row, self.grid.cursor.col);
        if self.capabilities.last_caret_snapshot_cursor == Some(cursor) {
            return;
        }
        self.capabilities.last_caret_snapshot_cursor = Some(cursor);
        let document_start = self.block_tracker.screen_document_start().unwrap_or(0);
        let (_, _, cursor_line) = self.primary_screen_document_snapshot(document_start);
        self.capabilities.primary_screen_cursor_snapshot_line = cursor_line;
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

    /// v1.10.4: immediate snapshot refresh for keypress-driven redraws.
    ///
    /// Keystrokes are low-frequency (tens of ms apart at most) compared to
    /// the display-frame rate limit, and they drive the TUI's repaint — a
    /// selection change must show up on the next frame. Waiting out the 50ms
    /// window makes the browsing view lag a blink behind, which reads as a
    /// flicker: frame N shows the old selection, frame N+1 the new one.
    ///
    /// v1.10.4 (round 4): gate on screen-ownership instead of
    /// `primary_history_view`. Once a primary-screen TUI is screen-owned
    /// (`screen_document_start` set), `is_capturing()` returns false and the
    /// live block is ONLY updated through this snapshot — so a relative-only
    /// TUI (openclaw) kept in the BlockView needs the refresh even while
    /// following the live tail (history browsing off).
    ///
    /// v1.10.7: the v1.10.4 MEDIUM-1 absolute-addressing skip is REMOVED. A
    /// sparse repainter like pi does occasional CUP full-viewport repaints,
    /// flipping `primary_screen_absolute_addressing` true while following —
    /// the skip then froze the session block's snapshot (resumed sessions
    /// lost their replay body; the final block kept only the last pre-CUP
    /// frame). The snapshot is the ONLY content source for screen-owned
    /// blocks, so it must refresh regardless of the transient addressing
    /// mode. The rate limit still bounds the rescan cost.
    pub fn refresh_primary_history_snapshot_now(&mut self) -> bool {
        if self.block_tracker.screen_document_start().is_none() {
            return false;
        }
        self.snapshot_primary_screen_output();
        self.capabilities.primary_history_snapshot_at = Some(Instant::now());
        true
    }

    pub(super) fn refresh_primary_history_snapshot_at(&mut self, now: Instant) -> bool {
        if self.block_tracker.screen_document_start().is_none() {
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
        let (text, styled, cursor_line) = self.primary_screen_document_snapshot(document_start);
        let (text, styled) = space_primary_screen_exit_tail(text, styled);
        // v1.10.6: store the precisely-tracked cursor snapshot line so the
        // BlockView paint can place the caret/preedit on the exact document
        // row instead of guessing from a formula that breaks when the
        // snapshot skips empty rows.
        self.capabilities.primary_screen_cursor_snapshot_line = cursor_line;
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
        // v1.10.7: the render-mode lock belongs to the command being
        // finalized — release it here (covers the idle-timer settle AND the
        // 133;B settle; nested-marker paths never settle, so the lock
        // survives them). The next command re-detects and re-locks at its
        // first screen ownership.
        self.capabilities.primary_screen_block_view_locked = false;
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
    use crate::vt::ScreenOwner;

    #[test]
    fn exit_tail_spacing_shifts_parallel_style_line_indices() {
        let styled = StyledOutput {
            lines: (0..4)
                .map(|line| StyledLine {
                    line,
                    foregrounds: Vec::new(),
                    backgrounds: Vec::new(),
                    links: Vec::new(),
                    attributes: Vec::new(),
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

    #[test]
    fn tui_scroll_discards_blit_and_dirties_all_rows() {
        // v1.10.4: a primary-screen TUI (openclaw — relative cursor moves +
        // EL/ED redraws on the main screen) must NOT use the GPU scroll-blit
        // fast path: the app overwrites scrolled rows, so blitting stale
        // content under the redraw produced "content squeezed together".
        // `scroll_grid_up` on a TUI-owned viewport must clear pending_scroll
        // (renderer then rebuilds all rows instead of blitting).
        let mut t = Terminal::new(5, 20);
        // Shell integration → CommandExecuting, then TUI cursor addressing.
        t.process(b"\x1b]133;A\x07\x1b]133;B\x07tui\x1b]133;C\x07");
        // Fill the screen (5 rows → cursor lands on the bottom row).
        for i in 0..5 {
            t.process(format!("row{i}\r\n").as_bytes());
        }
        // Accumulate TUI cursor-addressing evidence (2+ ops) WITHOUT entering
        // alt-screen (the openclaw pattern: relative moves).
        t.process("\x1b[2A\x1b[3B".as_bytes());
        assert_eq!(t.screen_owner(), ScreenOwner::PrimaryScreenApp);

        // Scroll the TUI viewport up.
        t.process("\x1b[1S".as_bytes());
        // The renderer must NOT see a pending scroll blit delta.
        assert_eq!(
            t.grid().take_pending_scroll(),
            0,
            "TUI scroll must discard the blit delta (disable GPU scroll blit)"
        );
        // All rows dirty → renderer rebuilds the whole viewport.
        assert!(
            t.grid().dirty_rows().count() >= 5,
            "all rows must be dirty after TUI scroll"
        );

        // Sanity: the top row changed after scrolling (content moved up).
        let g = t.grid();
        let mut row0 = String::new();
        for c in 0..g.num_cols {
            row0.push(g.cell(0, c).character);
        }
        assert_ne!(row0.trim_end(), "row0", "top row must change after scroll");
    }

    #[test]
    fn dec2026_synchronized_output_triggers_tui_detection() {
        // v1.10.6: pi (coding-agent CLI) uses DEC 2026 synchronized output
        // (?2026h) on every repaint. A plain shell command never emits it.
        // Without counting it toward TUI detection, cursor_ops stays < 2
        // (pi sends only one CUU at startup) and the TUI is never detected
        // — it stays in the BlockView where IME/cursor/color are broken.
        let mut t = Terminal::new(5, 20);
        t.process(b"\x1b]133;A\x07\x1b]133;B\x07pi\x1b]133;C\x07");
        // Startup: one CUU + CHR 1 (<2 ops, not detected yet).
        t.process("\x1b[3A\x1b[1G".as_bytes());
        assert!(!t.primary_screen_app_active(), "<2 ops: not yet a TUI");
        assert!(t.show_block_view());
        // First keystroke: synchronized output begins → TUI detected.
        t.process("\x1b[?2026h".as_bytes());
        assert!(
            t.primary_screen_app_active(),
            "DEC 2026 synchronized output must count as TUI evidence"
        );
        assert!(
            t.show_block_view(),
            "DEC 2026 TUI (non-absolute) stays in BlockView"
        );
    }

    #[test]
    fn chr_input_line_redraw_keeps_block_view() {
        // v1.10.6: pi (coding-agent CLI) uses CHR (horizontal-only) for its
        // input-line redraw + DEC 2026 sync output. CHR counts toward TUI
        // detection but NOT toward absolute addressing — pi stays in the
        // BlockView (bottom-aligned live block + history blocks), matching
        // the Warp-style layout the user expects. IME uses the precisely
        // tracked cursor snapshot line (v1.10.6) + steady caret + preedit
        // dedup so composition works in the BlockView.
        let mut t = Terminal::new(5, 20);
        t.process(b"\x1b]133;A\x07\x1b]133;B\x07pi\x1b]133;C\x07");
        t.process("\x1b[3A\x1b[1G\x1b[?25l".as_bytes());
        assert!(!t.primary_screen_app_active(), "<2 ops: not yet a TUI");
        assert!(t.show_block_view());
        t.process("\x1b[?2026h\x1b[2Ka\x1b[2G\x1b[?2026l".as_bytes());
        assert!(t.primary_screen_app_active(), "TUI detected (>= 2 ops)");
        assert!(
            t.show_block_view(),
            "non-absolute TUI stays in BlockView (bottom-aligned, history blocks)"
        );
    }

    #[test]
    fn relative_addressing_tui_keeps_block_view() {
        // openclaw/pi pattern — relative cursor moves only (A/B/D, CHR).
        // v1.10.6 (final direction): non-absolute TUIs stay in the
        // BlockView (bottom-aligned live block + history blocks), the
        // Warp-style layout the user expects. Only full-viewport CUP
        // TUIs (claude code) switch to the live grid.
        let mut t = Terminal::new(5, 20);
        t.process(b"\x1b]133;A\x07\x1b]133;B\x07openclaw\x1b]133;C\x07");
        t.process("\x1b[999D\x1b[915A".as_bytes());
        assert_eq!(t.screen_owner(), ScreenOwner::PrimaryScreenApp);
        assert!(t.show_block_view(), "non-absolute TUI stays in BlockView");
    }

    #[test]
    fn absolute_addressing_tui_switches_to_live_grid() {
        // The Claude Code pattern — CUP addresses. Same live-grid path.
        let mut t = Terminal::new(5, 20);
        t.process(b"\x1b]133;A\x07\x1b]133;B\x07claude\x1b]133;C\x07");
        t.process("\x1b[H\x1b[2;1H".as_bytes());
        assert_eq!(t.screen_owner(), ScreenOwner::PrimaryScreenApp);
        assert!(
            !t.show_block_view(),
            "absolute-addressing TUI needs the live grid"
        );
    }

    #[test]
    fn osc133_reset_clears_absolute_addressing_flag() {
        // The absolute-addressing evidence is per-command, like cursor_ops:
        // the 133;D end marker (and 133;A/B, defensively) must clear it.
        // (The flag is actually reset at 133;D here; the 133;A reset is
        // redundant defense for the interrupt path.)
        let mut t = Terminal::new(5, 20);
        t.process(b"\x1b]133;A\x07\x1b]133;B\x07claude\x1b]133;C\x07");
        t.process("\x1b[H\x1b[2;1H".as_bytes());
        assert!(!t.show_block_view());
        // Command ends, next prompt, then a new command.
        t.process(b"\x1b]133;D;0\x07\x1b]133;A\x07");
        t.settle_primary_screen_exit();
        t.process(b"sh\x1b]133;B\x07\x1b]133;C\x07\x1b[2A\x1b[3B");
        assert!(t.primary_screen_app_active());
        // v1.10.6 (final direction): the flag cleared → the relative-only
        // move sequence does NOT count as absolute, so the TUI keeps the
        // BlockView (only CUP/VPA-driven full repaints switch to the grid).
        assert!(
            t.show_block_view(),
            "absolute flag cleared → relative-only TUI keeps BlockView"
        );
    }

    #[test]
    fn screen_owned_snapshot_refreshes_without_history_view() {
        // v1.10.6: snapshot refresh is driven by screen ownership for
        // history browsing. A primary-screen TUI uses the live grid
        // (show_block_view == false), but the snapshot must still be
        // publishable for the moment the user scrolls into history
        // browsing (show_block_view → true via primary_history_view).
        let mut t = Terminal::new(5, 48);
        t.process(b"\x1b]133;A\x07\x1b]133;B\x07openclaw\x1b]133;C\x07");
        t.process("\x1b[999D\x1b[915A".as_bytes());
        assert!(t.primary_screen_app_active());
        assert!(t.show_block_view(), "non-absolute TUI stays in BlockView");
        assert!(
            t.block_tracker().screen_document_start().is_some(),
            "cursor addressing must begin screen ownership"
        );

        t.process("choice A".as_bytes());
        assert!(
            !t.primary_history_view(),
            "precondition: following the live tail, not browsing history"
        );
        assert!(
            t.refresh_primary_history_snapshot_now(),
            "screen-owned snapshot refresh must work without history browsing"
        );
        assert!(
            t.block_tracker()
                .in_flight()
                .is_some_and(|live| live.output.contains("choice A")),
            "live block must publish the repainted content"
        );
    }

    #[test]
    fn snapshot_refresh_requires_screen_ownership() {
        // Plain command output (no cursor addressing) is NOT screen-owned:
        // print capture still feeds the live block, so the snapshot refresh
        // must stay a no-op.
        let mut t = Terminal::new(5, 48);
        t.process(b"\x1b]133;A\x07echo hi\x1b]133;B\x07\x1b]133;C\x07");
        t.process("plain output".as_bytes());
        assert!(!t.primary_screen_app_active());
        assert!(t.block_tracker().screen_document_start().is_none());
        assert!(
            !t.refresh_primary_history_snapshot_now(),
            "non-screen-owned output must not refresh a screen snapshot"
        );
    }

    #[test]
    fn non_integrated_addressing_engages_blit_discard_but_not_screen_ownership() {
        // v1.10.4 (reviewer MEDIUM-2): in a genuinely non-integrated session
        // (no OSC 133 ever — phase stays NotIntegrated) relative addressing
        // must still engage the TUI-safe scroll path (`tui_owned_scroll`),
        // while screen ownership / BlockView stay OFF because
        // `primary_screen_app_active()` requires CommandExecuting. The
        // count never resets without 133 markers — that leak is accepted and
        // documented on `note_primary_screen_cursor_addressing`.
        let mut t = Terminal::new(5, 20);
        assert_eq!(t.screen_owner(), ScreenOwner::Shell);
        t.process("\x1b[2A\x1b[3B".as_bytes());
        assert!(
            t.tui_owned_scroll(),
            "relative addressing must engage the TUI-safe scroll path"
        );
        assert_eq!(
            t.screen_owner(),
            ScreenOwner::Shell,
            "non-integrated phase must NOT grant screen ownership"
        );
        assert!(
            t.block_tracker().screen_document_start().is_none(),
            "no screen-owned snapshot state for a non-integrated session"
        );
        // The count persists (no 133 to reset it) — scroll path stays engaged.
        t.process("\x1b[4B".as_bytes());
        assert!(t.tui_owned_scroll());
    }

    #[test]
    fn nested_run_starting_with_a_keeps_the_block_view_lock() {
        // v1.10.7 (reviewer HIGH): the FIRST marker of a nested run can be
        // `133;A` with NO pending exit (pi's inner zsh prompt before any
        // internal command completed). The old defer-branch unlock released
        // the render-mode lock there, so the next CUP repaint flipped the
        // session to the live grid mid-task. The lock must survive until the
        // REAL settle.
        let mut t = Terminal::new(8, 40);
        t.process(b"\x1b]133;A\x07pi\x1b]133;B\x07\x1b]133;C\x07");
        t.process("\x1b[3A\x1b[1G\x1b[?25l".as_bytes());
        t.process("\x1b[?2026h\x1b[2Ka\x1b[2G\x1b[?2026l".as_bytes());
        assert!(t.primary_screen_app_active());
        assert!(t.show_block_view(), "relative-detected TUI locks BlockView");

        // Nested run #1: A is the FIRST marker (no pending exit yet).
        t.process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
        assert!(
            t.show_block_view(),
            "a nested 133;A with no pending exit must not unlock BlockView"
        );
        assert!(t.block_tracker().phase() == ShellPhase::CommandExecuting);
        t.process("\x1b[2J\x1b[Hworking...".as_bytes());
        assert!(
            t.show_block_view(),
            "CUP repaint after a nested A-start must keep BlockView"
        );

        // Nested run #2: D-then-A pair.
        t.process(b"\x1b]133;D;0\x07\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
        assert!(t.show_block_view());

        // Real settle (idle timer path) releases the lock for the next command.
        t.process(b"\x1b]133;D;0\x07");
        assert!(t.settle_primary_screen_exit());
        assert!(!t.capabilities.primary_screen_block_view_locked);
    }

    #[test]
    fn sparse_repainter_locks_block_view_through_later_cup_repaints() {
        // v1.10.7: a sparse repainter (pi) detected with relative-only
        // addressing locks the BlockView for the whole command. Its
        // occasional full-viewport CUP repaint (task start, layout change)
        // must NOT flip the renderer to the live grid — that flip caused
        // per-task layout flicker and hid the Warp-style history blocks.
        let mut t = Terminal::new(8, 40);
        t.process(b"\x1b]133;A\x07pi\x1b]133;B\x07\x1b]133;C\x07");
        t.process("\x1b[3A\x1b[1G\x1b[?25l".as_bytes());
        t.process("\x1b[?2026h\x1b[2Ka\x1b[2G\x1b[?2026l".as_bytes());
        assert!(t.primary_screen_app_active(), "TUI detected");
        assert!(
            t.show_block_view(),
            "relative-detected TUI locks the BlockView"
        );

        // Task start: pi clears the viewport and repaints (CUP addressing).
        t.process("\x1b[2J\x1b[Hworking...\x1b[2;1Hprogress".as_bytes());
        assert!(t.primary_screen_app_active());
        assert!(
            t.show_block_view(),
            "a CUP repaint inside a locked session must keep the BlockView"
        );
        assert_eq!(
            t.primary_screen_visible_row_start(),
            None,
            "no row hiding for BlockView-locked TUIs"
        );
        assert_eq!(t.primary_screen_viewport_ownership(), None);

        // Nested marker bursts must not unlock the render mode.
        t.process(b"\x1b]133;D;0\x07\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
        assert!(
            t.show_block_view(),
            "nested markers must not unlock the BlockView"
        );

        // Real exit settles the block; the NEXT command re-detects its mode.
        t.process(b"\x1b]133;D;0\x07");
        assert!(t.settle_primary_screen_exit());
        t.process(b"\x1b]133;A\x07claude\x1b]133;B\x07\x1b]133;C\x07");
        t.process("\x1b[H\x1b[2;1H".as_bytes());
        assert!(
            !t.show_block_view(),
            "a later absolute-addressing TUI re-detects to the live grid"
        );
    }

    #[test]
    fn absolute_tui_following_tail_refreshes_snapshot_for_the_final_block() {
        // v1.10.7: the v1.10.4 MEDIUM-1 skip is removed. An absolute-
        // addressing TUI (Claude Code) follows in the live grid, so the
        // BlockView has no snapshot consumer WHILE following — but the
        // session block's final content (exit history, resumed sessions)
        // comes from the snapshot, so it must keep refreshing. A sparse
        // repainter like pi toggles CUP per repaint; freezing on that
        // transient flag made blocks lose their body.
        let mut t = Terminal::new(5, 48);
        t.process(b"\x1b]133;A\x07claude\x1b]133;B\x07\x1b]133;C\x07");
        t.process("\x1b[H\x1b[2;1H".as_bytes());
        assert!(t.primary_screen_app_active());
        assert!(
            !t.show_block_view(),
            "absolute TUI follows in the live grid"
        );

        t.process("answer".as_bytes());
        assert!(
            t.refresh_primary_history_snapshot_now(),
            "following an absolute TUI must still refresh the snapshot (final block content)"
        );
        assert!(t
            .block_tracker()
            .in_flight()
            .is_some_and(|live| live.output.contains("answer")));
        // History browsing keeps refreshing too.
        t.set_primary_history_view(true);
        assert!(t.refresh_primary_history_snapshot_now());
        assert!(t
            .block_tracker()
            .in_flight()
            .is_some_and(|live| live.output.contains("answer")));
    }

    #[test]
    fn tui_lf_overflow_discards_blit_and_dirties_all_rows() {
        // v1.10.4 (reviewer HIGH): the LF-overflow path — content streaming
        // past the bottom row — is the DOMINANT scroll route for primary-
        // screen TUIs (openclaw streams lines with \r\n). It goes through
        // `index_primary_screen` → `grid.index()`, NOT `scroll_grid_rows`.
        // The TUI-safe discard must apply there too, or the GPU blit fires
        // under the app's redraw and reproduces the squeeze corruption.
        let mut t = Terminal::new(5, 20);
        // Shell integration → CommandExecuting, then TUI cursor addressing.
        t.process(b"\x1b]133;A\x07\x1b]133;B\x07tui\x1b]133;C\x07");
        // Establish TUI ownership (relative cursor moves, no alt-screen).
        t.process("\x1b[2A\x1b[3B".as_bytes());
        assert_eq!(t.screen_owner(), ScreenOwner::PrimaryScreenApp);

        // Overflow the viewport with plain line feeds.
        for i in 0..8 {
            t.process(format!("overflow-{i}\r\n").as_bytes());
        }
        assert_eq!(
            t.grid().take_pending_scroll(),
            0,
            "LF overflow on a TUI viewport must discard the blit delta"
        );
        assert!(
            t.grid().dirty_rows().count() >= 5,
            "all rows must be dirty after TUI LF overflow"
        );
    }
}
