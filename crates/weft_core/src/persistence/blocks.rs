//! `BlockStore` — SQLite-backed store of finished command blocks.
//!
//! See [`crate::persistence`] for the module-level overview.

use std::path::Path;
use std::sync::Arc;

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use crate::block_id_sequence::{BlockIdPool, ID_RESERVE};
use crate::blocks::{Block, BlockId};
use crate::persistence::migrations::{ensure_column, ensure_tabs_active_column};
use crate::persistence::{millis_to_system_time, system_time_to_millis, PersistenceError};

/// Upper bound on the JSON-encoded `styled_output` blob we are willing to
/// read/write. Larger payloads are dropped (treated as `NULL`) so a runaway
/// styled-output buffer cannot bloat the DB or OOM the decoder.
///
/// v1.13.12 FIX-G (D3): raised 256KiB → 1 MiB to ALIGN with the capture
/// budget [`crate::blocks::DEFAULT_OUTPUT_CAP`] — a block whose captured
/// output fits the live cap used to silently lose its styling across a
/// restart. The same constant is injected into every reader's SQL
/// (`format!` — `concat!` cannot interpolate a const value), so the write
/// filter and the read-side SQL gate can never drift apart. Parameters
/// stay `?N`-bound; the interpolation is a compile-time constant with no
/// injection surface.
pub(crate) const MAX_STYLED_OUTPUT_JSON_BYTES: usize = 1024 * 1024;

/// SQLite-backed store of finished command blocks.
///
/// Wraps a single [`rusqlite::Connection`]. rusqlite 0.31 implements
/// `Connection: Send` (moving the whole store across threads is fine) but
/// **not** `Sync` — `&BlockStore` must never be shared between threads.
/// Every background consumer (prune routine, palette search worker) opens
/// its own connection to the same file; that design is required by
/// rusqlite's thread-safety model, not an optimization.
pub struct BlockStore {
    pub(crate) conn: Connection,
    pub(crate) block_id_allocator: Arc<BlockIdPool>,
    /// T14 (PLAN_v11217 §3.9): the file was opened in `auto_vacuum=NONE`
    /// mode. The pragma value itself is the persistent truth — the prune
    /// routine converts to INCREMENTAL with a one-time VACUUM and clears
    /// this flag; a later `open` re-reads the pragma (naturally idempotent).
    pub(crate) needs_vacuum_migration: bool,
}

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS blocks (\
    id           INTEGER PRIMARY KEY,\
    command      TEXT    NOT NULL,\
    cwd          TEXT,\
    output       TEXT    NOT NULL,\
    styled_output TEXT,\
    exit_code    INTEGER,\
    started_ms   INTEGER NOT NULL,\
    finished_ms  INTEGER,\
    collapsed    INTEGER NOT NULL DEFAULT 0\
);\
CREATE INDEX IF NOT EXISTS idx_blocks_started ON blocks(started_ms);\
CREATE TABLE IF NOT EXISTS tabs (\
    id                  INTEGER PRIMARY KEY,\
    position            INTEGER NOT NULL,\
    active              INTEGER NOT NULL DEFAULT 0,\
    cwd                 TEXT,\
    block_scroll_offset INTEGER NOT NULL DEFAULT 0,\
    editor_buffer       TEXT,\
    shell_phase         TEXT,\
    block_ids           TEXT,\
    panes               TEXT\
);\
CREATE INDEX IF NOT EXISTS idx_tabs_position ON tabs(position);\
CREATE TABLE IF NOT EXISTS meta (\
    key TEXT PRIMARY KEY,\
    v   INTEGER NOT NULL\
);";

