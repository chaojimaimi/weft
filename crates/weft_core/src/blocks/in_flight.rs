// The in-flight command view (v1.14.2 B1 备料: moved byte-identical out of
// blocks.rs — the file sat at its 800-line budget edge and B1's title-
// generation bumps needed the headroom. Reached as
// `weft_core::blocks::InFlightBlock` unchanged via the re-export).

use std::sync::Arc;
use std::time::SystemTime;

use super::StyledOutput;

/// A view of the currently-running command (borrowed from [`BlockTracker`]),
/// for the renderer's live block during CommandExecuting.
pub struct InFlightBlock<'a> {
    pub command: &'a str,
    pub cwd: Option<&'a str>,
    pub output: &'a str,
    /// v1.10.5: borrow the `Arc<StyledOutput>` itself (not the dereferenced
    /// `&StyledOutput`) so callers can cheap-clone the Arc instead of deep-
    /// copying the styled lines on the render hot path.
    /// FIX_LIVE_STYLED_OUTPUT: on the plain capture path this is the 100ms-
    /// throttled streaming snapshot (cleared when a rewrite removes all
    /// styles); on screen-owned paths it is the caller-supplied snapshot.
    pub styled_output: Option<&'a Arc<StyledOutput>>,
    /// v1.10.23: live-output content version (LiveLayoutCache key) — bumped
    /// on every mutation, so version equality ⇔ byte-identical output.
    pub version: u64,
    /// v1.10.26 Batch B review blocker (BL-1): whether the live output is a
    /// primary-screen TUI document (`screen_document_start` was set during the
    /// command). The renderer's live layout splits by this flag: screen-owned
    /// frames clip on a narrow window, ordinary streaming output soft-wraps —
    /// the initial Batch B "live is always screen-origin" clip was a functional
    /// regression for plain commands emitting long lines.
    pub screen_origin: bool,
    /// M6-a (PLAN_M6 §A-1): shared handle to the capture's rewrite watermark.
    /// Both `LiveLayoutCache::sync` call sites must `take_min_write_offset()`
    /// here and pass the value as the sync's authoritative append guard —
    /// the watermark proves "no byte below the synced boundary was rewritten
    /// since the last consumption". Public like the other fields (detached
    /// test/bench handles use [`InFlightBlock::detached_watermark`]).
    pub min_write_offset: &'a std::sync::atomic::AtomicUsize,
    /// Start of the running command (`133;B` time) — the live elapsed label's
    /// anchor. Continuation sessions surface the base block's start so the
    /// header shows the whole session's age, not just the current command's.
    pub started_at: SystemTime,
}

impl<'a> InFlightBlock<'a> {
    /// Read and reset the rewrite watermark of the backing capture (take
    /// semantics: the value is consumed and the capture is "pure append"
    /// again). Must be called exactly once per `LiveLayoutCache::sync`, by
    /// the caller passing the value in.
    pub fn take_min_write_offset(&self) -> usize {
        use std::sync::atomic::Ordering;
        self.min_write_offset.swap(usize::MAX, Ordering::Relaxed)
    }

    /// Always-fresh watermark for detached handles (tests/benches construct
    /// an `InFlightBlock` without a backing capture). Swapping `usize::MAX`
    /// for MAX is a no-op, so sharing one static across tests is safe.
    pub fn detached_watermark() -> &'static std::sync::atomic::AtomicUsize {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static DETACHED: AtomicUsize = AtomicUsize::new(usize::MAX);
        debug_assert_eq!(DETACHED.load(Ordering::Relaxed), usize::MAX);
        &DETACHED
    }
}
