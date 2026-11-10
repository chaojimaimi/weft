//! v1.14.3 (T2, PLAN_v1143 §2): Pane 级对账判重 — memoized
//! `reconciled_terminal_block_scroll` for the recompute-layout call site.
//!
//! `geometry_controller::recompute_layout` reconciles every tab's detached
//! block-scroll on EVERY recompute. The memo makes a same-key repeat O(1).
//! The key MIRRORS the renderer-side `BlockScrollMetricsKey`
//! (`paint/live_cache.rs`) field for field, with one structural exclusion
//! and one substitution:
//!
//! - `pane_session_id` — the memo lives ON the Pane, structurally per-pane;
//! - `cache_rebuilds` → **`cache_epoch`** — v1.18.0 WP1.5 F2 unfroze plan
//!   §T2-2 (see geometry_controller.rs): the compute closure now reads the
//!   layout cache through `block_content_metrics_with_cache` (stale
//!   scalars), so the layout-cache rebuild count CAN change the value. The
//!   key carries it as `cache_epoch` = `BlockLayoutCache::total_misses()`:
//!   entries rebuilding after the final cols bump the fingerprint, so a
//!   memoized stale total cannot survive past the rebuild (the
//!   `cache_rebuilds` exclusion's original rationale — "the UNCACHED
//!   computation cannot see it" — no longer holds).
//!
//! Dropping any REMAINING field reintroduces two real stale bugs (the R1
//! review's findings): a pure height resize (`num_rows`) would keep a stale
//! clamped scroll, and a mid-command OSC 7 cwd change (`cwd_present`) would
//! miss the `+1` total row (the version counter does not bump on cwd —
//! renderer/runtime.rs:320-323).

use crate::layout::LayoutCtx;
use weft_core::vt::Terminal;

/// Memo key: every input the reconcile's metrics depend on (see the module
/// doc for the two deliberate `BlockScrollMetricsKey` exclusions).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReconcileMemoKey {
    pub(crate) cols: u32,
    pub(crate) num_rows: u32,
    /// `Some(version)` while a live block exists; `None` at prompt.
    pub(crate) live_version: Option<u64>,
    pub(crate) blocks_len: u32,
    pub(crate) first_block_id: Option<u64>,
    pub(crate) last_block_id: Option<u64>,
    pub(crate) header_rows: u32,
    pub(crate) visible: u32,
    /// Whether the live block shows a cwd line — the
    /// `+usize::from(live.cwd.or(terminal.cwd()).is_some())` total term.
    pub(crate) cwd_present: bool,
    /// WP1.5 F2: the `BlockLayoutCache` fingerprint the metrics were
    /// computed against (= `total_misses()`, 0 when no cache was available).
    /// An entry rebuild (WidthOnly or Both) bumps the count, so a same-geometry
    /// key computed from pre-rebuild stale scalars can never hit again —
    /// the memo cannot fixate a stale total past the final-cols rebuild.
    pub(crate) cache_epoch: u64,
}

/// Pane-held memo state: the last key + the metrics it produced. A matching
/// key guarantees identical metrics (the same guarantee the renderer-side
/// `BlockScrollMetricsKey` contracts).
pub(crate) type ReconcileMemo = Option<(ReconcileMemoKey, (usize, usize))>;

