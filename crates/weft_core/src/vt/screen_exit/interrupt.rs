//! Primary-screen interrupt capture — the Ctrl-C frozen-transcript window and
//! its tail capture primitives.

use super::Terminal;
use crate::blocks::{CapturedStyle, OutputCapture, ShellPhase, StyledOutput, MAX_OUTPUT_BYTES};

pub(in crate::vt) struct PrimaryScreenInterruptCapture {
    pub(in crate::vt) frozen_text: String,
    pub(in crate::vt) frozen_styled: StyledOutput,
    pub(in crate::vt) tail: OutputCapture,
    pub(in crate::vt) origin_row: Option<usize>,
}

impl Terminal {
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

    /// v1.10.20 (S2): true while the Ctrl-C interrupt capture window is
    /// active. During the window the snapshot rewrites the transcript
    /// (interrupt tail merged in / `space_primary_screen_exit_tail`), so
    /// viewport-relative line mappings computed against the live grid do not
    /// match the rendered snapshot rows — the drag-selection migration must
    /// not run (see `migrate_grid_selection_to_primary_history`).
    pub fn primary_screen_interrupt_capture_active(&self) -> bool {
        self.capabilities.primary_screen_interrupt_capture.is_some()
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

    pub(in crate::vt) fn capture_primary_screen_interrupt_print(
        &mut self,
        c: char,
        style: CapturedStyle,
    ) {
        if let Some(capture) = &mut self.capabilities.primary_screen_interrupt_capture {
            capture.tail.print(c, style, MAX_OUTPUT_BYTES);
        }
    }

    pub(in crate::vt) fn capture_primary_screen_interrupt_ascii(
        &mut self,
        bytes: &[u8],
        style: CapturedStyle,
    ) {
        if let Some(capture) = &mut self.capabilities.primary_screen_interrupt_capture {
            capture.tail.print_ascii(bytes, style, MAX_OUTPUT_BYTES);
        }
    }

    pub(in crate::vt) fn capture_primary_screen_interrupt_newline(&mut self) {
        if let Some(capture) = &mut self.capabilities.primary_screen_interrupt_capture {
            capture.tail.newline(MAX_OUTPUT_BYTES);
        }
    }

    pub(in crate::vt) fn capture_primary_screen_interrupt_carriage_return(&mut self) {
        if let Some(capture) = &mut self.capabilities.primary_screen_interrupt_capture {
            capture.tail.carriage_return();
        }
    }

    pub(in crate::vt) fn capture_primary_screen_interrupt_backspace(&mut self) {
        if let Some(capture) = &mut self.capabilities.primary_screen_interrupt_capture {
            capture.tail.backspace();
        }
    }

    pub(in crate::vt) fn capture_primary_screen_interrupt_erase_line(&mut self, mode: u16) {
        if let Some(capture) = &mut self.capabilities.primary_screen_interrupt_capture {
            capture.tail.erase_line(mode);
        }
    }

    pub(in crate::vt) fn capture_primary_screen_interrupt_cursor_position(
        &mut self,
        clear_line: bool,
    ) {
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
}
