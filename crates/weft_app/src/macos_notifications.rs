//! v1.11.5 (PLAN_v1115 §M4): native notification sink on
//! UNUserNotificationCenter.
//!
//! # BLOCK-level hard gate (foreign unwind)
//!
//! `UNUserNotificationCenter.currentNotificationCenter` throws an
//! `NSInternalInconsistencyException` when the process has no proper
//! bundle identity (bare `cargo run` binary outside a .app). NSException
//! is a FOREIGN unwind — Rust `catch_unwind` cannot catch it and the
//! frame-crawl ABORTS the process (same lesson as `set_dock_icon`).
//! Therefore:
//!
//! 1. [`build_sink`] STOPS before touching the UN classes unless
//!    `NSBundle.mainBundle.bundleIdentifier` is `Some` and non-empty →
//!    returns a [`NoopSink`] + explicit one-time log (dev binaries degrade
//!    loudly, never crash; DMG/.app-only acceptance item).
//! 2. Belt-and-suspenders: the objc2 `exception` feature is enabled; the
//!    center construction and `addNotificationRequest` are wrapped in
//!    [`objc2::exception::catch`] so a surprise exception downgrades to a
//!    logged drop instead of an abort.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use block2::{Block, RcBlock};
use objc2::exception::catch;
use objc2::rc::Retained;
use objc2::runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{declare_class, msg_send_id, mutability, ClassType, DeclaredClass};
use objc2_foundation::{MainThreadMarker, NSBundle, NSDictionary, NSError, NSNumber, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotification,
    UNNotificationDefaultActionIdentifier, UNNotificationPresentationOptions,
    UNNotificationRequest, UNNotificationResponse, UNNotificationSound, UNUserNotificationCenter,
    UNUserNotificationCenterDelegate,
};

use crate::AppEvent;

/// One notification to post.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationPayload {
    pub title: String,
    pub body: String,
    pub sound: bool,
    /// Carried in `userInfo["block_id"]`; the activation handler (M6)
    /// resolves it back to the command block. `None` for remote
    /// (OSC 9/777) notifications, which activate the window without a
    /// jump target.
    pub block_id: Option<i64>,
}

/// Sendable notification sink. `post` is never a panic channel: every ObjC
/// touch inside is catch-guarded and failures drop the notification with a
/// log (a notification is decorative; a crash is not).
pub trait NotificationSink: Send {
    fn post(&self, payload: NotificationPayload);
}

/// Degraded sink for processes without a bundle identity (dev binaries).
/// Keeps the `NotificationSink` contract so callers never branch on the
/// sink kind.
pub struct NoopSink;

impl NotificationSink for NoopSink {
    fn post(&self, payload: NotificationPayload) {
        tracing::debug!(
            title = %payload.title,
            "notification dropped (NoopSink: no bundle identity)"
        );
    }
}

/// Process-global proxy for the delegate callbacks — MENU_PROXY pattern
/// (the delegate is a static `Retained` class holding no proxy ivar, so it
/// stays reachable from AppKit callbacks).
static NOTIFICATION_PROXY: OnceLock<winit::event_loop::EventLoopProxy<AppEvent>> = OnceLock::new();

/// The delegate must stay alive for the center's lifetime (the center holds
/// a weak reference); stored once, never dropped.
static DELEGATE: OnceLock<Retained<WeftNotifDelegate>> = OnceLock::new();

/// Monotonic notification-request identifier source (UNNotificationRequest
/// ids must be unique or the request replaces itself).
static NEXT_NOTIFICATION_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

// ── v1.11.13 (PLAN_v11113 §M2): cold-start activation gate ──────────

/// Activation readiness + cold-start backlog. ONE Mutex (P1-2): splitting
/// ready/pending into an AtomicBool + Mutex would let check-then-push and
/// flip-then-take interleave and permanently strand an id.
#[derive(Debug, Default)]
pub(crate) struct ActivationGate {
    ready: bool,
    pending: VecDeque<i64>,
}

/// What the delegate must do with an activation id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActivationRoute {
    /// App restore finished → forward through the proxy now.
    Deliver,
    /// Restore still pending → queue for the
    /// `finish_restore_notifications` replay.
    Queue,
}

