//! SQLite persistence for command blocks and tab snapshots.
//!
//! Historical note: this module began as a single `persistence.rs` (v0.4
//! "Fabric", phase 3) and was split into submodules in v1.6 to stay under
//! the 800-line file budget while preserving the public API
//! ([`BlockStore`], [`TabSnapshot`]).
//!
//! - [`blocks`]: `BlockStore` + block table CRUD.
//! - [`tabs`]: `TabSnapshot` + tab table CRUD (methods on `BlockStore`).
//! - [`prune`]: T14 dual-gate auto-cleanup (age + size) with three-table
//!   cascade deletes, hi/lo id-counter seeding context, and incremental
//!   space reclamation.
//! - [`migrations`]: schema evolution helpers used by `BlockStore::open`.
//!
//! [`BlockStore`] wraps a single [`rusqlite::Connection`] and stores finished
//! [`Block`](crate::blocks::Block)s: command, output, exit code, timing, and
//! collapse state. Blocks are written as the tracker finishes them and loaded
//! back on startup so the history panel survives restarts. Tab snapshots
//! (v1.0 H4) persist tab layout and editor drafts the same way.
//!
//! SQLite is bundled (compiled from source), so there is no runtime dependency
//! on a system `libsqlite3` — important for the future `.app` distribution.
//! The schema is created idempotently on [`BlockStore::open`].
//!
//! This module is pure logic over the DB file path the caller supplies; the app
//! layer wires it to `<cache>/weft/blocks.db` (see `weft_cache_dir`).

pub mod blocks;
pub mod migrations;
pub mod prune;
pub mod tabs;

pub use blocks::BlockStore;
pub use prune::{run_block_prune, PrunePlan, PruneReport, PruneTerminal};
pub use tabs::TabSnapshot;

use std::time::{Duration, SystemTime};

/// Errors from the block store: filesystem (opening/creating the DB file) or
/// SQLite (query/prepare).
#[derive(Debug, thiserror::Error)]
pub enum PersistenceError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// `SystemTime` → Unix epoch millis (signed: pre-epoch times are negative).
pub(crate) fn system_time_to_millis(t: SystemTime) -> i64 {
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    }
}

/// Unix epoch millis → `SystemTime`.
pub(crate) fn millis_to_system_time(ms: i64) -> SystemTime {
    if ms >= 0 {
        SystemTime::UNIX_EPOCH + Duration::from_millis(ms as u64)
    } else {
        SystemTime::UNIX_EPOCH - Duration::from_millis(ms.unsigned_abs())
    }
}
