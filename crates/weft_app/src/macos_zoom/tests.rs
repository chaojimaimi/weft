use super::anim::*;

/// Serializes the process-global ZOOM_ANIM / ZOOM_RESTORE_FRAME state
/// (paint/zoom_render/tests.rs GLOBAL_ZOOM_STATE_LOCK pattern); every
/// other test here stays pure-local by design.
static ZOOM_STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

const EPS: f64 = ZOOM_EPSILON_PT;

#[test]
fn ease_endpoints_and_midpoint() {
    assert_eq!(ease_in_out_quad(0.0), 0.0);
    assert_eq!(ease_in_out_quad(1.0), 1.0);
    assert_eq!(ease_in_out_quad(0.5), 0.5);
    // Clamp: a lagging clock must never overshoot the target frame.
    assert_eq!(ease_in_out_quad(-0.25), 0.0);
    assert_eq!(ease_in_out_quad(1.25), 1.0);
}

#[test]
fn ease_is_monotonic_across_samples() {
    let mut previous = -1.0;
    for step in 0..=100 {
        let p = step as f64 / 100.0;
        let value = ease_in_out_quad(p);
        assert!(
            value >= previous,
            "ease regressed at p={p}: {value} < {previous}"
        );
        assert!((0.0..=1.0).contains(&value));
        previous = value;
    }
}

#[test]
fn lerp_endpoints_and_midpoint() {
    assert_eq!(lerp(10.0, 20.0, 0.0), 10.0);
    assert_eq!(lerp(10.0, 20.0, 1.0), 20.0);
    assert_eq!(lerp(10.0, 20.0, 0.5), 15.0);
    // Extrapolation-free: t is eased-clamped upstream.
    assert_eq!(lerp(-5.0, 5.0, 0.25), -2.5);
}

#[test]
fn zoom_direction_truth_table() {
    let vis_origin = (0.0, 0.0);
    let vis_size = (1440.0, 855.0);
    // Small window: zoom-in.
    assert_eq!(
        zoom_direction((100.0, 100.0), (800.0, 600.0), vis_origin, vis_size, EPS),
        ZoomDirection::ZoomIn
    );
    // Already at visibleFrame: restore.
    assert_eq!(
        zoom_direction(vis_origin, vis_size, vis_origin, vis_size, EPS),
        ZoomDirection::Restore
    );
    // Partially zoomed (halfway): still zoom-in.
    assert_eq!(
        zoom_direction((50.0, 50.0), (1100.0, 700.0), vis_origin, vis_size, EPS),
        ZoomDirection::ZoomIn
    );
    // Origin-difference boundary: exactly the epsilon counts as zoomed...
    assert_eq!(
        zoom_direction((1.0, 1.0), vis_size, vis_origin, vis_size, EPS),
        ZoomDirection::Restore
    );
    // ...one hair past it does not.
    assert_eq!(
        zoom_direction((1.0 + 2.0 * EPS, 0.0), vis_size, vis_origin, vis_size, EPS),
        ZoomDirection::ZoomIn
    );
    // Size difference alone blocks the restore verdict.
    assert_eq!(
        zoom_direction(
            vis_origin,
            (vis_size.0 - 4.0, vis_size.1),
            vis_origin,
            vis_size,
            EPS
        ),
        ZoomDirection::ZoomIn
    );
}

#[test]
fn should_refresh_restore_truth_table() {
    let vis: ZoomFrame = (0.0, 0.0, 1440.0, 855.0);
    let dragged: ZoomFrame = (60.0, 40.0, 900.0, 600.0);
    let restore: ZoomFrame = (100.0, 80.0, 800.0, 600.0);
    // Ordinary drag resize: refresh follows the user.
    assert!(should_refresh_restore(false, dragged, vis, EPS));
    // Anim running: never touch the bookkeeping mid-flight.
    assert!(!should_refresh_restore(true, dragged, vis, EPS));
    // Zoom-in TAIL Resized (anim cleared, frame == visibleFrame): the
    // mandatory non-zoomed gate blocks the overwrite.
    assert!(!should_refresh_restore(false, vis, vis, EPS));
    // Restore-direction TAIL Resized (anim cleared, frame back at the
    // restore frame ≠ visibleFrame): refresh fires but writes the SAME
    // value -- harmless by construction.
    assert!(should_refresh_restore(false, restore, vis, EPS));
    // Epsilon boundary: within tolerance of visibleFrame counts zoomed.
    let near_vis: ZoomFrame = (EPS, EPS, 1440.0 - EPS, 855.0 - EPS);
    assert!(!should_refresh_restore(false, near_vis, vis, EPS));
}