impl BlockStore {
    /// Open (creating if needed) the block DB at `path`, ensuring its parent
    /// directory exists and the schema is in place.
    ///
    /// Ordering invariant (PLAN_v11217 §3.9 2b): this must stay the FIRST
    /// open of `blocks.db` in the startup sequence (before
    /// `AnnotationStore` / `SearchIndex` / the palette worker) — the
    /// one-time DELETE→WAL journal switch requires no other active
    /// connections; later opens see the persisted WAL mode and their
    /// repeated SET is a no-op.
    pub fn open(path: &Path) -> Result<Self, PersistenceError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // T14: a freshly-created file gets incremental auto_vacuum BEFORE any
        // table exists, so the pragma is persisted without a migration
        // VACUUM (review P3a). Existing NONE-mode DBs are recorded below and
        // migrated inside the prune routine — off the startup path.
        let file_existed = path.exists();
        let mut conn = Connection::open(path)?;
        if !file_existed {
            // NOTE: the auto_vacuum SET form returns NO row on an empty DB,
            // so this must run via execute_batch, not query_row.
            conn.execute_batch("PRAGMA auto_vacuum=INCREMENTAL;")?;
        }
        // T14 (§3.9 2b): switch the journal to WAL once, here. Readers stop
        // blocking writers, so the background prune / palette connections
        // never stall the main thread's inserts. Best-effort: a busy failure
        // (e.g. a second Weft instance mid-open) logs and keeps the previous
        // journal mode rather than disabling persistence entirely; the main
        // thread's insert-drop window argument in prune.rs covers this mode.
        if let Err(e) =
            conn.query_row::<String, _, _>("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        {
            tracing::warn!(error = %e, "journal_mode=WAL failed; keeping current journal mode");
        }
        // T15a (§3.10): cap the WAL high-water mark on THIS connection —
        // the pragma is per-connection and the main writer must be covered
        // (idempotent: a repeated SET is a no-op).
        crate::persistence::apply_wal_limits(&conn)?;
        conn.execute_batch(SCHEMA)?;
        // CREATE TABLE IF NOT EXISTS does not evolve databases created by an
        // older Weft version, so migrate the D5 active-tab field explicitly.
        ensure_tabs_active_column(&conn)?;
        ensure_column(&conn, "blocks", "cwd", "TEXT")?;
        ensure_column(&conn, "blocks", "styled_output", "TEXT")?;
        ensure_column(
            &conn,
            "blocks",
            "screen_origin",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(&conn, "tabs", "block_ids", "TEXT")?;
        // v1.12.28 (P1-02 ①): per-pane split-tree JSON (single-pane tabs and
        // pre-v1.12.28 rows store NULL → single-pane restore).
        ensure_column(&conn, "tabs", "panes", "TEXT")?;
        let needs_vacuum_migration = if file_existed {
            let mode: i64 = conn.query_row("PRAGMA auto_vacuum", [], |row| row.get(0))?;
            mode == 0 // 0=NONE, 1=FULL, 2=INCREMENTAL
        } else {
            false
        };
        // T14 (§3.9 2): persistent monotonic id counter — seed `next_id` to
        // MAX(id)+1 exactly once (the NOT EXISTS guard closes the seed race
        // in one statement), then atomically grab a reservation segment
        // [hi-RESERVE, hi) in the SAME transaction. Concurrent openers
        // serialize on the write lock and never overlap segments; a crash
        // discards the unused reservation (an id hole — harmless) and never
        // rolls the counter back, so pruned ids are never reused and the
        // old multi-instance INSERT-OR-REPLACE collision is closed.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO meta(key, v) \
             SELECT 'next_id', COALESCE((SELECT MAX(id) FROM blocks), 0) + 1 \
             WHERE NOT EXISTS (SELECT 1 FROM meta WHERE key = 'next_id')",
            [],
        )?;
        tx.execute(
            "UPDATE meta SET v = v + ?1 WHERE key = 'next_id'",
            [ID_RESERVE as i64],
        )?;
        let reserved_hi: i64 =
            tx.query_row("SELECT v FROM meta WHERE key = 'next_id'", [], |row| {
                row.get(0)
            })?;
        tx.commit()?;
        let reserved_hi = reserved_hi.max(ID_RESERVE as i64) as u64;
        let reserved_lo = reserved_hi - ID_RESERVE;
        let allocator = Arc::new(BlockIdPool::new(reserved_lo, reserved_hi));
        // Reservation re-grab runs on a short-lived dedicated connection: the
        // pool must not hold shared access to `self.conn` (Connection is Send
        // but not Sync, see the struct docs). Fires once per ID_RESERVE
        // allocations; a busy grab fails and the pool overshoots (ids stay
        // monotonic — the hole is harmless), retrying on a later allocation.
        let refill_path = path.to_path_buf();
        // Reviewer MEDIUM-2: every refill failure mode must be visible — a
        // persistently busy refill hides a cross-process id collision window
        // behind nothing but an (harmless) id hole.
        allocator.install_refill(Box::new(move || {
            let grab = || -> Result<u64, PersistenceError> {
                let mut conn = Connection::open(&refill_path)?;
                let _ = conn.busy_timeout(std::time::Duration::from_millis(2000));
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                tx.execute(
                    "UPDATE meta SET v = v + ?1 WHERE key = 'next_id'",
                    [ID_RESERVE as i64],
                )?;
                let v: i64 =
                    tx.query_row("SELECT v FROM meta WHERE key = 'next_id'", [], |row| {
                        row.get(0)
                    })?;
                tx.commit()?;
                Ok(v as u64)
            };
            match grab() {
                Ok(hi) => Some(hi),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "id-pool refill failed: pool overshoots (ids stay monotonic)"
                    );
                    None
                }
            }
        }));
        Ok(Self {
            conn,
            block_id_allocator: allocator,
            needs_vacuum_migration,
        })
    }

    /// Shared allocator used by every tab writing to this store, preventing
    /// per-terminal BlockId sequences from replacing each other in SQLite.
    /// T14: a hi/lo [`BlockIdPool`] over the persistent `meta.next_id`
    /// counter — see [`crate::block_id_sequence`] for the protocol.
    pub fn block_id_allocator(&self) -> Arc<BlockIdPool> {
        self.block_id_allocator.clone()
    }

    /// Insert (or replace by id) a single block.
    pub fn insert(&self, block: &Block) -> Result<(), PersistenceError> {
        let styled_output = block
            .styled_output
            .as_deref()
            .and_then(|styled| serde_json::to_string(styled).ok())
            .filter(|json| json.len() <= MAX_STYLED_OUTPUT_JSON_BYTES);
        self.conn.execute(
            "INSERT OR REPLACE INTO blocks \
             (id, command, cwd, output, styled_output, exit_code, started_ms, finished_ms, collapsed, screen_origin) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                block.id.0 as i64,
                &block.command,
                block.cwd.as_deref(),
                block.output.as_ref(),
                styled_output,
                block.exit_code,
                system_time_to_millis(block.started_at),
                block.finished_at.map(system_time_to_millis),
                block.collapsed as i64,
                block.screen_origin as i64,
            ],
        )?;
        Ok(())
    }

    /// The most recent `limit` blocks, newest first (by start time, then id).
    pub fn recent(&self, limit: usize) -> Result<Vec<Block>, PersistenceError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT id, command, cwd, output, \
                    CASE WHEN length(CAST(styled_output AS BLOB)) <= {MAX_STYLED_OUTPUT_JSON_BYTES} THEN styled_output END, \
                    exit_code, started_ms, finished_ms, collapsed, screen_origin \
             FROM blocks ORDER BY started_ms DESC, id DESC LIMIT ?1",
        ))?;
        let rows = stmt.query_map(params![limit as i64], row_to_block)?;
        rows.map(|r| r.map_err(PersistenceError::from)).collect()
    }

    /// Blocks whose command or output contains `query` (case-insensitive),
    /// newest first. Plain substring match via `INSTR` — no FTS overhead for
    /// v0.4; can upgrade to FTS5 if the history grows large.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Block>, PersistenceError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT id, command, cwd, output, \
                    CASE WHEN length(CAST(styled_output AS BLOB)) <= {MAX_STYLED_OUTPUT_JSON_BYTES} THEN styled_output END, \
                    exit_code, started_ms, finished_ms, collapsed, screen_origin \
             FROM blocks \
             WHERE INSTR(LOWER(command), LOWER(?1)) > 0 \
                OR INSTR(LOWER(output), LOWER(?1)) > 0 \
             ORDER BY started_ms DESC, id DESC LIMIT ?2",
        ))?;
        let rows = stmt.query_map(params![query, limit as i64], row_to_block)?;
        rows.map(|r| r.map_err(PersistenceError::from)).collect()
    }

    /// v1.11.2 X4 (PLAN_v1112 §1.3): keyset-paginated history for the panel's
    /// "load older" action. Returns up to `limit` blocks strictly OLDER than
    /// `started_ms_exclusive`, newest first — uses idx_blocks_started so a
    /// deep page scan never degenerates into a full-table sort. Strict `<`
    /// makes repeated pages disjoint: a block whose started_ms equals the
    /// caller's cursor is never returned twice.
    pub fn older_than(
        &self,
        started_ms_exclusive: i64,
        limit: usize,
    ) -> Result<Vec<Block>, PersistenceError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT id, command, cwd, output, \
                    CASE WHEN length(CAST(styled_output AS BLOB)) <= {MAX_STYLED_OUTPUT_JSON_BYTES} THEN styled_output END, \
                    exit_code, started_ms, finished_ms, collapsed, screen_origin \
             FROM blocks \
             WHERE started_ms < ?1 \
             ORDER BY started_ms DESC, id DESC LIMIT ?2",
        ))?;
        let rows = stmt.query_map(params![started_ms_exclusive, limit as i64], row_to_block)?;
        rows.map(|r| r.map_err(PersistenceError::from)).collect()
    }

    /// Delete every stored block.
    pub fn clear(&self) -> Result<(), PersistenceError> {
        self.conn.execute("DELETE FROM blocks", [])?;
        Ok(())
    }

    /// v1.7.3-D: Fetch a single block by id. Returns `None` if not found.
    /// Used by the search-index integration to look up a block's command
    /// and cwd when indexing a bookmark annotation.
    pub fn get(&self, block_id: BlockId) -> Result<Option<Block>, PersistenceError> {
        self.conn
            .query_row(
                &format!(
                    "SELECT id, command, cwd, output, \
                        CASE WHEN length(CAST(styled_output AS BLOB)) <= {MAX_STYLED_OUTPUT_JSON_BYTES} THEN styled_output END, \
                        exit_code, started_ms, finished_ms, collapsed, screen_origin \
                 FROM blocks WHERE id = ?1",
                ),
                params![block_id.0 as i64],
                row_to_block,
            )
            .optional()
            .map_err(PersistenceError::from)
    }
}

