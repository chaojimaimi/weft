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

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use weft_core::find::{find_in_snapshot, FindMatch, FindSnapshot};

/// A query + snapshot to be scanned by the worker.
struct FindQuery {
    generation: u64,
    query: String,
    case_sensitive: bool,
    is_regex: bool,
    snapshot: Arc<FindSnapshot>,
}

struct DebounceRequest {
    generation: u64,
    deadline: Instant,
}

pub(crate) type WakeCallback = Arc<dyn Fn() + Send + Sync + 'static>;

/// Incremental or final result streamed back to the main thread.
pub enum FindResult {
    /// Partial results (incremental — emitted every ~5000 rows).
    /// The UI can paint these immediately and refine as more arrive.
    Partial {
        generation: u64,
        matches: Vec<FindMatch>,
    },
    /// Final result (full scan complete or MAX_MATCHES hit).
    Complete {
        generation: u64,
        matches: Vec<FindMatch>,
        truncated: bool,
    },
    /// Worker hit a regex compile error (only when `is_regex` is true).
    RegexInvalid { generation: u64, message: String },
    /// Cancelled — a newer query arrived before this one finished. The main
    /// thread can ignore this (the newer query's results will arrive soon).
    Cancelled { generation: u64 },
}

/// Handle to the background find worker. Cheap to clone (just channel
/// handles); the worker thread itself is shared.
pub struct FindWorker {
    query_tx: Sender<FindQuery>,
    query_rx: Receiver<FindQuery>,
    debounce_tx: Sender<DebounceRequest>,
    debounce_rx: Receiver<DebounceRequest>,
    result_rx: Receiver<FindResult>,
    next_generation: Arc<AtomicU64>,
}

impl FindWorker {
    /// Spawn a new worker thread. The thread runs for the lifetime of the
    /// returned handle — when `FindWorker` is dropped, the query channel
    /// closes and the worker exits.
    #[cfg(test)]
    pub fn spawn() -> Self {
        Self::spawn_with_waker(Arc::new(|| {}))
    }

    /// Spawn a worker that wakes the application whenever a debounce deadline
    /// expires or an async result becomes available.
    pub(crate) fn spawn_with_waker(waker: WakeCallback) -> Self {
        let (query_tx, query_rx) = bounded::<FindQuery>(1);
        let (result_tx, result_rx) = bounded::<FindResult>(32);
        let (debounce_tx, debounce_rx) = bounded::<DebounceRequest>(1);
        let submit_query_rx = query_rx.clone();
        let replace_debounce_rx = debounce_rx.clone();
        let next_generation = Arc::new(AtomicU64::new(0));
        let result_waker = waker.clone();
        thread::Builder::new()
            .name("weft-find-worker".to_string())
            .spawn(move || {
                Self::run(query_rx, result_tx, result_waker);
            })
            .expect("spawn find worker");
        let debounce_generation = next_generation.clone();
        thread::Builder::new()
            .name("weft-find-debounce".to_string())
            .spawn(move || {
                Self::run_debounce(debounce_rx, debounce_generation, waker);
            })
            .expect("spawn find debounce worker");
        FindWorker {
            query_tx,
            query_rx: submit_query_rx,
            debounce_tx,
            debounce_rx: replace_debounce_rx,
            result_rx,
            next_generation,
        }
    }

