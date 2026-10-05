//! Redraw orchestration submodules extracted from `redraw_controller.rs`
//! (v1.12.25 3-B-2 P2-02) — modeled after the `vt/screen_exit/` precedent.
//!
//! `run_redraw` was a single 808-line function; its pump/process segment and
//! the owned pre-draw snapshot helpers now live here as verbatim-moved
//! `impl App` extensions (zero behavior change). The early `return`s inside
//! the extracted segment are converted to the [`PhaseOutcome`] signal so the
//! caller in `redraw_controller.rs` can `return` from `run_redraw` at the
//! exact original points.

/// v1.12.25 (3-B-2 P2-02): the early-return signal for extracted `run_redraw`
/// phases. A phase that hit one of the original inline `return`s reports
/// [`PhaseOutcome::Abort`]; the caller `match`es and performs the plain
/// `return` at the original timing — no return-timing semantics change.
#[must_use]
pub(crate) enum PhaseOutcome {
    /// The extracted phase ran to its end — continue with the next phase.
    Continue,
    /// The extracted phase hit an original early `return` — the caller must
    /// return from `run_redraw` immediately (skipping draw + tail, as before).
    Abort,
}

pub(crate) mod pump_process;
pub(crate) mod snapshots;
