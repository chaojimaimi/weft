//! v1.8 AI integration — local Ollama only.
//!
//! ## Status (v1.8.0 cut, 2026-07-31)
//!
//! v1.3 carried scaffolding for OpenAI / Anthropic / custom backends and
//! was never wired into the app shell. v1.8 collapses to a single local
//! Ollama backend per `docs/V18_IMPLEMENTATION_PLAN.md` §1: no API keys,
//! no public endpoints, no cloud providers. The `AiBackend` trait is now
//! `async` and accepts a [`CancelFlag`] so callers can abort an in-flight
//! stream when the user closes the palette or switches blocks.
//!
//! ## Module layout
//!
//! - [`prompt`] — pure-logic prompt builders (fully unit-tested).
//! - [`client`] — `AiBackend` trait + the single Ollama implementation.
//! - [`redact`] — secret redaction wrapper around `weft_core::secrets`.
//! - [`AiState`] — main-thread state. Background `tokio` tasks communicate
//!   with it via a `crossbeam-channel`, matching the existing `FindWorker`
//!   pattern.

pub mod client;
pub mod metrics;
pub mod prompt;
pub mod redact;

#[cfg(test)]
mod mock_tests;
#[cfg(test)]
mod real_tests;

use std::sync::Arc;
use std::time::Instant;

use crossbeam_channel::{unbounded, Receiver, Sender};
use tracing::{debug, warn};

use weft_core::config::AiConfig;

use client::{
    build_backend, build_http_client, effective_base_url, fetch_ollama_models, is_loopback_url,
    AiBackend, AiError, CancelFlag, TagModel,
};

pub use metrics::{AiMetrics, AiMetricsSnapshot};
pub use prompt::{
    build_command_gen_messages, build_diagnose_messages, classify_command_risk,
    clean_command_output, clean_diagnose_output, AiRiskLevel, CommandGenPrompt, DiagnosePrompt,
};

// Re-export the byte/history caps so the Settings UI / palette can hint at
// the limits without depending on `prompt` directly. Currently unused at
// runtime (v1.8.0 doesn't surface them in the UI yet); gated by `allow`
// until v1.8.1 consumes them from the palette.
#[allow(unused_imports)]
pub use prompt::{MAX_HISTORY_ENTRIES, MAX_OUTPUT_BYTES};

/// Result of a completed AI request. Returned by [`AiState::poll`].
#[derive(Debug, Clone)]
pub enum AiResultEvent {
    /// A command-generation request finished. The string is the cleaned
    /// (markdown-fence-stripped, prompt-stripped) shell command. The caller
    /// should populate the palette with a `PaletteEntry::AiSuggestion`.
    CommandGen { id: u64, command: String },
    /// A diagnosis request finished. The string is the model's plain-text
    /// explanation. The caller should attach it to the corresponding block.
    Diagnose { id: u64, explanation: String },
    /// v1.8.3: A `/api/tags` model-list refresh finished. The result is the
    /// list of installed models (on success) or an error message (on failure).
    /// The caller updates the Settings AI panel's model dropdown + status.
    ModelsRefreshed {
        id: u64,
        result: Result<Vec<TagModel>, String>,
    },
    /// A request failed. The error string is suitable for direct display
    /// in the UI (palette status line / block diagnostic panel).
    Error { id: u64, message: String },
    /// A request was cancelled (either the user issued a new one or closed
    /// the UI). The caller should clear any "thinking…" indicator for `id`.
    Cancelled { id: u64 },
}

