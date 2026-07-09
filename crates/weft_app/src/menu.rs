//! v1.1: Native macOS menu bar (NSMenu).
//!
//! Builds a 6-menu bar (Weft/File/Edit/View/Find/Window) whose items map to
//! the existing `Action` enum. Clicks flow through a custom ObjC target class
//! (`WeftMenuTarget`) → `EventLoopProxy::send_event(AppEvent::MenuAction)` →
//! `App::user_event` → `execute_action` — the same dispatch keybindings use, so
//! menu items and shortcuts stay in sync by construction.
//!
//! `keyEquivalent`s are intentionally left empty for weft-action items: weft's
//! `KeyBindings` handle all shortcuts from winit keyboard events, and an
//! NSMenuItem `keyEquivalent` would intercept those events before winit sees
//! them. Standard system items (About/Hide/Quit) DO keep their key equivalents
//! (⌘H/⌘Q) since those route to NSApplication stock selectors, not weft.

use std::sync::OnceLock;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, Sel};
use objc2::{declare_class, msg_send, msg_send_id, mutability, sel, ClassType, DeclaredClass};
use objc2_app_kit::{NSApplication, NSMenu, NSMenuItem};
use objc2_foundation::{ns_string, MainThreadMarker, NSString};
use winit::event_loop::EventLoopProxy;

use weft_core::config::Action;

use crate::AppEvent;

// ── Action ↔ isize conversion ──────────────────────────────────────────
// `Action` has no `TryFrom<u8>`; do an explicit round-trip so the menu item
// `tag` carries the action deterministically. Any unknown tag is ignored.

fn action_to_isize(a: Action) -> isize {
    a as isize
}

fn action_from_isize(tag: isize) -> Option<Action> {
    // Match on the discriminant values (declaration order). Keep in sync with
    // `weft_core::config::Action`.
    Some(match tag {
        0 => Action::Copy,
        1 => Action::Paste,
        2 => Action::ReloadConfig,
        3 => Action::ScrollPageUp,
        4 => Action::ScrollPageDown,
        5 => Action::ScrollLineUp,
        6 => Action::ScrollLineDown,
        7 => Action::ScrollToTop,
        8 => Action::ScrollToBottom,
        9 => Action::ToggleBlockPanel,
        10 => Action::ToggleCommandPalette,
        11 => Action::ZoomIn,
        12 => Action::ZoomOut,
        13 => Action::ZoomReset,
        14 => Action::FindInGrid,
        15 => Action::ToggleTheme,
        16 => Action::NewTab,
        17 => Action::CloseTab,
        18 => Action::NextTab,
        19 => Action::PrevTab,
        20 => Action::ToggleSettings,
        _ => return None,
    })
}

// ── Target class ────────────────────────────────────────────────────────

// The ObjC action target that all weft menu items point at. Each menu item
// carries its `Action` in `NSMenuItem.tag` (set via `setTag:`); the single
// `weftAction:` handler reads that tag and forwards it to the main thread via
// a process-global `EventLoopProxy`.
declare_class!(
    #[derive(Debug)]
    struct WeftMenuTarget;

    unsafe impl ClassType for WeftMenuTarget {
        type Super = NSObject;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "WeftMenuTarget";
    }

    impl DeclaredClass for WeftMenuTarget {
        type Ivars = ();
    }

    unsafe impl NSObjectProtocol for WeftMenuTarget {}

    unsafe impl WeftMenuTarget {
        /// The single selector all weft menu items use. Reads the sender's
        /// `tag` (= `Action as isize`) and forwards it as `MenuAction`.
        #[method(weftAction:)]
        fn weft_action(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            if let Some(action) = action_from_isize(tag) {
                if let Some(proxy) = MENU_PROXY.get() {
                    let _ = proxy.send_event(AppEvent::MenuAction(action));
                }
            }
        }
    }
);

impl WeftMenuTarget {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        unsafe { msg_send_id![super(mtm.alloc().set_ivars(())), init] }
    }
}

/// Process-global EventLoopProxy set once in `resumed` so the menu target can
/// send events without holding the proxy in an ObjC ivar.
static MENU_PROXY: OnceLock<EventLoopProxy<AppEvent>> = OnceLock::new();