/// Pure gate routing (caller holds the lock) — unit-test matrix in the
/// tests module and at the ui_events replay seam.
fn gate_route(gate: &mut ActivationGate, block_id: i64) -> ActivationRoute {
    if gate.ready {
        ActivationRoute::Deliver
    } else {
        gate.pending.push_back(block_id);
        ActivationRoute::Queue
    }
}

/// Pure ready-flip + FIFO drain (caller holds the lock). Under the same
/// lock as `gate_route`, so a click arriving "during" the flip either
/// delivers (saw ready) or queues-then-drains (saw !ready) — never lost.
fn gate_open_and_drain(gate: &mut ActivationGate) -> Vec<i64> {
    gate.ready = true;
    gate.pending.drain(..).collect()
}

/// The process-wide gate. Static because the UN delegate callback and the
/// restore completion points live in different call trees.
static ACTIVATION_GATE: Mutex<ActivationGate> = Mutex::new(ActivationGate {
    ready: false,
    pending: VecDeque::new(),
});

/// Delegate-facing routing step: lock + route. `true` = forward the
/// activation to the app (proxy path); `false` = queued (replayed by
/// `App::finish_restore_notifications`). A poisoned lock is recovered —
/// a dropped notification click must not become a panic.
pub(crate) fn route_notification_activation(block_id: i64) -> bool {
    match ACTIVATION_GATE.lock() {
        Ok(mut gate) => gate_route(&mut gate, block_id) == ActivationRoute::Deliver,
        Err(poisoned) => {
            gate_route(&mut poisoned.into_inner(), block_id) == ActivationRoute::Deliver
        }
    }
}

/// Ready-flip + FIFO drain of the queued cold-start clicks. Called by
/// `App::finish_restore_notifications` at the two restore completion
/// points (resumed Normal arm / apply_recovery_choice).
pub(crate) fn open_activation_gate() -> Vec<i64> {
    match ACTIVATION_GATE.lock() {
        Ok(mut gate) => gate_open_and_drain(&mut gate),
        Err(poisoned) => gate_open_and_drain(&mut poisoned.into_inner()),
    }
}

/// Real sink. `center` is `None` when construction already threw a foreign
/// unwind (P0 fix: NEVER re-call `currentNotificationCenter` after a throw —
/// the second call would throw again and NSException aborts the process).
/// All mutable state is atomics — safe to share across threads.
pub struct UnSink {
    center: Option<Retained<UNUserNotificationCenter>>,
    /// Authorization result from `requestAuthorizationWithOptions`
    /// (completion arrives on a background thread; true = user granted).
    authorized: Arc<AtomicBool>,
}

impl UnSink {
    /// Construct the real sink. Main-thread only, AFTER the bundle gate in
    /// [`build_sink`] has passed.
    ///
    /// HARD ORDER (plan M4, reviewer-watch item):
    /// 1. bundleIdentifier gate lives in `build_sink` — no UN class is
    ///    touched before it;
    /// 2. setDelegate BEFORE requestAuthorizationWithOptions.
    ///
    /// Authorization tradeoff (P2-1, accepted): this runs eagerly at launch,
    /// so a bundled first launch shows the system permission dialog even if
    /// the user never triggers a notification. Chosen over lazy-request
    /// because a first notification would otherwise be lost to the
    /// permission round-trip; revisit if GUI acceptance flags it.
    pub fn new(mtm: MainThreadMarker, proxy: winit::event_loop::EventLoopProxy<AppEvent>) -> Self {
        let _ = NOTIFICATION_PROXY.set(proxy);

        // Belt-and-suspenders: construction inside exception::catch (the
        // bundle gate above already excludes the known abort path). On
        // throw we degrade to center=None — every post drops with a trail.
        let center = match unsafe {
            catch(|| UNUserNotificationCenter::currentNotificationCenter())
        } {
            Ok(center) => Some(center),
            Err(exception) => {
                tracing::error!(
                    ?exception,
                    "currentNotificationCenter threw a foreign unwind (caught); notifications disabled"
                );
                None
            }
        };

        let authorized = Arc::new(AtomicBool::new(false));
        // Delegate install + authorization request only when a center
        // exists; on the degraded path (None) authorized stays false and
        // every post drops with a trail — NO second constructor call
        // (reviewer P0-2: the second call would throw again → abort).
        if let Some(center) = &center {
            let authorized_block = authorized.clone();
            let _ = unsafe {
                catch(std::panic::AssertUnwindSafe(|| {
                    // 1) delegate FIRST — a response arriving before the delegate is
                    //    installed would be lost.
                    let delegate = WeftNotifDelegate::new(mtm);
                    let _ = DELEGATE.set(delegate);
                    if let Some(delegate) = DELEGATE.get() {
                        center.setDelegate(Some(ProtocolObject::from_ref(&**delegate)));
                    }
                    // 2) THEN request authorization. The completion callback runs on
                    //    a background queue — only the atomic is touched there, then
                    //    the value round-trips to the main thread per the plan's
                    //    ordering contract.
                    let block = RcBlock::new(move |granted: Bool, _error: *mut NSError| {
                        authorized_block.store(granted.as_bool(), Ordering::Relaxed);
                        let granted = granted.as_bool();
                        dispatch2::DispatchQueue::main().exec_async(move || {
                            tracing::info!(granted, "notification authorization resolved");
                        });
                    });
                    center.requestAuthorizationWithOptions_completionHandler(
                        UNAuthorizationOptions::UNAuthorizationOptionAlert
                            | UNAuthorizationOptions::UNAuthorizationOptionSound
                            | UNAuthorizationOptions::UNAuthorizationOptionBadge,
                        &block,
                    );
                }))
            }
            .map_err(|exception| {
                tracing::error!(
                    ?exception,
                    "requestAuthorization threw a foreign unwind (caught); notifications disabled"
                );
            });
        }

        Self { center, authorized }
    }
}

