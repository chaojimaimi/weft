//! Background find worker (v0.9 U-P1).
//!
//! Runs `find_in_snapshot` on a dedicated thread so a multi-megabyte
//! scrollback search never blocks the render thread. The main thread
//! submits a `FindQuery` (query + flags + a `FindSnapshot`); the worker
//! streams `FindResult` messages back via a channel. Submitting a new query
//! while the old one is running cancels the old one (the worker peeks the
//! query channel between scan chunks and bails out if a newer query has
//! arrived).
//!
//! Design notes:
//! - The snapshot is created on the main thread (`Grid::find_snapshot()` ≈ 2-3ms
//!   for 10K rows) and sent to the worker. This avoids any shared-state
//!   synchronization on the Grid itself (which is owned by the parse thread).
//! - The worker scans in 1000-row chunks and yields a `Partial` result every
//!   5000 rows so the UI can paint incremental matches without waiting for
//!   the full scan.
//! - Cancellation is cooperative: the worker checks `query_rx.is_empty()`
//!   between chunks. If a newer query is pending, it sends `Cancelled` and
//!   starts the new query.

use crossbeam_channel::{bounded, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use weft_core::find::{find_in_snapshot, FindMatch, FindSnapshot};

/// A query + snapshot to be scanned by the worker.
struct FindQuery {
    query: String,
    case_sensitive: bool,
    is_regex: bool,
    snapshot: Arc<FindSnapshot>,
}

/// Incremental or final result streamed back to the main thread.
pub enum FindResult {
    /// Partial results (incremental — emitted every ~5000 rows).
    /// The UI can paint these immediately and refine as more arrive.
    Partial { matches: Vec<FindMatch> },
    /// Final result (full scan complete or MAX_MATCHES hit).
    Complete {
        matches: Vec<FindMatch>,
        truncated: bool,
    },
    /// Worker hit a regex compile error (only when `is_regex` is true).
    RegexInvalid(String),
    /// Cancelled — a newer query arrived before this one finished. The main
    /// thread can ignore this (the newer query's results will arrive soon).
    Cancelled,
}

/// Handle to the background find worker. Cheap to clone (just channel
/// handles); the worker thread itself is shared.
pub struct FindWorker {
    query_tx: Sender<FindQuery>,
    result_rx: Receiver<FindResult>,
}

impl FindWorker {
    /// Spawn a new worker thread. The thread runs for the lifetime of the
    /// returned handle — when `FindWorker` is dropped, the query channel
    /// closes and the worker exits.
    pub fn spawn() -> Self {
        let (query_tx, query_rx) = bounded::<FindQuery>(1);
        let (result_tx, result_rx) = bounded::<FindResult>(32);
        thread::Builder::new()
            .name("weft-find-worker".to_string())
            .spawn(move || {
                Self::run(query_rx, result_tx);
            })
            .expect("spawn find worker");
        FindWorker {
            query_tx,
            result_rx,
        }
    }

    /// Worker loop: read queries, scan snapshots, stream results.
    fn run(query_rx: Receiver<FindQuery>, result_tx: Sender<FindResult>) {
        while let Ok(q) = query_rx.recv() {
            // Drain any newer queries that arrived while we were busy —
            // only the latest matters.
            let mut current = q;
            while let Ok(newer) = query_rx.try_recv() {
                current = newer;
            }

            let total = current.snapshot.rows.len();
            let case = current.case_sensitive;
            let is_regex = current.is_regex;
            let query = current.query.clone();
            let snapshot = current.snapshot.clone();

            // For regex mode, validate the query first. An invalid regex
            // short-circuits with a `RegexInvalid` message so the UI can
            // surface "invalid regex" instead of silently returning nothing.
            if is_regex {
                if let Err(e) = regex::Regex::new(&query) {
                    let _ = result_tx.send(FindResult::RegexInvalid(e.to_string()));
                    continue;
                }
            }

            // Scan in chunks of CHUNK_SIZE rows, peeking the query channel
            // between chunks so a newer query cancels this scan promptly.
            const CHUNK_SIZE: usize = 1000;
            const YIELD_EVERY: usize = 5000;
            let mut matches: Vec<FindMatch> = Vec::with_capacity(64);
            let mut last_yield = 0usize;
            let mut cancelled = false;

            for chunk_start in (0..total).step_by(CHUNK_SIZE) {
                // Cancellation check: if a newer query has arrived, bail.
                if !query_rx.is_empty() {
                    let _ = result_tx.send(FindResult::Cancelled);
                    cancelled = true;
                    break;
                }
                let chunk_end = (chunk_start + CHUNK_SIZE).min(total);
                // Build a sub-snapshot view for this chunk and run find on it.
                let chunk_matches =
                    scan_chunk(&snapshot, chunk_start, chunk_end, &query, case, is_regex);
                matches.extend(chunk_matches);
                if matches.len() >= weft_core::find::MAX_MATCHES {
                    break;
                }
                if chunk_end - last_yield >= YIELD_EVERY && chunk_end < total {
                    let _ = result_tx.send(FindResult::Partial {
                        matches: matches.clone(),
                    });
                    // Yield to let the main thread redraw with partial
                    // results. A short sleep also reduces CPU pressure
                    // during very large scans.
                    thread::sleep(Duration::from_millis(8));
                    last_yield = chunk_end;
                }
            }

            if cancelled {
                continue;
            }

            let truncated = matches.len() >= weft_core::find::MAX_MATCHES;
            let _ = result_tx.send(FindResult::Complete { matches, truncated });
        }
    }

    /// Submit a new query (cancels any in-flight scan). Non-blocking: if the
    /// worker is busy, the query queues up (queue depth 1, so the latest
    /// query always wins — see `run`'s drain logic).
    pub fn submit(
        &self,
        query: String,
        case_sensitive: bool,
        is_regex: bool,
        snapshot: Arc<FindSnapshot>,
    ) {
        // try_send: don't block if the previous query hasn't been picked up
        // yet — replace it with the newer one (the worker drains stale
        // queries in `run`).
        let _ = self.query_tx.try_send(FindQuery {
            query,
            case_sensitive,
            is_regex,
            snapshot,
        });
    }

    /// Non-blocking result poll. The main thread calls this in the redraw
    /// loop to drain pending results.
    pub fn try_recv_result(&self) -> Option<FindResult> {
        self.result_rx.try_recv().ok()
    }
}

/// Scan rows `[start, end)` of the snapshot and return matches (with row
/// indices rebased to the original unified indexing).
fn scan_chunk(
    snapshot: &FindSnapshot,
    start: usize,
    end: usize,
    query: &str,
    case_sensitive: bool,
    is_regex: bool,
) -> Vec<FindMatch> {
    // Build a sub-snapshot view that shares the row data via Arc cloning
    // (Vec<(char,u8)> clone is the cost here, but chunks are small).
    let mut sub_rows = Vec::with_capacity(end - start);
    for i in start..end {
        sub_rows.push(snapshot.rows[i].clone());
    }
    let sub = FindSnapshot {
        rows: sub_rows,
        num_cols: snapshot.num_cols,
    };
    let mut matches = match find_in_snapshot(&sub, query, case_sensitive, is_regex) {
        Ok(m) => m,
        Err(_) => return Vec::new(),
    };
    // Rebase row indices: find_in_snapshot returns 0-based indices within
    // the sub-snapshot; we need to add `start` to get back to the original
    // unified row index.
    for m in &mut matches {
        m.row += start;
    }
    matches
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use weft_core::find::FindSnapshot;

    fn snap(rows: &[&str]) -> Arc<FindSnapshot> {
        let rows: Vec<Vec<(char, u8)>> = rows
            .iter()
            .map(|r| r.chars().map(|c| (c, 1u8)).collect())
            .collect();
        Arc::new(FindSnapshot { rows, num_cols: 80 })
    }

    #[test]
    fn scan_chunk_finds_simple_query() {
        let s = snap(&["hello world", "foo bar"]);
        let m = scan_chunk(&s, 0, 2, "world", false, false);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].row, 0);
        assert_eq!(m[0].col, 6);
        assert_eq!(m[0].len, 5);
    }

    #[test]
    fn scan_chunk_rebases_row_indices() {
        let s = snap(&["a", "b", "hello", "c"]);
        // Scan chunk starting at row 2.
        let m = scan_chunk(&s, 2, 4, "hello", false, false);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].row, 2);
    }

    #[test]
    fn scan_chunk_case_insensitive_default() {
        let s = snap(&["Hello World"]);
        let m = scan_chunk(&s, 0, 1, "hello", false, false);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].row, 0);
    }

    #[test]
    fn scan_chunk_case_sensitive() {
        let s = snap(&["Hello World"]);
        // Case-sensitive: "hello" should NOT match "Hello".
        let m = scan_chunk(&s, 0, 1, "hello", true, false);
        assert_eq!(m.len(), 0);
        // "Hello" should match.
        let m = scan_chunk(&s, 0, 1, "Hello", true, false);
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn scan_chunk_empty_query_returns_empty() {
        let s = snap(&["hello", "world"]);
        let m = scan_chunk(&s, 0, 2, "", false, false);
        assert!(m.is_empty());
    }

    #[test]
    fn scan_chunk_regex_mode() {
        let s = snap(&["abc123def", "xyz"]);
        let m = scan_chunk(&s, 0, 2, r"\d+", false, true);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].row, 0);
        assert_eq!(m[0].col, 3);
        assert_eq!(m[0].len, 3);
    }

    #[test]
    fn find_worker_spawn_and_complete() {
        let worker = FindWorker::spawn();
        let s = snap(&["hello world", "foo"]);
        worker.submit("world".to_string(), false, false, s);
        // Poll for up to ~2s waiting for Complete.
        let mut got_complete = false;
        for _ in 0..200 {
            if let Some(FindResult::Complete { matches, .. }) = worker.try_recv_result() {
                assert_eq!(matches.len(), 1);
                assert_eq!(matches[0].row, 0);
                got_complete = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(got_complete, "worker should send Complete");
    }

    #[test]
    fn find_worker_regex_invalid() {
        let worker = FindWorker::spawn();
        let s = snap(&["hello"]);
        worker.submit("(unclosed".to_string(), false, true, s);
        let mut got_err = false;
        for _ in 0..200 {
            if let Some(FindResult::RegexInvalid(_)) = worker.try_recv_result() {
                got_err = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(got_err, "worker should send RegexInvalid");
    }

    // ── T3: additional scan_chunk coverage ─────────────────────────────

    #[test]
    fn scan_chunk_regex_invalid_pattern_returns_empty() {
        // scan_chunk swallows the RegexError from find_in_snapshot and returns
        // an empty Vec (the worker surfaces RegexInvalid separately via the
        // pre-scan in run()).
        let s = snap(&["hello world", "abc"]);
        let m = scan_chunk(&s, 0, 2, "(unclosed", false, true);
        assert!(
            m.is_empty(),
            "invalid regex should produce empty matches, got {m:?}"
        );
    }

    #[test]
    fn scan_chunk_finds_multiple_matches() {
        // A single chunk containing multiple matches on different rows must
        // return all of them, with row indices rebased to the original.
        let s = snap(&["foo bar foo", "baz", "foo qux foo"]);
        // Scan rows 0..3 (the whole snapshot).
        let m = scan_chunk(&s, 0, 3, "foo", false, false);
        assert_eq!(m.len(), 4, "expected 4 foo matches");
        // Row 0 has two matches.
        assert_eq!(m[0].row, 0);
        assert_eq!(m[0].col, 0);
        assert_eq!(m[0].len, 3);
        assert_eq!(m[1].row, 0);
        assert_eq!(m[1].col, 8);
        // Row 2 has two matches (row 1 has none).
        assert_eq!(m[2].row, 2);
        assert_eq!(m[2].col, 0);
        assert_eq!(m[3].row, 2);
        assert_eq!(m[3].col, 8);
    }
}
