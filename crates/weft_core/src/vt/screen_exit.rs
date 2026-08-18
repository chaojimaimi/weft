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
mod tail;

use tail::{merge_primary_screen_interrupt_tail, space_primary_screen_exit_tail};

pub(in crate::vt) use ownership::PrimaryScreenOwnership;

pub const PRIMARY_SCREEN_EXIT_SETTLE_DELAY: Duration = Duration::from_millis(200);
pub const PRIMARY_HISTORY_SNAPSHOT_INTERVAL: Duration = Duration::from_millis(50);

/// v1.10.28 (FIX_TRANSIENT_ALT_COLS_FLIP): minimum *continuous* alternate-
/// screen residency before `tui_cols_kind()` reports [`TuiColsKind::Full`].
/// omp 17.3.7 wraps its SIGWINCH repaint in a 1049h → full redraw (~129ms) →
/// 1049l excursion; a real alt TUI (vim/less) stays resident for seconds.
/// 250ms ≈ 2x the omp residency and far below any real TUI, so transient
/// excursions never flip the cols target and the ioctl → SIGWINCH → 1049
/// feedback loop is broken at the source. See docs/FIX_TRANSIENT_ALT_COLS_FLIP.md.
pub(crate) const SUSTAINED_ALT_COLS_MS: u64 = 250;

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

/// v1.10.25 Batch 2 (FIX_TUI_INPUT_WIDTH_ALIGNMENT): PTY cols policy for
/// the active pane. The value drives
/// `Tab::active_pane_dimensions_for_rect` / `Tab::resize_all_panes_for_rect`
/// (weft_app), which choose between
/// [`weft_app::layout::terminal_full_cols`] and
/// [`weft_app::layout::terminal_content_cols`].
///
/// Anti-oscillation history (v1.10.19 → v1.10.25; deduped in v1.10.26 Batch
/// D — the authoritative anti-cycle design lives in
/// `weft_app::tab::resize::Tab::burst_locked_cols` / `ALT_RESCALE_DEBOUNCE`):
/// the mapping here is a pure constant function of the alt flag (a toggle
/// burst can never ratchet the target into a third value), and the
/// primary↔alt Content/Full alternation is bounded at the app cols mirror
/// sites by the burst hysteresis — locked to Content while a two-flip storm
/// signature is fresh, converging to the live kind once it goes quiet.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TuiColsKind {
    /// Alt-screen TUI (vim/htop/less): paints edge-to-edge with no
    /// BlockView gutter. The grid origin is the pane's left edge, so
    /// every column is renderable.
    Full,
    /// Primary-screen TUI (omp/pi/openclaw), including while its exit is
    /// settling, and plain shell output: the BlockView content width.
    /// The grid renders inset by the gutter
    /// (`weft_app::paint::grid::grid_content_origin_x`), so full-width
    /// cols would overflow the content area by the gutter (~1.5 cols,
    /// right edge clipped — v1.10.19's full-width scheme). Content width
    /// makes three independently-computed widths identical: PTY target
    /// cols == grid render cols == block wrap cols, so the omp input-line
    /// border `|]` never exceeds the renderable area and never folds to a
    /// continuation chunk after settle.
    Content,
}

