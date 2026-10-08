//! v1.7.3: Block annotations — bookmarks, notes, and tags.
//!
//! Stored in a sidecar `block_annotations` table within the same `blocks.db`
//! file. Decoupled from the `blocks` table so that `INSERT OR REPLACE` on a
//! block (e.g. output updates) never wipes an annotation, and so old DBs
//! migrate with zero schema churn on the hot path.
//!
//! ## Why a side table (not columns on `blocks`)
//!
//! `BlockStore::insert` uses `INSERT OR REPLACE` — non-mentioned columns get
//! reset to defaults on every update. A separate table avoids that footgun
//! and keeps the `Block` struct untouched (no test-fixture churn across 20+
//! construction sites). `AnnotationStore` opens its own `Connection` to the
//! same DB file, mirroring how `SearchIndex` already sidecars `blocks.db`.
//!
//! ## Tag storage
//!
//! Tags are stored as a comma-separated `TEXT` column. The parse/serialize
//! pair lives in [`parse_tags`]/[`serialize_tags`] as pure functions so they
//! can be unit-tested without a DB.

use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

use rusqlite::{params, Connection, OptionalExtension};

use crate::blocks::BlockId;
use crate::persistence::{millis_to_system_time, system_time_to_millis, PersistenceError};

/// v1.7.3: User annotation on a block — bookmark flag, freeform note, tags.
///
/// A block has an annotation only when the user explicitly created one
/// (bookmark toggle, note edit, or tag add). Blocks without an annotation
/// row simply have no entry in `AnnotationStore::load_all`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockAnnotation {
    pub block_id: BlockId,
    /// `true` when the user bookmarked the block (star toggle).
    pub bookmarked: bool,
    /// Freeform note. `None` = no note; `Some("")` is normalized to `None`
    /// on save so empty notes don't clutter the DB.
    pub note: Option<String>,
    /// User-assigned tags. Stored as comma-separated TEXT in SQLite.
    pub tags: Vec<String>,
    pub updated_at: SystemTime,
}

/// SQLite-backed store of [`BlockAnnotation`]s. Opens its own connection to
/// the same `blocks.db` file. rusqlite 0.31 `Connection` is `Send` but not
/// `Sync` — `&self` access stays on the owning thread; background consumers
/// (e.g. the prune routine) must open their own connection, which is a
/// thread-safety requirement, not an optimization.
pub struct AnnotationStore {
    conn: Connection,
}

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS block_annotations (\
    block_id    INTEGER PRIMARY KEY,\
    bookmarked  INTEGER NOT NULL DEFAULT 0,\
    note        TEXT,\
    tags        TEXT,\
    updated_ms  INTEGER NOT NULL\
);";

impl AnnotationStore {
    /// Open (creating if needed) the annotation store at `path`, which is
    /// typically the same `blocks.db` file used by `BlockStore`.
    pub fn open(path: &Path) -> Result<Self, PersistenceError> {
        // T15a note: deliberately NOT covered by the WAL size-limit helper
        // (see persistence::apply_wal_limits) — bookmark/note upserts are
        // single-row and rare, orders of magnitude below WAL-bloat scale.
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Open an in-memory store (for tests).
    pub fn open_in_memory() -> Result<Self, PersistenceError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Upsert an annotation. If `annotation.bookmarked` is `false` AND note
    /// is empty AND tags is empty, the row is deleted instead — "toggle off
    /// the bookmark with no note/tags" should not leave an empty stub.
    pub fn upsert(&self, annotation: &BlockAnnotation) -> Result<(), PersistenceError> {
        let note = annotation
            .note
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let tags_csv = serialize_tags(&annotation.tags);

        // Auto-delete empty annotations (no bookmark, no note, no tags).
        if !annotation.bookmarked && note.is_none() && annotation.tags.is_empty() {
            self.conn.execute(
                "DELETE FROM block_annotations WHERE block_id = ?1",
                params![annotation.block_id.0 as i64],
            )?;
            return Ok(());
        }

        self.conn.execute(
            "INSERT OR REPLACE INTO block_annotations \
             (block_id, bookmarked, note, tags, updated_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                annotation.block_id.0 as i64,
                annotation.bookmarked as i64,
                note,
                tags_csv,
                system_time_to_millis(annotation.updated_at),
            ],
        )?;
        Ok(())
    }