/// Main-thread AI state. Holds the config snapshot, the optional backend,
/// and the receiving end of the result channel.
///
/// Clone-cheap: the backend is behind an `Arc`, the channel receivers are
/// `crossbeam` (also cheap). The main `App` struct can hold this by value
/// without indirection.
pub struct AiState {
    /// v1.11.0: the `config` snapshot field + `config()`/`in_flight()`/`new()`
    /// accessors were removed — dead code (only `new_with_waker` is used at
    /// runtime; the Settings UI re-resolves values from the draft config
    /// directly). See AUDIT_v1.10.39 / PLAN_v111.
    /// `None` when AI is not configured (`provider = None` or construction
    /// failed). Callers should hide the "✨ Ask AI" UI in that case.
    backend: Option<Arc<dyn AiBackend>>,
    /// Result channel from background tasks → main thread.
    rx: Receiver<AiResultEvent>,
    /// Sender cloned by each spawned task. Kept here so callers can detect
    /// "is the channel still alive?" if needed.
    tx: Sender<AiResultEvent>,
    /// Monotonic request id. Incremented on every spawn so the caller can
    /// correlate `AiResultEvent` ids with the originating UI entry.
    next_id: u64,
    /// True when at least one request is in flight. Used by the redraw
    /// loop to keep the palette "thinking…" indicator alive.
    in_flight: usize,
    /// Cancel flags for in-flight requests, keyed by request id. When a
    /// new request supersedes an old one (or the user closes the UI), the
    /// old flag is flipped to `true` and the entry removed.
    cancellations: std::collections::HashMap<u64, CancelFlag>,
    /// v1.8.3: Start timestamps for in-flight requests, used to compute
    /// latency on completion. Removed in `poll()` when the result arrives.
    start_times: std::collections::HashMap<u64, Instant>,
    /// v1.8.3: Observability counters (requests / errors / cancellations /
    /// latencies). No prompt or response text is recorded.
    metrics: AiMetrics,
    /// v1.8.7: Optional wake callback invoked after each result is sent.
    /// When set, background tasks call this to wake the main event loop so
    /// `poll_ai_results` runs promptly — without it, results sit in the
    /// channel until the next unrelated event (key/mouse/timer) triggers
    /// the loop, causing the palette to show an empty result.
    waker: Option<Arc<dyn Fn() + Send + Sync + 'static>>,
}

impl AiState {
    /// v1.8.7: Build a new state with a wake callback that background tasks
    /// invoke after sending a result. The waker should trigger an
    /// `AppEvent::Wake` so `poll_ai_results` runs promptly.
    ///
    /// v1.11.0: this is now the only constructor — `new()` (plain, no waker)
    /// was removed as dead code (tests use `new_with_waker(config, None)`).
    pub fn new_with_waker(
        config: AiConfig,
        waker: Option<Arc<dyn Fn() + Send + Sync + 'static>>,
    ) -> Self {
        let backend = match build_backend(&config) {
            Ok(b) => b.map(Arc::<dyn AiBackend>::from),
            Err(e) => {
                warn!(error = %e, "AI backend construction failed; AI features disabled");
                None
            }
        };
        let (tx, rx) = unbounded();
        Self {
            backend,
            rx,
            tx,
            next_id: 1,
            in_flight: 0,
            cancellations: std::collections::HashMap::new(),
            start_times: std::collections::HashMap::new(),
            metrics: AiMetrics::new(),
            waker,
        }
    }

    /// True when AI features can be surfaced in the UI. Equivalent to
    /// `AiConfig::is_configured()` plus a successful backend build.
    pub fn is_configured(&self) -> bool {
        self.backend.is_some()
    }

    /// Cancel all in-flight requests. Called when the user closes the
    /// palette or switches blocks. The background tasks will return
    /// `Err(AiError::Cancelled)` at the next stream chunk boundary.
    pub fn cancel_all(&mut self) {
        for (_id, flag) in self.cancellations.drain() {
            flag.cancel();
        }
        // v1.8.3: start_times is drained by `poll()` when the Cancelled
        // event arrives; no need to clear it here. Keeping the entry lets
        // `poll()` compute latency for the cancellation log line.
    }

    /// Cancel a specific request by id. Returns `true` if the request
    /// was found and cancelled.
    pub fn cancel(&mut self, id: u64) -> bool {
        if let Some(flag) = self.cancellations.remove(&id) {
            flag.cancel();
            true
        } else {
            false
        }
    }

