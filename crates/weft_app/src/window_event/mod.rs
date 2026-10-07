//! `dispatch_window_event` external arm module, split from
//! `window_event_controller.rs` (v1.12.27b P1-04).
//!
//! The same-file split landed at 813 lines (>800), so the plan's sanctioned
//! contingency moved the `Resized` arm — the largest (~190 lines at
//! baseline) — into this child module (`paint/settings/`+`pages.rs`
//! precedent).

pub(crate) mod resized;
