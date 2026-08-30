//! v1.11.13 (PLAN_v11113 §M1): graphic Dock progress bar (OSC 9;4).
//!
//! The pipeline speaks [`DockVisual`]: `dispatch_ui_events` maps the VT
//! [`DockProgress`] through `notify_policy::dock_visual`, the 200 ms
//! debounce carries the enum (no more badge-text folding), and
//! [`set_dock_visual`] renders it onto the Dock icon.
//!
//! # Drawing path (settled during implementation, logged in PROGRESS)
//!
//! The plan's primary CG-bitmap path is UNAVAILABLE: objc2-app-kit 0.2.2
//! exposes no CGImage bridging for NSImage in either direction (no
//! `initWithCGImage:size:` and no `CGImage()` — verified against the
//! generated bindings). Per the plan's fallback decision tree we draw with
//! `lockFocus` + `NSBezierPath` (workspace features NSBezierPath/NSColor/
//! NSGraphics added). `lockFocus`/`unlockFocus` are soft-deprecated by
//! AppKit but fully functional; the deprecation is allow(ed) at the call
//! site.
//!
//! # Safety net (P0-1)
//!
//! Every ObjC touch is wrapped in `objc2::exception::catch` (NOT
//! `catch_unwind` — an NSException is a foreign unwind that aborts the
//! process; macos_notifications.rs module doc has the lesson) — including
//! the badge-text fallback itself (rust-reviewer B-1: it runs right after
//! ObjC threw) and a Drop guard that pairs unlockFocus with lockFocus on
//! the unwind path (B-2). Any failure falls back to badge text (`47%` /
//! `…` / `!` / none), so the feature can never regress below the v1.11.5
//! text-only behavior.
//!
//! # Icon lifecycle (P1-1)
//!
//! `ORIGINAL_ICON` is captured ONCE as the Bar compositing base only.
//! `Badge` and `Clear` restore through the existing `set_dock_icon(variant)`
//! (single-sourced bundle-None + variant branches) — never through the
//! capture: config reload re-calls `set_dock_icon` at runtime, so a
//! OnceLock capture would go stale, and the bundled-.app case requires
//! `setApplicationIconImage(None)` so Dock owns the geometry.
//!
//! Residual edge (accepted): a logo-variant config reload DURING an active
//! progress stream leaves `ORIGINAL_ICON`/`LAST_APPLIED` describing the old
//! variant until the next non-equal visual lands. Cosmetic only; revisit
//! with the v1.12 indeterminate-animation backlog item.

use std::sync::{Mutex, OnceLock};

use objc2::rc::Retained;
use objc2::ClassType;
use objc2_app_kit::{NSApplication, NSBezierPath, NSColor, NSCompositingOperation, NSImage};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize};
use weft_core::config::LogoVariant;

/// What the Dock should display for an OSC 9;4 state — the enum the whole
/// M1 pipeline carries (dispatch → debounce → drawing; no string folding).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DockVisual {
    /// Progress bar; `Some(p)` = determinate fill at p% (clamped 0..=100),
    /// `None` = the static indeterminate segment (animation timer is the
    /// v1.12 backlog item — PLAN_v11113 §M1).
    Bar(Option<u8>),
    /// Failure mark: restore the icon first, then badge `!` (P1-1.1).
    Badge,
    /// Full restore: `set_dock_icon` + badge cleared (P1-1.2/.3).
    Clear,
}

// ── pure geometry (unit-tested, no objc) ─────────────────────────────

/// Axis-aligned rect in icon-local points. Origin bottom-left (the AppKit
/// `lockFocus` non-flipped convention).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct GeometryRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Track band height as a fraction of the icon size.
pub(crate) const TRACK_HEIGHT_FRACTION: f64 = 0.12;
/// Left/right inset of the track band.
pub(crate) const TRACK_MARGIN_FRACTION: f64 = 0.08;
/// Indeterminate segment width as a fraction of the track width.
pub(crate) const INDETERMINATE_FRACTION: f64 = 0.40;

/// Fill color: system blue (palette pinned in PLAN_v11113 §M1).
pub(crate) const FILL_RGBA: (f64, f64, f64, f64) = (0.0, 0.478, 1.0, 1.0);
/// Track color: translucent black.
pub(crate) const TRACK_RGBA: (f64, f64, f64, f64) = (0.0, 0.0, 0.0, 0.35);

