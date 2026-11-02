//! Tests for the reconcile memo + WP1.5 F3 re-clamp consumer decision.
//! Sibling module (block_component/tests.rs / pty/tests.rs precedent) so
//! inline test lines stay out of the production-file budget.

use super::{block_reclamp_action, memoized_metrics, reconcile_key, reconciled_with_memo};
use super::{BlockReclampAction, ReconcileMemo};
use crate::layout::LayoutCtx;
use crate::paint::grid_cache::{BandSync, BlockLayoutCache};

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
    reconcile_key(t, layout, header_rows, 0)
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
        // WP1.5 F2: the cache fingerprint participates — a rebuild epoch
        // bump invalidates every same-geometry key.
        (
            "cache_epoch",
            super::ReconcileMemoKey {
                cache_epoch: base.cache_epoch + 1,
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
        crate::block_component::reconciled_terminal_block_scroll(&terminal, &layout, 2, 3, None);
    assert!(
        direct.is_some(),
        "FTCS block terminal must show the block view"
    );
    let mut memo: ReconcileMemo = None;
    let first = reconciled_with_memo(&mut memo, &terminal, &layout, 2, 3, None);
    assert_eq!(
        first, direct,
        "compute call must equal the UNCACHED direct run"
    );
    let second = reconciled_with_memo(&mut memo, &terminal, &layout, 2, 3, None);
    assert_eq!(
        second, direct,
        "memo-hit call must equal the UNCACHED direct run"
    );
    // A different scroll offset reconciles fresh (the clamp is not memoized):
    let clamped = reconciled_with_memo(&mut memo, &terminal, &layout, 2, 1_000, None);
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
        reconciled_with_memo(&mut memo, &terminal, &layout, 2, 0, None),
        crate::block_component::reconciled_terminal_block_scroll(&terminal, &layout, 2, 0, None),
    );
    assert!(memo.is_none(), "a None result must not populate the memo");
}

/// WP1.5 F4-2 (PLAN_v1180_WP15_G4_RECONCILE_CACHE): the memo key's
/// cache fingerprint — after a cache entry rebuild (WidthOnly AND Both)
/// a same-geometry call must MISS and recompute from the new scalars.
/// Without the fingerprint the memo would fixate the pre-rebuild stale
/// total forever (worse than the pre-WP1.5 uncached path).
#[test]
fn memo_cannot_fixate_stale_total_across_cache_rebuilds() {
    let mut terminal = weft_core::vt::Terminal::new(24, 80);
    // One block whose output wraps differently at narrow widths.
    terminal.process(
        b"\x1b]133;A\x07cargo build\x1b]133;B\x07\x1b]133;C\x07alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu\r\n\x1b]133;D;0\x07",
    );
    let layout = ctx(600.0);
    let mut cache = BlockLayoutCache::default();
    for blk in terminal.block_tracker().session_blocks() {
        cache.ensure_cached(blk, 80);
    }
    let mut memo: ReconcileMemo = None;
    let fresh = reconciled_with_memo(&mut memo, &terminal, &layout, 2, 3, Some(&cache))
        .unwrap()
        .1;
    assert_eq!(memo.as_ref().unwrap().0.cache_epoch, cache.total_misses());

    // WidthOnly rebuild at a narrower width: stale_output_rows grows.
    for blk in terminal.block_tracker().session_blocks() {
        cache.ensure_cached(blk, 20);
    }
    let epoch_width_only = cache.total_misses();
    assert!(
        epoch_width_only > 0,
        "the WidthOnly rebuild must bump the fingerprint"
    );
    let narrowed = reconciled_with_memo(&mut memo, &terminal, &layout, 2, 3, Some(&cache))
        .unwrap()
        .1;
    assert_ne!(
        narrowed, fresh,
        "the epoch bump must miss the memo and read the rebuilt scalars"
    );
    // Same state again: the memo works at the NEW epoch (cheap path intact).
    let again = reconciled_with_memo(&mut memo, &terminal, &layout, 2, 3, Some(&cache))
        .unwrap()
        .1;
    assert_eq!(again, narrowed, "a no-rebuild repeat must hit the memo");

    // Both rebuild: new output allocation (content changed).
    let mut grown = terminal.block_tracker().session_blocks()[0].clone();
    let mut output = grown.output.to_string();
    output.push_str("\r\nappended tail line for the both rebuild");
    grown.output = output.into();
    cache.ensure_cached(&grown, 80);
    assert!(
        cache.total_misses() > epoch_width_only,
        "the Both rebuild must bump the fingerprint too"
    );
    let regrown = reconciled_with_memo(&mut memo, &terminal, &layout, 2, 3, Some(&cache))
        .unwrap()
        .1;
    assert_ne!(
        regrown, narrowed,
        "the Both rebuild's epoch bump must miss the memo again"
    );
}