impl NotificationSink for UnSink {
    fn post(&self, payload: NotificationPayload) {
        // R1 invariant pinned: post is main-thread-only (dispatch drain /
        // effect tick); AppKit would tolerate no other thread anyway.
        debug_assert!(
            objc2_foundation::MainThreadMarker::new().is_some(),
            "UnSink::post called off the main thread"
        );
        if !self.authorized.load(Ordering::Relaxed) {
            tracing::debug!(
                title = %payload.title,
                "notification dropped: authorization not granted (or not yet resolved)"
            );
            return;
        }
        let Some(center) = &self.center else {
            tracing::debug!(
                title = %payload.title,
                "notification dropped: center construction previously threw (degraded)"
            );
            return;
        };
        let result = unsafe {
            catch(std::panic::AssertUnwindSafe(|| {
                let content = UNMutableNotificationContent::new();
                content.setBody(&NSString::from_str(&payload.body));
                if payload.sound {
                    content.setSound(Some(&UNNotificationSound::defaultSound()));
                } else {
                    content.setSound(None);
                }
                if let Some(block_id) = payload.block_id {
                    // userInfo["block_id"] — read back by the activation handler
                    // (M6 navigate_to_block). from_slice keys need `&NSString`
                    // (NSCopying is on the class, not on Retained<>), so the
                    // temporaries are bound first.
                    let key = NSString::from_str("block_id");
                    let value = NSNumber::new_i64(block_id);
                    let user_info = NSDictionary::from_slice(&[&*key], &[&*value]);
                    // from_slice types the dict as NSDictionary<NSString,
                    // NSNumber>; setUserInfo expects the erased form. The
                    // runtime object is identical — cast is sound.
                    let erased: Retained<objc2_foundation::NSDictionary> =
                        Retained::cast(user_info);
                    content.setUserInfo(&erased);
                }
                let id = NEXT_NOTIFICATION_ID.fetch_add(1, Ordering::Relaxed);
                let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
                    &NSString::from_str(&format!("weft-notify-{id}")),
                    &content,
                    None,
                );
                center.addNotificationRequest_withCompletionHandler(&request, None);
                Ok::<(), ()>(())
            }))
        }; // closure }, AssertUnwindSafe ), catch ), unsafe block }
        if result.is_err() {
            tracing::warn!(
                title = %payload.title,
                "notification post threw a foreign unwind (caught); dropped"
            );
        }
    }
}

/// Bundle-identity probe shared by [`build_sink`] and
/// [`install_early_delegate`]. `Some(id)` = proper .app identity. The
/// NSBundle query does not depend on NSApplication, so it is safe at
/// main() time — before the event loop starts (architect-verified,
/// PLAN_v11113 §M2).
fn probe_bundle_identity() -> Option<String> {
    unsafe {
        catch(|| {
            NSBundle::mainBundle()
                .bundleIdentifier()
                .map(|id| id.to_string())
        })
    }
    .ok()
    .flatten()
    .filter(|id| !id.is_empty())
}

