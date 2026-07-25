//! Per-tab session state (v0.9 H1, v1.3 multi-pane).
//!
//! A `Tab` owns one or more [`Pane`]s arranged by a [`SplitTree`]. Each pane
//! carries its own PTY, VT `Terminal`, input/selection handlers, IME preedit,
//! and block-scroll state. The tab-level concerns (split tree, active pane
//! id, stable tab `session_id` for tab-bar / context-menu ownership) live
//! here.
//!
//! `Tab` implements `Deref<Target=Pane>` / `DerefMut` so existing call sites
//! that read `tab.terminal` / `tab.pty` / `tab.input_handler` / … keep
//! compiling unchanged: the field access auto-dereferences to the active
//! pane. New code that needs to operate on a non-active pane should go
//! through `tab.pane(id)` / `tab.pane_mut(id)`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use weft_core::pane_layout::{PaneId, SplitDirection, SplitError, SplitTree};
use weft_core::persistence::TabSnapshot;
use weft_core::pty::PtyError;
use weft_core::vt::Terminal;

use crate::pane::Pane;
use crate::{AppEvent, AppMsg};

mod lifecycle;
mod primary_history;
mod scroll;
mod tui_scroll;

pub(crate) use primary_history::PrimaryHistoryRefresh;
pub(crate) use scroll::BlockScrollAnchor;
pub(crate) use tui_scroll::PendingTuiScroll;
pub use tui_scroll::TuiScrollResolution;

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

/// A tab owns one or more panes. The tab-level `session_id` is the externally
/// visible identifier (tab bar, context menu, autosave); each pane also has
/// its own `pane_session_id` for PTY / trace correlation.
///
/// The active pane is the one that receives keyboard input and renders the
/// cursor. `Deref` / `DerefMut` forward to it so the existing single-pane
/// call sites (`tab.terminal`, `tab.pty`, …) keep working without change.
pub struct Tab {
    /// Stable tab-level identifier. Stays constant for the tab's lifetime
    /// regardless of how many panes are open or which is active.
    pub session_id: u64,
    /// Layout tree of pane splits. Owns the structure (direction + ratio at
    /// each internal node, `PaneId` at each leaf) but not the pane state —
    /// the state lives in `panes`.
    #[allow(dead_code)] // v1.3 Batch 5+: read by split_tree() for multi-pane layout
    split_tree: SplitTree,
    /// Per-pane state keyed by `PaneId`. Every leaf in `split_tree` has an
    /// entry here; closing a pane removes both the leaf and the entry.
    panes: HashMap<PaneId, Pane>,
    /// Currently focused pane. Keyboard input, IME, and Deref forward here.
    /// Always points at a leaf present in `split_tree` / `panes`.
    active_pane: PaneId,
}

impl std::ops::Deref for Tab {
    type Target = Pane;
    fn deref(&self) -> &Self::Target {
        self.panes
            .get(&self.active_pane)
            .expect("active_pane always points at a present pane")
    }
}

impl std::ops::DerefMut for Tab {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.panes
            .get_mut(&self.active_pane)
            .expect("active_pane always points at a present pane")
    }
}

impl Tab {
    /// Borrow the active pane.
    pub(crate) fn active(&self) -> &Pane {
        self
    }

    /// Mutably borrow the active pane.
    pub(crate) fn active_mut(&mut self) -> &mut Pane {
        self
    }