/// WP1.5 F3 consumer truth table (P1-1 revision): the flag gates everything;
/// the FIRST pass after an arm re-clamps unconditionally (the no-Resized
/// families arm before any draw has rebuilt the cache — a stable-and-
/// quiescent-looking cache there is spurious, and the pre-P1-1 Clear arm
/// died in exactly that window); afterwards an epoch advance re-clamps and
/// a verified-stable + quiescent pass clears (the pump's frame gating must
/// not fake stability).
#[test]
fn block_reclamp_action_truth_table() {
    // Flag clear: skip regardless.
    assert_eq!(
        block_reclamp_action(false, false, false, false),
        BlockReclampAction::Skip
    );
    assert_eq!(
        block_reclamp_action(false, true, true, true),
        BlockReclampAction::Skip
    );
    // P1-1: first pass after an arm — re-clamp REGARDLESS of how settled
    // the cache looks, and never clear.
    assert_eq!(
        block_reclamp_action(true, false, true, false),
        BlockReclampAction::Reclamp
    );
    assert_eq!(
        block_reclamp_action(true, true, false, false),
        BlockReclampAction::Reclamp
    );
    assert_eq!(
        block_reclamp_action(true, true, true, false),
        BlockReclampAction::Reclamp
    );
    // Reclamped: an epoch advance re-clamps (stay armed — the pump may
    // still rebuild).
    assert_eq!(
        block_reclamp_action(true, true, false, true),
        BlockReclampAction::Reclamp
    );
    assert_eq!(
        block_reclamp_action(true, true, true, true),
        BlockReclampAction::Reclamp
    );
    // Reclamped + epoch stable + quiescent: clamps are current and final.
    assert_eq!(
        block_reclamp_action(true, false, true, true),
        BlockReclampAction::ReclampAndClear
    );
    // Reclamped + epoch stable + pump mid-drain: wait (a re-clamp would
    // repeat the last pass's result).
    assert_eq!(
        block_reclamp_action(true, false, false, true),
        BlockReclampAction::Skip
    );
}

// ── F4-3/4/5 sequence harness ──────────────────────────────────────
// One tab's worth of reconcile state (memo + scroll offset + the F3
// flag/epoch pair), driven through the same primitives the production
// sites (window_event/resized.rs F1, geometry_controller.rs F2) and the
// about_to_wait consumer (`reclamp_block_scroll_pending`) run. Each
// test walks a cols sequence, so removing the site's flag-set or the
// consumer's epoch trigger breaks the final equality.
//
// Fixture design note (honest semantics): the final state is
// history-dependent whenever the clamp binds mid-sequence — the plan's
// "converged offset == uncached truth" therefore pins the case where
// the FINAL clamp binds (deep scroll-back seed, sequence ending wide so
// the final headroom is the smallest): min() collapses every history to
// the same terminal value, cached and uncached alike. The deferred
// fixture's intermediates are allowed to drift (and MUST — saw_stale
// proves the stale path is actually exercised).

struct ReconcileBench {
    memo: ReconcileMemo,
    scroll: usize,
    pending: bool,
    reclamped: bool,
    epoch: u64,
    /// P2-1 observability: how many consumer re-clamps actually ran.
    reclamps: usize,
}

/// The uncached truth: clamping `from` against the direct-scan total at
/// the terminal's current cols — the exact value the cache-backed path
/// must land on once converged.
fn uncached_clamp(
    terminal: &weft_core::vt::Terminal,
    layout: &LayoutCtx,
    header_rows: usize,
    from: usize,
) -> usize {
    crate::block_component::reconciled_terminal_block_scroll(
        terminal,
        layout,
        header_rows,
        from,
        None,
    )
    .unwrap()
    .0
}

