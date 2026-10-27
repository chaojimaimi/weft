//! Offscreen-texture capacity tests for `paint/metal_backend/mod.rs`,
//! extracted to keep the production file under the 800-line architecture
//! gate (repo convention: `pty.rs` -> `pty/tests.rs`). Renamed from the
//! inline module `offscreen_capacity_tests` so the filename lands in the
//! gate's `*/tests.rs` exemption glob.

use super::{ceil_to_capacity_step, offscreen_blit_size, render_viewport_for_target};
use crate::renderer::MetalRenderer;
use weft_core::config::Theme;

/// Mirror the golden skip precedent: no Metal device (CI without GPU)
/// skips instead of failing.
fn headless_or_skip() -> Option<MetalRenderer> {
    metal::Device::system_default()?;
    Some(MetalRenderer::new_headless_paint(Theme::weft_dark()))
}

fn texture_dims(renderer: &MetalRenderer) -> (u64, u64) {
    let cache = renderer.offscreen_texture.borrow();
    let tex = cache
        .as_ref()
        .expect("ensure_offscreen_texture must have allocated");
    (tex.width(), tex.height())
}

#[test]
fn capacity_step_rounds_up_to_256px() {
    assert_eq!(ceil_to_capacity_step(0.0), 256);
    assert_eq!(ceil_to_capacity_step(1.0), 256);
    assert_eq!(ceil_to_capacity_step(256.0), 256);
    assert_eq!(ceil_to_capacity_step(257.0), 512);
    assert_eq!(ceil_to_capacity_step(840.0), 1024);
    assert_eq!(ceil_to_capacity_step(600.0), 768);
}

#[test]
fn render_viewport_is_viewport_for_capacity_and_target_for_fallback() {
    // Capacity-sized offscreen: NDC maps exactly over the viewport —
    // this is the B3-1 anti-stretch guarantee for set_viewport.
    let vp = render_viewport_for_target((840.0, 600.0), (1024.0, 768.0));
    assert_eq!((vp.originX, vp.originY), (0.0, 0.0));
    assert_eq!((vp.width, vp.height), (840.0, 600.0));
    // Drawable fallback (vp_mismatch frame): clamps to the drawable —
    // identical to the pre-B3-1 default full-target viewport, so the
    // async-lag frame's behavior is unchanged.
    let vp2 = render_viewport_for_target((840.0, 600.0), (800.0, 500.0));
    assert_eq!((vp2.width, vp2.height), (800.0, 500.0));
}

#[test]
fn blit_region_is_viewport_scoped_never_capacity() {
    // A retained oversized texture must never inflate the copy…
    assert_eq!(
        offscreen_blit_size((840.0, 600.0), (2048.0, 1536.0)),
        (840, 600)
    );
    // …and the live-resize drawable clamp is preserved.
    assert_eq!(
        offscreen_blit_size((840.0, 600.0), (800.0, 500.0)),
        (800, 500)
    );
}

/// The B3-1 core scenario: shrink keeps the texture (no realloc, no
/// forced rebuild), growth within capacity keeps it too (but forces one
/// full redraw for the newly exposed region), and only growth beyond the
/// capacity reallocates.
#[test]
fn offscreen_capacity_survives_shrink_and_grows_only_when_insufficient() {
    let Some(mut renderer) = headless_or_skip() else {
        eprintln!("skipping offscreen capacity test: no Metal device available");
        return;
    };
    renderer.viewport = (840.0, 600.0);
    assert!(renderer.ensure_offscreen_texture());
    assert_eq!(texture_dims(&renderer), (1024, 768));
    renderer.force_full_grid.set(false);

    // Shrink: capacity retained, no rebuild, valid region tracked.
    renderer.viewport = (400.0, 300.0);
    assert!(renderer.ensure_offscreen_texture());
    assert_eq!(
        texture_dims(&renderer),
        (1024, 768),
        "shrink must not realloc"
    );
    assert!(
        !renderer.force_full_grid.get(),
        "shrink keeps the rendered top-left valid"
    );
    assert_eq!(renderer.offscreen_dims.get(), (400.0, 300.0));

    // Grow within capacity: no realloc, but the new region was never
    // rendered → exactly one forced rebuild.
    renderer.viewport = (1000.0, 700.0);
    assert!(renderer.ensure_offscreen_texture());
    assert_eq!(
        texture_dims(&renderer),
        (1024, 768),
        "in-capacity growth must not realloc"
    );
    assert!(
        renderer.force_full_grid.get(),
        "region growth into unrendered texture must force a full redraw"
    );
    renderer.force_full_grid.set(false);

    // Grow beyond capacity: realloc at the new rounded capacity.
    renderer.viewport = (1500.0, 900.0);
    assert!(renderer.ensure_offscreen_texture());
    assert_eq!(texture_dims(&renderer), (1536, 1024));
    assert!(
        renderer.force_full_grid.get(),
        "realloc forces full redraw (undefined content)"
    );
    assert_eq!(renderer.offscreen_dims.get(), (1500.0, 900.0));
}
