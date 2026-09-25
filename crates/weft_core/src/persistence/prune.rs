//! T14 (PLAN_v11217 §3.9): SQLite block-library auto-cleanup.
//!
//! Dual-gate retention over `blocks.db`:
//! - **age gate** — blocks with `started_ms < now − history_max_age_days`
//!   are deleted (strict `<`, matching [`BlockStore::older_than`]);
//! - **size gate** — when the DB exceeds `history_max_db_mb`, the OLDEST
//!   blocks are deleted batch-wise until the file fits or the table is
//!   empty. The gate is absolute (no age-window exemption — the
//!   "everything is still inside the age window yet over budget" quadrant
//!   is this gate's legal working range, per the logrotate/SAVEHIST
//!   "oldest yields first" convention). A single block larger than the
//!   budget ends the pass in [`PruneTerminal::BudgetUnreachable`].
//!
//! Every delete is a three-table cascade in ONE transaction — `blocks`
//! rows, `block_annotations` rows (sidecar annotations), and `search_docs`
//! FTS documents (`kind=Block`, `stable_id=<block_id>` decimal string).
//! Missing one of the side tables would leave Palette dead hits AND make
//! the size gate never converge (orphan FTS pages count into page_bytes
//! and incremental_vacuum cannot free them). The `tabs` table is NEVER
//! touched (session snapshots have no growth problem).
//!
//! Space reclamation: a fresh file is created `auto_vacuum=INCREMENTAL`
//! (no migration pass); legacy NONE-mode files are converted once inside
//! the prune routine (`PRAGMA auto_vacuum=INCREMENTAL; VACUUM;`) — the
//! pragma value is the persistent truth, so the conversion is naturally
//! idempotent across opens.
//!
//! Concurrency (§3.9 2b): the prune routine runs on its own connection
//! (rusqlite `Connection` is Send, not Sync — see `blocks.rs`). The
//! connection's `busy_timeout` is shortened to 2s so prune yields to the
//! main thread instead of stalling it; any batch failure aborts the pass
//! and the partial [`PruneReport`] is returned (the next 24h window
//! retries; there is no retry loop). The migration VACUUM probes with a
//! short-timeout `BEGIN IMMEDIATE` first and skips this round when the
//! main thread is writing; a VACUUM that turns busy mid-flight follows
//! the same skip-and-retry path.
//!
//! Main-thread insert-drop window argument (kept as-is per spec): the
//! main thread's `BlockStore::insert` has no retry — a write that fails
//! after its 5s busy timeout drops the block. Prune never holds a write
//! lock longer than one ≤500-row cascade transaction, and its 2s
//! busy_timeout means prune LOSES every contention, so the main thread's
//! insert can only time out if IT is blocked >2s by something else —
//! after T5' the flood-period main thread runs a 16ms frame budget and
//! yields between frames, so a >2s continuous main-thread write that
//! overlaps a prune batch is an extreme corner (the residual window is
//! accepted, not eliminated).

use std::path::Path;
use std::time::Duration;

use rusqlite::params;
use rusqlite::{Connection, TransactionBehavior};

use crate::search::SearchDocumentKind;

use super::blocks::BlockStore;
use super::PersistenceError;

/// Rows deleted per cascade transaction (§3.9: 500-row batches keep every
/// write transaction short so the main thread's inserts interleave).
pub const PRUNE_BATCH: usize = 500;

/// Total rows one prune pass may delete (§3.9: caps transaction churn; a
/// pass that hits the cap ends in [`PruneTerminal::RowCapHit`] and the
/// next window continues where this one stopped).
pub const PRUNE_ROW_CAP: usize = 50_000;

/// busy_timeout for the prune connection — shorter than the main-thread
/// default (5s) so prune lets writers win instead of blocking them.
const PRUNE_BUSY_TIMEOUT_MS: u64 = 2000;

/// Quiet-probe timeout for the migration `BEGIN IMMEDIATE`: if the write
/// lock is not free almost immediately, the main thread is writing and the
/// migration is skipped for this round.
const PRUNE_PROBE_TIMEOUT_MS: u64 = 50;