impl ReconcileBench {
    fn new() -> Self {
        Self {
            memo: None,
            scroll: 0,
            pending: false,
            reclamped: false,
            epoch: 0,
            reclamps: 0,
        }
    }

    /// F1 site (window_event/resized.rs): direct reconcile + arm.
    fn site_resized(
        &mut self,
        terminal: &weft_core::vt::Terminal,
        layout: &LayoutCtx,
        header_rows: usize,
        cache: &BlockLayoutCache,
    ) {
        let previous = self.scroll;
        if let Some((reconciled, _, _)) = crate::block_component::reconciled_terminal_block_scroll(
            terminal,
            layout,
            header_rows,
            previous,
            Some(cache),
        ) {
            self.scroll = reconciled;
        }
        self.pending = true;
        self.reclamped = false;
    }

    /// F2 site (geometry_controller.rs): memoized twin + arm.
    fn site_recompute(
        &mut self,
        terminal: &weft_core::vt::Terminal,
        layout: &LayoutCtx,
        header_rows: usize,
        cache: &BlockLayoutCache,
    ) {
        let previous = self.scroll;
        if let Some((reconciled, _, _)) = reconciled_with_memo(
            &mut self.memo,
            terminal,
            layout,
            header_rows,
            previous,
            Some(cache),
        ) {
            self.scroll = reconciled;
        }
        self.pending = true;
        self.reclamped = false;
    }

    /// One consumer pass (mirrors `reclamp_block_scroll_pending`).
    fn consume(
        &mut self,
        terminal: &weft_core::vt::Terminal,
        layout: &LayoutCtx,
        header_rows: usize,
        cache: &BlockLayoutCache,
    ) {
        let epoch = cache.total_misses();
        let quiescent = cache.pending_ids_empty();
        let advanced = epoch != self.epoch;
        let arm = block_reclamp_action(self.pending, advanced, quiescent, self.reclamped);
        match arm {
            BlockReclampAction::Skip => {}
            BlockReclampAction::Reclamp | BlockReclampAction::ReclampAndClear => {
                if let Some((reconciled, _, _)) =
                    crate::block_component::reconciled_terminal_block_scroll(
                        terminal,
                        layout,
                        header_rows,
                        self.scroll,
                        Some(cache),
                    )
                {
                    self.scroll = reconciled;
                }
                self.epoch = epoch;
                self.reclamped = true;
                self.reclamps += 1;
                if matches!(arm, BlockReclampAction::ReclampAndClear) {
                    self.pending = false;
                }
            }
        }
    }
}

/// P2-1 (rust-reviewer): the production ORDER harness — the no-Resized
/// families arm during the event, BEFORE any draw has rebuilt the cache, so
/// the very next consumer pass sees epoch-stable AND (spuriously)
/// quiescent. The pre-P1-1 Clear arm died in exactly that window (the flag
/// died before the first re-clamp ever ran). Pins: pass 1 re-clamps and
/// stays armed; after the draw's rebuild the consumer converges (clamp at
/// the fresh epoch, then verify+clear) and the terminal state lands on the
/// uncached truth.
#[test]
fn consumer_reclamps_on_first_pass_before_any_rebuild() {
    let mut terminal = sequence_terminal();
    let layout = ctx(600.0);
    let header_rows = 2;
    let mut cache = synced_cache(&terminal, 80);
    let mut bench = ReconcileBench::new();
    let seed = 200;
    bench.scroll = seed;

    // Site arms during the event (font change), before any draw/rebuild.
    bench.site_recompute(&terminal, &layout, header_rows, &cache);
    // The very next pass: cache untouched since the site (no sync, no
    // rebuild) — epoch unchanged, quiescent. The first pass MUST re-clamp
    // unconditionally and MUST NOT clear.
    bench.consume(&terminal, &layout, header_rows, &cache);
    assert_eq!(
        bench.reclamps, 1,
        "the first pass after an arm must re-clamp unconditionally"
    );
    assert!(
        bench.pending,
        "the first pass must NOT clear the flag (the P1-1 window)"
    );
    assert_eq!(
        bench.scroll,
        uncached_clamp(&terminal, &layout, header_rows, seed)
    );

    // The draw then rebuilds at the final cols (epoch bump)...
    terminal.resize(24, 90);
    let blocks = terminal.block_tracker().session_blocks().to_vec();
    cache.sync_blocks(&blocks, 90, full_band());
    // ...and the consumer converges: pass 1 re-clamps at the fresh epoch,
    // pass 2 verifies quiescence and clears.
    bench.consume(&terminal, &layout, header_rows, &cache);
    bench.consume(&terminal, &layout, header_rows, &cache);
    assert!(!bench.pending, "converged: quiescent + verified stable");
    assert_eq!(
        bench.scroll,
        uncached_clamp(&terminal, &layout, header_rows, seed),
        "the converged offset must equal the uncached truth clamp"
    );
}

