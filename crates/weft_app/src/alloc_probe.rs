//! Opt-in global allocator probe: logs a backtrace for allocations at or
//! above a byte threshold (default 4 MiB). `WEFT_ALLOC_PROBE=1` enables
//! (else/unset = off, one cached-bool read, no extra allocations);
//! `WEFT_ALLOC_PROBE_MIN` overrides the threshold (decimal bytes; zero or
//! unparsable values fall back to the default with a one-line stderr warn —
//! see `parse_min_bytes`). Allocations made before the tracing subscriber
//! is installed are logged-to-nowhere (the backtrace is still paid for);
//! keep this window in mind when timing forensics. See
//! `docs/perf/warp-comparison/2026-08-18-baseline.md` §2.3, option B.

use std::alloc::{GlobalAlloc, Layout, System};
use std::backtrace::Backtrace;
use std::cell::Cell;
use std::sync::OnceLock;

/// `WEFT_ALLOC_PROBE=1` turns the probe on; anything else or unset = off.
pub(crate) const ENABLE_ENV: &str = "WEFT_ALLOC_PROBE";
/// Optional size-threshold override in bytes.
pub(crate) const MIN_ENV: &str = "WEFT_ALLOC_PROBE_MIN";
const DEFAULT_MIN_BYTES: usize = 4 * 1024 * 1024;

/// Delegates all allocations to `std::alloc::System`; when on, records a
/// `tracing` event for allocations at or above the threshold.
pub struct ProbeAllocator;

// Re-entrancy flag: backtrace capture, the `tracing` macro, and the one-time
// env read all allocate back through this allocator. While set, swallow.
thread_local! {
    static IN_PROBE: Cell<bool> = const { Cell::new(false) };
}
/// RAII: `IN_PROBE` set during capture; drop restores the prior (nested-safe).
struct InProbeGuard {
    prev: bool,
}

impl Drop for InProbeGuard {
    fn drop(&mut self) {
        IN_PROBE.set(self.prev);
    }
}

/// Env-parsed settings, frozen after the first allocation (env reads allocate).
struct ProbeConfig {
    enabled: bool,
    min_bytes: usize,
}

static CONFIG: OnceLock<ProbeConfig> = OnceLock::new();
/// Disabled stub for re-entrant calls while the first env read is in flight
/// (never re-enter `get_or_init` on one thread — it would spin forever).
static DISABLED_CONFIG: ProbeConfig = ProbeConfig {
    enabled: false,
    min_bytes: DEFAULT_MIN_BYTES,
};

fn config() -> &'static ProbeConfig {
    match CONFIG.get() {
        Some(cfg) => cfg,
        // Env-read allocation re-entered us mid-resolution → silently defer.
        None if IN_PROBE.with(Cell::get) => &DISABLED_CONFIG,
        None => CONFIG.get_or_init(|| {
            let _guard = InProbeGuard {
                prev: IN_PROBE.replace(true),
            };
            ProbeConfig {
                enabled: std::env::var(ENABLE_ENV).as_deref() == Ok("1"),
                min_bytes: match std::env::var(MIN_ENV) {
                    Ok(raw) => parse_min_bytes(&raw).unwrap_or_else(|| {
                        // stderr, not tracing: the subscriber is not installed
                        // yet when the first allocation resolves this config.
                        // eprintln re-enters the allocator under the guard — safe.
                        eprintln!(
                            "weft: invalid {MIN_ENV}={raw:?}; \
                             falling back to {DEFAULT_MIN_BYTES}"
                        );
                        DEFAULT_MIN_BYTES
                    }),
                    Err(_) => DEFAULT_MIN_BYTES,
                },
            }
        }),
    }
}

/// Pure parse for `WEFT_ALLOC_PROBE_MIN`: decimal bytes, `1..=usize::MAX`.
/// `None` for empty/garbage and for `0` — a zero threshold would backtrace
/// every allocation in the process (a probe footgun, not a feature).
fn parse_min_bytes(raw: &str) -> Option<usize> {
    let value = raw.trim().parse::<usize>().ok()?;
    (value >= 1).then_some(value)
}

/// Pure decision, extracted for unit tests (no env, no allocator state).
fn should_log(enabled: bool, reentrant: bool, size: usize, min: usize) -> bool {
    enabled && !reentrant && size >= min
}

impl ProbeAllocator {
    fn log_large(&self, size: usize, ptr: *mut u8) {
        if !ptr.is_null() {
            self.trace(size);
        }
    }

    fn trace(&self, size: usize) {
        let cfg = config();
        if !cfg.enabled {
            return; // off: one cached bool, then out — no TLS/allocation
        }
        // Re-entrant? a capture/log/env-read allocation landed back here.
        if !should_log(cfg.enabled, IN_PROBE.with(Cell::get), size, cfg.min_bytes) {
            return;
        }
        let _guard = InProbeGuard {
            prev: IN_PROBE.replace(true),
        };
        tracing::info!(
            size = size,
            "LARGE_ALLOC backtrace=\n{}",
            Backtrace::force_capture()
        );
    }
}

// SAFETY: methods forward to their `System` counterparts with the identical
// contract (probe adds no safety obligations); logging is `IN_PROBE`-guarded.
unsafe impl GlobalAlloc for ProbeAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        self.log_large(layout.size(), ptr);
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        self.log_large(layout.size(), ptr);
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        self.log_large(new_size, new_ptr);
        new_ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_min_bytes, should_log};

    const MIN: usize = 4 * 1024 * 1024;

    #[test]
    fn threshold_edges() {
        assert!(should_log(true, false, MIN, MIN));
        assert!(!should_log(true, false, MIN - 1, MIN));
        assert!(should_log(true, false, 0, 0));
        assert!(should_log(true, false, usize::MAX, 0));
    }

    #[test]
    fn reentrant_never_logs() {
        assert!(!should_log(true, true, usize::MAX, 0));
    }

    #[test]
    fn disabled_never_logs() {
        assert!(!should_log(false, false, usize::MAX, 0));
        assert!(!should_log(false, false, MIN, 0));
    }

    #[test]
    fn min_env_parse_rejects_garbage_zero_and_overflow() {
        assert_eq!(parse_min_bytes(""), None);
        assert_eq!(parse_min_bytes("   "), None);
        assert_eq!(parse_min_bytes("abc"), None);
        assert_eq!(parse_min_bytes("4MiB"), None);
        // underscores are accepted in literals, not in str::parse
        assert_eq!(parse_min_bytes("8_388_608"), None);
        // zero is the every-allocation footgun, rejected at the parse layer
        assert_eq!(parse_min_bytes("0"), None);
        assert_eq!(parse_min_bytes("-1"), None);
        assert_eq!(parse_min_bytes("99999999999999999999999999"), None);
        assert_eq!(parse_min_bytes("8388608"), Some(8_388_608));
        // documented str::parse behavior: a single leading '+' is accepted
        assert_eq!(parse_min_bytes("  +4194304 "), Some(4_194_304));
    }
}