    /// Toggle the bookmark flag on `block_id`. Returns the new bookmarked
    /// state. Creates an annotation row if none exists; auto-deletes if the
    /// toggle leaves an empty annotation (no note, no tags, not bookmarked).
    pub fn toggle_bookmark(&self, block_id: BlockId) -> Result<bool, PersistenceError> {
        let existing = self.get(block_id)?;
        let (prev_bookmarked, prev_note, prev_tags) = match existing {
            Some(a) => (a.bookmarked, a.note, a.tags),
            None => (false, None, Vec::new()),
        };
        let new_state = !prev_bookmarked;
        let annotation = BlockAnnotation {
            block_id,
            bookmarked: new_state,
            note: prev_note,
            tags: prev_tags,
            updated_at: SystemTime::now(),
        };
        self.upsert(&annotation)?;
        Ok(new_state)
    }

    /// Set the note on `block_id`. An empty/whitespace note clears it.
    /// Creates an annotation row if none exists.
    pub fn set_note(&self, block_id: BlockId, note: Option<&str>) -> Result<(), PersistenceError> {
        let existing = self.get(block_id)?;
        let (prev_bookmarked, prev_tags) = match existing {
            Some(a) => (a.bookmarked, a.tags),
            None => (false, Vec::new()),
        };
        let annotation = BlockAnnotation {
            block_id,
            bookmarked: prev_bookmarked,
            note: note.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
            tags: prev_tags,
            updated_at: SystemTime::now(),
        };
        self.upsert(&annotation)
    }

    /// Set tags on `block_id`. Creates an annotation row if none exists.
    pub fn set_tags(&self, block_id: BlockId, tags: Vec<String>) -> Result<(), PersistenceError> {
        let existing = self.get(block_id)?;
        let (prev_bookmarked, prev_note) = match existing {
            Some(a) => (a.bookmarked, a.note),
            None => (false, None),
        };
        let annotation = BlockAnnotation {
            block_id,
            bookmarked: prev_bookmarked,
            note: prev_note,
            tags,
            updated_at: SystemTime::now(),
        };
        self.upsert(&annotation)
    }

    /// Get a single annotation, if any.
    pub fn get(&self, block_id: BlockId) -> Result<Option<BlockAnnotation>, PersistenceError> {
        self.conn
            .query_row(
                "SELECT block_id, bookmarked, note, tags, updated_ms \
                 FROM block_annotations WHERE block_id = ?1",
                params![block_id.0 as i64],
                row_to_annotation,
            )
            .optional()
            .map_err(PersistenceError::from)
    }

    /// Load all annotations into a map keyed by block id. Used by the
    /// renderer to draw bookmark icons without per-block queries.
    pub fn load_all(&self) -> Result<HashMap<BlockId, BlockAnnotation>, PersistenceError> {
        let mut stmt = self.conn.prepare(
            "SELECT block_id, bookmarked, note, tags, updated_ms \
             FROM block_annotations",
        )?;
        let rows = stmt.query_map([], row_to_annotation)?;
        let mut map = HashMap::new();
        for row in rows {
            let ann = row?;
            map.insert(ann.block_id, ann);
        }
        Ok(map)
    }