/// How a prune pass ended (§3.9 5: extended report shape).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PruneTerminal {
    /// Age gate ran; the size gate was off or the budget was already met.
    AgeDone,
    /// Size gate deleted oldest-first until the file fit the budget.
    BudgetMet,
    /// The budget is still exceeded after deleting everything the gates
    /// can (e.g. one block larger than the whole budget). Self-healing:
    /// after reclamation the next pass sees a smaller file.
    BudgetUnreachable,
    /// The per-pass row cap stopped the pass mid-gate; the next window
    /// continues.
    RowCapHit,
}

/// One prune pass's outcome (§3.9 5: both gates counted separately).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PruneReport {
    /// Blocks deleted by the age gate.
    pub age_deleted: usize,
    /// Blocks deleted by the size gate.
    pub size_deleted: usize,
    /// `page_bytes()` before any deletion.
    pub bytes_before: i64,
    /// `page_bytes()` after deletions + reclamation.
    pub bytes_after: i64,
    pub terminal: PruneTerminal,
}

impl PruneReport {
    /// Total blocks deleted by both gates.
    pub fn deleted(&self) -> usize {
        self.age_deleted + self.size_deleted
    }
}

/// Pure decision for one prune pass — no DB access, so the dual-gate truth
/// table is unit-testable. `db_bytes` activates the size gate only when the
/// budget is on AND already exceeded; `0` disables either gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrunePlan {
    /// `Some(cutoff_ms)` when the age gate is on: blocks with
    /// `started_ms < cutoff_ms` are deleted.
    pub age_cutoff_ms: Option<i64>,
    /// Size-gate budget in bytes (`Some` when `history_max_db_mb > 0`).
    pub size_budget_bytes: Option<i64>,
    /// True when the size gate must actually delete this pass (budget on
    /// AND `db_bytes > budget`).
    pub size_gate_active: bool,
}

impl PrunePlan {
    /// The age gate is configured on.
    pub fn age_gate_on(&self) -> bool {
        self.age_cutoff_ms.is_some()
    }

    /// The size gate is configured on (whether or not it must delete now).
    pub fn size_gate_on(&self) -> bool {
        self.size_budget_bytes.is_some()
    }

    /// Both gates are 0=Off: the prune pass must not run at all — behavior
    /// stays bit-identical to pre-T14.
    pub fn is_off(&self) -> bool {
        !self.age_gate_on() && !self.size_gate_on()
    }
}

/// Build the plan: age cutoff = now − age_days (saturating; u32 days cannot
/// overflow i64 millis), size budget = max_mb MiB.
pub fn prune_plan(now_ms: i64, db_bytes: i64, age_days: u32, max_mb: u32) -> PrunePlan {
    let age_cutoff_ms = if age_days == 0 {
        None
    } else {
        let span_ms = (age_days as i64).saturating_mul(86_400_000);
        Some(now_ms.saturating_sub(span_ms))
    };
    let size_budget_bytes = if max_mb == 0 {
        None
    } else {
        Some((max_mb as i64).saturating_mul(1024 * 1024))
    };
    let size_gate_active = match size_budget_bytes {
        Some(budget) => db_bytes > budget,
        None => false,
    };
    PrunePlan {
        age_cutoff_ms,
        size_budget_bytes,
        size_gate_active,
    }
}

impl BlockStore {
    /// `PRAGMA page_count * page_size` — O(1) on-disk size measurement.
    pub fn page_bytes(&self) -> Result<i64, PersistenceError> {
        let page_count: i64 = self.conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let page_size: i64 = self.conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        Ok(page_count.saturating_mul(page_size))
    }

    /// Freelist-aware size: what the file becomes once reclamation returns
    /// freed pages to the OS. `page_count` includes freelist pages that pure
    /// DELETE never removes, so the size gate's convergence check must use
    /// this — otherwise it could never reach the budget within one pass.
    fn live_page_bytes(&self) -> Result<i64, PersistenceError> {
        let page_count: i64 = self.conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let freelist: i64 = self
            .conn
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
        let page_size: i64 = self.conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        Ok(page_count
            .saturating_sub(freelist)
            .saturating_mul(page_size))
    }

