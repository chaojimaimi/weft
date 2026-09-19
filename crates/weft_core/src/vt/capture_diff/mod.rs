//! PLAN_B Phase 0 (docs/PLAN_B_phase0.md P0-2, test landing point 3):
//! crate-internal capture/snapshot differential channel.
//!
//! The integration suite (`tests/capture_snapshot_equivalence.rs`) covers the
//! plain print channel end-to-end. Two capture paths are unreachable from
//! outside the crate because their state is `pub(in crate::vt)`:
//!
//! - the **staging channel** (`Terminal::preexec_staging`) — bytes printed
//!   between editor submission and `133;B`;
//! - the **interrupt channel** (`PrimaryScreenInterruptCapture`) — the Ctrl-C
//!   frozen transcript plus its double-fed tail.
//!
//! Both channels mirror the SAME print events the Grid receives, so the
//! differential contract is: normalized staged/tail text == normalized Grid
//! logical rows for the same window. Test-only — declared under
//! `#[cfg(test)]` in `vt/mod.rs`; the harness lives in `tests.rs` (directory
//! layout matches the architecture-gate `*/tests.rs` exemption glob).

#[path = "tests.rs"]
mod tests;
