//! T15d (PLAN_v11217 §3.10): single-instance gate.
//!
//! Multi-instance Weft caused notification-authorization contention (22.5s
//! cold start, field report), recovery-directory races, and double DB
//! writes. [`ensure_single_instance_or_exit`] takes an exclusive `flock` on
//! `~/.cache/weft/single-instance.lock` as the first startup step (before
//! the event loop, right after tracing init):
//! - the fd is held (leaked) for the process lifetime — the kernel releases
//!   the lock on process death, so no stale-lock file can ever remain;
//! - a still-held lock after the short retry window (Sparkle relaunch order
//!   old-exit/new-start is not ours to control) means a true second
//!   instance: it activates the running one (bundle id `dev.weft.terminal`,
//!   self-filtered) and exits 0;
//! - dev binaries have no bundle id: stderr hint + exit 0 (invisible under
//!   a GUI launch — accepted corner, noted for developers);
//! - `WEFT_ALLOW_SECOND_INSTANCE=1` skips the gate (dev / nested testing).

use std::time::Duration;

use nix::fcntl::{Flock, FlockArg};
use objc2_app_kit::NSApplicationActivationOptions;
use objc2_app_kit::NSRunningApplication;

/// The packaged app's bundle id (dev binaries have none).
/// Sparkle relaunch race window: the updater starts the new build BEFORE
/// the old process fully exits (order not ours to control); without a
/// re-check window the new process is misjudged as a second instance and
/// the update "fails". Retry ~750ms at 50ms intervals to let the dying
/// process release the lock.
const LOCK_RETRY_BUDGET: Duration = Duration::from_millis(750);
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(50);

/// What a still-locked second instance should do — decided by
/// [`second_instance_action`] from `(has_bundle_id, found_prior)`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SecondInstanceAction {
    /// Foreground the running Weft (unhide + activate), then exit 0.
    ActivatePrior,
    /// No packaged identity / no running prior to foreground: exit 0
    /// quietly (dev path additionally prints a stderr hint).
    ExitSilent,
}

/// Truth table (PLAN_v11217 §3.10 T15d): only a packaged build that actually
/// found a running prior foregrounds it; every other shape exits silently.
fn second_instance_action(has_bundle_id: bool, found_prior: bool) -> SecondInstanceAction {
    if has_bundle_id && found_prior {
        SecondInstanceAction::ActivatePrior
    } else {
        SecondInstanceAction::ExitSilent
    }
}

/// Single-instance gate; see the module docs. Called once from `main`
/// before the event loop is built.
pub(crate) fn ensure_single_instance_or_exit() {
    // Escape hatch for dev / nested-test usage.
    if std::env::var_os("WEFT_ALLOW_SECOND_INSTANCE").is_some_and(|v| v == "1") {
        return;
    }
    let Some(cache_dir) = crate::weft_cache_dir() else {
        tracing::debug!("single-instance: no cache dir, gate skipped");
        return;
    };
    let _ = std::fs::create_dir_all(&cache_dir);
    let lock_path = cache_dir.join("single-instance.lock");
    // Create+write without truncate: the file's CONTENT is never read —
    // only the advisory flock on it matters.
    let mut file = match std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
    {
        Ok(f) => f,
        Err(_) => {
            tracing::debug!("single-instance: lock file unopenable, gate skipped");
            return;
        }
    };
    let attempts = (LOCK_RETRY_BUDGET.as_millis() / LOCK_RETRY_INTERVAL.as_millis()) as u32;
    for attempt in 0..=attempts {
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(guard) => {
                // Leak the guard deliberately: the lock must live as long as
                // the process (a Drop would UNLOCK). The kernel releases the
                // flock on process death — no stale lock can ever remain.
                std::mem::forget(guard);
                return;
            }
            Err((f, nix::errno::Errno::EWOULDBLOCK)) => {
                if attempt == attempts {
                    break; // genuinely still locked → a second instance
                }
                std::thread::sleep(LOCK_RETRY_INTERVAL);
                file = f; // retry with the same fd
            }
            Err((_, error)) => {
                // Not a contention signal (EBADF/…): degrade to the
                // historical no-lock behavior rather than blocking startup.
                tracing::warn!(%error, "single-instance: flock failed, gate skipped");
                return;
            }
        }
    }
    act_as_second_instance();
}

