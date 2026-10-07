//! v1.13.0 (PLAN_v1.13.0_SPARKLE WP1): Sparkle 2 auto-update bridge — the
//! production promotion of the spike (`updater_spike.rs`, deleted; evidence
//! in docs/SPIKE_V1.13.0_SPARKLE_BRIDGE.md).
//!
//! Sparkle.framework is loaded at RUNTIME through NSBundle from
//! `Contents/Frameworks/` — no link-time dependency, so a bare `cargo run`
//! binary (no bundle) degrades to a fully functional terminal with update
//! features off (plan R1: `framework_available() == false`).
//!
//! objc2 0.5.2 retain semantics pinned by the spike report §三: `alloc`
//! yields a bare `Allocated<T>` consumed BY VALUE by the init family via
//! `msg_send_id!`; property getters return +0 autoreleased pointers — plain
//! `msg_send!` only (`msg_send_id!` would over-release); `exception::catch`'s
//! Err is `Option<Retained<Exception>>` (a foreign unwind is `None`) and
//! `Exception::name()` is private — log via `class().name()`.
//!
//! Controller lifetime: leaked + `static AtomicUsize` (spike-verified). An
//! `App` field would drag the non-`Send` Obj-C object across the event-loop
//! `Send` bounds — deliberately never done (plan WP1).
//!
//! Thread contract: every entry point runs on the MAIN thread (App::new is
//! pre-runloop main thread; menu items and the Settings button land in
//! `execute_action` on the event-loop thread). The `exception::catch`
//! guards below rely on that contract.
//!
//! File split (plan WP1: 单文件预算 ≤320, 超限拆 `updater/{mod,bridge}.rs`):
//! this module owns the tier state machine and public API; [`bridge`]
//! (`bridge.rs`) owns the raw objc2 leaf calls (bundle load, controller
//! init, path resolution).

mod bridge;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use objc2::exception::catch;
use objc2::msg_send;
use objc2::runtime::AnyObject;
use tracing::{info, warn};
use weft_core::config::UpdateCheckTier;

use self::bridge::{
    describe_exception, framework_path_next_to_exe, init_controller, load_framework,
};

/// Raw pointer to the leaked `SPUStandardUpdaterController` (usize because
/// Obj-C objects are not `Send`; only ever dereferenced on the main thread).
/// 0 = no controller.
static CONTROLLER_RAW: AtomicUsize = AtomicUsize::new(0);
/// `init_controller` has been attempted (success or failure) — never retry:
/// a broken Sparkle bundle must not spam warnings on every config save.
static CONTROLLER_ATTEMPTED: AtomicBool = AtomicBool::new(false);
/// `startUpdater` has been dispatched. Sparkle requires a started updater
/// before `checkForUpdates` / scheduled background checks work.
static UPDATER_STARTED: AtomicBool = AtomicBool::new(false);
/// Sparkle.framework loaded from `Contents/Frameworks` (R1 degrade signal).
static FRAMEWORK_LOADED: AtomicBool = AtomicBool::new(false);
/// Framework load attempted — same no-retry discipline as the controller.
static LOAD_ATTEMPTED: AtomicBool = AtomicBool::new(false);
/// Current tier mirror for the menu-triggered on-demand init (written by
/// [`init`]/[`apply_tier`], read by [`check_for_updates`]).
static CURRENT_TIER: AtomicUsize = AtomicUsize::new(0);

/// App::new entry (plan WP1 timing: at the end of `App::new`, after
/// `config_state` is built — the tier source is closed here, no second
/// config load; still main-thread pre-runloop, spike-equivalent).
pub fn init(tier: UpdateCheckTier) {
    info!(?tier, "updater: init");
    apply_tier(tier);
}

