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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

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
    let bundle_id = unsafe {
        catch(|| {
            NSBundle::mainBundle()
                .bundleIdentifier()
                .map(|id| id.to_string())
        })
    }
    .ok()
    .flatten()
    .filter(|id| !id.is_empty());
    match bundle_id {
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
    if let Some(block_id) = block_id {
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
