//! TUI scroll pending logic — handles the ambiguous window where a wheel
//! gesture may belong to a TUI whose alt-screen sequence hasn't reached
//! the parser yet.

use std::time::Instant;

use weft_core::input::Modifiers;

use super::Tab;

#[derive(Clone, Copy)]
pub(crate) struct PendingTuiScroll {
    pub(crate) rows: i32,
    pub(crate) col: usize,
    pub(crate) row: usize,
    pub(crate) mods: Modifiers,
    pub(crate) resolve_at: Instant,
}

pub enum TuiScrollResolution {
    PtyBytes(Vec<u8>),
    LocalRows(i32),
}

impl Tab {
    /// Open a bounded window in which an early wheel gesture may belong to a
    /// TUI whose alternate-screen sequence has not reached the parser yet.
    pub fn arm_tui_scroll_window(&mut self) {
        self.pending_tui_scroll = None;
        self.tui_scroll_deadline = Some(
            std::time::Instant::now()
                .checked_add(std::time::Duration::from_secs(2))
                .unwrap_or_else(std::time::Instant::now),
        );
    }

    pub fn tui_scroll_window_active(&self) -> bool {
        self.tui_scroll_deadline
            .is_some_and(|deadline| std::time::Instant::now() <= deadline)
    }

    /// Accumulate a wheel gesture while a launched TUI is taking ownership.
    pub fn queue_tui_scroll(&mut self, rows: i32, col: usize, row: usize, mods: Modifiers) -> bool {
        if rows == 0 || !self.tui_scroll_window_active() {
            return false;
        }
        if let Some(pending) = &mut self.pending_tui_scroll {
            pending.rows = pending.rows.saturating_add(rows).clamp(-100, 100);
            pending.col = col;
            pending.row = row;
            pending.mods = mods;
        } else {
            self.pending_tui_scroll = Some(PendingTuiScroll {
                rows: rows.clamp(-100, 100),
                col,
                row,
                mods,
                resolve_at: std::time::Instant::now() + std::time::Duration::from_millis(50),
            });
            self.tui_scroll_wake_scheduled = false;
        }
        true
    }

    /// Return the delay for the single wake needed to resolve the current
    /// ambiguous startup gesture. Repeated wheel events share the same wake.
    pub fn take_tui_scroll_wake_delay(&mut self) -> Option<std::time::Duration> {
        // v1.3: snapshot `resolve_at` (Copy) so the `pending_tui_scroll` borrow
        // releases before we mutate `tui_scroll_wake_scheduled` — both fields
        // live on the active pane and would conflict through `DerefMut`.
        let resolve_at = self.pending_tui_scroll.as_ref()?.resolve_at;
        if self.tui_scroll_wake_scheduled {
            return None;
        }
        self.tui_scroll_wake_scheduled = true;
        Some(resolve_at.saturating_duration_since(std::time::Instant::now()))
    }

    /// Resolve an early gesture after the 50ms protocol grace period. If the
    /// command entered alt screen, encode against its final mouse modes;
    /// otherwise return rows for normal local block/grid scrolling.
    pub fn resolve_pending_tui_scroll(&mut self) -> Option<TuiScrollResolution> {
        let pending = self.pending_tui_scroll.as_ref()?;
        if std::time::Instant::now() < pending.resolve_at {
            return None;
        }
        let pending = self.pending_tui_scroll.take()?;
        self.tui_scroll_wake_scheduled = false;
        // v1.3: snapshot every value we need from `terminal` inside a single
        // `if let` block so the immutable terminal borrow releases before we
        // mutate `input_handler` / `tui_scroll_deadline` (which both deref
        // through the active pane and would otherwise conflict).
        let terminal_state = self.terminal.as_ref()?;
        let is_alt_screen = terminal_state.is_alt_screen_active();
        if !is_alt_screen {
            // The first ambiguous gesture has now been classified as ordinary
            // local scrolling. Consume the launch window so subsequent
            // trackpad events are immediate instead of paying 50ms each.
            self.tui_scroll_deadline = None;
            return Some(TuiScrollResolution::LocalRows(pending.rows));
        }

        let app_cursor_keys = terminal_state.app_cursor_keys();
        let mouse_protocol = terminal_state.mouse_protocol();
        let sgr_mouse = terminal_state.sgr_mouse();
        let mouse_protocol_off = mouse_protocol == weft_core::input::MouseProtocol::Off;
        // NLL releases the immutable `pane.terminal` borrow at the end of the
        // last expression above, so the mutations below are free to take a
        // fresh `&mut` through `DerefMut`.

        self.input_handler.app_cursor_keys = app_cursor_keys;
        self.input_handler.mouse_protocol = mouse_protocol;
        self.input_handler.sgr_mouse = sgr_mouse;
        self.tui_scroll_deadline = None;
        let count = pending.rows.unsigned_abs() as usize;
        let mut bytes = Vec::new();

        if !mouse_protocol_off {
            for _ in 0..count {
                if let Some(encoded) = self.input_handler.encode_scroll(
                    pending.rows > 0,
                    pending.col,
                    pending.row,
                    pending.mods,
                ) {
                    bytes.extend_from_slice(&encoded);
                }
            }
            return Some(TuiScrollResolution::PtyBytes(bytes));
        }

        let key = if pending.rows > 0 {
            weft_core::input::KeyCode::Up
        } else {
            weft_core::input::KeyCode::Down
        };
        let single = self
            .input_handler
            .encode_key(key, pending.mods & weft_core::input::Modifiers::SHIFT);
        bytes.reserve(single.len() * count);
        for _ in 0..count {
            bytes.extend_from_slice(&single);
        }
        Some(TuiScrollResolution::PtyBytes(bytes))
    }
}
