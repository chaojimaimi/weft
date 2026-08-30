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
use crate::blocks::ShellPhase;
use crate::input::MouseProtocol;
use crate::vt::TuiRenderMode;
use std::time::{Duration, Instant};

mod freeze;
mod ownership;
mod tail;

mod capture;
mod interrupt;
mod scroll;
mod settle;
mod snapshot;

pub(in crate::vt) use interrupt::PrimaryScreenInterruptCapture;
pub(in crate::vt) use ownership::PrimaryScreenOwnership;
pub(in crate::vt) use settle::PendingPrimaryScreenExit;
pub use settle::PRIMARY_SCREEN_EXIT_SETTLE_DELAY;

pub const PRIMARY_HISTORY_SNAPSHOT_INTERVAL: Duration = Duration::from_millis(50);

// v1.11.8 (PLAN_v1118 M-C2): cols policy moved to `cols` (pure functions,
// zero Terminal coupling). Re-export at the same paths the module root used
// to provide them — `vt/mod.rs`'s two re-export lines stay untouched.
mod cols;

pub(crate) use cols::sustained_alt_cols_kind;
pub use cols::TuiColsKind;
// v1.11.8 (M-C2): consumed only by vt/mod.rs's `#[cfg(test)]` re-export.
#[cfg(test)]
pub(crate) use cols::SUSTAINED_ALT_COLS_MS;

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
        // v1.10.31 (FIX_BREW_PROGRESS_TUI_MISCLASSIFY): DEC 2026 synchronized
        // output (`?2026h`) no longer unconditionally counts as TUI evidence.
        // Brew's progress frames emit `?2026h/?2026l` on every repaint but only
        // use EL + CHA(column 1), which is not real TUI cursor addressing.
        // Now we reset the window flag and only count cursor_ops when non-trivial
        // addressing (CUP `H`, CUU/CUD `A`/`B`, VPA `d`, CHA with parameter >1)
        // is seen inside the window. This was validated by a controlled experiment:
        // the same brew byte stream with DEC 2026 removed produces correct
        // single-line block output, while with 2026 (old behavior) it produced 7
        // duplicate lines. pi/openclaw compatibility is preserved because their
        // repaint frames include genuine cursor addressing.
        self.capabilities.synchronized_frame_addressing_seen = false;
    }

    pub(super) fn finish_primary_screen_synchronized_frame(&mut self) {
        if self.synchronized_output_started.is_some() {
            let complete_primary_frame = self.primary_screen_app_active()
                && self.synchronized_frame_cleared_rows >= self.grid.num_rows;
            if complete_primary_frame {
                // The viewport already holds the NEW frame (every row was
                // cleared + repainted inside the sync window) — preserve only
                // the scrollback rows the clear is about to destroy, so the
                // surviving frame is not duplicated in the block history.
                self.discard_superseded_primary_screen_frame(false);
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
                // CSI 2J: the viewport is blanked right after this call, so
                // the whole superseded document (scrollback + viewport) is
                // preserved before both are destroyed.
                self.discard_superseded_primary_screen_frame(true);
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

    pub(super) fn note_primary_screen_cursor_addressing(&mut self, relative: bool) {
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
            // v1.10.12: relative moves mark a sparse repainter — it paints
            // incrementally, so row-boundary hiding must stay off.
            // v1.11.8 (PLAN_v1118 M-C1): the param used to arrive as
            // `absolute` and write the (now deleted) zero-read
            // `primary_screen_absolute_addressing` flag; its only remaining
            // consumer is `!absolute` below, so the negation moved to the
            // single call site (perform.rs) and the param is `relative`.
            self.capabilities.primary_screen_relative_addressing_seen |= relative;
            if self.primary_screen_app_active() {
                self.begin_primary_screen_output_capture();
            }
        }
    }

    /// v1.10.31 (FIX_BREW_PROGRESS_TUI_MISCLASSIFY): check if non-trivial cursor
    /// addressing has been seen in the current DEC 2026 synchronized output window.
    /// If not, mark it as seen and count it toward TUI detection. This should be
    /// called from cursor addressing operations (CUP, CUU, CUD, VPA, CHA with param >1).
    /// Returns true if the addressing was just counted (first time in this window).
    /// v1.10.31 invariant (FIX_BREW_PROGRESS_TUI_MISCLASSIFY): the removed
    /// per-`?2026h` +1 is compensated by the window-bonus double count on the
    /// first addressing op inside the frame — an old frame counted `1 + k`
    /// (2026 begin + k addressing ops), a new frame counts `k + 1` (k via the
    /// generic hook + this window bonus). Do not "simplify" either side alone:
    /// detection timing for pi/omp depends on this equivalence.
    pub(super) fn note_synchronized_frame_addressing(&mut self) {
        // Only count if we're in a synchronized window, not in alt screen,
        // and haven't seen addressing yet in this window.
        if self.synchronized_output_started.is_some()
            && !self.capabilities.alt_active
            && !self.capabilities.synchronized_frame_addressing_seen
        {
            self.capabilities.synchronized_frame_addressing_seen = true;
            // Count this as a cursor operation for TUI detection
            self.capabilities.primary_screen_cursor_ops = self
                .capabilities
                .primary_screen_cursor_ops
                .saturating_add(1);
            // Start capture if threshold crossed
            if self.primary_screen_app_active() {
                self.begin_primary_screen_output_capture();
            }
        }
    }

    /// v1.10.12-fix: a primary-screen TUI (omp/pi/openclaw) owns the screen
    /// for the whole command — while it runs, the live grid IS its interface
    /// (full-screen repaint). The BlockView (document list) must NOT replace
    /// the live UI, otherwise the TUI renders as a truncated document block
    /// (half-empty screen) and scrolling is bounded by the captured document
    /// length instead of the screen + scrollback. Only explicit history
    /// browsing (`primary_history_view`) switches to the BlockView snapshot.
    /// `screen_owned_tui_active()` is stable across transient `cursor_ops`
    /// resets (nested 133 markers), so no render-mode lock is needed.
    ///
    /// v1.11.7 (PLAN_v1117_SHADOW_BLOCK_VIEW §三 M1.3, D-a/D-d): the tiered
    /// formula. `screen_document_start` still drives the (unchanged) capture
    /// data plane; this predicate ONLY decides the view policy per tier:
    /// - `classic`: original formula verbatim (v1.11.6 rollback switch).
    /// - `all`: `bootstrap_ready && (alt_screen_history_peek || !alt_active)`
    ///   — the three false items (`screen_owned_tui_active`,
    ///   `primary_screen_app_active`, `primary_screen_exit_pending`) are
    ///   dropped together (P0-1: keeping `exit_pending` would void the settle
    ///   flicker fix; it only appears in screen-owned sessions, so dropping
    ///   it cannot affect ordinary commands).
    /// - `noninteractive`: the `all` condition, unless interactive stdin was
    ///   seen or a mouse protocol is negotiated (P1-3) — then back to the
    ///   classic formula (which keeps its `exit_pending` item; an exempted
    ///   interactive command's 200ms settle flash is today's behavior,
    ///   accepted per D-c).
    pub fn show_block_view(&self) -> bool {
        match self.tui_render_mode {
            TuiRenderMode::Classic => self.show_block_view_classic(),
            TuiRenderMode::All => self.show_block_view_all(),
            TuiRenderMode::Noninteractive => {
                if self.capabilities.interactive_stdin_seen
                    || self.capabilities.mouse_protocol != MouseProtocol::Off
                {
                    // Interactive command (or mouse-reporting owner): the
                    // original heuristics own the viewport again.
                    self.show_block_view_classic()
                } else {
                    self.show_block_view_all()
                }
            }
        }
    }

    /// v1.11.6 formula, verbatim (the `classic` tier and the interactive
    /// fallback of the `noninteractive` tier).
    fn show_block_view_classic(&self) -> bool {
        self.block_tracker.bootstrap_ready()
            && (self.capabilities.alt_screen_history_peek
                || (!self.capabilities.alt_active
                    && (self.capabilities.primary_history_view
                        || (!self.primary_screen_exit_pending()
                            && !self.screen_owned_tui_active()
                            && !self.primary_screen_app_active()))))
    }

    /// v1.11.7: screen-owned sessions never leave the BlockView (P0-1 — the
    /// `primary_screen_exit_pending` item is deliberately absent so the
    /// 200ms settle window keeps the block visible; D-e).
    fn show_block_view_all(&self) -> bool {
        self.block_tracker.bootstrap_ready()
            && (self.capabilities.alt_screen_history_peek || !self.capabilities.alt_active)
    }

    /// v1.10.12-fix: whether a primary-screen TUI currently owns the screen
    /// (document capture started and not yet settled). While true, the live
    /// grid renders the TUI's own full-screen output.
    ///
    /// classic-tier only (v1.11.8 M-C3): the sole call site is the classic
    /// `show_block_view` formula (:627) — the all/noninteractive tiers drop
    /// this item entirely. `screen_owned_tui_active` itself stays: the
    /// classic tier is retained until v1.12.
    fn screen_owned_tui_active(&self) -> bool {
        self.block_tracker.screen_document_start().is_some()
    }

    pub fn primary_screen_exit_pending(&self) -> bool {
        self.capabilities.primary_screen_exit.is_some()
    }

    /// v1.10.25 Batch 2 (FIX_TUI_INPUT_WIDTH_ALIGNMENT): renamed from
    /// `wants_full_width_cols` and re-mapped — primary-screen TUIs (which
    /// returned Full since v1.10.19) now return [`TuiColsKind::Content`].
    /// omp draws its UI at exactly the PTY cols it receives; Full made its
    /// input-line border column (`|]`) land one cell past weft's renderable
    /// area (the right ~1.5 cols were clipped) and later fold into a
    /// continuation chunk at the settle transition. Content keeps the PTY
    /// target, the grid render width and the block wrap width identical.
    ///
    /// Each phase maps to one constant target, so transient `?1049h/l`
    /// feedback replays the same two constants instead of ratcheting into
    /// new values.
    ///
    /// v1.10.30 (FIX_LESS_ALT_COLS_JUMP): isolated vs burst re-entry —
    /// alt entries with no recent exit (or >= 400ms since last exit) are
    /// isolated and immediately map to Full, fixing less/vim startup
    /// jump. Burst re-entries (< 400ms) apply the 250ms sustained residency
    /// threshold to break the omp feedback loop. See
    /// docs/FIX_LESS_ALT_COLS_JUMP.md.
    ///
    /// v1.10.28 (FIX_TRANSIENT_ALT_COLS_FLIP): sustained-alt hysteresis —
    /// alt only maps to Full after [`SUSTAINED_ALT_COLS_MS`] (250ms) of
    /// *continuous* residency ([`sustained_alt_cols_kind`]); an unknown
    /// entry time (`None`) conservatively keeps the old Full mapping so an
    /// unknown state can never shrink a real alt TUI. Reason for the
    /// threshold: omp 17.3.7 wraps its SIGWINCH repaint in a 1049h → full
    /// redraw (~129ms) → 1049l excursion, so the old constant alt→Full
    /// mapping flipped the cols target (91↔94) every round, emitting a new
    /// ioctl → SIGWINCH → feedback loop (~330ms/circle — continuous flashing
    /// and side-to-side jitter). Transient in-and-out (<250ms) never flips
    /// the classification, breaking the loop at the source; a real alt TUI
    /// (vim/less) stays resident for seconds, crosses the threshold, and
    /// still gets Full (entry Full-ization delayed ≤250ms + one repaint —
    /// imperceptible). See docs/FIX_TRANSIENT_ALT_COLS_FLIP.md.
    ///
    /// The mapping still never ratchets into a third value: sustained alt →
    /// Full, everything else → Content. The ioctl dedup (pane.rs
    /// `should_send_winsize_ioctl`) and the 150ms debounce (tab/resize.rs)
    /// plus the v1.10.25 Batch 3 app-layer burst hysteresis
    /// (`Tab::burst_locked_cols`) remain as defense in depth; see
    /// FIX_SCROLL_SHIFT_AND_RESIZE_STORM.md 演进注记.
    pub fn tui_cols_kind(&self) -> TuiColsKind {
        let alt_active_for = self
            .capabilities
            .alt_active_since
            .map(|since| Instant::now().saturating_duration_since(since));
        let since_last_exit = self
            .capabilities
            .alt_last_exit
            .map(|exit| Instant::now().saturating_duration_since(exit));
        sustained_alt_cols_kind(
            self.capabilities.alt_active,
            alt_active_for,
            since_last_exit,
        )
    }

    /// v1.10.19: Whether a primary-screen TUI currently owns the live grid
    /// view. Rendering policy: when true (and not in alt mode), the grid
    /// content origin is inset by the BlockView gutter so scrolling up into
    /// `primary_history_view` (which switches to BlockView) keeps every
    /// column at the same physical x — without the inset, grid content
    /// renders ~1.5 cols left of BlockView content and the transcript
    /// visibly shifts right on the transition.
    pub fn primary_screen_owns_live_view(&self) -> bool {
        self.primary_screen_app_active() || self.primary_screen_exit_pending()
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
            // v1.10.12-fix: only pure full-viewport CUP TUIs (claude code)
            // hide the shell rows above their document boundary. A sparse
            // repainter (omp/pi) paints incrementally — hiding rows before
            // `document_start` would clip its output.
            || self.capabilities.primary_screen_relative_addressing_seen
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
            // v1.10.12-fix: sparse repainters never apply the ownership mask
            // (their partial row-touch pattern would hide the rest of the UI).
            || self.capabilities.primary_screen_relative_addressing_seen
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
}

fn viewport_row_for_document_start(start: u64, viewport_origin: u64, rows: usize) -> usize {
    start.saturating_sub(viewport_origin).min(rows as u64) as usize
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