    /// Worker loop: read queries, scan snapshots, stream results.
    fn run(query_rx: Receiver<FindQuery>, result_tx: Sender<FindResult>, waker: WakeCallback) {
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
            let generation = current.generation;

            // For regex mode, validate the query first. An invalid regex
            // short-circuits with a `RegexInvalid` message so the UI can
            // surface "invalid regex" instead of silently returning nothing.
            if is_regex {
                if let Err(e) = regex::Regex::new(&query) {
                    Self::publish_result(
                        &result_tx,
                        &waker,
                        FindResult::RegexInvalid {
                            generation,
                            message: e.to_string(),
                        },
                    );
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
                    Self::publish_result(&result_tx, &waker, FindResult::Cancelled { generation });
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
                    Self::publish_result(
                        &result_tx,
                        &waker,
                        FindResult::Partial {
                            generation,
                            matches: matches.clone(),
                        },
                    );
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
            Self::publish_result(
                &result_tx,
                &waker,
                FindResult::Complete {
                    generation,
                    matches,
                    truncated,
                },
            );
        }
    }

    fn publish_result(result_tx: &Sender<FindResult>, waker: &WakeCallback, result: FindResult) {
        if result_tx.send(result).is_ok() {
            waker();
        }
    }

    fn run_debounce(
        debounce_rx: Receiver<DebounceRequest>,
        next_generation: Arc<AtomicU64>,
        waker: WakeCallback,
    ) {
        while let Ok(mut request) = debounce_rx.recv() {
            loop {
                let wait = request.deadline.saturating_duration_since(Instant::now());
                if wait.is_zero() {
                    if next_generation.load(Ordering::Acquire) == request.generation {
                        waker();
                    }
                    break;
                }
                match debounce_rx.recv_timeout(wait) {
                    Ok(newer) => {
                        request = newer;
                        while let Ok(latest) = debounce_rx.try_recv() {
                            request = latest;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if next_generation.load(Ordering::Acquire) == request.generation {
                            waker();
                        }
                        break;
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
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
    ) -> u64 {
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed) + 1;
        let pending = FindQuery {
            generation,
            query,
            case_sensitive,
            is_regex,
            snapshot,
        };
        match self.query_tx.try_send(pending) {
            Ok(()) => {}
            Err(crossbeam_channel::TrySendError::Full(pending)) => {
                // Replace the single queued (not yet running) query. The UI
                // is the only submitter, so one drain+retry is sufficient;
                // the worker may concurrently take the stale query, which
                // simply makes the retry succeed without a drain.
                let _ = self.query_rx.try_recv();
                let _ = self.query_tx.try_send(pending);
            }
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => {}
        }
        generation
    }

    /// Invalidate every result issued before this lifecycle boundary without
    /// submitting a new scan (Find close/reset and tab ownership changes).
    pub fn invalidate(&self) -> u64 {
        self.next_generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Arrange one event-loop wake after the latest query's debounce period.
    /// The bounded channel replaces pending deadlines, so rapid typing uses a
    /// single persistent timer thread rather than one sleeping thread per key.
    pub fn schedule_debounce(&self, generation: u64, delay: Duration) {
        let pending = DebounceRequest {
            generation,
            deadline: Instant::now() + delay,
        };
        match self.debounce_tx.try_send(pending) {
            Ok(()) => {}
            Err(crossbeam_channel::TrySendError::Full(pending)) => {
                let _ = self.debounce_rx.try_recv();
                let _ = self.debounce_tx.try_send(pending);
            }
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => {}
        }
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
        let expected = worker.submit("world".to_string(), false, false, s);
        // Poll for up to ~2s waiting for Complete.
        let mut got_complete = false;
        for _ in 0..200 {
            if let Some(FindResult::Complete {
                generation,
                matches,
                ..
            }) = worker.try_recv_result()
            {
                assert_eq!(generation, expected);
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
        let expected = worker.submit("(unclosed".to_string(), false, true, s);
        let mut got_err = false;
        for _ in 0..200 {
            if let Some(FindResult::RegexInvalid { generation, .. }) = worker.try_recv_result() {
                if generation == expected {
                    got_err = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(got_err, "worker should send RegexInvalid");
    }

    #[test]
    fn rapid_queries_eventually_complete_with_latest_generation() {
        let worker = FindWorker::spawn();
        let rows = vec!["alpha beta"; 20_000];
        let large = snap(&rows);
        worker.submit("alpha".into(), false, false, large.clone());
        worker.submit("beta".into(), false, false, large.clone());
        let expected = worker.submit("needle".into(), false, false, snap(&["needle"]));

        let mut latest = None;
        for _ in 0..400 {
            if let Some(FindResult::Complete {
                generation,
                matches,
                ..
            }) = worker.try_recv_result()
            {
                if generation == expected {
                    latest = Some(matches);
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let matches = latest.expect("latest queued query should complete");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].row, 0);
    }

    #[test]
    fn complete_result_wakes_event_loop_without_external_timer() {
        let (wake_tx, wake_rx) = std::sync::mpsc::channel();
        let worker = FindWorker::spawn_with_waker(Arc::new(move || {
            let _ = wake_tx.send(());
        }));
        let expected = worker.submit("needle".into(), false, false, snap(&["needle"]));

        wake_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker completion should actively wake the event loop");
        let result = worker
            .try_recv_result()
            .expect("result must be available before its wake is published");
        assert!(matches!(
            result,
            FindResult::Complete { generation, .. } if generation == expected
        ));
    }

    #[test]
    fn debounce_scheduler_coalesces_rapid_deadlines() {
        let (wake_tx, wake_rx) = std::sync::mpsc::channel();
        let worker = FindWorker::spawn_with_waker(Arc::new(move || {
            let _ = wake_tx.send(());
        }));
        for _ in 0..20 {
            let generation = worker.invalidate();
            worker.schedule_debounce(generation, Duration::from_millis(30));
        }

        wake_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("latest debounce deadline should wake once");
        assert!(
            wake_rx.recv_timeout(Duration::from_millis(80)).is_err(),
            "superseded debounce deadlines must not create a wake burst"
        );
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
