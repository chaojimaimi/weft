//! Runloop timer / wake scheduling helpers, moved VERBATIM out of
//! `app_runtime.rs` (v1.14.2 B3 file-budget disposition, PLAN_v1142 §3:
//! the SpawnCompleted/SpawnFailed event arms pushed the file against its
//! 800-line ceiling; this cohesive timer-wake segment moves per the
//! `resize_transaction.rs` child-module precedent — pure code motion,
//! zero behavior change).

use std::sync::atomic::Ordering;

use super::spawn_named;
use crate::tab::Tab;
use crate::AppEvent;

/// One-shot `weft-timer` watchdog for the synchronized-output (DEC 2026)
/// timeout: arms once (swap-guarded), sleeps `delay`, disarms, and fires
/// `wake`. Returns `false` when a watchdog was already pending.
pub(super) fn schedule_synchronized_output_watchdog(
    pending: std::sync::Arc<std::sync::atomic::AtomicBool>,
    delay: std::time::Duration,
    wake: impl FnOnce() + Send + 'static,
) -> bool {
    if pending.swap(true, Ordering::AcqRel) {
        return false;
    }
    // v1.12.23 audit batch 1 (bespoke): failure must reset the armed flag or
    // the watchdog stays latched; `pending` moves into the closure, so clone.
    let rearm = pending.clone();
    if let Err(e) = std::thread::Builder::new()
        .name(String::from("weft-timer"))
        .spawn(move || {
            std::thread::sleep(delay);
            pending.store(false, Ordering::Release);
            wake();
        })
    {
        // Re-arm so a future request can retry; today's wake is lost but the
        // watchdog is not permanently disabled (audit S-3).
        rearm.store(false, Ordering::Release);
        tracing::warn!(error = ?e, "weft-timer spawn failed");
    }
    true
}

/// One-shot `weft-tab-wake` per pending primary-history refresh delay
/// (v1.11.12 M-C): schedule each tab's armed refresh wake on its own timer.
pub(super) fn schedule_primary_history_refresh_wakes(
    tabs: &mut [Tab],
    proxy: &winit::event_loop::EventLoopProxy<AppEvent>,
) {
    // v1.11.12 (PLAN_v11112 M-C): shared note for ALL wakeup-class send_event
    // sites (12×: app_runtime 154/234/260/681, mouse:943, config:445,
    // palette_search_worker:276, main.rs 463/466/472, app_state:673, pane:141)
    // plus the 5 timer sites below and the probe thread:
    // EventLoopClosed = loop exited, by design silent — no wake can ever be
    // delivered after exit, so a warn would be pure teardown noise.
    for delay in tabs
        .iter_mut()
        .flat_map(Tab::take_primary_history_refresh_wake_delays)
    {
        let proxy = proxy.clone();
        spawn_named("weft-tab-wake", move || {
            std::thread::sleep(delay);
            let _ = proxy.send_event(AppEvent::Wake);
        });
    }
}
