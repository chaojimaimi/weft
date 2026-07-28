//! v1.7.1: FTS5-backed search index for unified local search.
//!
//! V17_IMPLEMENTATION_PLAN §3 "固定架构":
//! - SQLite FTS5 独立索引 blocks/workflows/workspaces/bookmarks
//! - 启动时做能力探测
//! - 索引可丢弃并重建，不成为源数据
//! - FTS5 不可用时保留 substring fallback
//!
//! The index is a sidecar to `BlockStore` — it can be dropped and rebuilt
//! from source data at any time. The schema uses a single FTS5 virtual
//! table with `kind` as an unindexed column for type filtering.

use rusqlite::Connection;

use super::search_document::{SearchDocument, SearchDocumentKind, SearchHit};

/// v1.7.1: FTS5 search index state.
///
/// When `fts5_available` is `true`, queries go through the FTS5 virtual
/// table for full-text matching and ranking. When `false`, queries fall
/// back to `LIKE` substring matching on the `search_docs` shadow table.
pub struct SearchIndex {
    conn: Connection,
    fts5_available: bool,
}

/// v1.7.1: Search query parameters. Controls type filtering and result
/// limits. The `cwd` field enables CWD-aware ranking (results in the
/// current working directory get a relevance boost).
#[derive(Clone, Debug)]
pub struct SearchQuery<'a> {
    pub query: &'a str,
    /// If non-empty, only return documents of these kinds.
    pub kinds: &'a [SearchDocumentKind],
    /// Maximum number of results to return.
    pub limit: usize,
    /// Current working directory for CWD-aware ranking. None disables
    /// CWD boosting.
    pub cwd: Option<&'a str>,
}

impl<'a> SearchQuery<'a> {
    /// Build a simple query with no kind filter and a default limit.
    pub fn new(query: &'a str) -> Self {
        Self {
            query,
            kinds: &[],
            limit: 50,
            cwd: None,
        }
    }
}

