//! Layout-table memory budget (M6-c / WP2 F1): the byte ceiling governing
//! the L1/L2 layout tables held by `BlockLayoutCache`. Extracted from
//! grid_cache.rs in v1.18.7 (F7-1e) as an identity move for the 800-line
//! gate, and given a configurable value: `[blocks].layout_table_budget_mib`
//! is injected once at renderer construction
//! (`init_effective_table_budget_mib`) and read per sync
//! (`effective_table_budget`) — no runtime hot reload, restart to change.
//!
//! Why a process static and not a `BlockLayoutCache` field: the cache is
//! `#[derive(Default)]` — a `budget_bytes: usize` field would derive to 0
//! and evict everything on the first sync (P0 trap, PLAN_v1187 §2.1). Same
//! process-level-static precedent as `EXEMPT_TABLES_LAST_LOG` below.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::{BlockLayoutCache, GIANT_TABLE_EXEMPT_BYTES};

/// M6-c (PLAN_M6 §三): total byte budget across all cached L1/L2 layout
/// tables. A 1MiB block's tables are ~6-8MB, so the budget tolerates
/// ~130-170 such blocks; a 55.7MB-table giant (`seq 1 655355`) fits ~19
/// times (F7-1e: raised from 256MiB, which the real two-tab giant workset
/// overshot and thrashed). Exceeding it evicts foreign entries and degrades
/// the OLDEST non-exempt blocks to metrics-only shells (R5: a degraded block
/// re-entering the band pays one ~15-20ms Both rebuild — accepted, single).
const LAYOUT_TABLE_BUDGET_BYTES: usize = 1024 * 1024 * 1024;

/// v1.18.7 (F7-1e): the budget the production sync actually enforces —
/// `[blocks].layout_table_budget_mib` injected at renderer construction,
/// falling back to [`LAYOUT_TABLE_BUDGET_BYTES`]. Relaxed ordering suffices:
/// the value is write-once at startup and read on the render thread.
static EFFECTIVE_TABLE_BUDGET: AtomicUsize = AtomicUsize::new(LAYOUT_TABLE_BUDGET_BYTES);

/// v1.18.7 (F7-1e): inject `[blocks].layout_table_budget_mib` (MiB). The
/// config side already clamps to 64..=4096 (`normalize_blocks`); the
/// `debug_assert` only guards misuse. No MIN/MAX re-clamp here on purpose.
pub(crate) fn init_effective_table_budget_mib(mib: usize) {
    debug_assert!(mib >= 1);
    EFFECTIVE_TABLE_BUDGET.store(mib.saturating_mul(1024 * 1024), Ordering::Relaxed);
}

/// v1.18.7 (F7-1e): the production sync's budget source — `sync_blocks`
/// passes this to `enforce_table_budget`. One relaxed load per sync.
pub(crate) fn effective_table_budget() -> usize {
    EFFECTIVE_TABLE_BUDGET.load(Ordering::Relaxed)
}

/// WP2 A1 P5 (`exempt tables`) throttle: the overview logs at most once per
/// 10s (it sits on the over-budget frame path).
const EXEMPT_TABLES_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

/// WP2 A1 P5: last `exempt tables` emission. The cache lives on the render
/// thread, so the mutex is uncontended in steady state.
static EXEMPT_TABLES_LAST_LOG: std::sync::Mutex<Option<std::time::Instant>> =
    std::sync::Mutex::new(None);

impl BlockLayoutCache {
    /// WP2 A1 P5: the throttled `exempt tables` line (before the F1 memo
    /// skip). v1.18.7 (F7-1e): moved here from grid_cache.rs unchanged —
    /// `pub(crate)` because the `enforce_table_budget` call site lives in
    /// the parent module.
    pub(crate) fn log_exempt_tables_overview(&self) {
        let mut last = EXEMPT_TABLES_LAST_LOG
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if last.is_some_and(|at| at.elapsed() < EXEMPT_TABLES_LOG_INTERVAL) {
            return;
        }
        *last = Some(std::time::Instant::now());
        drop(last);
        let mut giant_blocks = 0usize;
        let mut giant_table_bytes = 0usize;
        for entry in self.entries.values() {
            let bytes = entry.estimated_table_bytes();
            if bytes > GIANT_TABLE_EXEMPT_BYTES {
                giant_blocks += 1;
                giant_table_bytes += bytes;
            }
        }
        tracing::info!(
            total_table_bytes = self.table_bytes_total,
            giant_blocks,
            giant_table_bytes,
            "exempt tables"
        );
    }
}

