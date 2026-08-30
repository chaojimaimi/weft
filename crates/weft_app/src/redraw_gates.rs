//! v1.11.10 (PLAN_v11110 M-B): the two redraw suppression gates as a pure
//! decision function. Extracted so the `forced` bypass (live-resize
//! same-tick draw) is regression-locked without needing an `App` fixture —
//! `App` carries an `EventLoopProxy`, which cannot be constructed off the
//! main thread (see the v1.11.10 review P1-1 round).

use crate::input_router::SessionInputRoute;

/// Whether this redraw must be suppressed (`true` = early-return, no frame).
///
/// `forced` (the live-resize same-tick draw) bypasses both gates: during a
/// drag the frozen/stretched frame is the worse artifact, and a modal that
/// consumed input repaints on top in the same tick's draw.
pub(crate) fn redraw_suppressed(
    forced: bool,
    synchronized_output: bool,
    route: SessionInputRoute,
) -> bool {
    !forced && (synchronized_output || route == SessionInputRoute::Consume)
}

#[cfg(test)]
mod tests {
    use super::*;
    use SessionInputRoute::{Consume, Dispatch};

    /// Full truth table — the `!forced &&` prefix is the v1.11.10 safety
    /// boundary (rust-reviewer P1-1): deleting it or mis-wiring the forced
    /// entry must turn these red.
    #[test]
    fn suppression_truth_table() {
        // Forced draws are never suppressed.
        for sync in [true, false] {
            for route in [Dispatch, Consume] {
                assert!(
                    !redraw_suppressed(true, sync, route),
                    "forced must bypass both gates (sync={sync}, route={route:?})"
                );
            }
        }
        // Non-forced: suppressed by sync alone, by route-consume alone, and
        // by both; clean passthrough only when neither gate applies.
        assert!(redraw_suppressed(false, true, Dispatch));
        assert!(redraw_suppressed(false, true, Consume));
        assert!(redraw_suppressed(false, false, Consume));
        assert!(!redraw_suppressed(false, false, Dispatch));
    }
}
