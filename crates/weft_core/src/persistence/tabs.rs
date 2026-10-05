//! Tab snapshots (v1.0 H4) — serializable UI state for tab layout and drafts.
//!
//! [`TabSnapshot`] is a serializable snapshot of a tab's UI state, persisted
//! to SQLite so the tab layout survives restarts. The PTY itself is NOT
//! restored (impossible); on restore, the tab shows the saved editor draft
//! + block history, and the user presses Enter to spawn a fresh shell.

use rusqlite::{params, Row};

use crate::persistence::blocks::BlockStore;
use crate::persistence::PersistenceError;

/// v1.0 H4: A serializable snapshot of a tab's UI state, persisted to
/// SQLite so the tab layout survives restarts. The PTY itself is NOT
/// restored (impossible); on restore, the tab shows the saved editor draft
/// + block history, and the user presses Enter to spawn a fresh shell.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TabSnapshot {
    /// Position in the tab bar (0-based).
    pub position: usize,
    /// Whether this was the active tab when the snapshot was saved.
    #[serde(default)]
    pub active: bool,
    /// Working directory at save time (from shell integration).
    pub cwd: Option<String>,
    /// Block-view scroll offset.
    pub block_scroll_offset: usize,
    /// JSON-serialized [`EditorBuffer`](crate::editor::EditorBuffer).
    pub editor_buffer: String,
    /// Shell phase as a string: "NotIntegrated" / "AtPrompt" /
    /// "CommandExecuting".
    pub shell_phase: String,
    /// v1.7.6: IDs of the blocks this tab owns. v1.12.24 (N-3): the full
    /// lineage — session-produced ∪ previously-restored ids — so ↑ recall
    /// survives restart-restore cycles; panel "load older" pages are
    /// deliberately excluded (they are other tabs' blocks). On Restore,
    /// each tab hydrates only its own blocks (filtered from the global
    /// SQLite history by these IDs), preventing all tabs from showing the
    /// same mixed global history. Empty for legacy snapshots predating
    /// v1.7.6 (tab starts with no restored block-view content).
    #[serde(default)]
    pub block_ids: Vec<u64>,
}

impl TabSnapshot {
    /// Index to activate after ordered snapshots are restored. Legacy data
    /// has no active marker and therefore safely falls back to the first tab.
    pub fn restored_active_index(snapshots: &[Self]) -> usize {
        snapshots
            .iter()
            .position(|snapshot| snapshot.active)
            .unwrap_or(0)
    }

    /// v1.0 H4: Serialize an [`EditorBuffer`](crate::editor::EditorBuffer)
    /// to a JSON string for storage. Returns `"{}"` on serialization
    /// failure (so a corrupt buffer doesn't block the save).
    pub fn encode_editor_buffer(buf: &crate::editor::EditorBuffer) -> String {
        serde_json::to_string(buf).unwrap_or_else(|_| "{}".into())
    }

    /// v1.0 H4: Deserialize an [`EditorBuffer`](crate::editor::EditorBuffer)
    /// from the stored JSON string. Returns `None` on parse failure (the
    /// caller falls back to an empty buffer).
    pub fn decode_editor_buffer(json: &str) -> Option<crate::editor::EditorBuffer> {
        serde_json::from_str(json).ok()
    }
}