    /// T14 (§3.9 2): delete blocks started before `started_ms_exclusive`
    /// (strict `<` — a block whose `started_ms` equals the cutoff is kept,
    /// matching `older_than`), looping [`PRUNE_BATCH`]-row cascade batches
    /// up to [`PRUNE_ROW_CAP`] rows per call. Returns the number of blocks
    /// deleted. A failed batch (e.g. SQLITE_BUSY) aborts the loop; earlier
    /// batches stay committed (partial prune — the caller's next window
    /// continues, there is no retry loop).
    pub fn delete_before(
        &mut self,
        started_ms_exclusive: i64,
        batch: usize,
    ) -> Result<usize, PersistenceError> {
        let (annotations_present, fts_present) = self.side_tables_present()?;
        let mut total = 0usize;
        while total < PRUNE_ROW_CAP {
            let this_batch = batch.min(PRUNE_ROW_CAP - total).max(1);
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let deleted = cascade_delete_batch(
                &tx,
                Some(started_ms_exclusive),
                this_batch,
                annotations_present,
                fts_present,
            )?;
            tx.commit()?;
            total += deleted;
            if deleted < this_batch {
                break; // age window drained
            }
        }
        Ok(total)
    }

    /// Run one prune pass per `plan` (age gate → size gate → reclamation).
    /// Batch failures abort the pass and return the partial report; only a
    /// measurement failure (page_bytes) surfaces as `Err`.
    pub fn prune(&mut self, plan: PrunePlan) -> Result<PruneReport, PersistenceError> {
        self.prune_with_cap(plan, PRUNE_ROW_CAP)
    }

    /// prune() with an injectable row cap — the production cap is
    /// [`PRUNE_ROW_CAP`]; tests pass a small one to exercise the
    /// `RowCapHit` terminal without seeding 50k rows.
    fn prune_with_cap(
        &mut self,
        plan: PrunePlan,
        row_cap: usize,
    ) -> Result<PruneReport, PersistenceError> {
        let bytes_before = self.page_bytes()?;
        let mut report = PruneReport {
            age_deleted: 0,
            size_deleted: 0,
            bytes_before,
            bytes_after: bytes_before,
            terminal: PruneTerminal::AgeDone,
        };
        if plan.is_off() {
            return Ok(report); // both gates Off → bit-identical no-op
        }
        let (annotations_present, fts_present) = self.side_tables_present()?;
        let mut aborted = false;

        // ── age gate ────────────────────────────────────────────────────
        if let Some(cutoff) = plan.age_cutoff_ms {
            loop {
                if report.deleted() >= row_cap {
                    report.terminal = PruneTerminal::RowCapHit;
                    break;
                }
                // Cap-aware batch sizing: the last batch never overshoots.
                let batch = PRUNE_BATCH.min(row_cap - report.deleted());
                match self.cascade_batch(Some(cutoff), batch, annotations_present, fts_present) {
                    Ok(deleted) => {
                        report.age_deleted += deleted;
                        if deleted < batch {
                            break; // age window drained
                        }
                    }
                    Err(e) => {
                        // Batch failed (busy) → abort this round, keep the
                        // partial counts (§3.9 2b: 24h natural retry).
                        tracing::warn!(error = %e, age_deleted = report.age_deleted,
                            "prune age gate aborted mid-pass");
                        aborted = true;
                        break;
                    }
                }
            }
        }

        // ── size gate: absolute oldest-first, no age-window exemption ──
        if !aborted && report.terminal != PruneTerminal::RowCapHit {
            if let Some(budget) = plan.size_budget_bytes {
                // Convergence measure: page_count includes freelist pages
                // that only a vacuum removes, so a pure page_bytes loop
                // could never reach the budget within one pass. The
                // freelist-aware live size is what the file becomes after
                // the reclamation step; page_bytes stays the report metric.
                let mut over_budget = self.live_page_bytes()? > budget;
                while over_budget {
                    if report.deleted() >= row_cap {
                        report.terminal = PruneTerminal::RowCapHit;
                        break;
                    }
                    let batch = PRUNE_BATCH.min(row_cap - report.deleted());
                    match self.cascade_batch(None, batch, annotations_present, fts_present) {
                        Ok(deleted) => {
                            if deleted == 0 {
                                break; // blocks table drained
                            }
                            report.size_deleted += deleted;
                            // Free the just-freed pages so the next
                            // measurement reflects the deletion (no-op when
                            // auto_vacuum is still NONE — the one-time
                            // migration in finish_report handles that mode).
                            // Must be drained to completion — see
                            // `run_void_pragma`. Busy here means the main
                            // thread is writing: abort the gate (not the
                            // report) — the next window converges.
                            if let Err(e) =
                                run_void_pragma(&self.conn, "PRAGMA incremental_vacuum;")
                            {
                                tracing::warn!(error = %e,
                                    "size-gate incremental_vacuum busy; aborting this pass");
                                aborted = true;
                                break;
                            }
                            over_budget = self.live_page_bytes()? > budget;
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, size_deleted = report.size_deleted,
                                "prune size gate aborted mid-pass");
                            aborted = true;
                            break;
                        }
                    }
                }
            }
        }