    /// All bookmarked annotations, newest first by `updated_ms`.
    /// Used to populate the search index with `SearchDocumentKind::Bookmark`.
    pub fn bookmarked(&self) -> Result<Vec<BlockAnnotation>, PersistenceError> {
        let mut stmt = self.conn.prepare(
            "SELECT block_id, bookmarked, note, tags, updated_ms \
             FROM block_annotations WHERE bookmarked = 1 \
             ORDER BY updated_ms DESC",
        )?;
        let rows = stmt.query_map([], row_to_annotation)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Set of bookmarked block IDs. Cheaper than `bookmarked()` (no note/tags
    /// decoding) and used by the renderer to draw ★ icons each frame.
    pub fn bookmarked_ids(&self) -> Result<std::collections::HashSet<BlockId>, PersistenceError> {
        let mut stmt = self
            .conn
            .prepare("SELECT block_id FROM block_annotations WHERE bookmarked = 1")?;
        let rows = stmt.query_map([], |row| {
            let id: i64 = row.get(0)?;
            Ok(BlockId(id as u64))
        })?;
        let mut set = std::collections::HashSet::new();
        for row in rows {
            set.insert(row?);
        }
        Ok(set)
    }

    /// Delete an annotation (e.g. when the underlying block is deleted).
    pub fn delete(&self, block_id: BlockId) -> Result<(), PersistenceError> {
        self.conn.execute(
            "DELETE FROM block_annotations WHERE block_id = ?1",
            params![block_id.0 as i64],
        )?;
        Ok(())
    }
}

/// Decode a stored row into a [`BlockAnnotation`].
fn row_to_annotation(row: &rusqlite::Row) -> rusqlite::Result<BlockAnnotation> {
    let block_id: i64 = row.get(0)?;
    let bookmarked: i64 = row.get(1)?;
    let note: Option<String> = row.get(2)?;
    let tags_csv: Option<String> = row.get(3)?;
    let updated_ms: i64 = row.get(4)?;
    Ok(BlockAnnotation {
        block_id: BlockId(block_id as u64),
        bookmarked: bookmarked != 0,
        note: note.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
        tags: parse_tags(tags_csv.as_deref()),
        updated_at: millis_to_system_time(updated_ms),
    })
}

// ---------------------------------------------------------------------------
// Pure tag parse/serialize — no DB, fully unit-testable.
// ---------------------------------------------------------------------------

/// Parse a comma-separated tag string into a `Vec<String>`. Empty/whitespace
/// tags are dropped. Duplicates are removed, preserving first-seen order.
///
/// ```
/// # use weft_core::blocks::annotations::parse_tags;
/// assert_eq!(parse_tags(Some("rust, terminal, rust")), vec!["rust", "terminal"]);
/// assert_eq!(parse_tags(None), Vec::<String>::new());
/// assert_eq!(parse_tags(Some("  ,  ")), Vec::<String>::new());
/// ```
pub fn parse_tags(csv: Option<&str>) -> Vec<String> {
    let Some(csv) = csv else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for tag in csv.split(',') {
        let trimmed = tag.trim();
        if trimmed.is_empty() {
            continue;
        }
        if seen.insert(trimmed.to_string()) {
            out.push(trimmed.to_string());
        }
    }
    out
}

/// Serialize a tag list back to comma-separated form. Tags are trimmed and
/// empties dropped before joining.
///
/// ```
/// # use weft_core::blocks::annotations::serialize_tags;
/// assert_eq!(serialize_tags(&["rust".into(), "terminal".into()]), "rust,terminal");
/// assert_eq!(serialize_tags(&[]), "");
/// ```
pub fn serialize_tags(tags: &[String]) -> String {
    tags.iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_db() -> std::path::PathBuf {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!("weft-annotations-{}-{}.db", std::process::id(), id));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn ann(block_id: u64, bookmarked: bool, note: Option<&str>, tags: &[&str]) -> BlockAnnotation {
        BlockAnnotation {
            block_id: BlockId(block_id),
            bookmarked,
            note: note.map(|s| s.to_string()),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            updated_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        }
    }

    // --- Pure tag functions ---

    #[test]
    fn parse_tags_basic() {
        assert_eq!(
            parse_tags(Some("rust, terminal, cli")),
            vec!["rust", "terminal", "cli"]
        );
    }

    #[test]
    fn parse_tags_empty() {
        assert_eq!(parse_tags(None), Vec::<String>::new());
        assert_eq!(parse_tags(Some("")), Vec::<String>::new());
        assert_eq!(parse_tags(Some("  ,  , ")), Vec::<String>::new());
    }

    #[test]
    fn parse_tags_dedupes_preserving_order() {
        assert_eq!(
            parse_tags(Some("rust, terminal, rust, cli, terminal")),
            vec!["rust", "terminal", "cli"]
        );
    }

    #[test]
    fn serialize_tags_round_trip() {
        let tags = vec!["rust".to_string(), "terminal".to_string()];
        let csv = serialize_tags(&tags);
        assert_eq!(csv, "rust,terminal");
        assert_eq!(parse_tags(Some(&csv)), tags);
    }

    #[test]
    fn serialize_tags_drops_empties() {
        let tags = vec!["rust".to_string(), "".to_string(), "  ".to_string()];
        assert_eq!(serialize_tags(&tags), "rust");
    }

    // --- AnnotationStore CRUD ---

    #[test]
    fn upsert_and_get() {
        let store = AnnotationStore::open_in_memory().unwrap();
        store
            .upsert(&ann(1, true, Some("deploy script"), &["deploy", "prod"]))
            .unwrap();
        let got = store.get(BlockId(1)).unwrap().unwrap();
        assert_eq!(got.block_id, BlockId(1));
        assert!(got.bookmarked);
        assert_eq!(got.note.as_deref(), Some("deploy script"));
        assert_eq!(got.tags, vec!["deploy", "prod"]);
    }

    #[test]
    fn get_missing_returns_none() {
        let store = AnnotationStore::open_in_memory().unwrap();
        assert!(store.get(BlockId(99)).unwrap().is_none());
    }

    #[test]
    fn toggle_bookmark_creates_then_deletes() {
        let store = AnnotationStore::open_in_memory().unwrap();
        // Toggle on: creates a row with bookmarked=true.
        assert!(store.toggle_bookmark(BlockId(1)).unwrap());
        assert!(store.get(BlockId(1)).unwrap().unwrap().bookmarked);

        // Toggle off with no note/tags: auto-deletes the empty annotation.
        assert!(!store.toggle_bookmark(BlockId(1)).unwrap());
        assert!(store.get(BlockId(1)).unwrap().is_none());
    }

    #[test]
    fn toggle_bookmark_preserves_note_on_off() {
        let store = AnnotationStore::open_in_memory().unwrap();
        store.set_note(BlockId(1), Some("important")).unwrap();
        assert!(store.toggle_bookmark(BlockId(1)).unwrap());
        // Toggle off — note should keep the row alive.
        assert!(!store.toggle_bookmark(BlockId(1)).unwrap());
        let got = store.get(BlockId(1)).unwrap().unwrap();
        assert!(!got.bookmarked);
        assert_eq!(got.note.as_deref(), Some("important"));
    }

    #[test]
    fn set_note_empty_clears_note() {
        let store = AnnotationStore::open_in_memory().unwrap();
        store.upsert(&ann(1, true, Some("note"), &["tag"])).unwrap();
        store.set_note(BlockId(1), Some("   ")).unwrap();
        let got = store.get(BlockId(1)).unwrap().unwrap();
        assert!(got.note.is_none());
        // Bookmark and tags preserved.
        assert!(got.bookmarked);
        assert_eq!(got.tags, vec!["tag"]);
    }

    #[test]
    fn set_tags_replaces() {
        let store = AnnotationStore::open_in_memory().unwrap();
        store
            .upsert(&ann(1, false, Some("note"), &["old"]))
            .unwrap();
        store
            .set_tags(BlockId(1), vec!["new1".into(), "new2".into()])
            .unwrap();
        let got = store.get(BlockId(1)).unwrap().unwrap();
        assert_eq!(got.tags, vec!["new1", "new2"]);
        assert_eq!(got.note.as_deref(), Some("note"));
    }

    #[test]
    fn load_all() {
        let store = AnnotationStore::open_in_memory().unwrap();
        store.upsert(&ann(1, true, None, &[])).unwrap();
        store
            .upsert(&ann(2, false, Some("note"), &["tag"]))
            .unwrap();
        // ann(3) not inserted.
        let map = store.load_all().unwrap();
        assert_eq!(map.len(), 2);
        assert!(map.contains_key(&BlockId(1)));
        assert!(map.contains_key(&BlockId(2)));
        assert!(!map.contains_key(&BlockId(3)));
    }

    #[test]
    fn bookmarked_returns_only_bookmarked() {
        let store = AnnotationStore::open_in_memory().unwrap();
        store.upsert(&ann(1, true, None, &[])).unwrap();
        store.upsert(&ann(2, false, Some("note"), &[])).unwrap();
        store
            .upsert(&ann(3, true, Some("important"), &["prod"]))
            .unwrap();
        let bm = store.bookmarked().unwrap();
        assert_eq!(bm.len(), 2);
        assert!(bm.iter().all(|a| a.bookmarked));
    }

    #[test]
    fn bookmarked_ids_returns_set() {
        let store = AnnotationStore::open_in_memory().unwrap();
        store.upsert(&ann(1, true, None, &[])).unwrap();
        store.upsert(&ann(2, false, Some("note"), &[])).unwrap();
        store
            .upsert(&ann(3, true, Some("important"), &["prod"]))
            .unwrap();
        let ids = store.bookmarked_ids().unwrap();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&BlockId(1)));
        assert!(ids.contains(&BlockId(3)));
        assert!(!ids.contains(&BlockId(2)));
    }

