//! v1.1: Native macOS menu bar (NSMenu).
//!
//! Builds a 6-menu bar (Weft/File/Edit/View/Find/Window) whose items map to
//! the existing `Action` enum. Clicks flow through a custom ObjC target class
//! (`WeftMenuTarget`) → `EventLoopProxy::send_event(AppEvent::MenuAction)` →
//! `App::user_event` → `execute_action` — the same dispatch keybindings use, so
//! menu items and shortcuts stay in sync by construction.
//!
//! Weft-action items leave `keyEquivalent` empty so configurable bindings
//! reach winit. Standard system items keep their native equivalents.

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
        // v1.3: pane splits / focus / close. Keep in sync with Action decl
        // order — `Action as isize` relies on declaration order matching
        // these discriminant values.
        21 => Action::SplitHorizontal,
        22 => Action::SplitVertical,
        23 => Action::FocusNextPane,
        24 => Action::FocusPrevPane,
        25 => Action::ClosePane,
        // v1.3.3: pane zoom + direction-aware focus. Appended after
        // ClosePane to preserve existing discriminant values.
        26 => Action::TogglePaneZoom,
        27 => Action::FocusPaneUp,
        28 => Action::FocusPaneDown,
        29 => Action::FocusPaneLeft,
        30 => Action::FocusPaneRight,
        // v1.8.1-1.8.2 AI actions (31-34) have no menu items today but are
        // mapped so the table covers EVERY variant (v1.13.0 sync test: Action
        // 变体数 == 映射覆盖数).
        31 => Action::GenerateCommand,
        32 => Action::InsertAiSuggestion,
        33 => Action::CancelAiRequest,
        34 => Action::DiagnoseBlock,
        // v1.13.0 (PLAN_v1.13.0_SPARKLE §WP3): one-shot update check.
        35 => Action::CheckForUpdates,
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
            // Project rule: msg_send! inside an ObjC callback must be
            // catch_unwind-guarded — a panic escaping into AppKit's dispatch
            // is a nounwind abort (see set_dock_icon lesson).
            let dispatched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let tag: isize = unsafe { msg_send![sender, tag] };
                if let Some(action) = action_from_isize(tag) {
                    if let Some(proxy) = MENU_PROXY.get() {
                        // v1.11.12 (PLAN_v11112 M-C): decision-loop send — a
                        // failure silently drops the menu action. The warn
                        // runs before the catch_unwind boundary, so it still
                        // lands if a later panic is captured.
                        // (if-let instead of inspect_err: MSRV 1.75 < 1.76)
                        if let Err(e) = proxy.send_event(AppEvent::MenuAction(action)) {
                            tracing::warn!(error = %e, "send_event failed: menu action lost");
                        }
                    }
                }
            }));
            if dispatched.is_err() {
                tracing::error!("weft_action panicked; menu action dropped");
            }
        }

        /// Route Quit through the Rust event loop so live PTYs can be
        /// confirmed before AppKit terminates the process.
        #[method(weftQuit:)]
        fn weft_quit(&self, _sender: &AnyObject) {
            if let Some(proxy) = MENU_PROXY.get() {
                // v1.11.12 (PLAN_v11112 M-C): decision-loop send — a failure
                // silently drops the quit request.
                // (if-let instead of inspect_err: MSRV 1.75 < 1.76)
                if let Err(e) = proxy.send_event(AppEvent::QuitRequested) {
                    tracing::warn!(error = %e, "send_event failed: quit request lost");
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
    let app_menu = build_app_menu(mtm, &target);
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
            sep(mtm),
            // v1.3: pane splits / focus / close. Menu items intentionally
            // omit `keyEquivalent` (see file header) — shortcuts come from
            // weft's KeyBindings so the PTY sees them first.
            action_item(
                mtm,
                &target,
                ns_string!("Split Horizontal"),
                Action::SplitHorizontal,
            ),
            action_item(
                mtm,
                &target,
                ns_string!("Split Vertical"),
                Action::SplitVertical,
            ),
            sep(mtm),
            action_item(
                mtm,
                &target,
                ns_string!("Focus Next Pane"),
                Action::FocusNextPane,
            ),
            action_item(
                mtm,
                &target,
                ns_string!("Focus Previous Pane"),
                Action::FocusPrevPane,
            ),
            // v1.3.3: spatial direction focus. Four entries grouped after
            // the cyclic focus items so all focus ops sit together.
            action_item(
                mtm,
                &target,
                ns_string!("Focus Pane Up"),
                Action::FocusPaneUp,
            ),
            action_item(
                mtm,
                &target,
                ns_string!("Focus Pane Down"),
                Action::FocusPaneDown,
            ),
            action_item(
                mtm,
                &target,
                ns_string!("Focus Pane Left"),
                Action::FocusPaneLeft,
            ),
            action_item(
                mtm,
                &target,
                ns_string!("Focus Pane Right"),
                Action::FocusPaneRight,
            ),
            sep(mtm),
            // v1.3.3: zoom active pane to full viewport.
            action_item(
                mtm,
                &target,
                ns_string!("Toggle Pane Zoom"),
                Action::TogglePaneZoom,
            ),
            sep(mtm),
            action_item(mtm, &target, ns_string!("Close Pane"), Action::ClosePane),
        ],
    );

    // Keep the target alive for the app's lifetime — NSMenuItem retains its
    // target weakly, so we must not let it drop. One per process.
    std::mem::forget(target);

    app.setMainMenu(Some(&menubar));
}

