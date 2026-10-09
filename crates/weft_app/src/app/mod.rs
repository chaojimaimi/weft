//! App orchestration modules extracted from `main.rs` (Batch 8).
//!
//! `main.rs` held 1705 lines with mixed responsibilities: effect dispatch,
//! keyboard routing, action execution, focus management, cursor animation,
//! and pure helpers. Each concern now lives in its own submodule so `main.rs`
//! stays under the 800-line architecture gate.
//!
//! All methods here are `impl App` extensions using `pub(crate)` visibility —
//! their callers span the whole top-level controller family, not just the
//! `app` tree — so `main.rs` retains ownership of the `App` struct definition
//! and `new()`.

pub(crate) mod action;
pub(crate) mod cursor_anim;
pub(crate) mod effect_dispatch;
pub(crate) mod focus;
pub(crate) mod helpers;
pub(crate) mod keyboard;
// v1.13.6 T10 P2 (PLAN_v1136 §1 D2): the per-pane parse worker — one
// `weft-parse-<session_id>` thread feeding the pane's FairMutex terminal
// directly; the main thread keeps only control events + the post pass.
pub(crate) mod parse_worker;
pub(crate) mod parse_worker_stats;
// v1.12.25 (3-B-2 P2-01): per-frame session I/O core (`spawn_pty` /
// `pump_pty` / `process_messages` / `request_redraw`) moved verbatim out of
// `main.rs`; bodies zero-rewritten, visibility `pub(crate)`.
// v1.13.6 T10 P2 (D6): `pump_pty` is retired — the parse worker owns the
// byte path; `process_messages` drains control events only.
pub(crate) mod session_pump;
// v1.11.5 (PLAN_v1115 §M2): UI-event dispatch table + Dock badge debounce.
pub(crate) mod ui_events;
// v1.13.5 (T16d, PLAN_v11217 §3.11): memory-pressure response —
// os_proc_available_memory dual thresholds on the 1 Hz tick; releases the
// render caches that have existing release paths (inventory + exclusions in
// the module docs).
pub(crate) mod pressure;