#[test]
fn plan_zoom_truth_table() {
    let vis: ZoomFrame = (0.0, 0.0, 1440.0, 855.0);
    let window: ZoomFrame = (100.0, 100.0, 800.0, 600.0);
    let restore: ZoomFrame = (100.0, 100.0, 800.0, 600.0);

    // Inactive + not zoomed -> zoom-in; persists the current frame and
    // aims for visibleFrame.
    assert_eq!(
        plan_zoom(false, None, window, vis, EPS),
        ZoomPlan::Animate {
            restore_write: Some(window),
            start: window,
            target: vis
        }
    );
    // Inactive + already zoomed + restore remembered -> animate home.
    assert_eq!(
        plan_zoom(false, Some(restore), vis, vis, EPS),
        ZoomPlan::Animate {
            restore_write: None,
            start: vis,
            target: restore
        }
    );
    // Inactive + zoomed + nothing remembered -> system zoom via super.
    assert_eq!(
        plan_zoom(false, None, vis, vis, EPS),
        ZoomPlan::ForwardToSuper
    );
    // Reentry (anim active): target = restore, restore NOT rewritten
    // (review P1-4 swap-pollution guard).
    assert_eq!(
        plan_zoom(true, Some(restore), window, vis, EPS),
        ZoomPlan::Animate {
            restore_write: None,
            start: window,
            target: restore
        }
    );
    // Reentry with no restore frame: cancel, stay at the current frame.
    assert_eq!(plan_zoom(true, None, window, vis, EPS), ZoomPlan::Cancel);
}

#[test]
fn reentry_semantics_through_global_state() {
    let _guard = ZOOM_STATE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    // Clean slate.
    zoom_anim_cancel();
    *lock_restore_frame() = None;
    assert!(!zoom_anim_active());
    assert_eq!(zoom_restore_frame(), None);

    let vis: ZoomFrame = (0.0, 0.0, 1440.0, 855.0);
    let window: ZoomFrame = (100.0, 100.0, 800.0, 600.0);
    let main_h = 900.0;

    // Zoom-in arms the animation and persists the restore frame.
    match plan_zoom(zoom_anim_active(), zoom_restore_frame(), window, vis, EPS) {
        ZoomPlan::Animate {
            restore_write: Some(restore),
            start,
            target,
        } => {
            set_zoom_restore_frame(restore);
            start_zoom_anim(start, target, main_h);
        }
        other => panic!("expected zoom-in Animate, got {other:?}"),
    }
    assert_eq!(zoom_restore_frame(), Some(window));
    let armed = zoom_anim_peek().expect("animation must be armed");
    assert_eq!(armed.origin_start, (100.0, main_h - 600.0 - 100.0));
    assert_eq!(armed.size_start, (800.0, 600.0));
    // visibleFrame target flipped into winit top-left coordinates.
    assert_eq!(armed.origin_target, (0.0, main_h - 855.0));
    assert_eq!(armed.size_target, (1440.0, 855.0));
    assert_eq!(armed.last_applied, (100.0, main_h - 700.0, 800.0, 600.0));
    assert_eq!(armed.steps, 0);

    // Reentry through the SAME global state: the decision keeps the
    // restore frame untouched (swap-pollution guard) and retargets to it.
    let mid_flight: ZoomFrame = (50.0, 45.0, 1100.0, 720.0);
    match plan_zoom(
        zoom_anim_active(),
        zoom_restore_frame(),
        mid_flight,
        vis,
        EPS,
    ) {
        ZoomPlan::Animate {
            restore_write,
            start,
            target,
        } => {
            assert_eq!(
                restore_write, None,
                "reentry must not rewrite the restore frame"
            );
            assert_eq!(target, window);
            // Reset semantics: a fresh segment from the CURRENT frame.
            start_zoom_anim(start, target, main_h);
        }
        other => panic!("expected reentry Animate, got {other:?}"),
    }
    assert_eq!(
        zoom_restore_frame(),
        Some(window),
        "restore frame must survive reentry"
    );
    let restarted = zoom_anim_peek().expect("reentry re-arms the animation");
    assert_eq!(restarted.origin_start, (50.0, main_h - 720.0 - 45.0));
    assert!(restarted.start >= armed.start, "reentry resets the clock");

    // Advance stamps applied steps; finish drains and clears.
    zoom_anim_advance((60.0, 44.0, 1150.0, 740.0));
    zoom_anim_advance((30.0, 20.0, 1300.0, 800.0));
    let finished = zoom_anim_finish().expect("animation still running");
    assert_eq!(finished.steps, 2);
    assert_eq!(finished.last_applied, (30.0, 20.0, 1300.0, 800.0));
    assert!(!zoom_anim_active());

    // Reentry WITHOUT a restore frame cancels cleanly.
    *lock_restore_frame() = None;
    start_zoom_anim(vis, vis, main_h);
    assert!(zoom_anim_active());
    match plan_zoom(true, zoom_restore_frame(), mid_flight, vis, EPS) {
        ZoomPlan::Cancel => zoom_anim_cancel(),
        other => panic!("expected Cancel, got {other:?}"),
    }
    assert!(!zoom_anim_active());
}