impl BlockStore {
    /// v1.0 H4: Replace the entire `tabs` table with `snapshots`. The
    /// table is cleared and re-inserted in a single transaction so the
    /// save is atomic — a crash mid-save leaves the previous state
    /// intact. Position ordering is preserved.
    ///
    /// v1.12.24 (N-2 wipe guard): an empty snapshot list only occurs in
    /// the exit transient (tabs already drained) — executing the DELETE
    /// below would wipe the table and the next launch would restore zero
    /// tabs. Explicit full clears go through [`BlockStore::clear_tabs`].
    pub fn save_tabs(&self, snapshots: &[TabSnapshot]) -> Result<(), PersistenceError> {
        if snapshots.is_empty() {
            tracing::warn!("refusing to save an empty tabs snapshot (wipe guard)");
            return Ok(());
        }
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM tabs", [])?;
        for snap in snapshots {
            let block_ids_json =
                serde_json::to_string(&snap.block_ids).unwrap_or_else(|_| "[]".into());
            tx.execute(
                "INSERT INTO tabs (id, position, active, cwd, block_scroll_offset, editor_buffer, shell_phase, block_ids) \
                 VALUES (NULL, ?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    snap.position as i64,
                    snap.active as i64,
                    snap.cwd.as_deref().unwrap_or(""),
                    snap.block_scroll_offset as i64,
                    &snap.editor_buffer,
                    &snap.shell_phase,
                    &block_ids_json,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// v1.0 H4: Load all saved tab snapshots, ordered by `position`.
    /// Returns an empty Vec when no tabs have been saved (first launch
    /// or after [`BlockStore::clear_tabs`]).
    pub fn load_tabs(&self) -> Result<Vec<TabSnapshot>, PersistenceError> {
        let mut stmt = self.conn.prepare(
            "SELECT position, active, cwd, block_scroll_offset, editor_buffer, shell_phase, block_ids \
             FROM tabs ORDER BY position ASC",
        )?;
        let rows = stmt.query_map([], row_to_tab_snapshot)?;
        let mut out = Vec::new();
        for row in rows {
            let mut snap = row?;
            // Empty string → None (matches save_tabs's encoding).
            if snap.cwd.as_deref() == Some("") {
                snap.cwd = None;
            }
            out.push(snap);
        }
        Ok(out)
    }

    /// v1.0 H4: Delete every saved tab snapshot for an explicit session reset.
    /// Normal startup deliberately retains the last atomic snapshot until the
    /// next save replaces it, so an early crash cannot erase recovery data.
    pub fn clear_tabs(&self) -> Result<(), PersistenceError> {
        self.conn.execute("DELETE FROM tabs", [])?;
        Ok(())
    }
}

/// v1.0 H4: Decode a stored row into a [`TabSnapshot`].
pub(crate) fn row_to_tab_snapshot(row: &Row) -> rusqlite::Result<TabSnapshot> {
    let position: i64 = row.get(0)?;
    let active: bool = row.get(1)?;
    let cwd: String = row.get(2)?;
    let block_scroll_offset: i64 = row.get(3)?;
    let editor_buffer: String = row.get(4)?;
    let shell_phase: String = row.get(5)?;
    let block_ids_json: Option<String> = row.get(6).ok();
    let block_ids: Vec<u64> = block_ids_json
        .as_deref()
        .and_then(|json| serde_json::from_str(json).ok())
        .unwrap_or_default();
    Ok(TabSnapshot {
        position: position as usize,
        active,
        cwd: Some(cwd),
        block_scroll_offset: block_scroll_offset as usize,
        editor_buffer,
        shell_phase,
        block_ids,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::BlockStore;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::SystemTime;

    static TEMP_STORE_COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A unique temp DB path for one test (auto-cleaned by the OS temp dir
    /// lifecycle; we also clear() to keep tests independent). Duplicated from
    /// `blocks::tests` because the helpers are tiny and test modules cannot
    /// share private items across files without a `pub(crate)` test-support
    /// module — overkill for two functions.
    fn temp_store() -> BlockStore {
        let path = std::env::temp_dir().join(format!(
            "weft-tabs-store-{}-{}-{}.db",
            std::process::id(),
            TEMP_STORE_COUNTER.fetch_add(1, Ordering::Relaxed),
            SystemTime::UNIX_EPOCH
                .elapsed()
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        BlockStore::open(&path).expect("open temp store")
    }

    fn snapshot(position: usize, cwd: &str, scroll: usize, phase: &str) -> TabSnapshot {
        TabSnapshot {
            position,
            active: position == 1,
            cwd: Some(cwd.to_string()),
            block_scroll_offset: scroll,
            editor_buffer: r#"{"lines":["ls -la"],"cursor":[0,7],"selection_anchor":null}"#
                .to_string(),
            shell_phase: phase.to_string(),
            block_ids: Vec::new(),
        }
    }

    /// v1.12.24 (N-2/N-3 lineage chain): a snapshot carrying the union of
    /// session-produced and loaded ids (what `Tab::to_snapshot` writes via
    /// `lineage_block_ids`) survives save→load byte-identical, so the next
    /// launch's hydrate can recall both generations.
    #[test]
    fn lineage_block_ids_roundtrip_through_sqlite() {
        let store = temp_store();
        let mut snap = snapshot(0, "/work", 0, "AtPrompt");
        snap.block_ids = vec![1, 2, 3, 4]; // sorted+deduped lineage
        store.save_tabs(&[snap]).unwrap();
        let loaded = store.load_tabs().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].block_ids, vec![1, 2, 3, 4]);
    }

    #[test]
    fn save_and_load_tabs_roundtrip() {
        let store = temp_store();
        let snaps = vec![
            snapshot(0, "/home/user", 3, "AtPrompt"),
            snapshot(1, "/tmp", 0, "CommandExecuting"),
        ];
        store.save_tabs(&snaps).unwrap();
        let loaded = store.load_tabs().unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].position, 0);
        assert!(!loaded[0].active);
        assert_eq!(loaded[0].cwd.as_deref(), Some("/home/user"));
        assert_eq!(loaded[0].block_scroll_offset, 3);
        assert_eq!(loaded[0].shell_phase, "AtPrompt");
        assert_eq!(loaded[1].position, 1);
        assert!(loaded[1].active);
        assert_eq!(loaded[1].cwd.as_deref(), Some("/tmp"));
        assert_eq!(loaded[1].shell_phase, "CommandExecuting");
    }

    #[test]
    fn block_ids_roundtrip_through_sqlite() {
        // v1.7.6: per-tab block IDs must survive save→load as JSON TEXT.
        let store = temp_store();
        let mut snap = snapshot(0, "/work", 0, "AtPrompt");
        snap.block_ids = vec![7, 42, 100, 9999];
        store.save_tabs(&[snap]).unwrap();
        let loaded = store.load_tabs().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].block_ids, vec![7, 42, 100, 9999]);
    }

    #[test]
    fn restored_active_index_uses_marker_and_falls_back_for_legacy_data() {
        let mut snaps = vec![
            snapshot(0, "/a", 0, "AtPrompt"),
            snapshot(1, "/b", 0, "AtPrompt"),
        ];
        assert_eq!(TabSnapshot::restored_active_index(&snaps), 1);
        snaps[1].active = false;
        assert_eq!(TabSnapshot::restored_active_index(&snaps), 0);
        assert_eq!(TabSnapshot::restored_active_index(&[]), 0);
    }

    #[test]
    fn save_tabs_replaces_previous() {
        let store = temp_store();
        store
            .save_tabs(&[snapshot(0, "/a", 0, "AtPrompt")])
            .unwrap();
        store
            .save_tabs(&[snapshot(0, "/b", 5, "AtPrompt")])
            .unwrap();
        let loaded = store.load_tabs().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].cwd.as_deref(), Some("/b"));
        assert_eq!(loaded[0].block_scroll_offset, 5);
    }

