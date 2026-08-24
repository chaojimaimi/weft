//! Pre-exec staging-buffer routing (FIX_ORPHAN_PARSE_ERROR_OUTPUT).
//!
//! Between editor submission ([`Terminal::submit_command`]) and the shell's
//! `133;B` preexec marker, printed bytes belong to one of two disjoint
//! worlds:
//!
//! - **Normal path**: zsh accepted the line; ZLE repaints/erases the command
//!   line before preexec fires. Those bytes are echo noise — they divert into
//!   [`Terminal::preexec_staging`] and are discarded at `133;B`, so they
//!   never enter a block (pre-fix behavior preserved).
//! - **Parse-error path**: zsh rejects the whole line before preexec
//!   (`zsh: parse error near …`) and no `133;B` ever arrives. The error
//!   report lands in staging, and the closing `133;D` finds no pending
//!   command — [`BlockTracker::on_orphan_command_end`] then synthesizes the
//!   block from the staged bytes instead of silently dropping them.
//!
//! The two paths are mutually exclusive by construction: a real `133;B`
//! clears the staging AND consumes `command_from_editor`, so an orphan
//! `133;D` can only find staged content when no `133;B` happened for this
//! submission. Both events log their byte volume, making an impossible
//! double-fire visible in traces (discipline item: timing assertion via
//! tracing, no panic).
//!
//! Why per-operation sinks instead of one `capture_sink() -> &mut
//! OutputCapture`: the in-flight target mutates through `BlockTracker::on_*`
//! methods (which also bump `live_output_version`), so sink selection has to
//! dispatch per operation rather than once per borrow. Grid writes themselves
//! are untouched by this module — staging mirrors bytes, never intercepts.

use super::Terminal;
use crate::blocks::{CapturedStyle, OutputCapture, ShellPhase, MAX_OUTPUT_BYTES};

impl Terminal {
    /// Whether pre-`133;B` bytes should divert into the staging buffer:
    /// an editor-submitted command still awaiting preexec, integrated phase
    /// AtPrompt, primary screen.
    ///
    /// WHY the non-empty check: an EMPTY submission (`submit_command` on bare
    /// Enter) already synthesizes its spacer block inline and leaves
    /// `command_from_editor = Some("")`. Arming staging for it made the
    /// shell's echo of the newline land in staging, and the precmd pair's
    /// unconditional `133;D;0` then synthesized a duplicate empty block via
    /// [`BlockTracker::on_orphan_command_end`]. Input passthrough is driven
    /// by the `command_from_editor` field itself and is unaffected by this
    /// predicate.
    fn staging_active(&self) -> bool {
        self.command_from_editor
            .as_deref()
            .is_some_and(|c| !c.is_empty())
            && self.block_tracker.phase() == ShellPhase::AtPrompt
            && !self.capabilities.alt_active
    }

    /// Sink for `Perform::print`: in-flight block capture while the phase
    /// predicate of the original call site holds, staging otherwise.
    pub(super) fn capture_print(&mut self, c: char, style: CapturedStyle) {
        let capturing = !self.capabilities.alt_active
            && self.block_tracker.phase() == ShellPhase::CommandExecuting;
        if capturing {
            self.block_tracker.on_print(c, style);
        } else if self.staging_active() {
            self.preexec_staging.print(c, style, MAX_OUTPUT_BYTES);
        }
    }

    /// Sink for the printable-ASCII fast path in `Terminal::process`.
    /// Mirrors that site's phase-only predicate (screen-owned sessions no-op
    /// inside the tracker exactly as before).
    pub(super) fn capture_print_ascii_run(&mut self, bytes: &[u8], style: CapturedStyle) {
        let capturing = !self.capabilities.alt_active
            && self.block_tracker.phase() == ShellPhase::CommandExecuting;
        if capturing {
            self.block_tracker.on_print_ascii_run(bytes, style);
        } else if self.staging_active() {
            self.preexec_staging
                .print_ascii(bytes, style, MAX_OUTPUT_BYTES);
        }
    }

    /// Sink for LF/VT/FF (`execute`).
    pub(super) fn capture_newline(&mut self) {
        if !self.capabilities.alt_active && self.block_tracker.is_capturing() {
            self.block_tracker.on_newline();
        } else if self.staging_active() {
            self.preexec_staging.newline(MAX_OUTPUT_BYTES);
        }
    }

    /// Sink for CR (`execute`).
    pub(super) fn capture_carriage_return(&mut self) {
        if !self.capabilities.alt_active && self.block_tracker.is_capturing() {
            self.block_tracker.on_carriage_return();
        } else if self.staging_active() {
            self.preexec_staging.carriage_return();
        }
    }

    /// Sink for BS (`execute`).
    pub(super) fn capture_backspace(&mut self) {
        if !self.capabilities.alt_active && self.block_tracker.is_capturing() {
            self.block_tracker.on_backspace();
        } else if self.staging_active() {
            self.preexec_staging.backspace();
        }
    }

    /// Sink for EL (`CSI K`).
    pub(super) fn capture_erase_line(&mut self, mode: u16) {
        if !self.capabilities.alt_active && self.block_tracker.is_capturing() {
            self.block_tracker.on_erase_line(mode);
        } else if self.staging_active() {
            self.preexec_staging.erase_line(mode);
        }
    }

    /// Drop any staged bytes and return how many there were. Callers choose
    /// the log level: `trace!` at the normal `133;B` discard, `warn!` at the
    /// `133;A` stale-leak fallback.
    pub(super) fn drop_preexec_staging(&mut self) -> usize {
        let bytes = self.preexec_staging.as_str().len();
        self.preexec_staging.clear();
        bytes
    }

    /// Pop `(command, staged)` for the orphan `133;D` synthesis. `None` when
    /// nothing was staged — a bare `133;D` keeps the historical noop
    /// semantics (`blocks.rs` `command_end_without_command_start_is_noop`)
    /// and `command_from_editor` is left untouched in that case (the `133;A`
    /// fallback still clears it).
    pub(super) fn take_orphan_staging(&mut self) -> Option<(String, OutputCapture)> {
        if self.preexec_staging.as_str().is_empty() {
            return None;
        }
        let command = self.command_from_editor.take()?;
        Some((command, std::mem::take(&mut self.preexec_staging)))
    }
}