/// Install the menu bar on the shared NSApplication. Call once from `resumed`
/// (which runs on the main thread, after winit installs its default menu).
/// Replaces winit's default menu with weft's full 6-menu bar.
pub fn install(mtm: MainThreadMarker, proxy: EventLoopProxy<AppEvent>) {
    let _ = MENU_PROXY.set(proxy);

    let app = NSApplication::sharedApplication(mtm);
    let target = WeftMenuTarget::new(mtm);

    let menubar = NSMenu::new(mtm);
    // Weft (app) menu — re-create standard items so About/Services/Hide/Quit
    // keep working after we replace winit's default menu.
    let app_menu = build_app_menu(mtm);
    let app_item = NSMenuItem::new(mtm);
    app_item.setSubmenu(Some(&app_menu));
    menubar.addItem(&app_item);

    // File / Edit / View / Find / Window.
    add_submenu(
        &menubar,
        mtm,
        &target,
        ns_string!("File"),
        &[
            action_item(mtm, &target, ns_string!("New Tab"), Action::NewTab),
            action_item(mtm, &target, ns_string!("Close Tab"), Action::CloseTab),
            sep(mtm),
            action_item(
                mtm,
                &target,
                ns_string!("Settings…"),
                Action::ToggleSettings,
            ),
        ],
    );
    add_submenu(
        &menubar,
        mtm,
        &target,
        ns_string!("Edit"),
        &[
            action_item(mtm, &target, ns_string!("Copy"), Action::Copy),
            action_item(mtm, &target, ns_string!("Paste"), Action::Paste),
        ],
    );
    add_submenu(
        &menubar,
        mtm,
        &target,
        ns_string!("View"),
        &[
            action_item(
                mtm,
                &target,
                ns_string!("Toggle Theme"),
                Action::ToggleTheme,
            ),
            action_item(
                mtm,
                &target,
                ns_string!("History Panel"),
                Action::ToggleBlockPanel,
            ),
            action_item(
                mtm,
                &target,
                ns_string!("Command Palette"),
                Action::ToggleCommandPalette,
            ),
            sep(mtm),
            action_item(mtm, &target, ns_string!("Zoom In"), Action::ZoomIn),
            action_item(mtm, &target, ns_string!("Zoom Out"), Action::ZoomOut),
            action_item(mtm, &target, ns_string!("Reset Zoom"), Action::ZoomReset),
            sep(mtm),
            action_item(
                mtm,
                &target,
                ns_string!("Reload Config"),
                Action::ReloadConfig,
            ),
        ],
    );
    add_submenu(
        &menubar,
        mtm,
        &target,
        ns_string!("Find"),
        &[
            action_item(mtm, &target, ns_string!("Find…"), Action::FindInGrid),
            sep(mtm),
            action_item(
                mtm,
                &target,
                ns_string!("Scroll to Top"),
                Action::ScrollToTop,
            ),
            action_item(
                mtm,
                &target,
                ns_string!("Scroll to Bottom"),
                Action::ScrollToBottom,
            ),
            action_item(mtm, &target, ns_string!("Page Up"), Action::ScrollPageUp),
            action_item(
                mtm,
                &target,
                ns_string!("Page Down"),
                Action::ScrollPageDown,
            ),
            action_item(mtm, &target, ns_string!("Line Up"), Action::ScrollLineUp),
            action_item(
                mtm,
                &target,
                ns_string!("Line Down"),
                Action::ScrollLineDown,
            ),
        ],
    );
    add_submenu(
        &menubar,
        mtm,
        &target,
        ns_string!("Window"),
        &[
            action_item(mtm, &target, ns_string!("Next Tab"), Action::NextTab),
            action_item(mtm, &target, ns_string!("Previous Tab"), Action::PrevTab),
        ],
    );

    // Keep the target alive for the app's lifetime — NSMenuItem retains its
    // target weakly, so we must not let it drop. One per process.
    std::mem::forget(target);

    app.setMainMenu(Some(&menubar));
}

/// Build the app (Weft) menu with standard macOS items wired to system
/// selectors: About, Services, Hide, Hide Others, Show All, Quit.
fn build_app_menu(mtm: MainThreadMarker) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    menu.addItem(&stock_item(
        mtm,
        ns_string!("About Weft"),
        Some(sel!(orderFrontStandardAboutPanel:)),
        ns_string!(""),
    ));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    // Services submenu — macOS fills it in; we just provide the placeholder
    // menu and register it via setServicesMenu.
    let services_item = stock_item(mtm, ns_string!("Services"), None, ns_string!(""));
    let services_menu = NSMenu::new(mtm);
    services_item.setSubmenu(Some(&services_menu));
    menu.addItem(&services_item);
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&stock_item(
        mtm,
        ns_string!("Hide Weft"),
        Some(sel!(hide:)),
        ns_string!("h"),
    ));
    menu.addItem(&stock_item(
        mtm,
        ns_string!("Hide Others"),
        Some(sel!(hideOtherApplications:)),
        ns_string!(""),
    ));
    menu.addItem(&stock_item(
        mtm,
        ns_string!("Show All"),
        Some(sel!(unhideAllApplications:)),
        ns_string!(""),
    ));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&stock_item(
        mtm,
        ns_string!("Quit Weft"),
        Some(sel!(terminate:)),
        ns_string!("q"),
    ));
    // Register the Services submenu so macOS populates it with services.
    let app = NSApplication::sharedApplication(mtm);
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        app.setServicesMenu(Some(&services_menu));
    }));
    menu
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Create a menu item that targets `WeftMenuTarget` with selector
/// `weftAction:` and carries `action` in its tag.
fn action_item(
    mtm: MainThreadMarker,
    target: &WeftMenuTarget,
    title: &NSString,
    action: Action,
) -> Retained<NSMenuItem> {
    let mi = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc(),
            title,
            Some(sel!(weftAction:)),
            ns_string!(""),
        )
    };
    unsafe {
        mi.setTarget(Some(target));
        mi.setTag(action_to_isize(action));
    }
    mi
}

/// A separator menu item.
fn sep(mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    NSMenuItem::separatorItem(mtm)
}

/// A menu item using a stock system selector (no custom target).
fn stock_item(
    mtm: MainThreadMarker,
    title: &NSString,
    action: Option<Sel>,
    key: &NSString,
) -> Retained<NSMenuItem> {
    unsafe { NSMenuItem::initWithTitle_action_keyEquivalent(mtm.alloc(), title, action, key) }
}

/// Create a titled submenu, add the given items, and attach it to `parent`.
fn add_submenu(
    parent: &NSMenu,
    mtm: MainThreadMarker,
    _target: &WeftMenuTarget,
    title: &NSString,
    items: &[Retained<NSMenuItem>],
) {
    let sub = NSMenu::new(mtm);
    unsafe { sub.setTitle(title) };
    for it in items {
        sub.addItem(it);
    }
    let parent_item = NSMenuItem::new(mtm);
    unsafe { parent_item.setTitle(title) };
    parent_item.setSubmenu(Some(&sub));
    parent.addItem(&parent_item);
}