/// Build the key from exactly the inputs the reconcile metrics read —
/// mirrors `renderer/runtime.rs block_scroll_metrics` key assembly line for
/// line (minus the excluded fields, plus the WP1.5 `cache_epoch`
/// fingerprint the caller reads off its borrowed cache).
pub(crate) fn reconcile_key(
    terminal: &Terminal,
    layout_ctx: &LayoutCtx,
    header_rows: usize,
    cache_epoch: u64,
) -> ReconcileMemoKey {
    let cols = terminal.grid().num_cols;
    let prompt_lines = crate::block_component::block_prompt_lines(terminal);
    let cwd_header = crate::layout::block_cwd_header_active(
        terminal.effective_input_mode() == weft_core::input::InputMode::Editor,
        terminal.cwd().is_some(),
    );
    let visible = crate::layout::block_visible_rows(layout_ctx, prompt_lines, cwd_header);
    let blocks = terminal.block_tracker().session_blocks();
    let in_flight = terminal.block_tracker().in_flight();
    let (live_version, cwd_present) = match &in_flight {
        Some(live) => (Some(live.version), live.cwd.or(terminal.cwd()).is_some()),
        None => (None, false),
    };
    ReconcileMemoKey {
        cols: cols as u32,
        num_rows: terminal.grid().num_rows as u32,
        live_version,
        blocks_len: blocks.len() as u32,
        first_block_id: blocks.first().map(|b| b.id.0),
        last_block_id: blocks.last().map(|b| b.id.0),
        header_rows: header_rows as u32,
        visible: visible as u32,
        cwd_present,
        cache_epoch,
    }
}

/// Same-key hit reuses the stored `(total, visible)`; a miss runs `compute`
/// (the metrics total — the expensive part; cache-backed since WP1.5 F2)
/// once and stores. Split key from compute so tests can count invocations
/// (成本计数器断言).
pub(crate) fn memoized_metrics(
    memo: &mut ReconcileMemo,
    key: ReconcileMemoKey,
    compute: impl FnOnce() -> usize,
) -> (usize, usize) {
    if let Some((cached_key, cached)) = memo {
        if *cached_key == key {
            return *cached;
        }
    }
    let total = compute();
    let cached = (total, key.visible as usize);
    *memo = Some((key, cached));
    cached
}

/// The memoized [`crate::block_component::reconciled_terminal_block_scroll`]
/// twin — same return value, same `None`-when-not-block-view contract, with
/// the metrics scan memoized per pane.
///
/// The memo stores ONLY `(total, visible)`; the O(1) clamp
/// (`reconciled_block_scroll(current, …)`) re-runs every call. `current` is
/// deliberately NOT memoized (nor keyed): it is the caller's live scroll
/// offset, and caching a clamp computed for a stale offset would yank the
/// user's scroll on a same-key recompute — the exact stale-钳制 bug class
/// this memo exists to prevent. (Deviation from the plan's
/// `(usize,usize,usize)` memo value, recorded in the delivery report.)
pub(crate) fn reconciled_with_memo(
    memo: &mut ReconcileMemo,
    terminal: &Terminal,
    layout_ctx: &LayoutCtx,
    header_rows: usize,
    current: usize,
    cache: Option<&crate::paint::grid_cache::BlockLayoutCache>,
) -> Option<(usize, usize, usize)> {
    if !terminal.show_block_view() {
        return None;
    }
    // WP1.5 F2: the cache fingerprint rides the key — a rebuild between two
    // same-geometry calls forces a recompute instead of returning the
    // pre-rebuild stale total forever.
    let cache_epoch = cache.map_or(0, |c| c.total_misses());
    let key = reconcile_key(terminal, layout_ctx, header_rows, cache_epoch);
    let (total, visible) = memoized_metrics(memo, key, || {
        crate::block_component::block_content_metrics_with_cache(
            terminal,
            terminal.grid().num_cols,
            header_rows,
            cache,
            None,
            // No region snapshot on the reconcile path (see block_component.rs
            // reconciled_terminal_block_scroll 备案).
            None,
        )
        .0
    });
    Some((
        crate::block_component::reconciled_block_scroll(current, total, visible),
        total,
        visible,
    ))
}