/// Pure bar geometry over an icon of `icon_size` points. Returns
/// `(track, fill)`; `percent == None` is the indeterminate form (a
/// centered 40%-of-track segment instead of a left-anchored fill).
pub(crate) fn bar_geometry(icon_size: f64, percent: Option<u8>) -> (GeometryRect, GeometryRect) {
    let margin = icon_size * TRACK_MARGIN_FRACTION;
    let track_w = icon_size - 2.0 * margin;
    let track_h = icon_size * TRACK_HEIGHT_FRACTION;
    let track = GeometryRect {
        x: margin,
        y: margin,
        w: track_w,
        h: track_h,
    };
    let (fill_x, fill_w) = match percent {
        Some(p) => (margin, track_w * f64::from(p.clamp(0, 100)) / 100.0),
        None => (
            margin + track_w * (1.0 - INDETERMINATE_FRACTION) / 2.0,
            track_w * INDETERMINATE_FRACTION,
        ),
    };
    let fill = GeometryRect {
        x: fill_x,
        y: margin,
        w: fill_w,
        h: track_h,
    };
    (track, fill)
}

/// Badge-text fallback for a [`DockVisual`] — byte-compatible with the
/// v1.11.5 `badge_text` mapping so the degraded path IS the old behavior.
pub(crate) fn fallback_badge_text(visual: DockVisual) -> Option<String> {
    match visual {
        DockVisual::Bar(Some(p)) => Some(format!("{p}%")),
        DockVisual::Bar(None) => Some("…".to_string()),
        DockVisual::Badge => Some("!".to_string()),
        DockVisual::Clear => None,
    }
}

// ── drawing (ObjC, main thread, exception::catch guarded) ────────────

/// `Retained<NSImage>` is `!Send` in objc2-app-kit 0.2.2 (no generated
/// Send/Sync impls for NSImage, unlike NSColorSpace), so the OnceLock static
/// needs this wrapper. Soundness: NSImage is documented thread-safe by
/// Apple; the pattern mirrors the handwritten `unsafe impl Send/Sync` on
/// `WeftNotifDelegate` in macos_notifications.rs.
struct SyncImage(Retained<NSImage>);
unsafe impl Send for SyncImage {}
unsafe impl Sync for SyncImage {}

/// The pristine application icon, captured on the FIRST Bar synthesis as
/// the compositing base only. The capture is required because after the
/// first synthesis `applicationIconImage` IS the composited icon — drawing
/// onto the live icon would compound the bar every update.
static ORIGINAL_ICON: OnceLock<SyncImage> = OnceLock::new();

/// P2-5: last visual actually applied. A repeated value (flush_if_due
/// replays, streams re-sending an unchanged percent) skips re-synthesis.
static LAST_APPLIED: Mutex<Option<DockVisual>> = Mutex::new(None);

fn last_applied_is(visual: DockVisual) -> bool {
    // A poisoned lock is recovered (into_inner) — the visual cache is
    // decorative and must never panic the caller.
    *LAST_APPLIED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        == Some(visual)
}

fn record_last_applied(visual: DockVisual) {
    *LAST_APPLIED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(visual);
}

/// Apply one Dock visual (same-value replays are skipped, P2-5). Any ObjC
/// failure degrades to the badge-text fallback — never a foreign-unwind
/// abort (P0-1).
///
/// SAFETY: AppKit Dock calls; the caller is the main event-loop thread
/// (dispatch drain / flush tick), asserted via `MainThreadMarker`.
pub(crate) unsafe fn set_dock_visual(visual: DockVisual, logo_variant: LogoVariant) {
    let Some(_mtm) = MainThreadMarker::new() else {
        tracing::warn!("set_dock_visual skipped — not on main thread");
        return;
    };
    if last_applied_is(visual) {
        return;
    }
    let applied = unsafe {
        objc2::exception::catch(std::panic::AssertUnwindSafe(|| {
            apply_dock_visual(visual, logo_variant)
        }))
    };
    match applied {
        Ok(Ok(())) => record_last_applied(visual),
        Ok(Err(reason)) => {
            tracing::warn!(?reason, "dock visual synthesis failed; badge-text fallback");
            apply_badge_fallback(visual);
        }
        Err(exception) => {
            tracing::warn!(
                ?exception,
                ?visual,
                "dock visual synthesis threw a foreign unwind (caught); badge-text fallback"
            );
            apply_badge_fallback(visual);
        }
    }
}