/// v1.11.13 (PLAN_v11113 §M2): install the UN delegate at main() time —
/// BEFORE winit's `didFinishLaunching` (which runs inside `run_app`) — so
/// a COLD-START notification click (app relaunched by clicking a
/// notification, still restoring) is delivered to `WeftNotifDelegate`
/// instead of dropped (Apple contract: set the delegate before launch
/// completes). Same bundle-identity hard gate as [`build_sink`]; the
/// delegate only routes responses, so a config-disabled notification
/// system gains zero side effects (the post side keeps its config gate).
///
/// Idempotent with [`UnSink::new`]: the DELEGATE static is get-or-init
/// (P2-1) and the NOTIFICATION_PROXY set is a OnceLock — whichever
/// installer runs second reuses the same delegate instance.
pub fn install_early_delegate(proxy: &winit::event_loop::EventLoopProxy<AppEvent>) {
    if probe_bundle_identity().is_none() {
        tracing::debug!("early notification delegate skipped: no bundle identity (dev binary)");
        return;
    }
    let _ = NOTIFICATION_PROXY.set(proxy.clone());
    let Some(mtm) = MainThreadMarker::new() else {
        tracing::warn!("early notification delegate skipped: not on main thread");
        return;
    };
    let _ = unsafe {
        catch(std::panic::AssertUnwindSafe(|| {
            let center = UNUserNotificationCenter::currentNotificationCenter();
            // P2-1 get-or-init: shared static with UnSink::new — its later
            // `DELEGATE.set` is a natural no-op once we are installed.
            let delegate = DELEGATE.get_or_init(|| WeftNotifDelegate::new(mtm));
            center.setDelegate(Some(ProtocolObject::from_ref(&**delegate)));
        }))
    }
    .map_err(|exception| {
        tracing::warn!(
            ?exception,
            "early delegate install threw a foreign unwind (caught); \
             the resumed() install path remains"
        );
    });
}

/// Build the process's notification sink. Call once from `resumed` (main
/// thread).
///
/// The BLOCK gate lives here: when `bundleIdentifier` is missing or empty,
/// no UNUserNotificationCenter class is ever touched — the caller gets a
/// [`NoopSink`] and keeps running (log once, degrade loudly, never abort).
pub fn build_sink(
    mtm: MainThreadMarker,
    proxy: winit::event_loop::EventLoopProxy<AppEvent>,
) -> Box<dyn NotificationSink> {
    match probe_bundle_identity() {
        Some(id) => {
            tracing::debug!(
                bundle_id = %id,
                "notification sink: bundle identity ok, constructing UNUserNotificationCenter"
            );
            Box::new(UnSink::new(mtm, proxy))
        }
        None => {
            tracing::warn!(
                "notification sink degraded to NoopSink: no bundle identifier (dev binary?); \
                 notifications require a bundled .app (acceptance list)"
            );
            Box::new(NoopSink)
        }
    }
}

// The delegate class is zero-state (no ivars, immutable mutability) — the
// objc2 declared class does not auto-derive Send/Sync for the raw objc
// object handle; sharing the instance across threads is sound because every
// callback only reads immutable state and forwards through the proxy.
unsafe impl Send for WeftNotifDelegate {}
unsafe impl Sync for WeftNotifDelegate {}

// Same rationale for the sink: the center handle is only ever touched on
// the main thread (post callers are the dispatch drain / effect tick), and
// all shared mutable state is atomics. `NotificationSink: Send` requires
// this; Sync is deliberately NOT implemented (no cross-thread borrows).
unsafe impl Send for UnSink {}