/// WP1.5 F3: what the epoch-driven re-clamp consumer does this pass (pure
/// decision, truth-table tested). Inputs: the pending flag armed by the two
/// reconcile sites, whether the cache epoch advanced past the last re-clamp
/// (an entry rebuild can shift the stale scalars the clamps read), whether
/// the cache is quiescent (`deferred_ids ∪ degraded_ids` empty — every
/// entry cols-fresh, so stale scalar reads ARE the exact values), and —
/// P1-1 (rust-reviewer) — whether at least one re-clamp has completed since
/// the arm (`reclamped_since_arm`). The no-Resized families (font/settings/
/// scale changes) arm during the event, BEFORE any draw has rebuilt the
/// cache, so the very next `about_to_wait` pass can see epoch-stable AND
/// (spuriously) quiescent; a Clear arm there killed the flag before the
/// first re-clamp ever ran and left the terminal state at the stale clamp.
///
/// - !pending → Skip.
/// - pending && !reclamped → Reclamp: the FIRST pass after an arm
///   unconditionally re-clamps once (idempotent min-clamp) and stays
///   armed — this is the P1-1 window's fix.
/// - pending && reclamped && epoch_advanced → Reclamp (stay armed; the
///   convergence pump may still rebuild and bump the epoch again).
/// - pending && reclamped && !epoch_advanced && quiescent → ReclampAndClear:
///   the clamps ran at the current epoch and nothing is pending → final.
/// - pending && reclamped && !epoch_advanced && !quiescent → Skip: wait for
///   the pump's next rebuild (re-clamping would repeat the last pass).
///
/// Self-clear stays gated on quiescence because pure epoch stability can be
/// punctured by the pump's frame gating (1-2 frames of fake stability), and
/// clearing early would leave the terminal state with unreconverged offsets
/// once the pump resumed (R3 review P2). Degraded-session shape (known,
/// accepted): a resident degraded entry keeps quiescent false forever, so
/// the flag never self-clears and the streaming Both rebuilds keep driving
/// re-clamps — cost bounded (O(blocks) stale scalar reads per pass), the
/// min-clamp idempotent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockReclampAction {
    /// Nothing owed (flag clear or nothing changed): do nothing.
    Skip,
    /// Re-clamp all tabs and keep the flag armed.
    Reclamp,
    /// Re-clamp all tabs at the verified-stable epoch, then clear the flag.
    ReclampAndClear,
}

#[must_use]
pub(crate) fn block_reclamp_action(
    pending: bool,
    epoch_advanced: bool,
    quiescent: bool,
    reclamped_since_arm: bool,
) -> BlockReclampAction {
    if !pending {
        return BlockReclampAction::Skip;
    }
    if !reclamped_since_arm {
        // P1-1: the first pass after an arm re-clamps regardless of how
        // settled the cache looks — the settlement may be spurious (the
        // rebuild has not happened yet).
        return BlockReclampAction::Reclamp;
    }
    match (epoch_advanced, quiescent) {
        (true, _) => BlockReclampAction::Reclamp,
        (false, true) => BlockReclampAction::ReclampAndClear,
        (false, false) => BlockReclampAction::Skip,
    }
}

/// F7-1 (PLAN_v1184 §2-F7-1) loop-scope predicate (pure; truth-table
/// tested): the three reconcile loops (the Resized site, `recompute_layout`,
/// and the re-clamp consumer) run the block-metrics reconciliation for the
/// ACTIVE tab ONLY. Non-active tabs keep their PTY/Grid resize queueing
/// (callers preserve that around this check) but skip the metrics pass —
/// for a retain-evicted cache entry it is an O(document)
/// `completed_output_rows` fallback rebuild (RECONCILE_PROBE: ~170ms per
/// event), and the tab is not painted (Warp background-freeze semantics). A
/// background tab clamps on activation via the generation hook
/// (`App::arm_reclamp_on_tab_activation`) or its next active pass here.
#[must_use]
pub(crate) fn reconcile_in_scope(tab_index: usize, active_index: usize) -> bool {
    tab_index == active_index
}

// WP1.5 review follow-up: the WP1.5 test family (F4-1..6 + the harness)
// pushed this file past the 800-line hard ceiling, so the tests moved to
// the exempted sibling module (block_component/tests.rs convention,
// pty/tests.rs precedent) so inline test lines stay out of the
// production-file budget.
#[cfg(test)]
#[path = "reconcile_scroll/tests.rs"]
mod tests;