/// The degraded path: shape the visual as badge text (v1.11.5 semantics).
/// rust-reviewer B-1: this runs right after ObjC threw (or aborted) — the
/// highest foreign-unwind-risk moment in the module — so the badge call gets
/// its own `exception::catch` (set_dock_badge's internal catch_unwind cannot
/// stop an NSException). A second failure has nothing left to degrade to:
/// log and keep the process alive.
fn apply_badge_fallback(visual: DockVisual) {
    let badge = fallback_badge_text(visual);
    // SAFETY: AppKit Dock call on the main event-loop thread (same callers
    // as set_dock_visual).
    let caught = unsafe {
        objc2::exception::catch(std::panic::AssertUnwindSafe(|| {
            crate::macos_notifications::set_dock_badge(badge.as_deref());
        }))
    };
    if caught.is_err() {
        tracing::warn!(?visual, "badge fallback also threw; dock visual dropped");
    }
    // The visual WAS applied (in fallback form) — record it so a repeat
    // doesn't thrash the badge.
    record_last_applied(visual);
}

/// Why a synthesis aborted (logged on the fallback path). `NoBaseIcon`:
/// `applicationIconImage` was nil (no bundle icon). `OffMainThread`: the
/// main-thread invariant broke (unreachable past set_dock_visual's gate;
/// distinct from NoBaseIcon so the log doesn't mislead).
#[derive(Debug)]
enum SynthAbort {
    NoBaseIcon,
    OffMainThread,
}

/// Route one visual to its AppKit effect. Runs inside `exception::catch`.
/// `Err` → caller applies the badge fallback.
unsafe fn apply_dock_visual(
    visual: DockVisual,
    logo_variant: LogoVariant,
) -> Result<(), SynthAbort> {
    match visual {
        DockVisual::Bar(percent) => synthesize_progress_icon(percent),
        DockVisual::Badge => {
            // P1-1.1: roll the icon back FIRST so a 47% bar never lingers
            // behind the "!" badge.
            crate::macos_system::set_dock_icon(logo_variant);
            crate::macos_notifications::set_dock_badge(Some("!"));
            Ok(())
        }
        DockVisual::Clear => {
            // P1-1.2/.3: restore via set_dock_icon (bundle case reinstates
            // the CFBundleIconFile through setApplicationIconImage(None)).
            crate::macos_system::set_dock_icon(logo_variant);
            crate::macos_notifications::set_dock_badge(None);
            Ok(())
        }
    }
}

/// rust-reviewer B-2: pairs `unlockFocus` with `lockFocus`. Any ObjC throw
/// between the two (drawInRect / NSColor factory / fillRect) is caught by
/// the OUTER `exception::catch` — but without this guard the unlock gets
/// skipped and AppKit's focus stack stays locked on the half-drawn image
/// for the life of the process (unrecoverable, and every later synthesis
/// fails on nested focus). Drop runs as the foreign unwind passes through
/// this frame (Itanium-ABI cleanup — the same mechanism
/// `objc2::exception::catch` itself depends on). PRECONDITION: the unwind
/// path only works while the workspace keeps the default `panic = "unwind"`
/// (no [profile] override anywhere today) — a `panic = "abort"` profile
/// would strip the landing pads and silently disable this guard.
struct FocusGuard<'a>(&'a NSImage);
impl Drop for FocusGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: releases the focus our matching lockFocus acquired; the
        // context is still valid on both the normal and unwind paths.
        #[allow(deprecated)]
        unsafe {
            self.0.unlockFocus();
        }
    }
}