    /// Spawn a command-generation request. Returns the assigned id so the
    /// caller can store it on the palette entry and match it against the
    /// eventual `AiResultEvent::CommandGen { id, .. }`.
    ///
    /// Returns `None` when AI is not configured — the caller should hide
    /// the entry or show a "configure AI in Settings" hint.
    ///
    /// Any previous in-flight request is cancelled first (new query
    /// supersedes old).
    pub fn spawn_command_gen(&mut self, request: CommandGenPrompt) -> Option<u64> {
        let backend = self.backend.clone()?;
        // Cancel any in-flight command-gen requests (new supersedes old).
        self.cancel_all();
        let id = self.next_id;
        self.next_id += 1;
        self.in_flight += 1;
        self.metrics.record_request();
        self.start_times.insert(id, Instant::now());
        let tx = self.tx.clone();
        let cancel = CancelFlag::new();
        self.cancellations.insert(id, cancel.clone());
        let messages = build_command_gen_messages(&request);
        // v1.8.7: clone the waker so the task can wake the main loop.
        let waker = self.waker.clone();

        tokio::spawn(async move {
            let result = backend.complete(messages, cancel).await;
            let event = match result {
                Ok(raw) => {
                    let command = clean_command_output(&raw);
                    if command.is_empty() {
                        AiResultEvent::Error {
                            id,
                            message: "AI returned an empty command".into(),
                        }
                    } else {
                        AiResultEvent::CommandGen { id, command }
                    }
                }
                Err(AiError::Cancelled) => AiResultEvent::Cancelled { id },
                Err(e) => AiResultEvent::Error {
                    id,
                    message: e.to_string(),
                },
            };
            let _ = tx.send(event);
            // v1.8.7: wake the main event loop so poll_ai_results runs now.
            if let Some(waker) = &waker {
                waker();
            }
        });

        Some(id)
    }

    /// Spawn a failed-block diagnosis request. Mirrors
    /// [`spawn_command_gen`]. The caller passes the block's command/output/
    /// exit_code/cwd via [`DiagnosePrompt`].
    pub fn spawn_diagnose(&mut self, request: DiagnosePrompt) -> Option<u64> {
        let backend = self.backend.clone()?;
        let id = self.next_id;
        self.next_id += 1;
        self.in_flight += 1;
        self.metrics.record_request();
        self.start_times.insert(id, Instant::now());
        let tx = self.tx.clone();
        let cancel = CancelFlag::new();
        self.cancellations.insert(id, cancel.clone());
        let messages = build_diagnose_messages(&request);
        // v1.8.7: clone the waker so the task can wake the main loop.
        let waker = self.waker.clone();

        tokio::spawn(async move {
            let result = backend.complete(messages, cancel).await;
            let event = match result {
                Ok(raw) => {
                    // v1.8.8: Clean up the diagnosis output — small models
                    // like gemma4:e4b can produce excessive whitespace or
                    // scattered fragments. This normalises presentation
                    // without altering semantic content.
                    let explanation = clean_diagnose_output(&raw);
                    if explanation.is_empty() {
                        AiResultEvent::Error {
                            id,
                            message: "AI returned an empty diagnosis".into(),
                        }
                    } else {
                        AiResultEvent::Diagnose { id, explanation }
                    }
                }
                Err(AiError::Cancelled) => AiResultEvent::Cancelled { id },
                Err(e) => AiResultEvent::Error {
                    id,
                    message: e.to_string(),
                },
            };
            let _ = tx.send(event);
            // v1.8.7: wake the main event loop so poll_ai_results runs now.
            if let Some(waker) = &waker {
                waker();
            }
        });

        Some(id)
    }

