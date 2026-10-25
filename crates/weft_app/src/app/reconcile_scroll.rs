//! v1.14.3 (T2, PLAN_v1143 §2): Pane 级对账判重 — memoized
//! `reconciled_terminal_block_scroll` for the recompute-layout call site.
//!
//! `geometry_controller::recompute_layout` reconciles every tab's detached
//! block-scroll on EVERY recompute, and the reconcile pays a full UNCACHED
//! `block_content_metrics` O(blocks) scan (`block_component.rs:401 → :273`)
//! each time. The memo makes a same-key repeat O(1). The key MIRRORS the
//! renderer-side `BlockScrollMetricsKey` (`paint/live_cache.rs`) field for
//! field, minus exactly two documented exclusions (plan §T2-2 — 禁止子集):
//!
//! - `pane_session_id` — the memo lives ON the Pane, structurally per-pane;
//! - `cache_rebuilds` — the memo's value comes from the UNCACHED direct
//!   computation (`block_content_metrics`), so the layout-cache miss count
//!   cannot change it.
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
}

/// Pane-held memo state: the last key + the metrics it produced. A matching
/// key guarantees identical metrics (the same guarantee the renderer-side
/// `BlockScrollMetricsKey` contracts).
pub(crate) type ReconcileMemo = Option<(ReconcileMemoKey, (usize, usize))>;

/// Build the key from exactly the inputs the reconcile metrics read —
/// mirrors `renderer/runtime.rs block_scroll_metrics` key assembly line for
/// line (minus the two excluded fields).
pub(crate) fn reconcile_key(
    terminal: &Terminal,
    layout_ctx: &LayoutCtx,
    header_rows: usize,
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
    }
}

/// Same-key hit reuses the stored `(total, visible)`; a miss runs `compute`
/// (the UNCACHED total — the expensive part) once and stores. Split key from
/// compute so tests can count invocations (成本计数器断言).
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
) -> Option<(usize, usize, usize)> {
    if !terminal.show_block_view() {
        return None;
    }
    let key = reconcile_key(terminal, layout_ctx, header_rows);
    let (total, visible) = memoized_metrics(memo, key, || {
        crate::block_component::block_content_metrics(
            terminal,
            terminal.grid().num_cols,
            header_rows,
        )
        .0
    });
    Some((
        crate::block_component::reconciled_block_scroll(current, total, visible),
        total,
        visible,
    ))
}

#[cfg(test)]
mod tests {
    use super::{memoized_metrics, reconcile_key, reconciled_with_memo, ReconcileMemo};
    use crate::layout::LayoutCtx;

    fn ctx(height: f32) -> LayoutCtx {
        LayoutCtx::new((800.0, height), 10.0, 20.0, 0.0, 0.0)
    }

    /// A Terminal carrying one finished shell-integration block (the
    /// grid_cache/tests.rs FTCS convention) — block view content for the
    /// key/equivalence tests.
    fn terminal_with_block() -> weft_core::vt::Terminal {
        let mut terminal = weft_core::vt::Terminal::new(24, 80);
        terminal.process(
            b"\x1b]133;A\x07cargo test\x1b]133;B\x07\x1b]133;C\x07one\r\ntwo\r\nthree\r\n\x1b]133;D;0\x07",
        );
        terminal
    }

    fn key_of(
        t: &weft_core::vt::Terminal,
        layout: &LayoutCtx,
        header_rows: usize,
    ) -> super::ReconcileMemoKey {
        reconcile_key(t, layout, header_rows)
    }

    /// 同键命中复用 (成本计数器断言): a second lookup with the same key must
    /// NOT re-run the compute closure.
    #[test]
    fn same_key_hit_reuses_without_recompute() {
        let key = key_of(&terminal_with_block(), &ctx(600.0), 2);
        let mut calls = 0;
        let mut memo: ReconcileMemo = None;
        let first = memoized_metrics(&mut memo, key, || {
            calls += 1;
            42
        });
        let second = memoized_metrics(&mut memo, key, || {
            calls += 1;
            999
        });
        assert_eq!(calls, 1, "the second same-key lookup must reuse the memo");
        assert_eq!(first, (42, key.visible as usize));
        assert_eq!(second, first, "the reused value must equal the stored one");
    }