/// v1.10.28 (FIX_TRANSIENT_ALT_COLS_FLIP): pure decision for the sustained-alt
/// cols hysteresis feeding [`Terminal::tui_cols_kind`].
///
/// Only alt that has been *continuously* resident for at least
/// [`SUSTAINED_ALT_COLS_MS`] (250ms) reports Full; anything shorter — omp
/// 17.3.7's ~129ms 1049h→repaint→1049l excursion — stays Content, so a
/// transient in-and-out can never flip the classification and feed the
/// SIGWINCH feedback loop. A `None` entry time (unknown start) conservatively
/// returns Full: the pre-hysteresis mapping, so an unknown state can never
/// shrink a real alt TUI to content width. See docs/FIX_TRANSIENT_ALT_COLS_FLIP.md.
pub(crate) fn sustained_alt_cols_kind(
    alt_active: bool,
    alt_active_for: Option<Duration>,
) -> TuiColsKind {
    if !alt_active {
        return TuiColsKind::Content;
    }
    match alt_active_for {
        // Unknown start: conservatively keep the old alt→Full mapping.
        None => TuiColsKind::Full,
        // Below the sustained threshold — a transient excursion must not flip.
        Some(elapsed) if elapsed < Duration::from_millis(SUSTAINED_ALT_COLS_MS) => {
            TuiColsKind::Content
        }
        // Continuous residency at/over the threshold — a real alt TUI.
        Some(_) => TuiColsKind::Full,
    }
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
            // v1.10.12: relative moves mark a sparse repainter — it paints
            // incrementally, so row-boundary hiding must stay off.
            self.capabilities.primary_screen_relative_addressing_seen |= !absolute;
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
        // v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL): capture rows pushed out of
        // the viewport into the screen prefix before the ownership transform
        // rotates the viewport mask (a no-op for down-scrolls — pushed is 0).
        self.capture_scrolled_out_screen_rows(origin_before);
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
        // v1.10.23 (FIX_OMP_CONTENT_LOSS): a new screen-owned session starts
        // with a clean preservation history. Nested 133 markers keep
        // `screen_document_start` set, so the accumulated frames survive
        // them; only a real boundary (settle → next command) resets it.
        let starting = self.block_tracker.screen_document_start().is_none();
        if starting {
            self.capabilities.screen_history = crate::vt::capability::ScreenHistory::default();
        }
        self.block_tracker
            .begin_screen_owned_output(self.capabilities.primary_screen_document_candidate);
        // v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL): first ownership — fold the
        // retained owned pre-capture rows into the screen prefix so the
        // composed transcript keeps document order.
        if starting && self.block_tracker.screen_document_start().is_some() {
            self.rebase_screen_prefix_at_capture_start();
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
            // v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL): capture the row pushed
            // out of the viewport into the screen prefix before the ownership
            // transform rotates the viewport mask.
            self.capture_scrolled_out_screen_rows(origin);
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

    /// v1.10.12-fix: a primary-screen TUI (omp/pi/openclaw) owns the screen
    /// for the whole command — while it runs, the live grid IS its interface
    /// (full-screen repaint). The BlockView (document list) must NOT replace
    /// the live UI, otherwise the TUI renders as a truncated document block
    /// (half-empty screen) and scrolling is bounded by the captured document
    /// length instead of the screen + scrollback. Only explicit history
    /// browsing (`primary_history_view`) switches to the BlockView snapshot.
    /// `screen_owned_tui_active()` is stable across transient `cursor_ops`
    /// resets (nested 133 markers), so no render-mode lock is needed.
    pub fn show_block_view(&self) -> bool {
        self.block_tracker.bootstrap_ready()
            && (self.capabilities.alt_screen_history_peek
                || (!self.capabilities.alt_active
                    && (self.capabilities.primary_history_view
                        || (!self.primary_screen_exit_pending()
                            && !self.screen_owned_tui_active()
                            && !self.primary_screen_app_active()))))
    }

    /// v1.10.12-fix: whether a primary-screen TUI currently owns the screen
    /// (document capture started and not yet settled). While true, the live
    /// grid renders the TUI's own full-screen output.
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
        sustained_alt_cols_kind(self.capabilities.alt_active, alt_active_for)
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

    /// v1.10.20: snapshot line index of a live viewport row, computed with
    /// the exact same walk parameters as [`Self::primary_screen_document_snapshot`]
    /// (same document start and ownership masks). `None` when no screen
    /// document is captured or the row is skipped by the snapshot (unowned /
    /// empty). Used by the drag-selection anchor migration — the mapping
    /// must match the snapshot the history BlockView renders, and the
    /// empty-row skip breaks any 1:1 row arithmetic.
    pub fn primary_screen_snapshot_line_for_viewport_row(
        &self,
        viewport_row: usize,
    ) -> Option<usize> {
        let document_start = self.block_tracker.screen_document_start()?;
        let viewport_origin = self.grid.scrollback.position();
        let (scrollback_start, viewport_start) = if document_start <= viewport_origin {
            (self.grid.scrollback.index_since(document_start), 0)
        } else {
            (
                self.grid.scrollback.len(),
                document_start.saturating_sub(viewport_origin) as usize,
            )
        };
        let line = match self
            .capabilities
            .primary_screen_ownership
            .viewport
            .as_deref()
        {
            Some(owned) => self.grid.snapshot_line_index_for_viewport_row(
                viewport_row,
                scrollback_start,
                viewport_start,
                Some(&self.capabilities.primary_screen_ownership.scrollback),
                Some(owned),
            ),
            None => self.grid.snapshot_line_index_for_viewport_row(
                viewport_row,
                scrollback_start,
                viewport_start,
                None,
                None,
            ),
        };
        // v1.10.23: the rendered block prepends the preserved-frame history,
        // so the anchor's rendered line shifts by that many lines.
        // v1.10.25: the scroll-out prefix shifts it too (three-part compose).
        line.map(|line| line + self.screen_history_lines() + self.screen_prefix_lines())
    }

    /// v1.10.12: alt-screen history peek — true while the user is browsing the
    /// terminal's history BlockView over an alt-screen TUI (omp/less/man).
    pub fn is_alt_screen_history_peek(&self) -> bool {
        self.capabilities.alt_screen_history_peek
    }

    /// v1.10.12: enter/exit the alt-screen history peek. Entering makes
    /// `show_block_view()` true (BlockView overlays the TUI); exiting restores
    /// the live alt grid. Does NOT touch `grid.scroll_offset` or primary-screen
    /// snapshots — block positioning uses the pane's `block_scroll_anchor`.
    pub fn set_alt_screen_history_peek(&mut self, on: bool) {
        self.capabilities.alt_screen_history_peek = on;
    }

    /// v1.10.6: the cursor's line index in the most recent primary-screen
    /// snapshot. `None` until the first snapshot, or when the cursor sat on
    /// a row the snapshot omitted (unowned, leading, or trailing empty).
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
        // v1.10.26 (FIX_IME_PREEDIT): re-anchor from the published composed
        // text (`freeze::composed_cursor_snapshot_line`) — the prefix grows
        // between 50ms publishes, so counted offsets drift past painted rows.
        let segment_len = self
            .capabilities
            .primary_screen_cursor_segment_len
            .unwrap_or(0);
        self.capabilities.primary_screen_cursor_snapshot_line =
            self.composed_cursor_snapshot_line(cursor_line, segment_len);
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
                &capture.tail,
            );
            let segment_len = text.len();
            let (text, styled) = self.compose_screen_history(text, styled);
            self.publish_screen_snapshot(text, styled, segment_len);
            // v1.10.26 review B1: the merged text replaced the in-flight
            // output — the stored segment length must describe THIS string,
            // or a mid-window keypress slices at a stale offset (a
            // non-char-boundary panic under CJK). The raw grid cursor row
            // anchors the caret here; the interrupt window is transient.
            self.capabilities.primary_screen_cursor_segment_len = Some(segment_len);
            return;
        }
        let Some(document_start) = self.block_tracker.screen_document_start() else {
            return;
        };
        let (text, styled, cursor_line) = self.primary_screen_document_snapshot(document_start);
        let (text, styled) = space_primary_screen_exit_tail(text, styled);
        // v1.10.23 (FIX_OMP_CONTENT_LOSS): prepend the preserved superseded
        // frames so the block transcript stays complete across full-frame
        // repaints; the cursor line shifts by the prepended history.
        // v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL): the scroll-out prefix joins
        // between the frames and the snapshot.
        let segment_len = text.len();
        let (text, styled) = self.compose_screen_history(text, styled);
        self.publish_screen_snapshot(text, styled, segment_len);
        // v1.10.6/25/26 (FIX_IME_PREEDIT): store the caret snapshot line
        // AFTER publish from the composed text actually written
        // (`freeze::composed_cursor_snapshot_line`) — pre-split offsets or
        // recomputed head counts drift past the painted rows on splits.
        self.capabilities.primary_screen_cursor_snapshot_line =
            self.composed_cursor_snapshot_line(cursor_line, segment_len);
        self.capabilities.primary_screen_cursor_segment_len = Some(segment_len);
    }

    /// v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL): publish a composed screen
    /// snapshot to the in-flight block — or split it into 1MiB finished
    /// blocks plus an in-flight tail when the session history crossed
    /// `MAX_OUTPUT_BYTES` (long TUI sessions no longer truncate at the
    /// snapshot budget).
    fn publish_screen_snapshot(&mut self, text: String, styled: StyledOutput, segment_len: usize) {
        if text.len() > MAX_OUTPUT_BYTES {
            self.split_screen_history(text, styled, segment_len);
        } else {
            self.block_tracker.replace_screen_snapshot(&text, styled);
        }
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

    /// v1.10.26 Batch D (D-3): consume the head count of the most recent 1MiB
    /// history split, if any. The app reads this after settling/draining a
    /// frame (or closing a tab) and advances a detached `block_scroll_anchor`
    /// by `heads × BLOCK_SPLIT_HEAD_CHROME_ROWS` so the user's viewport stays
    /// put when the split blocks' chrome rows are inserted (see
    /// `Tab::compensate_anchor_for_split` in weft_app).
    pub fn take_pending_screen_split_heads(&mut self) -> Option<usize> {
        self.capabilities.pending_screen_split_heads.take()
    }
}