/// Decode a stored row into a [`Block`].
pub(crate) fn row_to_block(row: &rusqlite::Row) -> rusqlite::Result<Block> {
    let id: i64 = row.get(0)?;
    let command: String = row.get(1)?;
    let cwd: Option<String> = row.get(2)?;
    let output: String = row.get(3)?;
    let styled_json: Option<String> = row.get(4)?;
    let exit_code: Option<i32> = row.get(5)?;
    let started_ms: i64 = row.get(6)?;
    let finished_ms: Option<i64> = row.get(7)?;
    let collapsed: i64 = row.get(8)?;
    // v1.10.26 Batch B review closure (SF-1): screen_origin persisted since
    // Batch B — a restored TUI block keeps clip-not-wrap across Restore. The
    // DEFAULT 0 covers legacy rows (shell-wrap semantics), matching the
    // runtime default for ordinary commands.
    let screen_origin: i64 = row.get(9)?;
    Ok(Block {
        id: BlockId(id as u64),
        command,
        cwd,
        output: output.into(),
        styled_output: styled_json
            .filter(|json| json.len() <= MAX_STYLED_OUTPUT_JSON_BYTES)
            .and_then(|json| serde_json::from_str(&json).ok())
            .map(Arc::new),
        exit_code,
        started_at: millis_to_system_time(started_ms),
        finished_at: finished_ms.map(millis_to_system_time),
        collapsed: collapsed != 0,
        screen_origin: screen_origin != 0,
    })
}

// v1.13.12 FIX-G: styled-budget persistence tests split out of the inline
// `mod tests` to keep this file under the architecture gate's 800-line
// budget (same #[path] pattern as paint/block_view/b_path_tests.rs).
#[cfg(test)]
#[path = "blocks_budget_tests.rs"]
mod blocks_budget_tests;

// Tests live in the gate-exempt sibling module (repo test-module
// convention, pty/tests.rs precedent) so inline test lines stay out of
// the production-file budget.
#[cfg(test)]
#[path = "blocks/tests.rs"]
mod tests;