/// Fixture: the LONG block is the OLDEST one, so the narrow band
/// defers exactly the cols-sensitive block (the newest short block
/// rebuilds in band every tick). Enough content that the document
/// towers over the viewport.
fn sequence_terminal() -> weft_core::vt::Terminal {
    let mut terminal = weft_core::vt::Terminal::new(24, 80);
    let long_output: String = (0..11)
        .map(|i| {
            format!(
                "payload line {i} {} pad pad pad pad pad pad pad pad pad pad pad pad\r\n",
                "x".repeat(150)
            )
        })
        .collect();
    terminal.process(
        format!(
            "\x1b]133;A\x07cargo build with a long command line\x1b]133;B\x07\x1b]133;C\x07{long_output}\x1b]133;D;0\x07"
        )
        .as_bytes(),
    );
    terminal.process(
        b"\x1b]133;A\x07echo header\x1b]133;B\x07\x1b]133;C\x07one\r\ntwo\r\nthree\r\n\x1b]133;D;0\x07",
    );
    terminal.process(
        b"\x1b]133;A\x07echo tail\x1b]133;B\x07\x1b]133;C\x07four\r\nfive\r\n\x1b]133;D;0\x07",
    );
    terminal
}

fn synced_cache(terminal: &weft_core::vt::Terminal, cols: usize) -> BlockLayoutCache {
    let mut cache = BlockLayoutCache::default();
    cache.sync_blocks(
        terminal.block_tracker().session_blocks(),
        cols,
        BandSync {
            low_rows: 0,
            high_rows: usize::MAX,
        },
    );
    cache
}

/// Narrow band deferral: only the newest block's interval is in band,
/// everything above defers (the "band 之上 deferred" fixture semantics).
fn narrow_band() -> BandSync {
    BandSync {
        low_rows: 0,
        high_rows: 1,
    }
}

fn full_band() -> BandSync {
    BandSync {
        low_rows: 0,
        high_rows: usize::MAX,
    }
}