/// v1.11.5 (PLAN_v1115 §M7): Dock icon badge text via `NSDockTile
/// .badgeLabel` — typed objc2-app-kit path (`NSApplication::dockTile()` +
/// `NSDockTile::setBadgeLabel`, NO raw msg_send). The text mapping lives in
/// `notify_policy::badge_text` (42% / … / !); updates are debounced to
/// 200 ms latest-wins by the app layer. `None` clears the badge (OSC 9;4;0).
///
/// Lives here (not macos_system.rs) because the badge is the notification
/// system's Dock-facing output; macos_system.rs stays under its
/// architecture-gate ceiling.
///
/// Same panic discipline as `set_dock_icon` (catch_unwind belt): failures
/// leave the current badge untouched — the Dock badge is decorative.
///
/// SAFETY: dockTile/setBadgeLabel are main-thread AppKit calls; the caller
/// is the main event-loop thread (dispatch table drain).
pub(super) unsafe fn set_dock_badge(badge: Option<&str>) {
    use objc2_app_kit::NSApplication;
    use objc2_foundation::{MainThreadMarker, NSString};

    let Some(mtm) = MainThreadMarker::new() else {
        tracing::warn!("set_dock_badge skipped — not on main thread");
        return;
    };
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let app = NSApplication::sharedApplication(mtm);
        // Creating the NSString per call is cheap: the caller debounces
        // badge updates to 200 ms and the string is tiny.
        let label = badge.map(NSString::from_str);
        app.dockTile().setBadgeLabel(label.as_deref());
    }));
}

/// Route a notification response to the app. Extracted so the delegate
/// callback stays minimal: completion FIRST, then exec_async + proxy.
///
/// M4 hard rules:
/// - `completion_handler` is called SYNCHRONOUSLY inside the callback —
///   never deferred into `exec_async` (the center would leak the
///   completion).
/// - the ONLY path into winit state is dispatch2-main + proxy (red line).
fn handle_notification_response(
    _center: &UNUserNotificationCenter,
    response: &UNNotificationResponse,
    completion_handler: &Block<dyn Fn()>,
) {
    // 1) completion first, synchronous — OUTSIDE the catch (hard rule M4;
    //    the center would leak the completion if it were deferred).
    completion_handler.call(());

    // 2-4) activation parse + userInfo read, inside exception::catch
    //    (reviewer P1-2: this face is reachable from arbitrary AppKit
    //    state — an unexpected object must degrade to "no jump", never a
    //    foreign unwind through the UN framework).
    let parsed = unsafe {
        catch(std::panic::AssertUnwindSafe(|| {
            let is_activation = response.actionIdentifier().to_string()
                == UNNotificationDefaultActionIdentifier.to_string();
            if !is_activation {
                return None;
            }
            let user_info = response.notification().request().content().userInfo();
            let key = NSString::from_str("block_id");
            user_info.objectForKey(&key).and_then(|obj| {
                // Type-guard before the raw selector: only our own NSNumber
                // carries longLongValue (reviewer P1-2 fix). `objectForKey`
                // erases to AnyObject — isKindOfClass is the only sound
                // check available here.
                let is_number: bool = objc2::msg_send![&*obj, isKindOfClass: NSNumber::class()];
                if !is_number {
                    return None;
                }
                let value: i64 = objc2::msg_send![&*obj, longLongValue];
                Some(value)
            })
        }))
    };
    let block_id = match parsed {
        Ok(block_id) => block_id,
        Err(exception) => {
            tracing::warn!(
                ?exception,
                "notification response parse threw (caught); no jump"
            );
            None
        }
    };

    // 5) forward to the main thread.
    //    v1.11.13 (PLAN_v11113 §M2): gate FIRST, inside the lock — a
    //    cold-start click arriving before the restore finished queues for
    //    the finish_restore_notifications replay instead of hitting the
    //    "该命令块已被清理" toast (the block exists in the history store;
    //    the app just isn't hydrated yet).
    if let Some(block_id) = block_id {
        if route_notification_activation(block_id) {
            if let Some(proxy) = NOTIFICATION_PROXY.get() {
                dispatch2::DispatchQueue::main().exec_async(move || {
                    // v1.11.12 (PLAN_v11112 M-C): a failure here silently drops
                    // the notification click (no jump to the command block).
                    // (if-let instead of inspect_err: MSRV 1.75 < 1.76)
                    if let Err(e) = proxy.send_event(AppEvent::NotificationActivated(block_id)) {
                        tracing::warn!(
                            error = %e,
                            block_id,
                            "send_event failed: notification click lost"
                        );
                    }
                });
            } else {
                // rust-reviewer M2: unreachable under the current install
                // order (install_early_delegate injects the proxy before the
                // delegate exists) — but if that invariant ever breaks, make
                // the loss audible instead of silently dropping the click.
                tracing::warn!(block_id, "activation delivered with no proxy: dropped");
            }
        }
    }
}