/// Re-apply a tier at runtime (Settings change / config hot reload).
///
/// 全档语义 (plan WP1, r2):
/// - `Daily` → auto_checks = true, interval 86 400 s;
/// - `Manual` → auto_checks = false;
/// - `Off` → auto_checks = false too (same property face as Manual); the
///   ONLY difference is that Off never late-starts a never-started updater;
/// - `automaticallyDownloadsUpdates` is NEVER set programmatically (D5):
///   Info.plist `SUAutomaticallyUpdate=false` is the default and Sparkle
///   owns the user's checkbox in UserDefaults afterwards — setting it here
///   would silently roll back the user's choice on every init/apply;
/// - `resetUpdateCycleAfterShortDelay` is deliberately not called
///   (SPUUpdater.h:373 — the property setters re-arm the cycle themselves).
pub fn apply_tier(tier: UpdateCheckTier) {
    store_tier(tier);
    if !load_framework_once() || !ensure_controller_once() {
        return; // R1 degrade path — nothing to drive
    }
    apply_tier_properties(tier);
    // Late start (r2): only Manual/Daily start a never-started updater;
    // Off never does.
    if tier != UpdateCheckTier::Off {
        start_updater_once();
    }
}

/// Menu / Settings "Check Now" entry. ANY tier performs the one-shot check
/// (plan D4): a user-initiated `checkForUpdates` does not touch
/// `automaticallyChecksForUpdates` and cannot re-arm the schedule. When the
/// updater was never started (Off since launch), this is the menu-triggered
/// ON-DEMAND init with the plan-pinned order: `controller(start:false)` →
/// tier properties → `startUpdater` → `checkForUpdates` (properties BEFORE
/// start so `startUpdater` can never re-arm the automatic schedule from the
/// plist defaults).
pub fn check_for_updates() {
    if !load_framework_once() || !ensure_controller_once() {
        return;
    }
    if !UPDATER_STARTED.load(Ordering::SeqCst) {
        apply_tier_properties(current_tier());
        start_updater_once();
    }
    dispatch_check();
}

/// True when Sparkle.framework was loaded successfully (Settings status row).
pub fn framework_available() -> bool {
    FRAMEWORK_LOADED.load(Ordering::SeqCst)
}

/// 纯函数：档位 → Sparkle 属性面 (auto_checks, interval)。autoDownloads 永不
/// 编程设置（见 D5 / [`apply_tier`]）。
pub(crate) fn tier_properties(tier: UpdateCheckTier) -> (bool, f64) {
    match tier {
        UpdateCheckTier::Daily => (true, 86_400.0),
        UpdateCheckTier::Manual | UpdateCheckTier::Off => (false, 86_400.0),
    }
}

fn store_tier(tier: UpdateCheckTier) {
    CURRENT_TIER.store(tier as usize, Ordering::SeqCst);
}

fn current_tier() -> UpdateCheckTier {
    match CURRENT_TIER.load(Ordering::SeqCst) {
        1 => UpdateCheckTier::Manual,
        2 => UpdateCheckTier::Off,
        _ => UpdateCheckTier::Daily,
    }
}

/// Load the Sparkle framework once per process. `false` on any failure —
/// callers treat it as the R1 degrade path and bail out.
fn load_framework_once() -> bool {
    if FRAMEWORK_LOADED.load(Ordering::SeqCst) {
        return true;
    }
    if LOAD_ATTEMPTED.swap(true, Ordering::SeqCst) {
        return false; // previous attempt failed — do not retry / spam
    }
    let Some(framework_path) = framework_path_next_to_exe() else {
        warn!("updater: no Sparkle.framework next to the executable — update features disabled");
        return false;
    };
    info!(?framework_path, "updater: loading framework bundle");
    if load_framework(&framework_path) {
        FRAMEWORK_LOADED.store(true, Ordering::SeqCst);
        true
    } else {
        false
    }
}

/// Create the controller once. Always `initWithStartingUpdater:NO`: tier
/// properties are applied BEFORE `startUpdater` so the plist defaults can
/// never re-arm the automatic schedule ahead of the configured tier.
fn ensure_controller_once() -> bool {
    if CONTROLLER_RAW.load(Ordering::SeqCst) != 0 {
        return true;
    }
    if CONTROLLER_ATTEMPTED.swap(true, Ordering::SeqCst) {
        return false; // previous init failed — do not retry
    }
    match init_controller(false) {
        Ok(raw) => {
            CONTROLLER_RAW.store(raw, Ordering::SeqCst);
            true
        }
        Err(reason) => {
            warn!(reason, "updater: controller init failed");
            false
        }
    }
}

