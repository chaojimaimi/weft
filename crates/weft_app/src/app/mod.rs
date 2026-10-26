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
// v1.14.3 (T2/T1, PLAN_v1143 §2): pane-divider drag throttle kernels + the
// shared three-state DragConsume verdict (panel_controller stays within its
// line budget; the 8ms gate itself reuses panel_controller::drag_apply_step).
pub(crate) mod cursor_anim;
pub(crate) mod divider_drag;
pub(crate) mod effect_dispatch;
pub(crate) mod focus;
// v1.14.0 (PLAN_v1140 T4): per-frame hover fade sync — one chokepoint
// retargeting both HoverFades registries (tab bar + block/scrollbar).
pub(crate) mod helpers;
pub(crate) mod hover_fades;
pub(crate) mod keyboard;
// v1.14.0 (PLAN_v1140 T0): the motion hub — Tween (RAII live counter) +
// ease_out_cubic + the Reduce Motion gate. Declared here (not main.rs) so
// the MAIN_RS_MAX ceiling is untouched; the tick thread wires in
// app_runtime.rs beside the blink/spinner/autoscroll timers.
pub(crate) mod motion;
// v1.13.6 T10 P2 (PLAN_v1136 §1 D2): the per-pane parse worker — one
// `weft-parse-<session_id>` thread feeding the pane's FairMutex terminal
// directly; the main thread keeps only control events + the post pass.
// v1.14.0 (PLAN_v1140 batch splits): the theme-picker keyboard handler,
// moved verbatim out of lifecycle_controller.rs (800-line gate).
pub(crate) mod palette_theme_key;
pub(crate) mod parse_worker;
pub(crate) mod parse_worker_stats;
// v1.14.2 (B3, PLAN_v1142 §3): async PTY spawn — request assembly, fork
// worker, and the main-thread adoption step (placeholder panes, pending
// stdin, SpawnCompleted/SpawnFailed handling).
pub(crate) mod pane_spawn;
// v1.14.3 (T2, PLAN_v1143 §2): Pane 级对账判重 — the memoized
// `reconciled_terminal_block_scroll` twin for the recompute-layout call site.
pub(crate) mod reconcile_scroll;
// v1.15.1 (T3, PLAN_v1151): PTY 耗尽诊断页一键重试 — spawn-dead 谓词 +
// 裸 R 键判定 + 原地重发 spawn（terminal 原位重置见模块 doc）。
pub(crate) mod retry_spawn;
// v1.12.25 (3-B-2 P2-01): per-frame session I/O core (`spawn_pty` /
// `pump_pty` / `process_messages` / `request_redraw`) moved verbatim out of
// `main.rs`; bodies zero-rewritten, visibility `pub(crate)`.
// v1.13.6 T10 P2 (D6): `pump_pty` is retired — the parse worker owns the
// byte path; `process_messages` drains control events only.
pub(crate) mod session_pump;
// v1.14.2 (B1, PLAN_v1142 §3): the generation-keyed tab-title snapshot
// cache (tab 风暴债①).
pub(crate) mod tab_title_cache;
// v1.11.5 (PLAN_v1115 §M2): UI-event dispatch table + Dock badge debounce.
pub(crate) mod ui_events;
// v1.13.5 (T16d, PLAN_v11217 §3.11): memory-pressure response —
// os_proc_available_memory dual thresholds on the 1 Hz tick; releases the
// render caches that have existing release paths (inventory + exclusions in
// the module docs).
pub(crate) mod pressure;