    #[test]
    fn load_tabs_empty_when_unsaved() {
        let store = temp_store();
        let loaded = store.load_tabs().unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn clear_tabs_removes_all() {
        let store = temp_store();
        store
            .save_tabs(&[snapshot(0, "/a", 0, "AtPrompt")])
            .unwrap();
        assert_eq!(store.load_tabs().unwrap().len(), 1);
        store.clear_tabs().unwrap();
        assert!(store.load_tabs().unwrap().is_empty());
    }

    /// v1.12.24 (N-2 wipe guard): an empty save must NOT wipe existing rows —
    /// the exit transient (tabs already drained) used to DELETE the whole
    /// table on `save_tabs([])`, so the next launch restored zero tabs.
    /// (Log line "wipe guard" is emitted via tracing::warn!; this workspace
    /// has no log-capture test infra, so the DB state carries the assertion.)
    #[test]
    fn save_empty_tabs_refuses_to_wipe_existing_rows() {
        let store = temp_store();
        store
            .save_tabs(&[snapshot(0, "/a", 0, "AtPrompt")])
            .unwrap();
        store.save_tabs(&[]).unwrap();
        let loaded = store.load_tabs().unwrap();
        assert_eq!(loaded.len(), 1, "wipe guard must keep the existing row");
        assert_eq!(loaded[0].cwd.as_deref(), Some("/a"));
        // A fresh (never-populated) DB stays empty — the guard is a no-op.
        let fresh = temp_store();
        fresh.save_tabs(&[]).unwrap();
        assert!(fresh.load_tabs().unwrap().is_empty());
    }

    #[test]
    fn load_preserves_position_ordering() {
        let store = temp_store();
        let snaps = vec![
            snapshot(2, "/c", 0, "AtPrompt"),
            snapshot(0, "/a", 0, "AtPrompt"),
            snapshot(1, "/b", 0, "AtPrompt"),
        ];
        store.save_tabs(&snaps).unwrap();
        let loaded = store.load_tabs().unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].position, 0);
        assert_eq!(loaded[1].position, 1);
        assert_eq!(loaded[2].position, 2);
    }

    #[test]
    fn tabs_survive_reopen() {
        let path = std::env::temp_dir().join(format!(
            "weft-tabs-reopen-{}-{}.db",
            std::process::id(),
            SystemTime::UNIX_EPOCH.elapsed().unwrap().as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        {
            let store = BlockStore::open(&path).unwrap();
            store
                .save_tabs(&[snapshot(0, "/persisted", 7, "AtPrompt")])
                .unwrap();
        }
        let store = BlockStore::open(&path).unwrap();
        let loaded = store.load_tabs().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].cwd.as_deref(), Some("/persisted"));
        assert_eq!(loaded[0].block_scroll_offset, 7);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn open_migrates_legacy_tabs_without_losing_snapshots() {
        let path = std::env::temp_dir().join(format!(
            "weft-tabs-legacy-{}-{}.db",
            std::process::id(),
            SystemTime::UNIX_EPOCH.elapsed().unwrap().as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE tabs (\
                    id INTEGER PRIMARY KEY, position INTEGER NOT NULL, cwd TEXT,\
                    block_scroll_offset INTEGER NOT NULL DEFAULT 0,\
                    editor_buffer TEXT, shell_phase TEXT\
                 );\
                 INSERT INTO tabs (position, cwd, block_scroll_offset, editor_buffer, shell_phase)\
                 VALUES (0, '/legacy', 4, '{}', 'AtPrompt');",
            )
            .unwrap();
        }
        let store = BlockStore::open(&path).unwrap();
        let loaded = store.load_tabs().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].cwd.as_deref(), Some("/legacy"));
        assert_eq!(loaded[0].block_scroll_offset, 4);
        assert!(!loaded[0].active, "legacy snapshots default to first tab");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn encode_decode_editor_buffer_roundtrip() {
        use crate::editor::EditorBuffer;
        let buf = EditorBuffer {
            lines: vec!["git status".to_string(), "second line".to_string()],
            cursor: (1, 5),
            selection_anchor: Some((0, 0)),
            scroll_offset: 0,
        };
        let json = TabSnapshot::encode_editor_buffer(&buf);
        let decoded = TabSnapshot::decode_editor_buffer(&json).expect("decode");
        assert_eq!(decoded, buf);
    }

    #[test]
    fn decode_legacy_editor_buffer_defaults_scroll_offset() {
        let legacy = r#"{"lines":["echo old"],"cursor":[0,8],"selection_anchor":null}"#;
        let decoded = TabSnapshot::decode_editor_buffer(legacy).expect("legacy buffer decodes");
        assert_eq!(decoded.lines, ["echo old"]);
        assert_eq!(decoded.cursor, (0, 8));
        assert_eq!(decoded.scroll_offset, 0);
    }

    #[test]
    fn decode_invalid_json_returns_none() {
        assert!(TabSnapshot::decode_editor_buffer("not json").is_none());
        assert!(TabSnapshot::decode_editor_buffer("").is_none());
    }

    #[test]
    fn snapshot_with_real_editor_buffer_roundtrips() {
        use crate::editor::EditorBuffer;
        let store = temp_store();
        let buf = EditorBuffer {
            lines: vec!["ls -la /home".to_string()],
            cursor: (0, 13),
            selection_anchor: None,
            scroll_offset: 0,
        };
        let snap = TabSnapshot {
            position: 0,
            active: true,
            cwd: Some("/home/user".to_string()),
            block_scroll_offset: 2,
            editor_buffer: TabSnapshot::encode_editor_buffer(&buf),
            shell_phase: "AtPrompt".to_string(),
            block_ids: Vec::new(),
        };
        store.save_tabs(&[snap]).unwrap();
        let loaded = store.load_tabs().unwrap();
        assert_eq!(loaded.len(), 1);
        let decoded = TabSnapshot::decode_editor_buffer(&loaded[0].editor_buffer).expect("decode");
        assert_eq!(decoded.lines, vec!["ls -la /home".to_string()]);
        assert_eq!(decoded.cursor, (0, 13));
    }
}