    /// v1.8.3: Spawn a `/api/tags` model-list refresh. Builds a temporary
    /// HTTP client from `config` (not `self.backend`) so the Settings panel
    /// can test a draft config before saving it — and crucially, without
    /// requiring a `model` field to be set (the user may not have picked
    /// one yet). Returns the assigned id so the caller can match the
    /// eventual `AiResultEvent::ModelsRefreshed { id }`.
    ///
    /// Returns `None` when `config.provider` is not `"ollama"` or the
    /// `base_url` fails loopback validation. The caller should surface the
    /// reason directly — no async result is coming.
    pub fn spawn_list_models(&mut self, config: &AiConfig) -> Option<u64> {
        // Only attempt discovery when the draft is configured for Ollama.
        if !config.is_configured() {
            return None;
        }
        let base_url = effective_base_url(config);
        if !is_loopback_url(&base_url) {
            return None;
        }
        // Use the same no-proxy/no-redirect transport policy as completions.
        // For this non-streaming endpoint it bounds header/body read
        // inactivity while preserving the same transport policy.
        let timeout = std::time::Duration::from_secs(config.effective_timeout_secs());
        let http = match build_http_client(std::time::Duration::from_secs(10), timeout) {
            Ok(c) => c,
            Err(_) => return None,
        };

        let id = self.next_id;
        self.next_id += 1;
        self.in_flight += 1;
        self.metrics.record_request();
        self.start_times.insert(id, Instant::now());
        let tx = self.tx.clone();
        // v1.8.7: clone the waker so the task can wake the main loop.
        let waker = self.waker.clone();

        tokio::spawn(async move {
            let result = fetch_ollama_models(&http, &base_url).await;
            let event = AiResultEvent::ModelsRefreshed {
                id,
                result: result.map_err(|e| e.to_string()),
            };
            let _ = tx.send(event);
            // v1.8.7: wake the main event loop so poll_ai_results runs now.
            if let Some(waker) = &waker {
                waker();
            }
        });

        Some(id)
    }