/// Build the app (Weft) menu. Non-destructive standard items use system
/// selectors; Quit routes through `WeftMenuTarget` for live-process checks.
fn build_app_menu(mtm: MainThreadMarker, target: &WeftMenuTarget) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    menu.addItem(&stock_item(
        mtm,
        ns_string!("About Weft"),
        Some(sel!(orderFrontStandardAboutPanel:)),
        ns_string!(""),
    ));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    // v1.13.0 (PLAN_v1.13.0_SPARKLE §WP3): standard macOS position, right
    // after the About separator — every tier dispatches a one-shot check
    // (plan D4). Tag 35 → action_from_isize (keep-in-sync test below).
    menu.addItem(&action_item(
        mtm,
        target,
        ns_string!("Check for Updates…"),
        Action::CheckForUpdates,
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
    let quit = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc(),
            ns_string!("Quit Weft"),
            Some(sel!(weftQuit:)),
            ns_string!("q"),
        )
    };
    unsafe { quit.setTarget(Some(target)) };
    menu.addItem(&quit);
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

#[cfg(test)]
mod tests {
    use super::{action_from_isize, action_to_isize};
    use weft_core::config::Action;

    /// v1.13.0 (PLAN_v1.13.0_SPARKLE §WP3): `action_from_isize`'s table must
    /// cover every `Action` variant at its exact discriminant — a missed arm
    /// means the menu click is silently swallowed (`_ => None`). The table
    /// comment requires "Keep in sync"; this test enforces it.
    #[test]
    fn menu_tag_table_covers_every_action_variant() {
        // Declaration order == discriminant order (the `a as isize` cast
        // relies on it). A new variant MUST be appended here AND to the
        // `action_from_isize` table.
        let all = [
            Action::Copy,
            Action::Paste,
            Action::ReloadConfig,
            Action::ScrollPageUp,
            Action::ScrollPageDown,
            Action::ScrollLineUp,
            Action::ScrollLineDown,
            Action::ScrollToTop,
            Action::ScrollToBottom,
            Action::ToggleBlockPanel,
            Action::ToggleCommandPalette,
            Action::ZoomIn,
            Action::ZoomOut,
            Action::ZoomReset,
            Action::FindInGrid,
            Action::ToggleTheme,
            Action::NewTab,
            Action::CloseTab,
            Action::NextTab,
            Action::PrevTab,
            Action::ToggleSettings,
            Action::SplitHorizontal,
            Action::SplitVertical,
            Action::FocusNextPane,
            Action::FocusPrevPane,
            Action::ClosePane,
            Action::TogglePaneZoom,
            Action::FocusPaneUp,
            Action::FocusPaneDown,
            Action::FocusPaneLeft,
            Action::FocusPaneRight,
            Action::GenerateCommand,
            Action::InsertAiSuggestion,
            Action::CancelAiRequest,
            Action::DiagnoseBlock,
            Action::CheckForUpdates,
        ];
        for (tag, action) in all.iter().enumerate() {
            assert_eq!(
                action_to_isize(*action),
                tag as isize,
                "discriminant drift at index {tag} ({action:?})"
            );
            assert_eq!(
                action_from_isize(tag as isize),
                Some(*action),
                "action_from_isize table miss at index {tag} ({action:?})"
            );
        }
        // One past the end must NOT map — if it does, the table grew without
        // this list (a new variant was added to the table only).
        assert_eq!(
            action_from_isize(all.len() as isize),
            None,
            "table longer than the known variant list — append the new variant above"
        );
    }
}
