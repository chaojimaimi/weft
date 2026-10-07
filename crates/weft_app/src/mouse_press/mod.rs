//! `handle_mouse_press` cascade stages, moved verbatim out of
//! `mouse_press_controller.rs` (v1.12.27b P1-02) — modeled after the
//! `redraw/` precedent (v1.12.25 3-B-2).
//!
//! Zero-rewrite: every stage body below is byte-identical to the original
//! cascade level except the inline `return`s, which became the
//! [`PressOutcome`] signal. The caller in `mouse_press_controller.rs`
//! performs the plain `return` at the exact original points, so the
//! baseline-:808 tail `request_redraw` still runs only when every cascade
//! level fell through.

use super::*;

/// v1.12.27b (P1-02): the binary cascade signal for extracted
/// `handle_mouse_press` stages (`PhaseOutcome` precedent, v1.12.25 3-B-2).
/// The original cascade's contract — "hit ⇒ return, miss ⇒ next level" — is
/// kept verbatim: `Consumed` marks a level that hit one of its inline
/// `return`s (the caller returns from `handle_mouse_press` immediately, so
/// the tail `request_redraw` does not run, exactly as before); `NotHit`
/// marks a level that fell through to the next one.
#[must_use]
pub(crate) enum PressOutcome {
    /// The stage consumed the press — return from `handle_mouse_press`.
    Consumed,
    /// The stage missed — fall through to the next cascade level.
    NotHit,
}

pub(crate) mod block;
pub(crate) mod modal;
pub(crate) mod overlay;
pub(crate) mod panel;
pub(crate) mod prompt;
pub(crate) mod tab_bar;
pub(crate) mod terminal;