    /// 键矩阵 (R2 P2 指定七变体): every key field participates — mutating
    /// each one must MISS (compute re-runs). Covers the four R1-漏 fields
    /// (num_rows/visible/cwd_present/header_rows) plus blocks_len/
    /// live_version/first/last/cols.
    #[test]
    fn key_matrix_every_field_participates() {
        let terminal = terminal_with_block();
        let layout = ctx(600.0);
        let base = key_of(&terminal, &layout, 2);

        let mut other_layout = ctx(600.0);
        other_layout.viewport = (800.0, 900.0); // changes `visible`
        let variants: Vec<(&str, super::ReconcileMemoKey)> = vec![
            (
                "cols",
                super::ReconcileMemoKey {
                    cols: base.cols + 1,
                    ..base
                },
            ),
            (
                "num_rows",
                super::ReconcileMemoKey {
                    num_rows: base.num_rows + 1,
                    ..base
                },
            ),
            (
                "live_version",
                super::ReconcileMemoKey {
                    live_version: Some(7),
                    ..base
                },
            ),
            (
                "blocks_len",
                super::ReconcileMemoKey {
                    blocks_len: base.blocks_len + 1,
                    ..base
                },
            ),
            (
                "first_block_id",
                super::ReconcileMemoKey {
                    first_block_id: Some(999),
                    ..base
                },
            ),
            (
                "last_block_id",
                super::ReconcileMemoKey {
                    last_block_id: Some(999),
                    ..base
                },
            ),
            (
                "header_rows",
                super::ReconcileMemoKey {
                    header_rows: base.header_rows + 1,
                    ..base
                },
            ),
            (
                "visible",
                super::ReconcileMemoKey {
                    visible: base.visible + 1,
                    ..base
                },
            ),
            (
                "cwd_present",
                super::ReconcileMemoKey {
                    cwd_present: !base.cwd_present,
                    ..base
                },
            ),
            // The derived-key cross-check: a taller layout really does move
            // `visible` (the stale-钳制 scenario the R1 key missed).
            (
                "derived visible via layout height",
                key_of(&terminal, &other_layout, 2),
            ),
        ];
        for (name, key) in variants {
            assert_ne!(key, base, "variant `{name}` must differ from the base key");
            let mut memo: ReconcileMemo = Some((base, (10, 5)));
            let mut calls = 0;
            let (total, _) = memoized_metrics(&mut memo, key, || {
                calls += 1;
                77
            });
            assert_eq!(calls, 1, "variant `{name}` must invalidate the memo");
            assert_eq!(total, 77);
        }
    }

    /// 返回值与 UNCACHED 直算等价 (同输入双跑断言): the memoized twin returns
    /// exactly what the direct `reconciled_terminal_block_scroll` returns —
    /// on the first (compute) call AND on the second (memo-hit) call.
    #[test]
    fn memoized_result_equals_uncached_direct_run() {
        let terminal = terminal_with_block();
        let layout = ctx(600.0);
        let direct =
            crate::block_component::reconciled_terminal_block_scroll(&terminal, &layout, 2, 3);
        assert!(
            direct.is_some(),
            "FTCS block terminal must show the block view"
        );
        let mut memo: ReconcileMemo = None;
        let first = reconciled_with_memo(&mut memo, &terminal, &layout, 2, 3);
        assert_eq!(
            first, direct,
            "compute call must equal the UNCACHED direct run"
        );
        let second = reconciled_with_memo(&mut memo, &terminal, &layout, 2, 3);
        assert_eq!(
            second, direct,
            "memo-hit call must equal the UNCACHED direct run"
        );
        // A different scroll offset reconciles fresh (the clamp is not memoized):
        let clamped = reconciled_with_memo(&mut memo, &terminal, &layout, 2, 1_000);
        assert_eq!(
            clamped.map(|(scroll, _, _)| scroll),
            Some(0),
            "an oversized offset clamps to the document top-minus-viewport"
        );
    }

    /// 非块视图 (fresh terminal, no shell integration) → None, 同直算。
    #[test]
    fn non_block_view_returns_none_like_direct() {
        let terminal = weft_core::vt::Terminal::new(24, 80);
        let layout = ctx(600.0);
        let mut memo: ReconcileMemo = None;
        assert_eq!(
            reconciled_with_memo(&mut memo, &terminal, &layout, 2, 0),
            crate::block_component::reconciled_terminal_block_scroll(&terminal, &layout, 2, 0),
        );
        assert!(memo.is_none(), "a None result must not populate the memo");
    }
}