/// F7-1e (PLAN_v1187 §4 test 1): the ONE test allowed to touch
/// [`EFFECTIVE_TABLE_BUDGET`] — cargo test runs suites in parallel, so every
/// read/write of the process static is serialized into this single function:
/// (a) default == 1GiB, (b) `init(1)` round-trip, (c) the production
/// `sync_blocks` call site really consumes the static — a 1MiB budget must
/// evict the foreign set A on the set-B sync (`budget evict` arm; foreign
/// eviction has no exemptions), (d) restore the default and pin the restore.
///
/// Window discipline (WHY the fixture is built BEFORE `init(1)`): while the
/// static sits at 1MiB, EVERY concurrent suite's `sync_blocks` enforce reads
/// that value too — and the `budget_tests.rs` giant fixtures hold 32MB+
/// synthetic tables (`install_giant`), so a table-BUILDING sync inside the
/// window would turn their early-return into a real over-budget walk and
/// flake them (observed: `memo_arms_but_new_candidate_still_degrades`). The
/// expensive builds therefore all happen under the DEFAULT static (no walk
/// fires at 1GiB); the 1MiB window spans only a Hit-path sync of the tiny
/// set B (no rebuilds) plus atomic reads — microsecond scale.
#[test]
fn effective_table_budget_default_init_and_production_sync() {
    use weft_core::blocks::{Block, BlockId};

    use super::BandSync;

    // ~100B of tables per `line-N` row: two 7k-line blocks ≈ 1.4MB of
    // tables (~80KB of text) — clears the 1MiB test floor with one block
    // still well under it, so the eviction walk must stop mid-set-A.
    let block_with_lines = |id: u64, lines: usize| {
        let output = (0..lines)
            .map(|i| format!("line-{i}\n"))
            .collect::<String>();
        Block {
            id: BlockId(id),
            command: "echo x".to_string(),
            cwd: None,
            output: output.into(),
            styled_output: None,
            exit_code: Some(0),
            started_at: std::time::SystemTime::UNIX_EPOCH,
            finished_at: Some(std::time::SystemTime::UNIX_EPOCH),
            collapsed: false,
            screen_origin: false,
        }
    };
    let full_band = BandSync {
        low_rows: 0,
        high_rows: usize::MAX,
    };

    // (a) default: 1GiB, unchanged by config absence.
    assert_eq!(LAYOUT_TABLE_BUDGET_BYTES, 1024 * 1024 * 1024);
    assert_eq!(effective_table_budget(), LAYOUT_TABLE_BUDGET_BYTES);

    // Fixture build — STILL under the default static, so the sync's enforce
    // early-returns (1.4MB < 1GiB) and no other suite can observe anything.
    // Set A ≤ 4 blocks: even if a walk fired here, newest-4 shields them all.
    let set_a: Vec<Block> = (1..=2).map(|i| block_with_lines(i, 7000)).collect();
    let set_b: Vec<Block> = (10..=11).map(|i| block_with_lines(i, 4)).collect();
    let mut cache = BlockLayoutCache::default();
    let set_ab = [set_a.clone(), set_b.clone()].concat();
    cache.sync_blocks(&set_ab, 80, full_band);
    let a_bytes = cache.get(1).estimated_table_bytes();
    assert_eq!(
        a_bytes,
        cache.get(2).estimated_table_bytes(),
        "same fixture"
    );
    assert!(
        2 * a_bytes > 1024 * 1024,
        "fixture must clear the 1MiB line: {a_bytes}"
    );
    assert_eq!(
        cache.degraded_count(),
        0,
        "default budget must not degrade anything"
    );

    // (b) init(1) round-trip: 1MiB — the smallest fixture-provable budget.
    init_effective_table_budget_mib(1);
    assert_eq!(effective_table_budget(), 1024 * 1024);

    // (c) production wiring proof: `sync_blocks` reads THIS static at its
    // enforce call. Switching the current set to B (Hit-path — B is already
    // cached, nothing rebuilds) makes set A foreign; the 1MiB static must
    // evict the OLDEST A block, then stop: the second A block survives iff
    // one A table alone fits under 1MiB (asserted from measured bytes, so
    // the expectation tracks any table-size formula drift).
    cache.sync_blocks(&set_b, 80, full_band);
    assert!(
        cache.get_if_cached(1).is_none(),
        "oldest foreign entry must be evicted via the injected static budget"
    );
    assert_eq!(
        cache.get_if_cached(2).is_none(),
        a_bytes > 1024 * 1024,
        "eviction must stop exactly at the static line"
    );
    assert!(
        cache.get_if_cached(10).is_some() && cache.get_if_cached(11).is_some(),
        "the current set is never evicted"
    );
    assert_eq!(
        cache.degraded_count(),
        0,
        "the degrade arm must not fire (budget met by foreign eviction)"
    );

    // (d) restore the default and pin the restore — same function, same
    // thread, no cross-test window.
    EFFECTIVE_TABLE_BUDGET.store(LAYOUT_TABLE_BUDGET_BYTES, Ordering::Relaxed);
    assert_eq!(effective_table_budget(), LAYOUT_TABLE_BUDGET_BYTES);
}