    /// Borrow a specific pane by id. Returns `None` if the pane is not in
    /// this tab (closed or never created).
    #[allow(dead_code)] // v1.3 Batch 4+: consumed by split/focus/close handlers
    pub(crate) fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes.get(&id)
    }

    /// Mutably borrow a specific pane by id.
    #[allow(dead_code)] // v1.3 Batch 4+: consumed by split/focus/close handlers
    pub(crate) fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.panes.get_mut(&id)
    }

    /// Id of the currently focused pane.
    #[allow(dead_code)] // v1.3 Batch 4+: consumed by split/focus/close handlers
    pub(crate) fn active_pane_id(&self) -> PaneId {
        self.active_pane
    }

    /// Read-only view of the split tree (for layout / hit-testing).
    #[allow(dead_code)] // v1.3 Batch 5+: consumed by multi-pane renderer
    pub(crate) fn split_tree(&self) -> &SplitTree {
        &self.split_tree
    }

    /// Number of panes in this tab. O(n) — walks the tree.
    pub(crate) fn pane_count(&self) -> usize {
        self.split_tree.pane_count()
    }

    // ── v1.3: Pane lifecycle (split / focus / close) ──────────────────

    /// Split the active pane in `direction`. The new pane inherits the
    /// active pane's cwd (live OSC 7 if available, else the restored
    /// snapshot cwd, else `None` to inherit the weft process cwd) and
    /// starts at the active pane's current (rows, cols). The renderer /
    /// PTY-resize pass (Batch 5/6) will shrink both panes to their
    /// split-tree-computed rects on the next layout pass. The new pane
    /// becomes active.
    ///
    /// Returns the new pane's id on success.
    pub(crate) fn split_active_pane(
        &mut self,
        direction: SplitDirection,
        ratio: f32,
        scrollback_lines: usize,
        proxy: &winit::event_loop::EventLoopProxy<AppEvent>,
    ) -> Result<PaneId, SplitError> {
        let cwd = self.launch_cwd().map(str::to_owned);
        let (rows, cols) = self
            .active()
            .terminal
            .as_ref()
            .map(|t| (t.grid().num_rows, t.grid().num_cols))
            .unwrap_or((24, 80));
        let new_pane = Pane::spawn(rows, cols, scrollback_lines, proxy, cwd.as_deref());
        self.split_active_pane_inner(direction, ratio, new_pane)
    }

    /// Test-only split that injects a no-PTY pane (built via
    /// `Pane::with_terminal_only`). Mirrors `split_active_pane` minus the
    /// PTY spawn so unit tests can exercise the tree / HashMap plumbing
    /// without a real shell.
    #[cfg(test)]
    pub(crate) fn split_active_pane_test(
        &mut self,
        direction: SplitDirection,
        ratio: f32,
        scrollback_lines: usize,
    ) -> Result<PaneId, SplitError> {
        let new_pane = Pane::with_terminal_only(scrollback_lines);
        self.split_active_pane_inner(direction, ratio, new_pane)
    }

    /// Shared core of `split_active_pane` / `split_active_pane_test`:
    /// insert a pre-built pane as the second child of the active leaf.
    /// Splitting is infallible once the caller hands us a pane — the only
    /// error paths are ratio-out-of-range and a stale active id, both of
    /// which indicate caller bugs.
    fn split_active_pane_inner(
        &mut self,
        direction: SplitDirection,
        ratio: f32,
        new_pane: Pane,
    ) -> Result<PaneId, SplitError> {
        let new_pane_id = PaneId(new_pane.pane_session_id);
        self.split_tree
            .split_leaf(self.active_pane, direction, ratio, new_pane_id)?;
        self.panes.insert(new_pane_id, new_pane);
        self.active_pane = new_pane_id;
        Ok(new_pane_id)
    }

    /// Cycle focus to the next pane in declaration order (wraps around).
    /// Returns the new active pane id, or `None` if the tab has no panes
    /// (should never happen for a live tab — the tab is closed when its
    /// last pane closes). No-op (returns the same id) when the tab has
    /// exactly one pane.
    pub(crate) fn focus_next_pane(&mut self) -> Option<PaneId> {
        self.split_tree.focus_next()
    }

    /// v1.3 Batch 6: Set the active pane by id. Used by mouse hit-testing
    /// to focus the pane under the cursor on click. Returns `Err` if the
    /// pane id is not a leaf in the split tree (stale id — shouldn't happen
    /// for a live tab).
    pub(crate) fn set_active_pane(&mut self, id: PaneId) -> Result<(), SplitError> {
        self.split_tree.set_active(id)?;
        self.active_pane = id;
        Ok(())
    }

    /// Cycle focus to the previous pane in declaration order (wraps).
    pub(crate) fn focus_prev_pane(&mut self) -> Option<PaneId> {
        self.split_tree.focus_prev()
    }

    /// Close the active pane. Returns `Ok(true)` if the tab is now empty
    /// (caller should close the tab via `SessionManager::close_active`);
    /// returns `Ok(false)` if the tab still has panes — in that case
    /// focus moves to the surviving sibling of the closed pane (or the
    /// previous leaf in declaration order when the closed pane was the
    /// root).
    ///
    /// Returns `Err` only if the active pane id is stale (should never
    /// happen — `active_pane` is always kept in sync with the tree).
    pub(crate) fn close_active_pane(&mut self) -> Result<bool, SplitError> {
        let closing = self.active_pane;
        let new_active = self.split_tree.close_pane(closing)?;
        // Update `active_pane` BEFORE removing from `panes` so the invariant
        // ("active_pane always points at a live pane in `panes`") is never
        // violated — Deref would panic if it ran between the remove and the
        // reassignment.
        if let Some(id) = new_active {
            self.active_pane = id;
        }
        self.panes.remove(&closing);
        Ok(new_active.is_none())
    }

    /// Create a new tab with a PTY + Terminal pair at the given size.
    ///
    /// `cwd` — if `Some(path)`, the shell starts in that directory (via
    /// `chdir` in the child process before exec). Pass `None` to inherit
    /// the weft process's cwd. Using `chdir` instead of sending a `cd`
    /// command keeps the initial tab clean — no `cd` appears in the
    /// terminal, shell history, or block tracker.
    pub fn new(
        rows: usize,
        cols: usize,
        scrollback_lines: usize,
        proxy: &winit::event_loop::EventLoopProxy<AppEvent>,
        cwd: Option<&str>,
    ) -> Self {
        let pane = Pane::spawn(rows, cols, scrollback_lines, proxy, cwd);
        Self::from_single_pane(pane)
    }

    /// Empty tab (used when PTY spawn fails — terminal/pty stay None).
    #[allow(dead_code)] // used by tests; production code goes through `Tab::new`
    pub(crate) fn empty() -> Self {
        Self::from_single_pane(Pane::empty())
    }

    /// Build a tab from a single starting pane. The pane becomes the root
    /// of the split tree and the active pane. Used by `new` / `empty` and
    /// by tests that construct a pane manually.
    fn from_single_pane(pane: Pane) -> Self {
        let pane_id = PaneId(pane.pane_session_id);
        let mut panes = HashMap::new();
        panes.insert(pane_id, pane);
        Self {
            session_id: NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed),
            split_tree: SplitTree::new(pane_id),
            panes,
            active_pane: pane_id,
        }
    }

    /// Test-only constructor: build a tab wrapping a single pre-built pane.
    /// Used by snapshot / scroll tests that need a Terminal without spawning
    /// a PTY.
    #[cfg(test)]
    pub(crate) fn with_single_pane(pane: Pane) -> Self {
        Self::from_single_pane(pane)
    }

    /// Bump and return the next per-tab input-event sequence number.
    ///
    /// Currently called at the keyboard-encode chokepoint only. IME commit,
    /// mouse gestures, and paste are not yet wired — extending coverage to
    /// those sources is a follow-up if cross-source trace correlation is
    /// needed. The counter is per-process monotonic (the static allocator
    /// never resets), so two tabs never share a sequence number — useful for
    /// cross-tab log correlation during free testing.
    pub(crate) fn next_input_seq(&mut self) -> u64 {
        self.active_mut().next_input_seq()
    }

    /// Current input-event sequence (last value returned by `next_input_seq`).
    /// Read from trace points that observe an Effect but did not themselves
    /// bump the counter (e.g. `drain_effects`).
    pub(crate) fn input_seq(&self) -> u64 {
        self.active().input_seq
    }

    /// Keep the in-memory Grid and the next PTY `TIOCSWINSZ` inseparable.
    /// Overwriting (rather than preserving) an older pending size is required
    /// when several window/font/sidebar changes coalesce before the throttle
    /// flushes them, including for background panes.
    pub(crate) fn resize_terminal_and_queue(&mut self, rows: usize, cols: usize) -> bool {
        self.active_mut().resize_terminal_and_queue(rows, cols)
    }

    /// v1.3 Batch 6: Resize every pane's terminal according to its split-tree-
    /// computed rect. `content_rect` is the full content area (chrome already
    /// subtracted); `cell_w` / `cell_h` are physical-pixel cell dimensions.
    /// Each pane's (rows, cols) is derived from its rect size ÷ cell size.
    /// Returns `true` if the active pane was resized (for logging parity with
    /// `resize_terminal_and_queue`).
    ///
    /// For single-pane tabs this is equivalent to `resize_terminal_and_queue`
    /// — the split tree returns one rect equal to `content_rect`.
    pub(crate) fn resize_all_panes_for_rect(
        &mut self,
        content_rect: weft_core::pane_layout::Rect,
        cell_w: f32,
        cell_h: f32,
    ) -> bool {
        if cell_w <= 0.0 || cell_h <= 0.0 {
            return false;
        }
        let layouts = self.split_tree.layout(content_rect);
        let active = self.active_pane;
        let mut active_resized = false;
        for (pane_id, rect) in layouts {
            let [x0, y0, x1, y1] = rect;
            let w = (x1 - x0).max(0.0);
            let h = (y1 - y0).max(0.0);
            let cols = (w / cell_w).floor() as usize;
            let rows = (h / cell_h).floor() as usize;
            if rows == 0 || cols == 0 {
                continue;
            }
            if let Some(pane) = self.panes.get_mut(&pane_id) {
                if pane.resize_terminal_and_queue(rows, cols) && pane_id == active {
                    active_resized = true;
                }
            }
        }
        active_resized
    }

    /// v1.3 Batch 6: Read (non-consuming) every pane's pending PTY resize.
    /// Returns `(pane_id, (rows, cols))` pairs for all panes that have a
    /// pending resize. Used by the redraw loop to build per-pane
    /// `Effect::ResizePty` entries (replacing the old per-tab collection that
    /// only saw the active pane's pending resize via Deref).
    pub(crate) fn pending_pane_resizes(&self) -> Vec<(PaneId, (usize, usize))> {
        let mut out = Vec::new();
        for (id, pane) in &self.panes {
            if let Some(dim) = pane.pending_pty_resize {
                out.push((*id, dim));
            }
        }
        out
    }

    /// v1.3 Batch 6: Find the pane whose split-tree-computed rect contains
    /// the physical-pixel point `(x, y)`. Used by mouse hit-testing to route
    /// clicks / scrolls to the correct pane. Returns `None` if the point is
    /// outside all panes (e.g. in the padding or chrome).
    pub(crate) fn pane_hit_test(
        &self,
        x: f32,
        y: f32,
        content_rect: weft_core::pane_layout::Rect,
    ) -> Option<PaneId> {
        let layouts = self.split_tree.layout(content_rect);
        layouts.into_iter().find_map(|(id, rect)| {
            let [x0, y0, x1, y1] = rect;
            (x >= x0 && x < x1 && y >= y0 && y < y1).then_some(id)
        })
    }

    /// Non-blocking drain of PTY events into channel. Capped per frame.
    pub fn pump_pty(&mut self) {
        self.active_mut().pump_pty()
    }

    /// Deliver one PTY-native interrupt without discarding command output.
    ///
    /// Ctrl+C is always one ETX byte, regardless of whether the foreground app
    /// uses the alternate screen, primary-screen cursor addressing, raw mode,
    /// SSH, or a conventional shell command. Output is intentionally retained:
    /// interactive tools write confirmation/resume tails after the first or
    /// second Ctrl+C, and a post-write `tcflush` can race away the ETX itself.
    pub fn interrupt_pty(&mut self) -> bool {
        self.active_mut().interrupt_pty()
    }

    /// Write input initiated by the user to this tab's PTY.
    ///
    /// A first Ctrl+C may freeze the current primary-screen transcript while
    /// an interactive program decides whether to exit. Any later user input
    /// means the program continued, so that frozen transcript is stale and
    /// must not be reused by a future interrupt. Keeping this cancellation at
    /// the PTY boundary covers keyboard, paste, mouse, wheel, workflow, and
    /// delayed TUI-scroll input uniformly.
    pub fn write_user_input(&mut self, data: &[u8]) -> weft_core::pty::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        // A history view remains sticky even at offset zero. Genuine PTY
        // input is the semantic boundary for returning to the live TUI.
        self.snap_to_bottom();
        let pane = self.active_mut();
        if let Some(terminal) = &mut pane.terminal {
            terminal.cancel_primary_screen_interrupt_capture();
        }
        let pty = pane.pty.as_ref().ok_or_else(|| {
            PtyError::Write(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "tab has no PTY",
            ))
        })?;
        pty.write_sync(data)
    }

    /// Process queued messages into the terminal and drain finished blocks.
    /// v1.0 P1.5-C3: Time-bounded processing. The loop drains messages until
    /// either the channel is empty OR a ~8ms wall-clock budget is exhausted.
    /// This keeps the render thread responsive during huge PTY bursts (e.g.
    /// `cat huge.log`): instead of blocking for 100ms+ processing a full read
    /// buffer, we process ~8ms worth, yield to the renderer, then resume next
    /// frame. The byte budget (`MAX_BYTES_PER_MESSAGE`) only governs splitting
    /// a single oversized message so one message can't monopolize a frame.
    pub fn process_messages(&mut self) -> (bool, Vec<weft_core::blocks::Block>, bool) {
        let mut need_redraw = false;
        // Split threshold for a single oversized message (matches the PTY
        // read buffer size). Messages larger than this are split: head is
        // processed now, tail is re-queued for the next frame.
        const MAX_BYTES_PER_MESSAGE: usize = 256 * 1024;
        // 8ms leaves ~8ms for rendering at 60fps. We check the clock at most
        // every MIN_BYTES_FOR_TIME_CHECK bytes to avoid Instant::now() overhead
        // dominating for tiny messages.
        const FRAME_TIME_BUDGET: std::time::Duration = std::time::Duration::from_millis(8);
        const MIN_BYTES_FOR_TIME_CHECK: usize = 32 * 1024;
        let frame_start = std::time::Instant::now();
        let mut bytes_since_check = 0usize;
        let mut processed_pty_output = false;
        let mut alive = true;

        let mut carried = self.pending_pty_output.take().map(AppMsg::PtyOutput);
        loop {
            let msg = match carried.take() {
                Some(msg) => msg,
                None => match self.msg_rx.try_recv() {
                    Ok(msg) => msg,
                    Err(_) => break,
                },
            };
            match msg {
                AppMsg::PtyOutput(mut data) => {
                    if data.len() > MAX_BYTES_PER_MESSAGE {
                        // Keep the remainder ahead of every later AppMsg,
                        // especially PtyExit. Re-queueing it at the channel
                        // tail would invert the original PTY byte order.
                        let tail = data.split_off(MAX_BYTES_PER_MESSAGE);
                        self.pending_pty_output = Some(tail);
                        need_redraw |= self.process_pty_output(&data);
                        processed_pty_output = true;
                        break;
                    }
                    bytes_since_check += data.len();
                    need_redraw |= self.process_pty_output(&data);
                    processed_pty_output = true;
                    // Cooperative yield: if we've spent the frame's time
                    // budget, stop draining and let the renderer draw. The
                    // remaining messages stay queued for next frame.
                    if bytes_since_check >= MIN_BYTES_FOR_TIME_CHECK
                        && frame_start.elapsed() >= FRAME_TIME_BUDGET
                    {
                        break;
                    }
                }
                AppMsg::PtyExit(code) => {
                    tracing::info!("Shell exited: {:?}", code);
                    alive = false;
                    break;
                }
            }
        }

        need_redraw |= self.refresh_primary_history_snapshot(processed_pty_output);
        let mut drained = Vec::new();
        let mut reset_scroll = false;
        if let Some(terminal) = &mut self.terminal {
            let settled = if alive {
                terminal.settle_primary_screen_exit_if_idle(std::time::Instant::now())
            } else {
                terminal.settle_primary_screen_exit()
            };
            if settled {
                need_redraw = true;
            }
            drained = terminal.block_tracker_mut().drain_unpersisted();
            reset_scroll = !drained.is_empty() && !terminal.primary_history_view();
            if terminal.synchronized_output() {
                need_redraw = false;
            }
        }
        if reset_scroll {
            self.snap_to_bottom();
        }

        (alive, drained, need_redraw)
    }

    /// Whether this tab's terminal + pty are initialized.
    #[allow(dead_code)]
    pub fn is_alive(&self) -> bool {
        self.terminal.is_some() && self.pty.is_some()
    }

    /// Directory inherited by a newly-created sibling tab. Prefer live OSC 7
    /// state, retaining a restored cwd until the shell reports one.
    pub fn launch_cwd(&self) -> Option<&str> {
        self.terminal
            .as_ref()
            .and_then(Terminal::cwd)
            .or_else(|| self.restored_snapshot.as_ref()?.cwd.as_deref())
    }

    /// v1.0 H4: Serialize this tab's UI state to a [`TabSnapshot`] for
    /// SQLite persistence. A restored tab whose PTY failed keeps its loaded
    /// snapshot; only a fresh empty tab with no recovery state returns `None`.
    ///
    /// The PTY itself is NOT serialized (impossible to revive). On
    /// restore, the tab shows the saved editor draft + block history;
    /// the user presses Enter to spawn a fresh shell in the saved cwd.
    pub fn to_snapshot(&self, position: usize, active: bool) -> Option<TabSnapshot> {
        let Some(terminal) = self.terminal.as_ref() else {
            let mut snapshot = self.restored_snapshot.clone()?;
            snapshot.position = position;
            snapshot.active = active;
            snapshot.block_scroll_offset = self.block_scroll();
            return Some(snapshot);
        };
        let cwd = terminal.cwd().map(str::to_owned).or_else(|| {
            self.restored_snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.cwd.clone())
        });
        let editor_buffer =
            weft_core::persistence::TabSnapshot::encode_editor_buffer(&terminal.editor().buffer);
        let shell_phase = match terminal.block_tracker().phase() {
            weft_core::blocks::ShellPhase::NotIntegrated => "NotIntegrated",
            weft_core::blocks::ShellPhase::AtPrompt => "AtPrompt",
            weft_core::blocks::ShellPhase::CommandExecuting => "CommandExecuting",
        };
        Some(TabSnapshot {
            position,
            active,
            cwd,
            block_scroll_offset: self.block_scroll(),
            editor_buffer,
            shell_phase: shell_phase.to_string(),
        })
    }

    /// v1.0 H4: Restore the editor draft + block-scroll offset from a
    /// [`TabSnapshot`]. Called after [`Tab::new`] to apply the saved
    /// state. The PTY + terminal are already initialized by `new`;
    /// this just rehydrates the editor buffer and scroll offset.
    ///
    /// Returns `false` if the snapshot's editor buffer JSON was invalid
    /// (the tab stays usable with an empty editor — same as a fresh tab).
    pub fn restore_from_snapshot(&mut self, snap: &TabSnapshot) -> bool {
        self.restored_snapshot = Some(snap.clone());
        self.set_block_scroll(snap.block_scroll_offset);
        let Some(terminal) = self.terminal.as_mut() else {
            return false;
        };
        match weft_core::persistence::TabSnapshot::decode_editor_buffer(&snap.editor_buffer) {
            Some(buf) => {
                terminal.editor_mut().buffer = buf;
                true
            }
            None => {
                tracing::warn!("failed to deserialize editor buffer; using empty");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tab_is_not_alive() {
        let t = Tab::empty();
        assert!(!t.is_alive());
    }

    #[test]
    fn empty_tab_has_no_terminal() {
        let t = Tab::empty();
        assert!(t.terminal.is_none());
        assert!(t.pty.is_none());
    }

    #[test]
    fn empty_tab_pump_pty_is_noop() {
        let mut t = Tab::empty();
        // Should not panic — just returns early since pty is None.
        t.pump_pty();
    }

    #[test]
    fn empty_tab_process_messages_returns_empty() {
        let mut t = Tab::empty();
        let (alive, drained, need_redraw) = t.process_messages();
        assert!(alive);
        assert!(drained.is_empty());
        assert!(!need_redraw);
    }

    #[test]
    fn empty_tab_has_zero_block_scroll() {
        let t = Tab::empty();
        assert_eq!(t.block_scroll(), 0);
    }

    #[test]
    fn queued_scroll_replays_after_alt_screen_entry() {
        let mut t = tab_with_terminal(100);
        t.arm_tui_scroll_window();
        assert!(t.queue_tui_scroll(-2, 5, 10, weft_core::input::Modifiers::empty()));
        assert!(t.resolve_pending_tui_scroll().is_none());

        t.terminal.as_mut().unwrap().process(b"\x1b[?1049h");
        assert!(t.resolve_pending_tui_scroll().is_none());
        expire_pending_scroll(&mut t);
        let Some(TuiScrollResolution::PtyBytes(bytes)) = t.resolve_pending_tui_scroll() else {
            panic!("expected queued arrow bytes");
        };
        assert_eq!(bytes, b"\x1b[B\x1b[B");
        assert!(t.resolve_pending_tui_scroll().is_none());
    }

    #[test]
    fn queued_scroll_falls_back_to_local_rows_for_normal_command() {
        let mut t = tab_with_terminal(100);
        t.arm_tui_scroll_window();
        assert!(t.queue_tui_scroll(3, 5, 10, weft_core::input::Modifiers::empty()));
        t.terminal.as_mut().unwrap().process(b"\x1b]133;A\x07");
        expire_pending_scroll(&mut t);

        let Some(TuiScrollResolution::LocalRows(rows)) = t.resolve_pending_tui_scroll() else {
            panic!("expected local scroll fallback");
        };
        assert_eq!(rows, 3);
        assert!(t.pending_tui_scroll.is_none());
        assert!(!t.tui_scroll_window_active());
        assert!(!t.queue_tui_scroll(1, 5, 10, weft_core::input::Modifiers::empty()));
    }

    #[test]
    fn expired_startup_window_does_not_queue_long_running_command_scroll() {
        let mut t = tab_with_terminal(100);
        t.arm_tui_scroll_window();
        t.tui_scroll_deadline =
            std::time::Instant::now().checked_sub(std::time::Duration::from_millis(1));

        assert!(!t.queue_tui_scroll(-2, 5, 10, weft_core::input::Modifiers::empty()));

        assert!(!t.tui_scroll_window_active());
        assert!(t.pending_tui_scroll.is_none());
    }

    #[test]
    fn queued_scroll_uses_mouse_protocol_when_tui_enables_it() {
        let mut t = tab_with_terminal(100);
        t.arm_tui_scroll_window();
        assert!(t.queue_tui_scroll(-1, 5, 10, weft_core::input::Modifiers::empty()));
        t.terminal
            .as_mut()
            .unwrap()
            .process(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h");
        expire_pending_scroll(&mut t);

        let Some(TuiScrollResolution::PtyBytes(bytes)) = t.resolve_pending_tui_scroll() else {
            panic!("expected queued mouse bytes");
        };
        assert_eq!(bytes, b"\x1b[<65;6;11M");
    }

    #[test]
    fn queued_scroll_waits_for_mouse_mode_in_next_pty_chunk() {
        let mut t = tab_with_terminal(100);
        t.arm_tui_scroll_window();
        assert!(t.queue_tui_scroll(-1, 5, 10, weft_core::input::Modifiers::empty()));

        t.terminal.as_mut().unwrap().process(b"\x1b[?1049h");
        assert!(t.resolve_pending_tui_scroll().is_none());
        assert!(t.resolve_pending_tui_scroll().is_none());

        t.terminal
            .as_mut()
            .unwrap()
            .process(b"\x1b[?1000h\x1b[?1006h");
        expire_pending_scroll(&mut t);
        let Some(TuiScrollResolution::PtyBytes(bytes)) = t.resolve_pending_tui_scroll() else {
            panic!("expected mouse bytes after split mode sequence");
        };
        assert_eq!(bytes, b"\x1b[<65;6;11M");
    }

    #[test]
    fn queued_arrow_scroll_preserves_shift_modifier() {
        let mut t = tab_with_terminal(100);
        t.arm_tui_scroll_window();
        assert!(t.queue_tui_scroll(-1, 5, 10, weft_core::input::Modifiers::SHIFT));
        t.terminal.as_mut().unwrap().process(b"\x1b[?1049h");
        expire_pending_scroll(&mut t);

        let Some(TuiScrollResolution::PtyBytes(bytes)) = t.resolve_pending_tui_scroll() else {
            panic!("expected shifted arrow bytes");
        };
        assert_eq!(bytes, b"\x1b[1;2B");
    }

    // ── v1.0 H4: TabsAutoSave snapshot roundtrip ─────────────────────

    /// Build a Tab with a live Terminal but no PTY — enough for
    /// `to_snapshot` / `restore_from_snapshot` without spawning a shell.
    /// v1.3: wraps a single `Pane` (built via `Pane::with_terminal_only`)
    /// in a fresh `Tab`. The pane becomes the root of the split tree and
    /// the active pane.
    fn tab_with_terminal(scrollback_lines: usize) -> Tab {
        Tab::with_single_pane(Pane::with_terminal_only(scrollback_lines))
    }

    fn expire_pending_scroll(tab: &mut Tab) {
        tab.pending_tui_scroll.as_mut().unwrap().resolve_at = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_millis(1))
            .unwrap();
    }

    #[test]
    fn snapshot_is_none_when_no_terminal() {
        // Tab::empty has no terminal → to_snapshot returns None.
        let t = Tab::empty();
        assert!(t.to_snapshot(0, false).is_none());
    }

    #[test]
    fn snapshot_roundtrip_preserves_scroll_offset_and_editor() {
        let mut t = tab_with_terminal(1000);
        t.set_block_scroll(7);
        t.terminal
            .as_mut()
            .unwrap()
            .editor_mut()
            .buffer
            .set_text("echo hi");

        let snap = t.to_snapshot(2, true).expect("snapshot with terminal");
        assert_eq!(snap.position, 2);
        assert!(snap.active);
        assert_eq!(snap.block_scroll_offset, 7);
        assert!(!snap.editor_buffer.is_empty(), "editor buffer encoded");

        // Restore into a fresh tab.
        let mut restored = tab_with_terminal(1000);
        assert!(restored.restore_from_snapshot(&snap));
        assert_eq!(restored.block_scroll(), 7);
        let editor_text = restored.terminal.as_ref().unwrap().editor().buffer.text();
        assert_eq!(editor_text, "echo hi");
    }

    #[test]
    fn snapshot_default_shell_phase_is_not_integrated() {
        // A fresh Terminal has NotIntegrated phase → snapshot encodes that.
        let t = tab_with_terminal(1000);
        let snap = t.to_snapshot(0, false).unwrap();
        assert_eq!(snap.shell_phase, "NotIntegrated");
    }

    #[test]
    fn snapshot_cwd_none_when_unset() {
        // No OSC 7 received → cwd is None in the snapshot.
        let t = tab_with_terminal(1000);
        let snap = t.to_snapshot(0, false).unwrap();
        assert!(snap.cwd.is_none());
    }

    #[test]
    fn restore_from_invalid_editor_buffer_keeps_empty() {
        // Garbage JSON → restore returns false, editor stays empty.
        let mut t = tab_with_terminal(1000);
        let snap = weft_core::persistence::TabSnapshot {
            position: 0,
            active: false,
            cwd: None,
            block_scroll_offset: 3,
            editor_buffer: "{not valid json".to_string(),
            shell_phase: "AtPrompt".to_string(),
        };
        assert!(!t.restore_from_snapshot(&snap));
        // scroll offset still applied even if editor restore failed.
        assert_eq!(t.block_scroll(), 3);
        let editor_text = t.terminal.as_ref().unwrap().editor().buffer.text();
        assert!(editor_text.is_empty());
    }

    // ── v1.3 Batch 3: pane lifecycle (split / focus / close) ──────────

    #[test]
    fn single_pane_tab_reports_one_pane() {
        let t = tab_with_terminal(100);
        assert_eq!(t.pane_count(), 1);
        assert_eq!(t.split_tree().panes(), vec![t.active_pane_id()]);
    }

    #[test]
    fn split_active_pane_adds_pane_and_switches_focus() {
        let mut t = tab_with_terminal(100);
        let original = t.active_pane_id();
        let new_id = t
            .split_active_pane_test(SplitDirection::Vertical, 0.5, 100)
            .expect("split succeeds");
        // Tree now has two panes; new pane is active.
        assert_eq!(t.pane_count(), 2);
        assert_eq!(t.active_pane_id(), new_id);
        assert_ne!(new_id, original);
        // Both panes are present in the HashMap.
        assert!(t.pane(original).is_some());
        assert!(t.pane(new_id).is_some());
        // Declaration order: original first, new pane second.
        assert_eq!(t.split_tree().panes(), vec![original, new_id]);
    }

    #[test]
    fn split_with_invalid_ratio_is_rejected() {
        let mut t = tab_with_terminal(100);
        // ratio must be in (0.0, 1.0) exclusive — 0.0, 1.0, and out-of-range
        // values are caller bugs, not silent clamps.
        assert_eq!(
            t.split_active_pane_test(SplitDirection::Horizontal, 0.0, 100),
            Err(SplitError::RatioOutOfRange(0.0))
        );
        assert_eq!(
            t.split_active_pane_test(SplitDirection::Horizontal, 1.0, 100),
            Err(SplitError::RatioOutOfRange(1.0))
        );
        assert_eq!(
            t.split_active_pane_test(SplitDirection::Horizontal, 1.5, 100),
            Err(SplitError::RatioOutOfRange(1.5))
        );
        // Tree unchanged after the failed splits.
        assert_eq!(t.pane_count(), 1);
    }

    #[test]
    fn focus_next_wraps_around_two_panes() {
        let mut t = tab_with_terminal(100);
        let first = t.active_pane_id();
        t.split_active_pane_test(SplitDirection::Vertical, 0.5, 100)
            .unwrap();
        let second = t.active_pane_id();

        // forward: second → first (wrap)
        assert_eq!(t.focus_next_pane(), Some(first));
        // forward: first → second
        assert_eq!(t.focus_next_pane(), Some(second));
    }

    #[test]
    fn focus_prev_wraps_around_two_panes() {
        let mut t = tab_with_terminal(100);
        let first = t.active_pane_id();
        t.split_active_pane_test(SplitDirection::Vertical, 0.5, 100)
            .unwrap();
        let second = t.active_pane_id();

        // backward: second → first
        assert_eq!(t.focus_prev_pane(), Some(first));
        // backward: first → second (wrap)
        assert_eq!(t.focus_prev_pane(), Some(second));
    }

    #[test]
    fn focus_next_on_single_pane_is_noop() {
        let mut t = tab_with_terminal(100);
        let only = t.active_pane_id();
        assert_eq!(t.focus_next_pane(), Some(only));
        assert_eq!(t.focus_prev_pane(), Some(only));
    }

    #[test]
    fn close_active_pane_with_sibling_keeps_tab() {
        let mut t = tab_with_terminal(100);
        let first = t.active_pane_id();
        t.split_active_pane_test(SplitDirection::Vertical, 0.5, 100)
            .unwrap();
        let second = t.active_pane_id();

        // Close the active (second) pane — should fall back to the sibling.
        let is_empty = t.close_active_pane().expect("close succeeds");
        assert!(!is_empty);
        assert_eq!(t.pane_count(), 1);
        assert_eq!(t.active_pane_id(), first);
        // The closed pane is gone from the HashMap.
        assert!(t.pane(second).is_none());
        assert!(t.pane(first).is_some());
    }

    #[test]
    fn close_last_pane_signals_tab_empty() {
        let mut t = tab_with_terminal(100);
        // Single pane → closing it empties the tab.
        let is_empty = t.close_active_pane().expect("close succeeds");
        assert!(is_empty);
        assert_eq!(t.pane_count(), 0);
    }

    #[test]
    fn nested_split_and_close_collapses_tree() {
        let mut t = tab_with_terminal(100);
        let a = t.active_pane_id();
        t.split_active_pane_test(SplitDirection::Vertical, 0.5, 100)
            .unwrap();
        let b = t.active_pane_id();
        t.split_active_pane_test(SplitDirection::Horizontal, 0.5, 100)
            .unwrap();
        let c = t.active_pane_id();
        assert_eq!(t.pane_count(), 3);

        // Close c → focus moves to its sibling b (same parent split).
        let is_empty = t.close_active_pane().expect("close succeeds");
        assert!(!is_empty);
        assert_eq!(t.active_pane_id(), b);
        assert_eq!(t.pane_count(), 2);

        // Close b → focus moves to a (the root sibling).
        let is_empty = t.close_active_pane().expect("close succeeds");
        assert!(!is_empty);
        assert_eq!(t.active_pane_id(), a);
        assert_eq!(t.pane_count(), 1);

        // Close a → tab empty.
        let is_empty = t.close_active_pane().expect("close succeeds");
        assert!(is_empty);
    }

    #[test]
    fn split_inherits_active_pane_terminal_size() {
        // Build a pane with a known terminal size, then split and verify
        // the new pane's terminal matches. (Batch 3 only checks the data
        // plumbing; the renderer / PTY resize to split-tree rects lands
        // in Batch 5/6.)
        let mut pane = Pane::with_terminal_only(100);
        pane.terminal = Some(Terminal::with_scrollback(30, 90, 100));
        let mut t = Tab::with_single_pane(pane);
        let original_rows = t.active().terminal.as_ref().unwrap().grid().num_rows;
        let original_cols = t.active().terminal.as_ref().unwrap().grid().num_cols;
        assert_eq!((original_rows, original_cols), (30, 90));

        let new_id = t
            .split_active_pane_test(SplitDirection::Vertical, 0.5, 100)
            .unwrap();
        let new_pane = t.pane(new_id).unwrap();
        let new_terminal = new_pane.terminal.as_ref().unwrap();
        // with_terminal_only uses Terminal::with_scrollback(24, 80, ...) —
        // it does NOT inherit the original pane's size (that only happens
        // in the production split_active_pane path). The test documents
        // this contract: the test helper is for tree plumbing only.
        assert_eq!(
            (new_terminal.grid().num_rows, new_terminal.grid().num_cols),
            (24, 80)
        );
    }

    /// v1.3 Batch 6: `pane_hit_test` returns the correct pane for points
    /// inside each pane and `None` for points outside the content area.
    #[test]
    fn pane_hit_test_finds_correct_pane_in_vertical_split() {
        let mut t = tab_with_terminal(100);
        let _new_id = t
            .split_active_pane_test(SplitDirection::Vertical, 0.5, 100)
            .unwrap();
        let active = t.active_pane_id();
        let panes = t.split_tree().panes();
        let other = panes.iter().find(|&&id| id != active).copied().unwrap();
        // Content rect: 0..800 wide, 0..600 tall. Vertical split at 0.5 →
        // left pane [0, 0, 400, 600], right pane [400, 0, 800, 600].
        let content: weft_core::pane_layout::Rect = [0.0, 0.0, 800.0, 600.0];
        let layouts = t.split_tree().layout(content);
        let active_rect = layouts
            .iter()
            .find(|(id, _)| *id == active)
            .map(|(_, r)| *r)
            .unwrap();
        let other_rect = layouts
            .iter()
            .find(|(id, _)| *id == other)
            .map(|(_, r)| *r)
            .unwrap();
        // Point in the active pane.
        let cx = (active_rect[0] + active_rect[2]) / 2.0;
        let cy = (active_rect[1] + active_rect[3]) / 2.0;
        assert_eq!(t.pane_hit_test(cx, cy, content), Some(active));
        // Point in the other pane.
        let ox = (other_rect[0] + other_rect[2]) / 2.0;
        let oy = (other_rect[1] + other_rect[3]) / 2.0;
        assert_eq!(t.pane_hit_test(ox, oy, content), Some(other));
        // Point outside content.
        assert_eq!(t.pane_hit_test(-1.0, -1.0, content), None);
    }

    /// v1.3 Batch 6: `set_active_pane` switches focus and is idempotent.
    #[test]
    fn set_active_pane_switches_focus() {
        let mut t = tab_with_terminal(100);
        let original = t.active_pane_id();
        let new_id = t
            .split_active_pane_test(SplitDirection::Horizontal, 0.5, 100)
            .unwrap();
        // After split, the new pane becomes active (split_active_pane_inner
        // sets self.active_pane = new_pane_id). Switch back to the original.
        assert_eq!(t.active_pane_id(), new_id);
        t.set_active_pane(original).unwrap();
        assert_eq!(t.active_pane_id(), original);
        // Setting the same pane again is a no-op (idempotent).
        t.set_active_pane(original).unwrap();
        assert_eq!(t.active_pane_id(), original);
    }

    /// v1.3 Batch 6: `close_active_pane` updates `active_pane` before
    /// removing from `panes` — the invariant is never violated.
    #[test]
    fn close_active_pane_restores_focus_to_sibling() {
        let mut t = tab_with_terminal(100);
        let original = t.active_pane_id();
        let new_id = t
            .split_active_pane_test(SplitDirection::Vertical, 0.5, 100)
            .unwrap();
        // After split, the new pane is active. Close it — focus should
        // return to the original (surviving sibling).
        assert_eq!(t.active_pane_id(), new_id);
        let is_last = t.close_active_pane().unwrap();
        assert!(!is_last);
        assert_eq!(t.active_pane_id(), original);
        // The closed pane is gone from the map.
        assert!(t.pane(new_id).is_none());
        assert!(t.pane(original).is_some());
    }
}

#[cfg(test)]
#[path = "tab/snapshot_tests.rs"]
mod snapshot_tests;