fn start_updater_once() {
    if UPDATER_STARTED.swap(true, Ordering::SeqCst) {
        return; // already started
    }
    let raw = CONTROLLER_RAW.load(Ordering::SeqCst);
    if raw == 0 {
        UPDATER_STARTED.store(false, Ordering::SeqCst);
        return;
    }
    // SAFETY: the leaked controller is only dereferenced on the main thread
    // (see the module thread contract).
    let controller: &AnyObject = unsafe { &*(raw as *const AnyObject) };
    let outcome = unsafe {
        catch(std::panic::AssertUnwindSafe(|| {
            let _: () = msg_send![controller, startUpdater];
        }))
    };
    match outcome {
        Ok(()) => info!("updater: startUpdater dispatched"),
        Err(exception) => {
            UPDATER_STARTED.store(false, Ordering::SeqCst);
            warn!(
                reason = describe_exception(exception),
                "updater: startUpdater failed"
            );
        }
    }
}

/// Apply the tier's property face to the live SPUUpdater (+0 autoreleased
/// getter — plain `msg_send!` only, spike §三).
fn apply_tier_properties(tier: UpdateCheckTier) {
    let raw = CONTROLLER_RAW.load(Ordering::SeqCst);
    if raw == 0 {
        return;
    }
    let (auto_checks, interval) = tier_properties(tier);
    // SAFETY: main-thread-only dereference of the leaked controller.
    let controller: &AnyObject = unsafe { &*(raw as *const AnyObject) };
    let outcome = unsafe {
        catch(std::panic::AssertUnwindSafe(
            || -> Result<(bool, f64, bool), String> {
                let updater: *mut AnyObject = msg_send![controller, updater];
                let updater: &AnyObject = updater.as_ref().ok_or("updater getter returned nil")?;
                let _: () = msg_send![updater, setAutomaticallyChecksForUpdates: auto_checks];
                let _: () = msg_send![updater, setUpdateCheckInterval: interval];
                let read_checks: bool = msg_send![updater, automaticallyChecksForUpdates];
                let read_interval: f64 = msg_send![updater, updateCheckInterval];
                let can_check: bool = msg_send![updater, canCheckForUpdates];
                Ok((read_checks, read_interval, can_check))
            },
        ))
    };
    match outcome {
        Ok(Ok((checks, interval, can_check))) => {
            info!(
                checks,
                interval,
                can_check,
                ?tier,
                "updater: tier properties applied"
            )
        }
        Ok(Err(reason)) => warn!(reason, ?tier, "updater: property apply failed"),
        Err(exception) => warn!(
            reason = describe_exception(exception),
            ?tier,
            "updater: NSException applying tier properties"
        ),
    }
}

/// One-shot user-visible check (the standard Sparkle alert UI; spike §二 4).
fn dispatch_check() {
    let raw = CONTROLLER_RAW.load(Ordering::SeqCst);
    if raw == 0 {
        return;
    }
    // SAFETY: main-thread-only dereference of the leaked controller.
    let controller: &AnyObject = unsafe { &*(raw as *const AnyObject) };
    let outcome = unsafe {
        catch(std::panic::AssertUnwindSafe(|| -> Result<(), String> {
            let updater: *mut AnyObject = msg_send![controller, updater];
            let updater: &AnyObject = updater.as_ref().ok_or("updater getter returned nil")?;
            let _: () = msg_send![updater, checkForUpdates];
            Ok(())
        }))
    };
    match outcome {
        Ok(Ok(())) => info!("updater: checkForUpdates dispatched"),
        Ok(Err(reason)) => warn!(reason, "updater: checkForUpdates failed"),
        Err(exception) => warn!(
            reason = describe_exception(exception),
            "updater: NSException during checkForUpdates"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_properties_maps_all_three_tiers() {
        // Daily = scheduled daily auto-check; Manual/Off share the same
        // property face (auto-check off) — plan WP1 全档语义.
        assert_eq!(tier_properties(UpdateCheckTier::Daily), (true, 86_400.0));
        assert_eq!(tier_properties(UpdateCheckTier::Manual), (false, 86_400.0));
        assert_eq!(tier_properties(UpdateCheckTier::Off), (false, 86_400.0));
    }

    #[test]
    fn current_tier_round_trips_through_the_mirror() {
        for tier in [
            UpdateCheckTier::Daily,
            UpdateCheckTier::Manual,
            UpdateCheckTier::Off,
        ] {
            store_tier(tier);
            assert_eq!(current_tier(), tier);
        }
        store_tier(UpdateCheckTier::Daily); // restore the factory default
    }
}