/// WP1.5 F4-3: a zoom-style animation (continuous cols changes, band
/// deferral above the band) converges — after the final frame plus the
/// convergence sync, the consumer's re-clamp lands the offset exactly
/// at the uncached truth. Intermediate states are allowed to be stale,
/// and the fixture MUST actually exhibit the stale drift.
#[test]
fn animation_sequence_with_band_deferral_converges_to_uncached_clamp() {
    let mut terminal = sequence_terminal();
    let layout = ctx(600.0);
    let header_rows = 2;
    let mut cache = synced_cache(&terminal, 80);
    let mut bench = ReconcileBench::new();
    // The user is scrolled deep into the document when the zoom starts.
    let seed = 200;
    bench.scroll = seed;
    // Uncached twin: the same sequence with no cache at all — what the
    // pre-WP1.5 code computed every tick.
    let mut twin_scroll = seed;

    let mut saw_stale = false;
    // Shrink then widen, ending WIDE: the final headroom is the
    // smallest, so the terminal clamp collapses the clamp history.
    for cols in [64usize, 52, 44, 60, 80] {
        terminal.resize(24, cols);
        let blocks = terminal.block_tracker().session_blocks().to_vec();
        cache.sync_blocks(&blocks, cols, narrow_band());
        let before = bench.scroll;
        bench.site_resized(&terminal, &layout, header_rows, &cache);
        bench.consume(&terminal, &layout, header_rows, &cache);
        twin_scroll = uncached_clamp(&terminal, &layout, header_rows, twin_scroll);
        if bench.scroll != twin_scroll {
            saw_stale = true;
        }
        let _ = before;
    }
    assert!(
        saw_stale,
        "the deferral fixture must actually produce stale intermediates"
    );

    // 终帧 + 收敛泵补齐: full-band sync rebuilds everything (epoch bump).
    // P1-1 two-pass convergence: the site re-armed on the last tick, so
    // pass 1 re-clamps unconditionally at the fresh epoch and pass 2
    // verifies quiescence and clears — about_to_wait provides both
    // passes naturally.
    let blocks = terminal.block_tracker().session_blocks().to_vec();
    cache.sync_blocks(&blocks, 80, full_band());
    bench.consume(&terminal, &layout, header_rows, &cache);
    bench.consume(&terminal, &layout, header_rows, &cache);
    assert!(!bench.pending, "quiescent + fresh epoch must self-clear");
    assert_eq!(
        bench.scroll,
        uncached_clamp(&terminal, &layout, header_rows, seed),
        "the converged offset must equal the uncached truth clamp"
    );
    assert_eq!(bench.scroll, twin_scroll);
}

/// WP1.5 F4-3 (full-band fixture): every block rebuilds per tick, so
/// the cache-backed clamps are exact at EVERY step (逐值相等), not just
/// the end.
#[test]
fn animation_sequence_with_full_band_is_exact_at_every_step() {
    let mut terminal = sequence_terminal();
    let layout = ctx(600.0);
    let header_rows = 2;
    let mut cache = synced_cache(&terminal, 80);
    let mut bench = ReconcileBench::new();
    bench.scroll = 200;

    for cols in [64usize, 52, 44, 60, 80] {
        terminal.resize(24, cols);
        let blocks = terminal.block_tracker().session_blocks().to_vec();
        cache.sync_blocks(&blocks, cols, full_band());
        let before = bench.scroll;
        bench.site_resized(&terminal, &layout, header_rows, &cache);
        bench.consume(&terminal, &layout, header_rows, &cache);
        assert_eq!(
            bench.scroll,
            uncached_clamp(&terminal, &layout, header_rows, before),
            "cols={cols}"
        );
    }
    // P1-1 two-pass convergence (see the deferred variant's note): pass 1
    // re-clamps unconditionally, pass 2 verifies quiescence and clears.
    bench.consume(&terminal, &layout, header_rows, &cache);
    bench.consume(&terminal, &layout, header_rows, &cache);
    assert!(!bench.pending);
}

/// WP1.5 F4-4 (R2 review P1): a drag tick sequence — every tick
/// reconciles + arms through the F1 site (a drag has NO zoom flush;
/// the site's flag-set is the ONLY trigger), mouse-up's final Resized
/// still reconciles (size_changed=false does not skip the reconcile),
/// then convergence. Without the site arming, the final consume would
/// be a Skip and the offset would stay stale.
#[test]
fn drag_sequence_converges_after_mouse_up() {
    let mut terminal = sequence_terminal();
    let layout = ctx(600.0);
    let header_rows = 2;
    let mut cache = synced_cache(&terminal, 80);
    let mut bench = ReconcileBench::new();
    let seed = 200;
    bench.scroll = seed;

    // Drag ticks widening the window (live resize, inLiveResize=true,
    // zoom channel unarmed): the reconcile still runs per Resized —
    // same site, same arming. Ending wide keeps the final clamp
    // binding (collapse design, see the harness note).
    for cols in [68usize, 70, 74, 78] {
        terminal.resize(24, cols);
        let blocks = terminal.block_tracker().session_blocks().to_vec();
        cache.sync_blocks(&blocks, cols, narrow_band());
        bench.site_resized(&terminal, &layout, header_rows, &cache);
        bench.consume(&terminal, &layout, header_rows, &cache);
    }
    // Mouse-up: one more Resized (size_changed=false), still reconciles.
    let blocks = terminal.block_tracker().session_blocks().to_vec();
    cache.sync_blocks(&blocks, 78, narrow_band());
    bench.site_resized(&terminal, &layout, header_rows, &cache);
    // 收敛泵补齐 + P1-1 two-pass convergence (clamp, then verify+clear).
    cache.sync_blocks(&blocks, 78, full_band());
    bench.consume(&terminal, &layout, header_rows, &cache);
    bench.consume(&terminal, &layout, header_rows, &cache);
    assert!(!bench.pending);
    assert_eq!(
        bench.scroll,
        uncached_clamp(&terminal, &layout, header_rows, seed),
    );
}