    #[test]
    fn delete_removes_annotation() {
        let store = AnnotationStore::open_in_memory().unwrap();
        store.upsert(&ann(1, true, Some("note"), &[])).unwrap();
        store.delete(BlockId(1)).unwrap();
        assert!(store.get(BlockId(1)).unwrap().is_none());
    }

    #[test]
    fn upsert_empty_annotation_auto_deletes() {
        let store = AnnotationStore::open_in_memory().unwrap();
        store.upsert(&ann(1, true, Some("note"), &["tag"])).unwrap();
        // Now "clear" everything.
        store.upsert(&ann(1, false, None, &[])).unwrap();
        assert!(store.get(BlockId(1)).unwrap().is_none());
    }

    #[test]
    fn open_creates_schema_idempotently() {
        let path = temp_db();
        // Open twice — second open must not error on existing schema.
        {
            let _store = AnnotationStore::open(&path).unwrap();
        }
        let store = AnnotationStore::open(&path).unwrap();
        store.upsert(&ann(1, true, None, &[])).unwrap();
        assert!(store.get(BlockId(1)).unwrap().unwrap().bookmarked);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn note_whitespace_only_normalized_to_none() {
        let store = AnnotationStore::open_in_memory().unwrap();
        store.upsert(&ann(1, true, Some("   "), &[])).unwrap();
        let got = store.get(BlockId(1)).unwrap().unwrap();
        assert!(got.note.is_none());
    }
}