impl WeftNotifDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        unsafe { msg_send_id![super(mtm.alloc().set_ivars(())), init] }
    }
}

declare_class!(
    #[derive(Debug)]
    struct WeftNotifDelegate;

    unsafe impl ClassType for WeftNotifDelegate {
        type Super = NSObject;
        // P2-3 (reviewer): Send/Sync are the HANDWRITTEN `unsafe impl`s at
        // the bottom of this file (auto-deriving does not exist for the raw
        // objc object handle). Soundness: zero ivars, immutable mutability;
        // callbacks read only method parameters and the static proxy —
        // never app or winit state. The static `DELEGATE` OnceLock needs
        // Send+Sync; MainThreadOnly would force !Send and fail the static.
        type Mutability = mutability::Immutable;
        const NAME: &'static str = "WeftNotifDelegate";
    }

    impl DeclaredClass for WeftNotifDelegate {
        type Ivars = ();
    }

    unsafe impl NSObjectProtocol for WeftNotifDelegate {}

    // The two protocol methods the plan requires (both optional in the
    // protocol, implemented here): the response route (activation) and the
    // willPresent route (foreground banner).
    unsafe impl UNUserNotificationCenterDelegate for WeftNotifDelegate {
        #[method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:)]
        fn did_receive_notification_response(
            &self,
            center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion_handler: &Block<dyn Fn()>,
        ) {
            handle_notification_response(center, response, completion_handler);
        }

        #[method(userNotificationCenter:willPresentNotification:withCompletionHandler:)]
        fn will_present_notification(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            completion_handler: &Block<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            // Foreground presentation: banner only. Sound is deliberately
            // excluded — the payload-driven sound choice is the app's
            // business and this callback has no access to it.
            completion_handler.call((
                UNNotificationPresentationOptions(
                    UNNotificationPresentationOptions::UNNotificationPresentationOptionBanner
                        .bits(),
                ),
            ));
        }
    }
);

// ── v1.11.13 (PLAN_v11113 §M2): gate pure-logic tests (LOCAL gates — the
// process-global ACTIVATION_GATE lifecycle is covered once, at the
// ui_events replay seam, to keep cross-test global state deterministic).

#[cfg(test)]
mod gate_tests {
    use super::*;

    #[test]
    fn gate_queues_while_not_ready_and_replays_fifo() {
        let mut gate = ActivationGate::default();
        assert_eq!(gate_route(&mut gate, 7), ActivationRoute::Queue);
        assert_eq!(gate_route(&mut gate, 3), ActivationRoute::Queue);
        // Flip + drain: FIFO order preserved, backlog emptied.
        assert_eq!(gate_open_and_drain(&mut gate), vec![7, 3]);
    }

    #[test]
    fn gate_delivers_directly_once_ready() {
        let mut gate = ActivationGate::default();
        assert_eq!(
            gate_open_and_drain(&mut gate),
            Vec::<i64>::new(),
            "open on an empty gate drains nothing"
        );
        assert_eq!(gate_route(&mut gate, 9), ActivationRoute::Deliver);
        // A ready-gate click never lands in the backlog.
        assert!(gate.pending.is_empty());
    }

    #[test]
    fn gate_drain_twice_yields_empty() {
        let mut gate = ActivationGate::default();
        let _ = gate_route(&mut gate, 1);
        assert_eq!(gate_open_and_drain(&mut gate), vec![1]);
        assert_eq!(
            gate_open_and_drain(&mut gate),
            Vec::<i64>::new(),
            "repeat take is empty (no double replay)"
        );
    }

    #[test]
    fn gate_flip_and_click_ordering_never_strands() {
        // P1-2 invariant at the pure-fn level: whichever order the two
        // lock holders run in, the id is delivered or replayed — never
        // lost (the reason the gate is ONE Mutex, not AtomicBool+Mutex).
        let mut click_first = ActivationGate::default();
        let _ = gate_route(&mut click_first, 5);
        assert_eq!(gate_open_and_drain(&mut click_first), vec![5]);
        let mut flip_first = ActivationGate::default();
        let _ = gate_open_and_drain(&mut flip_first);
        assert_eq!(gate_route(&mut flip_first, 6), ActivationRoute::Deliver);
    }
}
