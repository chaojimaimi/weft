//! App orchestration modules extracted from `main.rs` (Batch 8).
//!
//! `main.rs` held 1705 lines with mixed responsibilities: effect dispatch,
//! keyboard routing, action execution, focus management, cursor animation,
//! and pure helpers. Each concern now lives in its own submodule so `main.rs`
//! stays under the 800-line architecture gate.
//!
//! All methods here are `impl App` extensions using `pub(super)` visibility,
//! so `main.rs` retains ownership of the `App` struct definition and `new()`.

pub(crate) mod action;
pub(crate) mod cursor_anim;
pub(crate) mod effect_dispatch;
pub(crate) mod focus;
pub(crate) mod helpers;
pub(crate) mod keyboard;
// v1.11.5 (PLAN_v1115 §M2): UI-event dispatch table + Dock badge debounce.
pub(crate) mod ui_events;