/// WP1.5 F4-5 (R3 review P1): the no-Resized families — divider drag
/// ticks mutate cols through `recompute_layout` only. The F2 site must
/// arm the flag (no Resized event exists to do it), else the consumer
/// never fires and the offset stays stale.
#[test]
fn divider_drag_sequence_without_resized_converges() {
    let mut terminal = sequence_terminal();
    let layout = ctx(600.0);
    let header_rows = 2;
    let mut cache = synced_cache(&terminal, 80);
    let mut bench = ReconcileBench::new();
    let seed = 200;
    bench.scroll = seed;

    // Divider drag: sidebar/divider width shifts change cols per tick
    // with zero window Resized events. Ending wide per the collapse
    // design.
    for cols in [66usize, 70, 76] {
        terminal.resize(24, cols);
        let blocks = terminal.block_tracker().session_blocks().to_vec();
        cache.sync_blocks(&blocks, cols, narrow_band());
        bench.site_recompute(&terminal, &layout, header_rows, &cache);
        bench.consume(&terminal, &layout, header_rows, &cache);
    }
    // Convergence + P1-1 two-pass convergence (clamp, then verify+clear).
    let blocks = terminal.block_tracker().session_blocks().to_vec();
    cache.sync_blocks(&blocks, 76, full_band());
    bench.consume(&terminal, &layout, header_rows, &cache);
    bench.consume(&terminal, &layout, header_rows, &cache);
    assert!(!bench.pending);
    assert_eq!(
        bench.scroll,
        uncached_clamp(&terminal, &layout, header_rows, seed),
    );
}

/// WP1.5 F4-5 second variant: a font change — cols change, zero Resized,
/// same F2 site + consumer contract.
#[test]
fn font_change_sequence_without_resized_converges() {
    let mut terminal = sequence_terminal();
    let layout = ctx(600.0);
    let header_rows = 2;
    let mut cache = synced_cache(&terminal, 80);
    let mut bench = ReconcileBench::new();
    let seed = 200;
    bench.scroll = seed;

    // One font-size step: cols widen (denser grid), then settle. The
    // wider final geometry has the smallest headroom → the final clamp
    // binds (collapse design).
    terminal.resize(24, 90);
    let blocks = terminal.block_tracker().session_blocks().to_vec();
    cache.sync_blocks(&blocks, 90, narrow_band());
    bench.site_recompute(&terminal, &layout, header_rows, &cache);
    // 收敛泵补齐 + P1-1 two-pass convergence (clamp, then verify+clear).
    cache.sync_blocks(&blocks, 90, full_band());
    bench.consume(&terminal, &layout, header_rows, &cache);
    bench.consume(&terminal, &layout, header_rows, &cache);
    assert!(!bench.pending);
    assert_eq!(
        bench.scroll,
        uncached_clamp(&terminal, &layout, header_rows, seed),
    );
}

// ── F7-1 (PLAN_v1184 §2-F7-1): loop-scope predicate (truth table ④) ─────

#[test]
fn f7_1_reconcile_scope_is_active_tab_only() {
    use super::reconcile_in_scope;
    // The active tab reconciles exactly as before.
    assert!(reconcile_in_scope(0, 0));
    assert!(reconcile_in_scope(3, 3));
    // Every non-active tab skips the metrics pass — a retain-evicted entry
    // must never reach the O(document) fallback from these loops.
    assert!(!reconcile_in_scope(0, 1));
    assert!(!reconcile_in_scope(2, 1));
    assert!(!reconcile_in_scope(5, 0));
}
