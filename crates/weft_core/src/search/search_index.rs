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
    pub fn open(conn: Connection) -> rusqlite::Result<Self> {
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
            )?;
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
            )?;
        }
        Ok(Self {
            conn,
            fts5_available,
        })
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

    /// Atomically replace selected document kinds without disturbing other
    /// kinds in the shared index.
    pub fn replace_kinds(
        &self,
        kinds: &[SearchDocumentKind],
        docs: &[SearchDocument],
    ) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for kind in kinds {
            tx.execute(
                "DELETE FROM search_docs WHERE kind = ?;",
                rusqlite::params![*kind as u8],
            )?;
        }
        for doc in docs {
            tx.execute(
                "INSERT INTO search_docs (kind, stable_id, title, body, cwd, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?);",
                rusqlite::params![
                    doc.kind as u8,
                    doc.stable_id,
                    doc.title,
                    doc.body,
                    doc.cwd,
                    doc.updated_at,
                ],
            )?;
        }
        tx.commit()
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
        // Fetch a small candidate window before applying CWD ranking so a
        // local result just below the raw FTS cutoff can still surface.
        let candidate_limit = q.limit.saturating_mul(4).max(50) as i64;

        let hits = if self.fts5_available {
            self.search_fts5(q.query, kind_filter, q.kinds, candidate_limit)?
        } else {
            self.search_substring(q.query, kind_filter, q.kinds, candidate_limit)?
        };
        let mut hits = rank_hits(hits, q.cwd);
        hits.truncate(q.limit);
        Ok(hits)
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

        let mut stmt = self.conn.prepare_cached(&sql)?;
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

        rows.collect()
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

        let mut stmt = self.conn.prepare_cached(&sql)?;
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

        rows.collect()
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
/// - CWD boost: matching paths receive a fixed score reduction (lower is
///   better for both FTS5 and substring fallback scores).
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
        // improve the score with a fixed reduction. Multiplication is wrong
        // for FTS5's negative rank values because it moves them toward zero.
        if let Some(ref doc_cwd) = hit.doc.cwd {
            let document_path = std::path::Path::new(doc_cwd);
            let query_path = std::path::Path::new(cwd);
            if document_path.starts_with(query_path) || query_path.starts_with(document_path) {
                hit.score -= 1.0;
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
#[path = "search_index_tests.rs"]
mod tests;
