//! v1.7.1: Background palette search worker.
//!
//! Runs FTS5 search queries on a dedicated thread so the winit event loop
//! is never blocked. Uses a generation token to cancel stale queries when
//! the user types faster than the query completes.

use crossbeam_channel::{bounded, Receiver, Sender};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use weft_core::search::{SearchDocumentKind, SearchHit, SearchIndex, SearchQuery};

pub(crate) type SearchWaker = Arc<dyn Fn() + Send + Sync + 'static>;

struct SearchRequest {
    generation: u64,
    query: String,
    kinds: Vec<SearchDocumentKind>,
    cwd: Option<String>,
    limit: usize,
}

pub(crate) struct PaletteSearchResult {
    pub generation: u64,
    pub hits: Vec<SearchHit>,
}

pub(crate) struct PaletteSearchWorker {
    query_tx: Sender<SearchRequest>,
    // Clone of the receiver used to drain stale pending queries in `submit`
    // (latest-wins replacement, mirroring FindWorker's pattern).
    query_rx: Receiver<SearchRequest>,
    result_rx: Receiver<PaletteSearchResult>,
    generation: Arc<AtomicU64>,
}

impl PaletteSearchWorker {
    /// Spawn the worker. Returns None if the DB can't be opened.
    pub(crate) fn spawn(db_path: PathBuf, waker: SearchWaker) -> Option<Self> {
        let conn = rusqlite::Connection::open(&db_path).ok()?;
        let index = SearchIndex::open(conn);
        let (query_tx, query_rx) = bounded::<SearchRequest>(1);
        let (result_tx, result_rx) = bounded::<PaletteSearchResult>(8);
        let generation = Arc::new(AtomicU64::new(0));
        let result_waker = waker.clone();
        let submit_query_rx = query_rx.clone();
        thread::Builder::new()
            .name("weft-palette-search".to_string())
            .spawn(move || {
                Self::run(index, query_rx, result_tx, result_waker);
            })
            .ok()?;
        Some(PaletteSearchWorker {
            query_tx,
            query_rx: submit_query_rx,
            result_rx,
            generation,
        })
    }

    fn run(
        index: SearchIndex,
        query_rx: Receiver<SearchRequest>,
        result_tx: Sender<PaletteSearchResult>,
        waker: SearchWaker,
    ) {
        while let Ok(req) = query_rx.recv() {
            let kinds_slice: &[SearchDocumentKind] = &req.kinds;
            let q = SearchQuery {
                query: &req.query,
                kinds: kinds_slice,
                limit: req.limit,
                cwd: req.cwd.as_deref(),
            };
            let hits = index.search(&q).unwrap_or_default();
            let _ = result_tx.send(PaletteSearchResult {
                generation: req.generation,
                hits,
            });
            waker();
        }
    }

    /// Submit a new search query. Returns the new generation number.
    /// Any pending query is replaced (latest-wins).
    pub(crate) fn submit(
        &self,
        query: &str,
        kinds: &[SearchDocumentKind],
        cwd: Option<&str>,
        limit: usize,
    ) -> u64 {
        let gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let pending = SearchRequest {
            generation: gen,
            query: query.to_string(),
            kinds: kinds.to_vec(),
            cwd: cwd.map(|s| s.to_string()),
            limit,
        };
        match self.query_tx.try_send(pending) {
            Ok(()) => {}
            Err(crossbeam_channel::TrySendError::Full(pending)) => {
                // Replace the single queued (not yet running) query. The UI
                // is the only submitter, so one drain+retry is sufficient.
                let _ = self.query_rx.try_recv();
                let _ = self.query_tx.try_send(pending);
            }
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => {}
        }
        gen
    }

    /// Current generation (for staleness checks).
    pub(crate) fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Try to receive a search result. Non-blocking.
    pub(crate) fn try_recv_result(&self) -> Option<PaletteSearchResult> {
        self.result_rx.try_recv().ok()
    }

    /// Invalidate any in-flight query by bumping the generation.
    pub(crate) fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }
}

/// v1.7.1: Open the main-thread search index (sidecar to blocks.db) for
/// upsert/delete (index maintenance). If the index is empty, rebuild it
/// from BlockStore so palette search has content on first launch.
///
/// Returns `None` when the DB can't be opened (palette search disabled).
pub(crate) fn open_search_index(
    db_path: Option<PathBuf>,
    block_store: Option<&weft_core::persistence::BlockStore>,
) -> Option<SearchIndex> {
    let path = db_path?;
    let conn = match rusqlite::Connection::open(&path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to open search index; palette search disabled");
            return None;
        }
    };
    let index = SearchIndex::open(conn);
    // Rebuild index from BlockStore if empty.
    if index.count().unwrap_or(0) == 0 {
        if let Some(store) = block_store {
            if let Ok(blocks) = store.recent(10000) {
                let docs: Vec<_> = blocks
                    .iter()
                    .map(|b| {
                        let started_ms = b
                            .started_at
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as i64)
                            .unwrap_or(0);
                        weft_core::search::SearchDocument::from_block(
                            b.id.0,
                            &b.command,
                            b.output.as_ref(),
                            b.cwd.as_deref(),
                            started_ms,
                        )
                    })
                    .collect();
                if let Err(e) = index.rebuild(&docs) {
                    tracing::warn!(error = %e, "failed to rebuild search index");
                } else {
                    tracing::info!(count = docs.len(), "search index rebuilt");
                }
            }
        }
    }
    Some(index)
}

/// v1.7.1: Spawn the palette search worker, wiring the waker to send
/// `AppEvent::Wake` so the event loop redraws when results are ready.
pub(crate) fn spawn_worker(
    db_path: PathBuf,
    proxy: winit::event_loop::EventLoopProxy<crate::AppEvent>,
) -> Option<PaletteSearchWorker> {
    let waker: SearchWaker = Arc::new(move || {
        let _ = proxy.send_event(crate::AppEvent::Wake);
    });
    PaletteSearchWorker::spawn(db_path, waker)
}