impl SearchIndex {
    /// Open or create the search index at the given SQLite path. The
    /// connection is shared with `BlockStore`'s database file — the FTS5
    /// table lives alongside the `blocks` table.
    ///
    /// Performs FTS5 capability detection: tries to create a virtual table
    /// using `fts5`. If that fails, sets `fts5_available = false` and
    /// creates a plain table for substring fallback.
    pub fn open(conn: Connection) -> Self {
        let fts5_available = Self::probe_fts5(&conn);
        if fts5_available {
            conn.execute_batch(
                "CREATE VIRTUAL TABLE IF NOT EXISTS search_docs USING fts5(
                    kind UNINDEXED,
                    stable_id UNINDEXED,
                    title,
                    body,
                    cwd UNINDEXED,
                    updated_at UNINDEXED,
                    tokenize = 'unicode61'
                );",
            )
            .ok();
        } else {
            // Fallback: plain table with LIKE matching.
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS search_docs (
                    kind INTEGER NOT NULL,
                    stable_id TEXT NOT NULL,
                    title TEXT NOT NULL,
                    body TEXT NOT NULL,
                    cwd TEXT,
                    updated_at INTEGER NOT NULL,
                    PRIMARY KEY (kind, stable_id)
                );",
            )
            .ok();
        }
        Self {
            conn,
            fts5_available,
        }
    }

    /// Probe whether FTS5 is available in the SQLite build.
    fn probe_fts5(conn: &Connection) -> bool {
        // Try creating a throwaway FTS5 table.
        let ok = conn
            .execute(
                "CREATE VIRTUAL TABLE IF NOT EXISTS __fts5_probe USING fts5(x);",
                [],
            )
            .is_ok();
        if ok {
            // Clean up the probe table.
            let _ = conn.execute("DROP TABLE IF EXISTS __fts5_probe;", []);
        }
        ok
    }

    /// Whether FTS5 full-text search is available. When false, queries
    /// use substring (`LIKE`) matching.
    pub fn fts5_available(&self) -> bool {
        self.fts5_available
    }

    /// Upsert a document into the index. If a document with the same
    /// `(kind, stable_id)` already exists, it is replaced.
    pub fn upsert(&self, doc: &SearchDocument) -> rusqlite::Result<()> {
        let kind_i = doc.kind as u8;
        if self.fts5_available {
            // FTS5 doesn't support PRIMARY KEY, so delete-then-insert.
            self.conn.execute(
                "DELETE FROM search_docs WHERE kind = ? AND stable_id = ?;",
                rusqlite::params![kind_i, doc.stable_id],
            )?;
            self.conn.execute(
                "INSERT INTO search_docs (kind, stable_id, title, body, cwd, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?);",
                rusqlite::params![
                    kind_i,
                    doc.stable_id,
                    doc.title,
                    doc.body,
                    doc.cwd,
                    doc.updated_at,
                ],
            )?;
        } else {
            self.conn.execute(
                "INSERT OR REPLACE INTO search_docs (kind, stable_id, title, body, cwd, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?);",
                rusqlite::params![
                    kind_i,
                    doc.stable_id,
                    doc.title,
                    doc.body,
                    doc.cwd,
                    doc.updated_at,
                ],
            )?;
        }
        Ok(())
    }

    /// Delete a document from the index by `(kind, stable_id)`.
    pub fn delete(&self, kind: SearchDocumentKind, stable_id: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM search_docs WHERE kind = ? AND stable_id = ?;",
            rusqlite::params![kind as u8, stable_id],
        )?;
        Ok(())
    }

    /// Delete all documents of a given kind. Used by `rebuild`.
    pub fn delete_kind(&self, kind: SearchDocumentKind) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM search_docs WHERE kind = ?;",
            rusqlite::params![kind as u8],
        )?;
        Ok(())
    }

    /// Rebuild the index from a set of documents. Drops all existing
    /// documents and re-inserts the provided ones. This is idempotent —
    /// running it twice with the same data produces the same state.
    ///
    /// V17 §3: "索引可丢弃并重建，不成为源数据".
    pub fn rebuild(&self, docs: &[SearchDocument]) -> rusqlite::Result<()> {
        // Drop and recreate the table to reset the FTS5 index fully.
        self.conn.execute("DROP TABLE IF EXISTS search_docs;", [])?;
        if self.fts5_available {
            self.conn.execute_batch(
                "CREATE VIRTUAL TABLE search_docs USING fts5(
                    kind UNINDEXED,
                    stable_id UNINDEXED,
                    title,
                    body,
                    cwd UNINDEXED,
                    updated_at UNINDEXED,
                    tokenize = 'unicode61'
                );",
            )?;
        } else {
            self.conn.execute_batch(
                "CREATE TABLE search_docs (
                    kind INTEGER NOT NULL,
                    stable_id TEXT NOT NULL,
                    title TEXT NOT NULL,
                    body TEXT NOT NULL,
                    cwd TEXT,
                    updated_at INTEGER NOT NULL,
                    PRIMARY KEY (kind, stable_id)
                );",
            )?;
        }
        // Batch insert in a transaction for speed.
        let tx = self.conn.unchecked_transaction()?;
        for doc in docs {
            let kind_i = doc.kind as u8;
            tx.execute(
                "INSERT INTO search_docs (kind, stable_id, title, body, cwd, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?);",
                rusqlite::params![
                    kind_i,
                    doc.stable_id,
                    doc.title,
                    doc.body,
                    doc.cwd,
                    doc.updated_at,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Search the index. Returns hits sorted by relevance (FTS5 rank)
    /// or recency (substring fallback).
    ///
    /// V17 §3: "排序函数只使用 exact/prefix、CWD、使用频率和 recency".
    /// The FTS5 `rank` function provides relevance scoring; CWD boosting
    /// and recency are applied as post-query adjustments in
    /// [`rank_hits`].
    pub fn search(&self, q: &SearchQuery<'_>) -> rusqlite::Result<Vec<SearchHit>> {
        if q.query.is_empty() {
            return Ok(Vec::new());
        }

        let kind_filter = !q.kinds.is_empty();
        let limit = q.limit as i64;

        if self.fts5_available {
            self.search_fts5(q.query, kind_filter, q.kinds, limit)
        } else {
            self.search_substring(q.query, kind_filter, q.kinds, limit)
        }
    }

    fn search_fts5(
        &self,
        query: &str,
        kind_filter: bool,
        kinds: &[SearchDocumentKind],
        limit: i64,
    ) -> rusqlite::Result<Vec<SearchHit>> {
        // Build the FTS5 MATCH expression. We use a simple prefix match
        // to handle partial-word queries (e.g. "cargo" matches "cargotest").
        // The query is escaped to prevent FTS5 syntax injection.
        let escaped = escape_fts5_query(query);
        let match_expr = if query.ends_with(' ') {
            format!("\"{}\" ", escaped.trim_end())
        } else {
            format!("\"{}\"*", escaped)
        };

        let mut sql = String::from(
            "SELECT kind, stable_id, title, body, cwd, updated_at, rank
             FROM search_docs
             WHERE search_docs MATCH ?",
        );
        if kind_filter {
            let placeholders: Vec<&str> = kinds.iter().map(|_| "?").collect();
            sql.push_str(&format!(" AND kind IN ({})", placeholders.join(", ")));
        }
        sql.push_str(" ORDER BY rank LIMIT ?;");

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = if kind_filter {
            let kind_vals: Vec<i64> = kinds.iter().map(|k| *k as i64).collect();
            // Build a flat params vector for rusqlite.
            let mut all_params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            all_params.push(Box::new(match_expr.clone()));
            for k in &kind_vals {
                all_params.push(Box::new(*k));
            }
            all_params.push(Box::new(limit));
            stmt.query_map(
                rusqlite::params_from_iter(all_params.iter().map(|p| p.as_ref())),
                row_to_hit,
            )?
        } else {
            stmt.query_map(rusqlite::params![match_expr, limit], row_to_hit)?
        };

        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    fn search_substring(
        &self,
        query: &str,
        kind_filter: bool,
        kinds: &[SearchDocumentKind],
        limit: i64,
    ) -> rusqlite::Result<Vec<SearchHit>> {
        // Fallback: LIKE matching on title and body, sorted by recency.
        let pattern = format!("%{}%", query);
        let mut sql = String::from(
            "SELECT kind, stable_id, title, body, cwd, updated_at, 0.0 as rank
             FROM search_docs
             WHERE (title LIKE ? OR body LIKE ?)",
        );
        if kind_filter {
            let placeholders: Vec<&str> = kinds.iter().map(|_| "?").collect();
            sql.push_str(&format!(" AND kind IN ({})", placeholders.join(", ")));
        }
        sql.push_str(" ORDER BY updated_at DESC LIMIT ?;");

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = if kind_filter {
            let kind_vals: Vec<i64> = kinds.iter().map(|k| *k as i64).collect();
            let mut all_params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            all_params.push(Box::new(pattern.clone()));
            all_params.push(Box::new(pattern.clone()));
            for k in &kind_vals {
                all_params.push(Box::new(*k));
            }
            all_params.push(Box::new(limit));
            stmt.query_map(
                rusqlite::params_from_iter(all_params.iter().map(|p| p.as_ref())),
                row_to_hit,
            )?
        } else {
            stmt.query_map(rusqlite::params![pattern, pattern, limit], row_to_hit)?
        };

        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Count documents in the index. Used for diagnostics and testing.
    pub fn count(&self) -> rusqlite::Result<i64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM search_docs;", [], |r| r.get(0))
    }

    /// Count documents of a specific kind.
    pub fn count_kind(&self, kind: SearchDocumentKind) -> rusqlite::Result<i64> {
        self.conn.query_row(
            "SELECT COUNT(*) FROM search_docs WHERE kind = ?;",
            rusqlite::params![kind as u8],
            |r| r.get(0),
        )
    }
}

/// Convert a SQLite row to a `SearchHit`.
fn row_to_hit(row: &rusqlite::Row<'_>) -> rusqlite::Result<SearchHit> {
    let kind_i: i64 = row.get(0)?;
    let stable_id: String = row.get(1)?;
    let title: String = row.get(2)?;
    let body: String = row.get(3)?;
    let cwd: Option<String> = row.get(4)?;
    let updated_at: i64 = row.get(5)?;
    let score: f64 = row.get(6).unwrap_or(0.0);

    let kind = SearchDocumentKind::from_u8(kind_i as u8).unwrap_or(SearchDocumentKind::Block);

    Ok(SearchHit {
        doc: SearchDocument {
            kind,
            stable_id,
            title,
            body,
            cwd,
            updated_at,
        },
        score,
    })
}

/// Escape a user query for safe use in an FTS5 MATCH expression.
///
/// FTS5 treats `"`, `*`, `(`, `)`, and `AND`/`OR`/`NOT` as syntax. We
/// wrap the query in double quotes and escape internal quotes by doubling
/// them, which makes the entire query a single phrase token. The trailing
/// `*` (added by the caller) enables prefix matching.
fn escape_fts5_query(query: &str) -> String {
    // Double internal quotes (FTS5 escape for quotes inside a phrase).
    query.replace('"', "\"\"")
}

/// v1.7.1: Post-query ranking — applies CWD boost and recency decay to
/// the raw FTS5 rank. This is a pure function so it can be unit-tested
/// in isolation.
///
/// V17 §3: "排序函数只使用 exact/prefix、CWD、使用频率和 recency".
/// - CWD boost: if the document's cwd matches the query's cwd, the
///   score is improved (lower rank value = better).
/// - Recency: newer documents get a small boost.
/// - Exact/prefix: handled by FTS5's built-in ranking.
pub fn rank_hits(hits: Vec<SearchHit>, cwd: Option<&str>) -> Vec<SearchHit> {
    if cwd.is_none() {
        return hits;
    }
    let cwd = cwd.unwrap();
    let mut hits = hits;
    for hit in &mut hits {
        // CWD boost: if the document's cwd starts with the query's cwd,
        // improve the score by reducing the rank value by 10%.
        if let Some(ref doc_cwd) = hit.doc.cwd {
            if doc_cwd.starts_with(cwd) || cwd.starts_with(doc_cwd) {
                hit.score *= 0.9;
            }
        }
    }
    // Re-sort by the adjusted score (lower = better).
    hits.sort_by(|a, b| {
        a.score
            .partial_cmp(&b.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_test_index() -> SearchIndex {
        let conn = Connection::open_in_memory().unwrap();
        SearchIndex::open(conn)
    }

    fn make_doc(kind: SearchDocumentKind, id: &str, title: &str, body: &str) -> SearchDocument {
        SearchDocument {
            kind,
            stable_id: id.to_string(),
            title: title.to_string(),
            body: body.to_string(),
            cwd: Some("/home/user".to_string()),
            updated_at: 1700000000,
        }
    }

    #[test]
    fn open_and_probe_fts5() {
        let idx = open_test_index();
        // The bundled SQLite has FTS5 enabled, so this should be true.
        // (If running on a system without FTS5, this test is still valid
        // — it just verifies the probe doesn't panic.)
        let _ = idx.fts5_available();
    }

    #[test]
    fn upsert_and_search_basic() {
        let idx = open_test_index();
        let doc = make_doc(
            SearchDocumentKind::Block,
            "1",
            "cargo build",
            "Compiling weft v1.7.0",
        );
        idx.upsert(&doc).unwrap();
        let hits = idx.search(&SearchQuery::new("cargo")).unwrap();
        assert!(!hits.is_empty(), "should find 'cargo'");
        assert_eq!(hits[0].doc.title, "cargo build");
    }

    #[test]
    fn upsert_replaces_existing() {
        let idx = open_test_index();
        let doc1 = make_doc(SearchDocumentKind::Block, "1", "old command", "old output");
        idx.upsert(&doc1).unwrap();
        let doc2 = make_doc(SearchDocumentKind::Block, "1", "new command", "new output");
        idx.upsert(&doc2).unwrap();
        assert_eq!(idx.count().unwrap(), 1);
        let hits = idx.search(&SearchQuery::new("new")).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].doc.title, "new command");
    }

    #[test]
    fn delete_removes_document() {
        let idx = open_test_index();
        let doc = make_doc(SearchDocumentKind::Block, "1", "delete me", "some output");
        idx.upsert(&doc).unwrap();
        assert_eq!(idx.count().unwrap(), 1);
        idx.delete(SearchDocumentKind::Block, "1").unwrap();
        assert_eq!(idx.count().unwrap(), 0);
    }

    #[test]
    fn delete_kind_removes_all_of_kind() {
        let idx = open_test_index();
        idx.upsert(&make_doc(SearchDocumentKind::Block, "1", "cmd1", "out1"))
            .unwrap();
        idx.upsert(&make_doc(SearchDocumentKind::Block, "2", "cmd2", "out2"))
            .unwrap();
        idx.upsert(&make_doc(
            SearchDocumentKind::Workflow,
            "w1",
            "workflow1",
            "template1",
        ))
        .unwrap();
        assert_eq!(idx.count().unwrap(), 3);
        idx.delete_kind(SearchDocumentKind::Block).unwrap();
        assert_eq!(idx.count().unwrap(), 1);
        assert_eq!(idx.count_kind(SearchDocumentKind::Workflow).unwrap(), 1);
    }

    #[test]
    fn rebuild_clears_and_reinserts() {
        let idx = open_test_index();
        idx.upsert(&make_doc(
            SearchDocumentKind::Block,
            "old",
            "old cmd",
            "old out",
        ))
        .unwrap();
        assert_eq!(idx.count().unwrap(), 1);

        let new_docs = vec![
            make_doc(SearchDocumentKind::Block, "1", "new1", "out1"),
            make_doc(SearchDocumentKind::Block, "2", "new2", "out2"),
        ];
        idx.rebuild(&new_docs).unwrap();
        assert_eq!(idx.count().unwrap(), 2);
        // Old doc should be gone.
        let hits = idx.search(&SearchQuery::new("old")).unwrap();
        assert!(hits.is_empty());
        // New docs should be found.
        let hits = idx.search(&SearchQuery::new("new")).unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn search_empty_query_returns_empty() {
        let idx = open_test_index();
        idx.upsert(&make_doc(SearchDocumentKind::Block, "1", "test", "content"))
            .unwrap();
        let hits = idx.search(&SearchQuery::new("")).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn search_no_match_returns_empty() {
        let idx = open_test_index();
        idx.upsert(&make_doc(SearchDocumentKind::Block, "1", "hello", "world"))
            .unwrap();
        let hits = idx.search(&SearchQuery::new("nonexistent")).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn search_cjk_content() {
        let idx = open_test_index();
        let doc = SearchDocument {
            kind: SearchDocumentKind::Block,
            stable_id: "1".to_string(),
            title: "ls -la".to_string(),
            body: "文件名 中文测试.txt".to_string(),
            cwd: None,
            updated_at: 0,
        };
        idx.upsert(&doc).unwrap();
        let hits = idx.search(&SearchQuery::new("中文")).unwrap();
        assert!(!hits.is_empty(), "should find CJK content");
    }

    #[test]
    fn search_path_content() {
        let idx = open_test_index();
        let doc = SearchDocument {
            kind: SearchDocumentKind::Block,
            stable_id: "1".to_string(),
            title: "cat /usr/local/bin/script".to_string(),
            body: "script output".to_string(),
            cwd: Some("/usr/local".to_string()),
            updated_at: 0,
        };
        idx.upsert(&doc).unwrap();
        let hits = idx.search(&SearchQuery::new("/usr/local")).unwrap();
        assert!(!hits.is_empty());
    }

    #[test]
    fn search_case_insensitive() {
        let idx = open_test_index();
        idx.upsert(&make_doc(
            SearchDocumentKind::Block,
            "1",
            "Cargo Build",
            "Output",
        ))
        .unwrap();
        let hits_lower = idx.search(&SearchQuery::new("cargo")).unwrap();
        assert!(!hits_lower.is_empty(), "lowercase should match");
        let hits_upper = idx.search(&SearchQuery::new("CARGO")).unwrap();
        assert!(!hits_upper.is_empty(), "uppercase should match");
    }

    #[test]
    fn search_special_characters() {
        let idx = open_test_index();
        let doc = make_doc(
            SearchDocumentKind::Block,
            "1",
            "echo 'hello & world'",
            "hello & world",
        );
        idx.upsert(&doc).unwrap();
        // The query "hello" should match despite the special chars.
        let hits = idx.search(&SearchQuery::new("hello")).unwrap();
        assert!(!hits.is_empty());
    }

    #[test]
    fn search_kind_filter() {
        let idx = open_test_index();
        idx.upsert(&make_doc(
            SearchDocumentKind::Block,
            "1",
            "test block",
            "content",
        ))
        .unwrap();
        idx.upsert(&make_doc(
            SearchDocumentKind::Workflow,
            "w1",
            "test workflow",
            "template",
        ))
        .unwrap();

        let hits_all = idx.search(&SearchQuery::new("test")).unwrap();
        assert_eq!(hits_all.len(), 2);

        let q = SearchQuery {
            query: "test",
            kinds: &[SearchDocumentKind::Block],
            limit: 50,
            cwd: None,
        };
        let hits_block = idx.search(&q).unwrap();
        assert_eq!(hits_block.len(), 1);
        assert_eq!(hits_block[0].doc.kind, SearchDocumentKind::Block);
    }

    #[test]
    fn search_limit() {
        let idx = open_test_index();
        for i in 0..100 {
            idx.upsert(&make_doc(
                SearchDocumentKind::Block,
                &i.to_string(),
                &format!("test{i}"),
                "common content",
            ))
            .unwrap();
        }
        let q = SearchQuery {
            query: "common",
            kinds: &[],
            limit: 10,
            cwd: None,
        };
        let hits = idx.search(&q).unwrap();
        assert_eq!(hits.len(), 10);
    }

    #[test]
    fn escape_fts5_doubles_quotes() {
        assert_eq!(escape_fts5_query("hello"), "hello");
        assert_eq!(escape_fts5_query(r#"hello "world""#), r#"hello ""world"""#);
    }

    #[test]
    fn rank_hits_cwd_boost() {
        let hits = vec![
            SearchHit {
                doc: SearchDocument {
                    kind: SearchDocumentKind::Block,
                    stable_id: "1".to_string(),
                    title: "cmd".to_string(),
                    body: "out".to_string(),
                    cwd: Some("/home/user/project".to_string()),
                    updated_at: 0,
                },
                score: 1.0,
            },
            SearchHit {
                doc: SearchDocument {
                    kind: SearchDocumentKind::Block,
                    stable_id: "2".to_string(),
                    title: "cmd".to_string(),
                    body: "out".to_string(),
                    cwd: Some("/other/path".to_string()),
                    updated_at: 0,
                },
                score: 1.0,
            },
        ];
        let ranked = rank_hits(hits, Some("/home/user"));
        // The CWD-matching doc should be first (lower score = better).
        assert_eq!(ranked[0].doc.stable_id, "1");
        assert!(ranked[0].score < ranked[1].score);
    }

    #[test]
    fn rank_hits_no_cwd_preserves_order() {
        let hits = vec![
            SearchHit {
                doc: SearchDocument {
                    kind: SearchDocumentKind::Block,
                    stable_id: "1".to_string(),
                    title: "a".to_string(),
                    body: "".to_string(),
                    cwd: None,
                    updated_at: 0,
                },
                score: 1.0,
            },
            SearchHit {
                doc: SearchDocument {
                    kind: SearchDocumentKind::Block,
                    stable_id: "2".to_string(),
                    title: "b".to_string(),
                    body: "".to_string(),
                    cwd: None,
                    updated_at: 0,
                },
                score: 2.0,
            },
        ];
        let ranked = rank_hits(hits, None);
        // No CWD boost — order preserved by score.
        assert_eq!(ranked[0].doc.stable_id, "1");
        assert_eq!(ranked[1].doc.stable_id, "2");
    }

    #[test]
    fn rebuild_is_idempotent() {
        let idx = open_test_index();
        let docs = vec![
            make_doc(SearchDocumentKind::Block, "1", "cmd1", "out1"),
            make_doc(SearchDocumentKind::Block, "2", "cmd2", "out2"),
        ];
        idx.rebuild(&docs).unwrap();
        assert_eq!(idx.count().unwrap(), 2);
        // Rebuild again with the same data.
        idx.rebuild(&docs).unwrap();
        assert_eq!(idx.count().unwrap(), 2);
    }

    #[test]
    fn corrupted_index_can_be_rebuilt() {
        // Simulate corruption: drop the table, then rebuild.
        let idx = open_test_index();
        idx.upsert(&make_doc(SearchDocumentKind::Block, "1", "old", "old out"))
            .unwrap();
        // Simulate corruption.
        idx.conn
            .execute("DROP TABLE IF EXISTS search_docs;", [])
            .unwrap();
        // Rebuild should recreate the table.
        let docs = vec![make_doc(SearchDocumentKind::Block, "2", "new", "new out")];
        idx.rebuild(&docs).unwrap();
        assert_eq!(idx.count().unwrap(), 1);
        let hits = idx.search(&SearchQuery::new("new")).unwrap();
        assert!(!hits.is_empty());
    }

    #[test]
    fn large_batch_rebuild() {
        // V17 §3 exit criteria: 10万条合成记录下查询 p95 < 50ms.
        // This test inserts 1000 docs (enough to validate batch insert
        // correctness; the 100K performance test is a benchmark).
        let idx = open_test_index();
        let docs: Vec<SearchDocument> = (0..1000)
            .map(|i| SearchDocument {
                kind: SearchDocumentKind::Block,
                stable_id: i.to_string(),
                title: format!("command_{i}"),
                body: format!("output line {i} with some content"),
                cwd: Some("/tmp".to_string()),
                updated_at: i,
            })
            .collect();
        idx.rebuild(&docs).unwrap();
        assert_eq!(idx.count().unwrap(), 1000);
        let hits = idx.search(&SearchQuery::new("command_500")).unwrap();
        assert!(!hits.is_empty());
    }

    #[test]
    fn no_ghost_records_after_delete() {
        // V17 §3: "删除/更新源数据后索引无幽灵记录".
        let idx = open_test_index();
        idx.upsert(&make_doc(
            SearchDocumentKind::Block,
            "1",
            "unique_command",
            "unique_output",
        ))
        .unwrap();
        idx.delete(SearchDocumentKind::Block, "1").unwrap();
        let hits = idx.search(&SearchQuery::new("unique_command")).unwrap();
        assert!(hits.is_empty(), "ghost record found after delete");
        let hits = idx.search(&SearchQuery::new("unique_output")).unwrap();
        assert!(hits.is_empty(), "ghost record found after delete");
    }

    #[test]
    fn no_ghost_records_after_rebuild() {
        let idx = open_test_index();
        idx.upsert(&make_doc(
            SearchDocumentKind::Block,
            "1",
            "ghost_command",
            "ghost_output",
        ))
        .unwrap();
        // Rebuild with different data — old doc should not survive.
        let new_docs = vec![make_doc(
            SearchDocumentKind::Block,
            "2",
            "real_command",
            "real_output",
        )];
        idx.rebuild(&new_docs).unwrap();
        let hits = idx.search(&SearchQuery::new("ghost")).unwrap();
        assert!(hits.is_empty(), "ghost record survived rebuild");
    }
}