        // ── reclamation + final measurement ─────────────────────────────
        self.finish_report(&mut report)?;
        if aborted || report.terminal == PruneTerminal::RowCapHit {
            return Ok(report);
        }

        // ── terminal (decided AFTER reclamation, freelist-aware) ────────
        if let Some(budget) = plan.size_budget_bytes {
            let live_after = self.live_page_bytes()?;
            if live_after > budget {
                // Nothing left to delete and the live data still exceeds
                // the budget — the gate cannot do any better (e.g. the
                // never-touched `tabs` table or one giant block's peers
                // hold the bytes). Self-healing: raise the budget.
                let blocks_empty: bool =
                    self.conn
                        .query_row("SELECT NOT EXISTS(SELECT 1 FROM blocks)", [], |row| {
                            row.get(0)
                        })?;
                if blocks_empty {
                    report.terminal = PruneTerminal::BudgetUnreachable;
                    return Ok(report);
                }
            }
            if report.size_deleted > 0 {
                report.terminal = PruneTerminal::BudgetMet;
            }
        }
        Ok(report)
    }

    /// One cascade batch on this store's connection.
    fn cascade_batch(
        &mut self,
        cutoff_ms: Option<i64>,
        batch: usize,
        annotations_present: bool,
        fts_present: bool,
    ) -> Result<usize, PersistenceError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let deleted =
            cascade_delete_batch(&tx, cutoff_ms, batch, annotations_present, fts_present)?;
        tx.commit()?;
        Ok(deleted)
    }

    /// Space reclamation (§3.9 2) + final byte measurement. Failures here
    /// degrade to "no reclamation this round" — the report is still valid.
    fn finish_report(&mut self, report: &mut PruneReport) -> Result<(), PersistenceError> {
        if self.needs_vacuum_migration {
            self.migrate_to_incremental_vacuum();
        } else if report.deleted() > 0 {
            // Return freed pages to the OS so page_bytes() (and the size
            // gate) actually converge; a no-op when auto_vacuum is NONE.
            // Drained to completion — see `run_void_pragma`. Best-effort:
            // a busy failure costs this round its reclamation, not the
            // partial report.
            if let Err(e) = run_void_pragma(&self.conn, "PRAGMA incremental_vacuum;") {
                tracing::warn!(error = %e, "prune incremental_vacuum busy; skipping reclamation");
            }
        }
        report.bytes_after = self.page_bytes()?;
        Ok(())
    }

    /// One-time NONE → INCREMENTAL conversion. Quiet probe first: a
    /// short-timeout `BEGIN IMMEDIATE` — if the main thread is writing we
    /// skip this round entirely; the flag stays set (the PRAGMA value is
    /// the persistent truth) and the next window retries idempotently. A
    /// VACUUM that turns busy after the probe follows the same path.
    fn migrate_to_incremental_vacuum(&mut self) {
        let _ = self
            .conn
            .busy_timeout(Duration::from_millis(PRUNE_PROBE_TIMEOUT_MS));
        // Scoped so the probe Transaction drops before the next conn use.
        let probe_ok = {
            match self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
            {
                Ok(tx) => tx.commit().is_ok(),
                Err(_) => false,
            }
        };
        let _ = self
            .conn
            .busy_timeout(Duration::from_millis(PRUNE_BUSY_TIMEOUT_MS));
        if !probe_ok {
            tracing::info!("prune vacuum migration skipped: main thread busy");
            return;
        }
        // The auto_vacuum SET form + VACUUM; both outside any transaction
        // (VACUUM takes the exclusive lock itself). VACUUM returns no rows
        // so execute_batch runs it to completion.
        match self
            .conn
            .execute_batch("PRAGMA auto_vacuum=INCREMENTAL; VACUUM;")
        {
            Ok(()) => {
                self.needs_vacuum_migration = false;
                tracing::info!("blocks.db migrated to auto_vacuum=INCREMENTAL");
            }
            Err(e) => {
                // The pragma SET itself persisted nothing until VACUUM
                // commits, so a failure leaves the DB on NONE — retry next
                // window (review P3: probe-failure and VACUUM-busy are the
                // same skip-and-retry path).
                tracing::warn!(error = %e, "prune vacuum migration failed; will retry");
            }
        }
    }

    /// Which side tables exist in this DB. Both are guaranteed in the app
    /// (AnnotationStore/SearchIndex open before any prune can run), but a
    /// standalone BlockStore (tests, partial startup) may lack them; the
    /// cascade then simply has nothing to clean there.
    fn side_tables_present(&self) -> Result<(bool, bool), PersistenceError> {
        Ok((
            table_exists(&self.conn, "block_annotations")?,
            table_exists(&self.conn, "search_docs")?,
        ))
    }
}