    /// Drain any completed results from the channel. Called by the redraw
    /// loop each frame (matching the FindWorker pattern). Decrements
    /// `in_flight` for each result so the "thinking…" indicator clears.
    /// v1.8.3: also records observability metrics (success/error/cancelled
    /// and latency) and emits a debug log line per completion. No prompt or
    /// response text is logged — only counts, durations, and error category.
    pub fn poll(&mut self) -> Vec<AiResultEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.rx.try_recv() {
            self.in_flight = self.in_flight.saturating_sub(1);
            // Clean up the cancellation map entry if present.
            let id = match &event {
                AiResultEvent::CommandGen { id, .. }
                | AiResultEvent::Diagnose { id, .. }
                | AiResultEvent::ModelsRefreshed { id, .. }
                | AiResultEvent::Error { id, .. }
                | AiResultEvent::Cancelled { id } => *id,
            };
            self.cancellations.remove(&id);
            // v1.8.3: record metrics + emit a body-free debug log.
            let started = self.start_times.remove(&id);
            let latency = started.map(|t| t.elapsed()).unwrap_or_default();
            match &event {
                AiResultEvent::CommandGen { .. }
                | AiResultEvent::Diagnose { .. }
                | AiResultEvent::ModelsRefreshed { result: Ok(_), .. } => {
                    self.metrics.record_success(latency);
                }
                AiResultEvent::ModelsRefreshed { result: Err(_), .. }
                | AiResultEvent::Error { .. } => {
                    self.metrics.record_error(latency);
                }
                AiResultEvent::Cancelled { .. } => {
                    self.metrics.record_cancellation();
                }
            }
            let kind = match &event {
                AiResultEvent::CommandGen { .. } => "command_gen",
                AiResultEvent::Diagnose { .. } => "diagnose",
                AiResultEvent::ModelsRefreshed { .. } => "models_refresh",
                AiResultEvent::Error { .. } => "error",
                AiResultEvent::Cancelled { .. } => "cancelled",
            };
            let ok = !matches!(
                event,
                AiResultEvent::Error { .. }
                    | AiResultEvent::Cancelled { .. }
                    | AiResultEvent::ModelsRefreshed { result: Err(_), .. }
            );
            debug!(
                kind = kind,
                ok = ok,
                latency_ms = latency.as_millis() as u64,
                "ai_request_completed"
            );
            events.push(event);
        }
        events
    }

    /// v1.8.3: Read-only metrics snapshot for the Settings AI panel's
    /// observability row and for debug logging. Contains no prompt/response
    /// text — only aggregate counts and a p95 latency.
    pub fn metrics_snapshot(&self) -> AiMetricsSnapshot {
        self.metrics.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use weft_core::config::AiConfig;

    /// A trivial mock backend that returns a fixed string. Used to verify
    /// `AiState`'s spawn/poll wiring without touching the network.
    struct MockBackend {
        response: String,
    }

    impl AiBackend for MockBackend {
        fn complete(
            &self,
            _messages: Vec<prompt::ChatMessage>,
            _cancel: CancelFlag,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = client::AiResult<String>> + Send>>
        {
            let resp = self.response.clone();
            Box::pin(async move { Ok(resp) })
        }
    }

    fn state_with_mock(response: &str) -> AiState {
        let mut state = AiState::new_with_waker(AiConfig::default(), None);
        state.backend = Some(Arc::new(MockBackend {
            response: response.to_string(),
        }));
        state
    }

    #[test]
    fn new_state_with_default_config_has_no_backend() {
        let state = AiState::new_with_waker(AiConfig::default(), None);
        assert!(!state.is_configured());
        assert_eq!(state.in_flight, 0);
    }

    #[test]
    fn spawn_returns_none_when_not_configured() {
        // No tokio runtime needed here: spawn_command_gen exits early when
        // backend is None, before reaching tokio::spawn.
        let mut state = AiState::new_with_waker(AiConfig::default(), None);
        let id = state.spawn_command_gen(CommandGenPrompt {
            user_query: "list files".into(),
            cwd: "/tmp".into(),
            recent_history: vec![],
        });
        assert!(id.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn spawn_returns_id_when_configured() {
        let mut state = state_with_mock("ls -la");
        let id = state.spawn_command_gen(CommandGenPrompt {
            user_query: "list files".into(),
            cwd: "/tmp".into(),
            recent_history: vec![],
        });
        assert!(id.is_some());
        assert_eq!(state.in_flight, 1);
        // Drain the pending result so the channel doesn't deadlock.
        for _ in 0..50 {
            if !state.poll().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn spawn_increments_ids_monotonically() {
        let mut state = state_with_mock("ls");
        let id1 = state.spawn_command_gen(CommandGenPrompt {
            user_query: "a".into(),
            cwd: "".into(),
            recent_history: vec![],
        });
        let id2 = state.spawn_command_gen(CommandGenPrompt {
            user_query: "b".into(),
            cwd: "".into(),
            recent_history: vec![],
        });
        assert!(id2 > id1, "ids must be monotonic: {id1:?} {id2:?}");
        // Drain.
        for _ in 0..50 {
            if !state.poll().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn poll_returns_command_gen_result() {
        let mut state = state_with_mock("ls -la");
        let id = state
            .spawn_command_gen(CommandGenPrompt {
                user_query: "list".into(),
                cwd: "".into(),
                recent_history: vec![],
            })
            .unwrap();

        // Wait for the background task to complete (best-effort).
        for _ in 0..100 {
            let events = state.poll();
            if !events.is_empty() {
                assert_eq!(events.len(), 1);
                match &events[0] {
                    AiResultEvent::CommandGen { id: eid, command } => {
                        assert_eq!(*eid, id);
                        assert_eq!(command, "ls -la");
                    }
                    other => panic!("expected CommandGen, got {other:?}"),
                }
                assert_eq!(state.in_flight, 0);
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("background task did not produce a result within 1s");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn poll_returns_diagnose_result() {
        let mut state = state_with_mock("file not found");
        let id = state
            .spawn_diagnose(DiagnosePrompt {
                command: "ls /nope".into(),
                output: "No such file".into(),
                exit_code: 1,
                cwd: "".into(),
            })
            .unwrap();

        for _ in 0..100 {
            let events = state.poll();
            if !events.is_empty() {
                match &events[0] {
                    AiResultEvent::Diagnose {
                        id: eid,
                        explanation,
                    } => {
                        assert_eq!(*eid, id);
                        assert_eq!(explanation, "file not found");
                    }
                    other => panic!("expected Diagnose, got {other:?}"),
                }
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("background task did not produce a result within 1s");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn poll_cleans_markdown_fences_from_command_output() {
        let mut state = state_with_mock("```sh\nfind . -name '*.ts'\n```");
        state
            .spawn_command_gen(CommandGenPrompt {
                user_query: "find ts".into(),
                cwd: "".into(),
                recent_history: vec![],
            })
            .unwrap();

        for _ in 0..100 {
            let events = state.poll();
            if let Some(ev) = events.first() {
                match ev {
                    AiResultEvent::CommandGen { command, .. } => {
                        assert_eq!(command, "find . -name '*.ts'");
                    }
                    other => panic!("expected CommandGen, got {other:?}"),
                }
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("timeout");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn poll_reports_empty_command_as_error() {
        let mut state = state_with_mock("   ");
        state
            .spawn_command_gen(CommandGenPrompt {
                user_query: "x".into(),
                cwd: "".into(),
                recent_history: vec![],
            })
            .unwrap();

        for _ in 0..100 {
            let events = state.poll();
            if let Some(ev) = events.first() {
                match ev {
                    AiResultEvent::Error { message, .. } => {
                        assert!(message.contains("empty"));
                    }
                    other => panic!("expected Error, got {other:?}"),
                }
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("timeout");
    }

    #[test]
    fn cancel_all_cancels_in_flight_requests() {
        let mut state = state_with_mock("ls");
        let cancel = CancelFlag::new();
        state.cancellations.insert(1, cancel.clone());
        state.cancel_all();
        assert!(cancel.is_cancelled());
        assert!(state.cancellations.is_empty());
    }

    #[test]
    fn cancel_by_id_removes_entry() {
        let mut state = state_with_mock("ls");
        let cancel = CancelFlag::new();
        state.cancellations.insert(5, cancel.clone());
        assert!(state.cancel(5));
        assert!(cancel.is_cancelled());
        assert!(!state.cancel(5)); // already removed
    }

    // v1.8.3: Settings "Test Connection" → spawn_list_models.
    #[test]
    fn spawn_list_models_returns_none_when_not_configured() {
        let mut state = AiState::new_with_waker(AiConfig::default(), None);
        assert!(state.spawn_list_models(&AiConfig::default()).is_none());
    }

    #[test]
    fn spawn_list_models_returns_none_for_non_loopback_url() {
        // Public HTTPS endpoint — must be rejected before any network call.
        let cfg = AiConfig {
            provider: Some("ollama".into()),
            base_url: Some("https://example.com".into()),
            ..Default::default()
        };
        let mut state = AiState::new_with_waker(cfg.clone(), None);
        assert!(state.spawn_list_models(&cfg).is_none());
    }

    #[test]
    fn spawn_list_models_returns_none_for_http_on_public_host() {
        let cfg = AiConfig {
            provider: Some("ollama".into()),
            base_url: Some("http://192.168.1.5:11434".into()),
            ..Default::default()
        };
        let mut state = AiState::new_with_waker(cfg.clone(), None);
        assert!(state.spawn_list_models(&cfg).is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn spawn_list_models_assigns_id_for_loopback_url() {
        // The request will fail (no Ollama running in CI), but the id
        // assignment happens before the network call — we only verify
        // that Some(id) is returned and in_flight is incremented.
        let cfg = AiConfig {
            provider: Some("ollama".into()),
            base_url: Some("http://127.0.0.1:11434".into()),
            timeout_secs: Some(1), // fail fast
            ..Default::default()
        };
        let mut state = AiState::new_with_waker(cfg.clone(), None);
        let id = state.spawn_list_models(&cfg);
        assert!(id.is_some(), "loopback URL should be accepted");
        assert_eq!(state.in_flight, 1);
        // Drain the eventual error event so the tokio task completes.
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let _ = state.poll();
        assert_eq!(state.in_flight, 0);
    }

    #[test]
    fn metrics_snapshot_starts_zero() {
        let state = AiState::new_with_waker(AiConfig::default(), None);
        let snap = state.metrics_snapshot();
        assert_eq!(snap.requests_total, 0);
        assert_eq!(snap.successes_total, 0);
        assert_eq!(snap.errors_total, 0);
        assert_eq!(snap.p95_latency_ms, 0);
    }
}