fn viewport_row_for_document_start(start: u64, viewport_origin: u64, rows: usize) -> usize {
    start.saturating_sub(viewport_origin).min(rows as u64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vt::ScreenOwner;

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
            !t.show_block_view(),
            "a detected TUI owns the screen — the live grid renders it"
        );
    }

    #[test]
    fn chr_input_line_redraw_uses_live_grid() {
        // v1.10.6: pi (coding-agent CLI) uses CHR (horizontal-only) for its
        // input-line redraw + DEC 2026 sync output. CHR counts toward TUI
        // detection. v1.10.12: a screen-owned TUI renders in the LIVE GRID
        // (full-screen, low-latency) — the BlockView only appears when the
        // user scrolls into history browsing.
        let mut t = Terminal::new(5, 20);
        t.process(b"\x1b]133;A\x07\x1b]133;B\x07pi\x1b]133;C\x07");
        t.process("\x1b[3A\x1b[1G\x1b[?25l".as_bytes());
        assert!(!t.primary_screen_app_active(), "<2 ops: not yet a TUI");
        assert!(t.show_block_view());
        t.process("\x1b[?2026h\x1b[2Ka\x1b[2G\x1b[?2026l".as_bytes());
        assert!(t.primary_screen_app_active(), "TUI detected (>= 2 ops)");
        assert!(
            !t.show_block_view(),
            "screen-owned TUI renders in the live grid"
        );
    }

    #[test]
    fn relative_addressing_tui_uses_live_grid() {
        // openclaw/pi pattern — relative cursor moves only (A/B/D, CHR).
        // v1.10.12: a screen-owned TUI (any addressing style) renders in
        // the live grid; the BlockView appears only while history browsing.
        let mut t = Terminal::new(5, 20);
        t.process(b"\x1b]133;A\x07\x1b]133;B\x07openclaw\x1b]133;C\x07");
        t.process("\x1b[999D\x1b[915A".as_bytes());
        assert_eq!(t.screen_owner(), ScreenOwner::PrimaryScreenApp);
        assert!(
            !t.show_block_view(),
            "screen-owned TUI renders in the live grid"
        );
        // History browsing switches to the BlockView document snapshot.
        t.set_primary_history_view(true);
        assert!(t.show_block_view(), "history browsing uses the BlockView");
        t.set_primary_history_view(false);
        assert!(!t.show_block_view());
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
        // v1.10.12: the flag cleared → the relative-only move sequence
        // does not count as absolute, but the screen-owned TUI still
        // renders in the live grid.
        assert!(
            !t.show_block_view(),
            "absolute flag cleared → relative-only TUI still uses the live grid"
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
        assert!(!t.show_block_view(), "screen-owned TUI uses the live grid");
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
    fn nested_run_starting_with_a_stays_in_live_grid() {
        // v1.10.12: the FIRST marker of a nested run can be `133;A` with NO
        // pending exit (omp/pi's inner zsh prompt before any internal command
        // completed). Screen ownership keeps the live grid stable across
        // nested markers and CUP repaints — no render-mode lock required.
        let mut t = Terminal::new(8, 40);
        t.process(b"\x1b]133;A\x07pi\x1b]133;B\x07\x1b]133;C\x07");
        t.process("\x1b[3A\x1b[1G\x1b[?25l".as_bytes());
        t.process("\x1b[?2026h\x1b[2Ka\x1b[2G\x1b[?2026l".as_bytes());
        assert!(t.primary_screen_app_active());
        assert!(!t.show_block_view(), "screen-owned TUI uses the live grid");

        // Nested run #1: A is the FIRST marker (no pending exit yet).
        t.process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
        assert!(
            !t.show_block_view(),
            "nested markers must not flip the render mode"
        );
        assert!(t.block_tracker().phase() == ShellPhase::CommandExecuting);
        t.process("\x1b[2J\x1b[Hworking...".as_bytes());
        assert!(
            !t.show_block_view(),
            "CUP repaint inside a screen-owned TUI must keep the live grid"
        );

        // Nested run #2: D-then-A pair.
        t.process(b"\x1b]133;D;0\x07\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
        assert!(!t.show_block_view());

        // History browsing switches to the BlockView snapshot.
        t.set_primary_history_view(true);
        assert!(t.show_block_view(), "history browsing uses the BlockView");

        // Real settle (idle timer path) ends the screen-owned session.
        t.process(b"\x1b]133;D;0\x07");
        assert!(t.settle_primary_screen_exit());
    }

    // v1.10.12 regression: while a nested 133;D defers the exit (pending
    // settle window), history browsing must still show the BlockView —
    // otherwise scrolling during a nested command lands on the empty grid
    // scrollback ("cannot scroll through the history").
    #[test]
    fn history_browsing_works_while_exit_is_pending() {
        let mut t = Terminal::new(8, 40);
        t.process(b"\x1b]133;A\x07omp\x1b]133;B\x07\x1b]133;C\x07");
        t.process("\x1b[3A\x1b[1G\x1b[?25l".as_bytes());
        t.process("\x1b[?2026h\x1b[2Ka\x1b[2G\x1b[?2026l".as_bytes());
        assert!(t.primary_screen_app_active());
        assert!(!t.show_block_view(), "screen-owned TUI uses the live grid");

        // Nested command finishes → pending exit (settle window open).
        t.process(b"\x1b]133;D;0\x07");
        assert!(
            t.primary_screen_exit_pending(),
            "nested D defers the exit into the settle window"
        );

        // Scrolling into history must still switch to the BlockView.
        t.set_primary_history_view(true);
        assert!(
            t.show_block_view(),
            "history browsing must work even while an exit is pending"
        );

        t.set_primary_history_view(false);
        assert!(
            !t.show_block_view(),
            "back to the live grid when not browsing"
        );
        // The pending exit still settles normally afterwards.
        assert!(t.settle_primary_screen_exit());
    }

    #[test]
    fn sparse_repainter_stays_in_live_grid_through_cup_repaints() {
        // v1.10.12: a sparse repainter (omp/pi) detected with relative-only
        // addressing renders in the live grid for the whole command. Its
        // occasional full-viewport CUP repaint (task start, layout change)
        // must NOT flip the renderer to the BlockView — that flip truncated
        // the UI and blocked history scrolling.
        let mut t = Terminal::new(8, 40);
        t.process(b"\x1b]133;A\x07pi\x1b]133;B\x07\x1b]133;C\x07");
        t.process("\x1b[3A\x1b[1G\x1b[?25l".as_bytes());
        t.process("\x1b[?2026h\x1b[2Ka\x1b[2G\x1b[?2026l".as_bytes());
        assert!(t.primary_screen_app_active(), "TUI detected");
        assert!(!t.show_block_view(), "screen-owned TUI uses the live grid");

        // Task start: pi clears the viewport and repaints (CUP addressing).
        t.process("\x1b[2J\x1b[Hworking...\x1b[2;1Hprogress".as_bytes());
        assert!(t.primary_screen_app_active());
        assert!(
            !t.show_block_view(),
            "a CUP repaint inside a screen-owned session must keep the live grid"
        );
        assert_eq!(
            t.primary_screen_visible_row_start(),
            None,
            "no row hiding for sparse repainters"
        );
        assert_eq!(t.primary_screen_viewport_ownership(), None);

        // Nested marker bursts must not flip the render mode.
        t.process(b"\x1b]133;D;0\x07\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
        assert!(
            !t.show_block_view(),
            "nested markers must not flip the live grid"
        );

        // History browsing switches to the BlockView snapshot.
        t.set_primary_history_view(true);
        assert!(t.show_block_view(), "history browsing uses the BlockView");
        t.set_primary_history_view(false);
        assert!(!t.show_block_view());

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
