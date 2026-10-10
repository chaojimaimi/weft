//! `SessionManager` tab-lifecycle and restore-path methods, moved verbatim
//! out of `app_state.rs` (v1.13.8 S3 zero-behavior file-budget split; `impl
//! SessionManager` cross-file block per the vt/modes.rs precedent).

use super::{adjust_index_after_move, SessionManager};
use crate::tab::Tab;

impl SessionManager {
    // ── Tab lifecycle ─────────────────────────────────────────────────

    /// Open a new tab and make it active. Returns the new tab index.
    pub fn open_tab(
        &mut self,
        rows: usize,
        cols: usize,
        scrollback: usize,
        proxy: &winit::event_loop::EventLoopProxy<crate::AppEvent>,
        cwd: Option<&str>,
    ) -> usize {
        let tab = Tab::new(rows, cols, scrollback, proxy, cwd);
        self.tabs.push(tab);
        let idx = self.tabs.len() - 1;
        self.active_tab = idx;
        idx
    }

    /// Switch to tab at `idx` (clamped). Returns `(new_idx, prev_idx)` so
    /// callers can run post-switch hooks (IME reset, find refresh).
    pub fn switch_to(&mut self, idx: usize) -> (usize, usize) {
        let prev = self.active_tab;
        let new = idx.min(self.tabs.len().saturating_sub(1));
        self.active_tab = new;
        (new, prev)
    }

    /// Switch to next tab (wraps around). Returns `(new_idx, prev_idx)`.
    pub fn next(&mut self) -> (usize, usize) {
        let n = self.tabs.len();
        if n <= 1 {
            return (self.active_tab, self.active_tab);
        }
        let prev = self.active_tab;
        self.active_tab = (self.active_tab + 1) % n;
        (self.active_tab, prev)
    }

    /// Switch to previous tab (wraps around). Returns `(new_idx, prev_idx)`.
    pub fn prev(&mut self) -> (usize, usize) {
        let n = self.tabs.len();
        if n <= 1 {
            return (self.active_tab, self.active_tab);
        }
        let prev = self.active_tab;
        self.active_tab = (self.active_tab + n - 1) % n;
        (self.active_tab, prev)
    }

    /// Close the active tab. Returns `is_last` so the caller can emit Exit.
    /// After close, `active_tab` moves to the previous tab (wrapping to the
    /// last tab when the first is closed), matching the original close_tab UX.
    pub fn close_active(&mut self) -> bool {
        if self.tabs.is_empty() {
            return true;
        }
        self.tabs.remove(self.active_tab);
        if self.tabs.is_empty() {
            self.active_tab = 0;
            return true;
        }
        if self.active_tab > 0 {
            self.active_tab -= 1;
        } else {
            self.active_tab = self.tabs.len() - 1;
        }
        false
    }

    /// Close a background tab at `idx`. Returns `is_last`. If the closed
    /// tab was before `active_tab`, adjust `active_tab` down.
    pub fn close_background(&mut self, idx: usize) -> bool {
        if idx >= self.tabs.len() {
            return self.tabs.is_empty();
        }
        self.tabs.remove(idx);
        if self.tabs.is_empty() {
            self.active_tab = 0;
            return true;
        }
        if idx < self.active_tab {
            self.active_tab -= 1;
        } else if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        }
        false
    }

    /// Remove a dead tab (shell exited) at `idx`. Same as `close_background`
    /// but semantically distinct for future cleanup hooks.
    ///
    /// v1.11.16 (Fix B3): removing the ACTIVE tab must match `close_active`
    /// semantics (focus falls to the previous tab) instead of
    /// `close_background`'s keep-index behavior (focus lands on the next).
    pub fn remove_dead(&mut self, idx: usize) -> bool {
        if idx == self.active_tab {
            return self.close_active();
        }
        self.close_background(idx)
    }

    // ── Restore path ──────────────────────────────────────────────────

    /// Push a pre-built tab (restore path). Does NOT change `active_tab`.
    pub fn push_tab(&mut self, tab: Tab) {
        self.tabs.push(tab);
    }

    /// Replace the tab at `idx` (restore first-tab rebuild). No-op when the
    /// index is out of bounds.
    pub fn replace_tab(&mut self, idx: usize, tab: Tab) {
        if idx < self.tabs.len() {
            self.tabs[idx] = tab;
        }
    }

    /// Set active tab (restore completion). Clamped to the last valid index.
    pub fn set_active(&mut self, idx: usize) {
        self.active_tab = idx.min(self.tabs.len().saturating_sub(1));
    }

    /// v1.11: Move a tab from `from` to `to` (drag-to-reorder). No-op when
    /// indices are equal or out of bounds. `active_tab` is adjusted so it
    /// continues to point at the same tab (by identity, not position).
    pub fn move_tab(&mut self, from: usize, to: usize) {
        if from == to || from >= self.tabs.len() || to >= self.tabs.len() {
            return;
        }
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.active_tab = adjust_index_after_move(self.active_tab, from, to);
    }
}