/// Composite the captured base icon + track/fill band into a new NSImage
/// and install it, clearing any badge (a bar replaces badge text).
/// `percent == None` draws the static indeterminate segment (animation is
/// the v1.12 backlog item).
unsafe fn synthesize_progress_icon(percent: Option<u8>) -> Result<(), SynthAbort> {
    let Some(mtm) = MainThreadMarker::new() else {
        return Err(SynthAbort::OffMainThread);
    };
    let app = NSApplication::sharedApplication(mtm);
    let base: &NSImage = match app.applicationIconImage() {
        Some(icon) => &ORIGINAL_ICON.get_or_init(|| SyncImage(icon)).0,
        None => {
            return Err(SynthAbort::NoBaseIcon);
        }
    };
    let size = base.size();
    let (track, fill) = bar_geometry(size.width, percent);

    let composed = NSImage::initWithSize(NSImage::alloc(), size);
    #[allow(deprecated)] // lockFocus/unlockFocus: soft-deprecated by AppKit;
    // still the only NSImage drawing path without the (absent here) CGImage
    // bridge — the deprecation is a resolution-independence advisory.
    unsafe {
        composed.lockFocus();
        // B-2: scope guard — if any draw call below throws, Drop still
        // unlocks before the unwind reaches the outer catch.
        let _focus = FocusGuard(&composed);
        // Base icon, 1:1 onto the full canvas.
        base.drawInRect_fromRect_operation_fraction(
            NSRect::new(NSPoint::new(0.0, 0.0), size),
            NSRect::new(NSPoint::new(0.0, 0.0), size),
            NSCompositingOperation::SourceOver,
            1.0,
        );
        // Track band, then the fill/segment on top.
        let track_color = NSColor::colorWithSRGBRed_green_blue_alpha(
            TRACK_RGBA.0,
            TRACK_RGBA.1,
            TRACK_RGBA.2,
            TRACK_RGBA.3,
        );
        track_color.setFill();
        NSBezierPath::fillRect(NSRect::new(
            NSPoint::new(track.x, track.y),
            NSSize::new(track.w, track.h),
        ));
        let fill_color = NSColor::colorWithSRGBRed_green_blue_alpha(
            FILL_RGBA.0,
            FILL_RGBA.1,
            FILL_RGBA.2,
            FILL_RGBA.3,
        );
        fill_color.setFill();
        NSBezierPath::fillRect(NSRect::new(
            NSPoint::new(fill.x, fill.y),
            NSSize::new(fill.w, fill.h),
        ));
        // Normal path: `_focus` drops here (unlockFocus) before the image
        // is installed.
    }
    app.setApplicationIconImage(Some(&composed));
    // A bar replaces any badge text (OSC 9;4 state-3 semantics).
    crate::macos_notifications::set_dock_badge(None);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn geometry_percent_bands() {
        // 100pt icon: margin 8, track 84 wide / 12 high at y=8.
        let (track, fill) = bar_geometry(100.0, Some(50));
        assert!(approx(track.x, 8.0) && approx(track.y, 8.0));
        assert!(approx(track.w, 84.0) && approx(track.h, 12.0));
        assert!(approx(fill.x, 8.0), "determinate fill is left-anchored");
        assert!(approx(fill.w, 42.0), "50% of track width");
        assert!(approx(fill.h, track.h));
        let (track100, fill100) = bar_geometry(100.0, Some(100));
        assert!(approx(fill100.w, track100.w), "100% spans the track");
        let (_, fill0) = bar_geometry(100.0, Some(0));
        assert!(approx(fill0.w, 0.0), "0% fill is empty");
    }

    #[test]
    fn geometry_scales_with_icon_size() {
        let (_, fill) = bar_geometry(512.0, Some(25));
        let expected_track_w = 512.0 * (1.0 - 2.0 * TRACK_MARGIN_FRACTION);
        assert!(approx(fill.w, expected_track_w * 0.25));
        assert!(approx(fill.h, 512.0 * TRACK_HEIGHT_FRACTION));
    }

    #[test]
    fn geometry_clamps_out_of_range_percent() {
        // The VT layer already clamps OSC 9;4 progress; the geometry
        // defends independently.
        let (_, fill_over) = bar_geometry(100.0, Some(200));
        assert!(approx(fill_over.w, 84.0));
        let (_, fill_under) = bar_geometry(100.0, Some(255));
        assert!(approx(fill_under.w, 84.0));
    }

    #[test]
    fn geometry_indeterminate_is_centered_segment() {
        let (track, fill) = bar_geometry(100.0, None);
        assert!(approx(fill.w, track.w * INDETERMINATE_FRACTION));
        let center = fill.x + fill.w / 2.0;
        let track_center = track.x + track.w / 2.0;
        assert!(approx(center, track_center), "segment centered on track");
        assert!(approx(fill.h, track.h));
    }

    #[test]
    fn fallback_badge_text_matches_v1115_mapping() {
        assert_eq!(
            fallback_badge_text(DockVisual::Bar(Some(47))),
            Some("47%".to_string())
        );
        assert_eq!(
            fallback_badge_text(DockVisual::Bar(Some(0))),
            Some("0%".into())
        );
        assert_eq!(fallback_badge_text(DockVisual::Bar(None)), Some("…".into()));
        assert_eq!(fallback_badge_text(DockVisual::Badge), Some("!".into()));
        assert_eq!(fallback_badge_text(DockVisual::Clear), None);
    }

    #[test]
    fn dock_visual_is_value_comparable() {
        // P2-5 short-circuit premise: same-value detection by equality.
        assert_eq!(DockVisual::Bar(Some(47)), DockVisual::Bar(Some(47)));
        assert_ne!(DockVisual::Bar(Some(47)), DockVisual::Bar(Some(48)));
        assert_ne!(DockVisual::Bar(Some(47)), DockVisual::Bar(None));
        assert_ne!(DockVisual::Badge, DockVisual::Clear);
    }
}
