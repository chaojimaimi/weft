//! Vertex builders extracted from `renderer.rs`.
//!
//! These are pure geometry → vertex-buffer functions with no dependency on
//! `MetalRenderer` state. UI overlay builders that still need glyph atlas
//! access remain as `MetalRenderer` methods for now.
//!
//! Layout mirrors the old `renderer.rs` function order so the call sites in
//! the renderer switch from `push_quad(...)` to `primitives::push_quad(...)`.

pub(crate) mod block_view;
pub(crate) mod block_view_model;
pub(crate) mod command_surface;
pub(crate) mod grid;
pub(crate) mod grid_cache;
pub(crate) mod key_hints;
pub(crate) mod metal_backend;
#[cfg(test)]
mod offscreen_snapshots;
pub(crate) mod overlays;
pub(crate) mod palette;
pub(crate) mod pane_dividers;
pub(crate) mod panel;
pub(crate) mod primitives;
pub(crate) mod prompt;
pub(crate) mod settings;
pub(crate) mod status_hint;
pub(crate) mod tab_bar;
pub(crate) mod text;
pub(crate) mod ui_helpers;
