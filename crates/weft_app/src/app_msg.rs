//! Worker → main control events (v1.13.6 T10 P2, PLAN_v1136 §1 D2/D6).
//!
//! Split out of main.rs when the AppMsg payload changed shape (commit-gate
//! MAIN_RS_MAX): the parse moved to the per-pane worker, so the pane's
//! crossbeam channel no longer carries raw PTY bytes — `PtyOutput` is gone
//! (the worker feeds the terminal directly under the pane's FairMutex).
//! What is left is the control traffic the MAIN thread must decide on:
//! alt-screen flip bookkeeping (timing belongs to the worker — see the
//! variant doc) and the exit contract.

/// See the module docs. Re-exported at the crate root (`crate::AppMsg`) so
/// every pump/controller consumer keeps its existing import path.
#[derive(Debug)]
pub(crate) enum AppMsg {
    /// The parse worker observed alt-screen (DEC 1049) toggles and/or an
    /// alt-screen history-peek exit across one processed batch. Carries the
    /// worker-side stamp of the flipping batch — stamping at the diff point
    /// (P2 review) instead of at main-thread consumption time avoids a
    /// systematic one-frame drift of the storm/debounce windows.
    AltFlipped {
        /// Instant captured inside the terminal lock right after the
        /// flipping batch was parsed.
        at: std::time::Instant,
        /// `Terminal::alt_flip_count` diff across the batch (≥1 when this
        /// event arms the rescale; a batch-internal h→l pair counts twice).
        flips: u64,
        /// `is_alt_screen_history_peek` went true → false across the batch
        /// (the `alt_peek_gate.note_exit()` trigger).
        peek_exited: bool,
    },
    /// The pane's shell exited (payload of `PtyEvent::Exit`). FIFO is the
    /// exit contract: the worker enqueues this strictly AFTER processing
    /// every queued `Output` batch, so the main thread sees the terminal
    /// fully caught up before learning of the death (D2 close contract —
    /// "close 前最后输出仍入块").
    PtyExited(Result<i32, String>),
}