/// T14 (§3.9 3): entry point for the background prune thread. Opens a
/// DEDICATED connection (the main thread's `BlockStore` is never shared —
/// rusqlite `Connection` is Send but not Sync), shortens its busy_timeout
/// so prune yields to writers, and runs one pass. `Ok(None)` = both gates
/// Off (the DB is not even opened — behavior identical to pre-T14).
pub fn run_block_prune(
    path: &Path,
    age_days: u32,
    max_db_mb: u32,
    now_ms: i64,
) -> Result<Option<PruneReport>, PersistenceError> {
    if prune_plan(now_ms, 0, age_days, max_db_mb).is_off() {
        return Ok(None);
    }
    let mut store = BlockStore::open(path)?;
    store
        .conn
        .busy_timeout(Duration::from_millis(PRUNE_BUSY_TIMEOUT_MS))?;
    let db_bytes = store.page_bytes()?;
    let plan = prune_plan(now_ms, db_bytes, age_days, max_db_mb);
    let report = store.prune(plan)?;
    Ok(Some(report))
}

/// The three-table cascade (§3.9 2, review P0): FTS docs → annotations →
/// block rows, all driven by the SAME oldest-first selection inside ONE
/// transaction, so a reader never sees a block without its annotation or
/// with a dead Palette hit. Selection: `started_ms < cutoff` (strict `<`;
/// `None` = unfiltered for the size gate), `started_ms ASC, id ASC` so
/// equal timestamps delete deterministically oldest-first.
fn cascade_delete_batch(
    tx: &rusqlite::Transaction,
    cutoff_ms: Option<i64>,
    batch: usize,
    annotations_present: bool,
    fts_present: bool,
) -> Result<usize, PersistenceError> {
    if fts_present {
        tx.execute(
            "DELETE FROM search_docs WHERE kind = ?3 AND stable_id IN (\
                 SELECT CAST(id AS TEXT) FROM blocks \
                 WHERE (?1 IS NULL OR started_ms < ?1) \
                 ORDER BY started_ms ASC, id ASC LIMIT ?2)",
            params![cutoff_ms, batch as i64, SearchDocumentKind::Block as i64],
        )?;
    }
    if annotations_present {
        tx.execute(
            "DELETE FROM block_annotations WHERE block_id IN (\
                 SELECT id FROM blocks \
                 WHERE (?1 IS NULL OR started_ms < ?1) \
                 ORDER BY started_ms ASC, id ASC LIMIT ?2)",
            params![cutoff_ms, batch as i64],
        )?;
    }
    let deleted = tx.execute(
        "DELETE FROM blocks WHERE id IN (\
             SELECT id FROM blocks \
             WHERE (?1 IS NULL OR started_ms < ?1) \
             ORDER BY started_ms ASC, id ASC LIMIT ?2)",
        params![cutoff_ms, batch as i64],
    )?;
    Ok(deleted)
}

/// Run a PRAGMA to completion. Some pragmas (`incremental_vacuum`) return
/// one row PER unit of work — stopping after the first row (query_row /
/// execute_batch behavior) leaves the operation half-done. Stepping to
/// exhaustion is what actually finishes it.
fn run_void_pragma(conn: &Connection, sql: &str) -> Result<(), PersistenceError> {
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query([])?;
    while rows.next()?.is_some() {}
    Ok(())
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool, PersistenceError> {
    use rusqlite::OptionalExtension;
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [name],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

#[cfg(test)]
#[path = "prune_tests.rs"]
mod tests;
