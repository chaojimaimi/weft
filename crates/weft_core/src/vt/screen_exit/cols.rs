//! v1.11.8 (PLAN_v1118 M-C2): PTY cols policy — extracted verbatim from the
//! screen_exit module (pure constant functions, zero Terminal coupling) to
//! keep the module root under the file-size gate. Visibility is unchanged:
//! `TuiColsKind` stays `pub` (vt/mod.rs's public re-export and the app's
//! tab/resize.rs consume it), the consts and the pure decision stay
//! `pub(crate)` (vt/mod.rs's test-only re-export + vt/tests.rs consume them).

use std::time::Duration;

/// v1.10.28 (FIX_TRANSIENT_ALT_COLS_FLIP): minimum *continuous* alternate-
/// screen residency before `tui_cols_kind()` reports [`TuiColsKind::Full`].
/// omp 17.3.7 wraps its SIGWINCH repaint in a 1049h → full redraw (~129ms) →
/// 1049l excursion; a real alt TUI (vim/less) stays resident for seconds.
/// 250ms ≈ 2x the omp residency and far below any real TUI, so transient
/// excursions never flip the cols target and the ioctl → SIGWINCH → 1049
/// feedback loop is broken at the source. See docs/FIX_TRANSIENT_ALT_COLS_FLIP.md.
pub(crate) const SUSTAINED_ALT_COLS_MS: u64 = 250;

/// v1.10.30 (FIX_LESS_ALT_COLS_JUMP): burst re-entry detection window.
/// omp flip-pairs 1049h→repaint→1049l toggle at ~130-330ms intervals.
/// 400ms covers the burst window while treating isolated alt entries
/// (less/vim startup, no recent exit) as immediate Full. See
/// docs/FIX_LESS_ALT_COLS_JUMP.md.
pub(crate) const ALT_REENTRY_BURST_MS: u64 = 400;

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

/// v1.10.30 (FIX_LESS_ALT_COLS_JUMP): pure decision for the sustained-alt
/// cols hysteresis feeding [`crate::vt::Terminal::tui_cols_kind`].
///
/// Distinguishes isolated vs burst re-entry:
/// - Isolated entry (no recent exit or >= 400ms since last exit): immediately Full
/// - Burst re-entry (< 400ms since last exit): requires sustained 250ms residency
///
/// This fixes less/vim startup jump (isolated entry gets Full immediately)
/// while maintaining omp loop suppression (burst re-entries wait 250ms).
/// A `None` entry time (unknown start) conservatively returns Full.
/// See docs/FIX_LESS_ALT_COLS_JUMP.md.
pub(crate) fn sustained_alt_cols_kind(
    alt_active: bool,
    alt_active_for: Option<Duration>,
    since_last_exit: Option<Duration>,
) -> TuiColsKind {
    if !alt_active {
        return TuiColsKind::Content;
    }
    // Isolated entry (no recent exit) → immediate Full (fixes less/vim startup)
    if since_last_exit.map_or(true, |d| d >= Duration::from_millis(ALT_REENTRY_BURST_MS)) {
        return TuiColsKind::Full;
    }
    // Burst re-entry: apply sustained residency threshold
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
