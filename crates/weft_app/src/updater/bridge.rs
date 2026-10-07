// Raw objc2 bridge leaves for the Sparkle update module (v1.13.0 WP1 file
// split: this file owns every direct objc2 call; `mod.rs` owns the tier
// state machine and never names selectors except through these helpers).
//!
//! objc2 0.5.2 retain semantics pinned by the spike report §三 — see the
//! parent module docs.

use std::path::{Path, PathBuf};

use objc2::exception::{catch, Exception};
use objc2::msg_send_id;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject};
use objc2_foundation::{NSBundle, NSString};

/// `exception::catch` downgrades a foreign unwind into `None` (not every
/// unwind carries an NSException) — render both shapes for the log. The
/// exception's own `name` accessor is private in objc2 0.5, so the class
/// name is the best public identity.
pub(super) fn describe_exception(exception: Option<Retained<Exception>>) -> String {
    match exception {
        Some(e) => format!("ObjC exception ({})", e.class().name()),
        None => "foreign unwind (no NSException)".to_string(),
    }
}

/// NSBundle load: registers Sparkle's Obj-C classes into the runtime. The
/// dylib stays loaded for the process lifetime even after the wrapper drops.
pub(super) fn load_framework(path: &Path) -> bool {
    let path_str = NSString::from_str(&path.to_string_lossy());
    let outcome: Result<Result<String, String>, _> = unsafe {
        catch(std::panic::AssertUnwindSafe(|| {
            let Some(bundle) = NSBundle::bundleWithPath(&path_str) else {
                return Err("bundleWithPath: nil".to_string());
            };
            if bundle.load() {
                Ok(String::new())
            } else {
                Err("NSBundle load() returned false".to_string())
            }
        }))
    };
    match outcome {
        Err(exception) => {
            tracing::warn!(
                reason = describe_exception(exception),
                "updater: NSException during bundle load"
            );
            false
        }
        Ok(Err(reason)) => {
            tracing::warn!(reason, "updater: bundle load error");
            false
        }
        // Empty reason = loaded; class resolution is the real proof.
        Ok(Ok(reason)) if reason.is_empty() => {
            AnyClass::get("SPUStandardUpdaterController").is_some()
        }
        Ok(Ok(reason)) => {
            tracing::warn!(reason, "updater: unexpected load outcome");
            false
        }
    }
}

/// Allocate + init `SPUStandardUpdaterController` with
/// `initWithStartingUpdater:{start} updaterDelegate:nil userDriverDelegate:nil`.
/// `alloc` via `msg_send_id!` (bare +1 `Allocated`), the init family via
/// `msg_send_id!` (converts the +1 into `Retained`) — spike §三 semantics.
pub(super) fn init_controller(start: bool) -> Result<usize, String> {
    unsafe {
        catch(std::panic::AssertUnwindSafe(|| {
            let cls = AnyClass::get("SPUStandardUpdaterController")
                .ok_or_else(|| "class SPUStandardUpdaterController not found".to_string())?;
            let allocated: Allocated<AnyObject> = msg_send_id![cls, alloc];
            let controller: Option<Retained<AnyObject>> = msg_send_id![
                allocated,
                initWithStartingUpdater: start,
                updaterDelegate: std::ptr::null_mut::<AnyObject>(),
                userDriverDelegate: std::ptr::null_mut::<AnyObject>()
            ];
            let controller = controller.ok_or_else(|| "init returned nil".to_string())?;
            // Leak for the app lifetime (raw ptr stored in the static).
            Ok(Retained::into_raw(controller) as usize)
        }))
        .map_err(describe_exception)?
    }
}

/// `…/Weft.app/Contents/MacOS/weft` → `…/Contents/Frameworks/Sparkle.framework`.
pub(super) fn framework_path_next_to_exe() -> Option<PathBuf> {
    framework_path_for_exe(&std::env::current_exe().ok()?)
}

/// Pure path resolution (testable: temp-dir fixture, plan §WP1 单测).
fn framework_path_for_exe(exe: &Path) -> Option<PathBuf> {
    let contents = exe.parent()?.parent()?;
    let fw = contents.join("Frameworks").join("Sparkle.framework");
    fw.is_dir().then_some(fw)
}

#[cfg(test)]
mod tests {
    use super::framework_path_for_exe;

    #[test]
    fn framework_path_resolves_next_to_exe() {
        let dir = std::env::temp_dir().join(format!("weft-updater-fw-{}", std::process::id()));
        let exe = dir.join("Weft.app/Contents/MacOS/weft");
        let fw = dir.join("Weft.app/Contents/Frameworks/Sparkle.framework");
        std::fs::create_dir_all(&fw).unwrap();
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        assert_eq!(framework_path_for_exe(&exe), Some(fw.clone()));
        // Missing framework dir → None (bare cargo run degrade path).
        std::fs::remove_dir_all(&fw).unwrap();
        assert_eq!(framework_path_for_exe(&exe), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