/// The lock is genuinely held by another live process: foreground it (when
/// identifiable) and exit before ANY PTY / DB / notification state exists.
fn act_as_second_instance() {
    // SAFETY: main-thread-only AppKit selectors, called from `main` (the
    // main thread) before the event loop exists; plain message sends.
    let current = unsafe { NSRunningApplication::currentApplication() };
    let bundle_id = unsafe { current.bundleIdentifier() };
    let has_bundle_id = bundle_id.as_ref().is_some_and(|s| !s.is_empty());
    let prior = has_bundle_id
        .then(|| {
            // SAFETY: same main-thread guarantees as above. Lookup keyed by
            // OUR OWN bundle id (P3): a future id change cannot silently
            // desync the query from the self-filter below.
            let own_id = bundle_id
                .as_ref()
                .expect("has_bundle_id implies Some")
                .clone();
            let candidates =
                unsafe { NSRunningApplication::runningApplicationsWithBundleIdentifier(&own_id) };
            // The list includes THIS process; exclude it by pid (rust-reviewer
            // M-1: pid equality is the documented identity, no reliance on
            // per-process instance uniqueness of NSRunningApplication).
            let own_pid = std::process::id() as i32;
            candidates
                .iter()
                .find(|app| unsafe { app.processIdentifier() } != own_pid)
                .map(|app| unsafe {
                    // SAFETY: the element belongs to the live `candidates`
                    // array being iterated — retain cannot fail here.
                    objc2::rc::Retained::retain(std::ptr::NonNull::from(app).as_ptr())
                        .expect("live NSArray element")
                })
        })
        .flatten();
    let found_prior = prior.is_some();
    match second_instance_action(has_bundle_id, found_prior) {
        SecondInstanceAction::ActivatePrior => {
            let app = prior.expect("found_prior implies a prior app");
            // SAFETY: main-thread-only selectors, main thread.
            // rust-reviewer H-1: activate the PRIOR instance
            // (NSRunningApplication::activateWithOptions) —
            // NSApplication::sharedApplication().activate() would activate
            // THIS process (self-activation) and the prior would never
            // come forward before we exit.
            unsafe {
                app.unhide();
                #[allow(deprecated)]
                let bring_to_front = {
                    // Deprecated on macOS 14 as a no-op default, but this
                    // targets the pinned 0.2.2 API and must force-activate a
                    // BACKWARD prior instance — keep the explicit flag.
                    NSApplicationActivationOptions::NSApplicationActivateIgnoringOtherApps
                };
                app.activateWithOptions(bring_to_front);
            }
            tracing::info!("second instance: activated the running Weft and exiting");
            std::process::exit(0);
        }
        SecondInstanceAction::ExitSilent => {
            // Dev binary (no bundle id) or no runnable prior. NOTE: under a
            // GUI launch stderr is invisible — this print is a dev aid only;
            // the packaged path above is the real UX.
            eprintln!("Weft is already running (single-instance lock held); exiting.");
            std::process::exit(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{second_instance_action, SecondInstanceAction};

    #[test]
    fn second_instance_action_truth_table() {
        // Packaged build with a running prior → foreground it.
        assert_eq!(
            second_instance_action(true, true),
            SecondInstanceAction::ActivatePrior
        );
        // Packaged build, no prior found (zombie / foreign lock holder) →
        // quiet exit rather than guessing at a foreground target.
        assert_eq!(
            second_instance_action(true, false),
            SecondInstanceAction::ExitSilent
        );
        // Dev binary: no bundle id to identify a prior with → quiet exit.
        assert_eq!(
            second_instance_action(false, false),
            SecondInstanceAction::ExitSilent
        );
        assert_eq!(
            second_instance_action(false, true),
            SecondInstanceAction::ExitSilent
        );
    }
}