#[test]
fn to_winit_top_left_matches_winit_flip() {
    // Window at AppKit origin (100, 50), 800x600, main display 900pt
    // tall -> winit top-left (100, 250): 900 - 600 - 50.
    assert_eq!(
        to_winit_top_left((100.0, 50.0), (800.0, 600.0), 900.0),
        (100.0, 250.0)
    );
    // Dock-adjacent visibleFrame: origin sits above the dock.
    assert_eq!(
        to_winit_top_left((0.0, 25.0), (1440.0, 855.0), 900.0),
        (0.0, 20.0)
    );
    // Round trip through the winit formula (y_top = main_h - h - y_ns).
    let (x, y_top) = to_winit_top_left((37.0, 41.0), (500.0, 400.0), 1000.0);
    assert_eq!((x, y_top), (37.0, 559.0));
}

/// Appendix I-6 revision: the direction getter -- zoom-in only when BOTH
/// dimensions grow; mixed/shrinking targets defer (conservative).
#[test]
fn zoom_anim_is_zoom_in_direction() {
    let _guard = ZOOM_STATE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let start = std::time::Instant::now() - std::time::Duration::from_millis(50);
    *super::anim::zoom_anim_slot() = Some(super::anim::ZoomAnim {
        origin_start: (0.0, 0.0),
        size_start: (800.0, 600.0),
        origin_target: (0.0, 0.0),
        size_target: (1728.0, 1084.0),
        start,
        duration: std::time::Duration::from_millis(220),
        last_applied: (0.0, 0.0, 800.0, 600.0),
        steps: 0,
    });
    assert!(super::zoom_anim_is_zoom_in());
    // shrink: width and height both decrease
    let a = zoom_anim_peek().unwrap();
    *super::anim::zoom_anim_slot() = Some(super::anim::ZoomAnim {
        size_target: (800.0, 600.0),
        size_start: (1728.0, 1084.0),
        ..a
    });
    assert!(!super::zoom_anim_is_zoom_in());
    // mixed (width grows, height shrinks): conservative zoom-out
    let a = zoom_anim_peek().unwrap();
    *super::anim::zoom_anim_slot() = Some(super::anim::ZoomAnim {
        size_target: (1728.0, 600.0),
        ..a
    });
    assert!(!super::zoom_anim_is_zoom_in());
    zoom_anim_finish();
    assert!(!super::zoom_anim_is_zoom_in());
}
