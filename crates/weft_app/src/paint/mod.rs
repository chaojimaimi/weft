//! Vertex builders extracted from `renderer.rs`.
//!
//! These are pure geometry → vertex-buffer functions with no dependency on
//! `MetalRenderer` state. UI overlay builders that still need glyph atlas
//! access remain as `MetalRenderer` methods for now.
//!
//! Layout mirrors the old `renderer.rs` function order so the call sites in
//! the renderer switch from `push_quad(...)` to `primitives::push_quad(...)`.

pub(crate) mod alt_peek_pill;
pub(crate) mod block_view;
// v1.11.6 (PLAN_v1116 M3/D-a): vertex goldens for
// `build_block_view_vertices` — the pre-split byte baseline the M4
// structural move kept byte-equal (now also pinning M5/M6 value-preserving
// consolidation and the M7 selection flip via the S4 scene). Mounted here
// (not inside block_view.rs, which the M4 split brought back under 800)
// to keep the paint-ordering module free of test fixtures.
#[cfg(test)]
#[path = "block_view/golden_tests.rs"]
mod block_view_golden_tests;
pub(crate) mod block_view_model;
// v1.11.6 (PLAN_v1116 M5 / C2): single source for ratio-based color
// derivations (bit-exact ports of the inline expressions they replace).
pub(crate) mod color_math;
pub(crate) mod command_surface;
pub(crate) mod grid;
#[cfg(test)]
mod grid_bench;
pub(crate) mod grid_cache;
pub(crate) mod grid_instances;
pub(crate) mod key_hints;
pub(crate) mod live_cache;
pub(crate) mod metal_backend;
#[cfg(test)]
mod offscreen_snapshots;
pub(crate) mod overlays;
pub(crate) mod palette;
pub(crate) mod pane_dividers;
pub(crate) mod panel;
pub(crate) mod preedit;
pub(crate) mod primitives;
// v1.11.3 (PLAN_v1113 §3.1): underline geometry + bold→bright helpers —
// own module so primitives.rs stays within the gate's 800-line budget.
pub(crate) mod prompt;
pub(crate) mod selection_color;
pub(crate) mod settings;
pub(crate) mod settings_profiles;
pub(crate) mod status_hint;
pub(crate) mod styled_line_cache;
pub(crate) mod tab_bar;
pub(crate) mod text;
pub(crate) mod ui_helpers;
pub(crate) mod underline;
