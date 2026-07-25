//! AI integration scaffolding — **deferred to v1.6** (kept here as the
//! implementation base for that release).
//!
//! ## Status (v1.3.0 cut, 2026-07-25)
//!
//! ROADMAP decision DC-8 redirected v1.3 from AI integration to **Split Pane**.
//! v1.4–v1.5 follow with WARP R3 deep items and config-system enhancements.
//! AI integration is now scheduled for **v1.6+**. This module is therefore
//! **not wired into the app shell** in v1.3: `AiState::new` / `spawn_command_gen`
//! / `poll` are scaffolding only, no UI surface invokes them, and several
//! helpers intentionally remain unused (gated by narrow `#[allow(dead_code)]`)
//! until v1.6 consumes them. The code is retained so v1.6 can build on a
//! reviewed baseline rather than starting from scratch.
//!
//! ## Module layout (frozen as the v1.6 base)
//!
//! - [`prompt`] — pure-logic prompt builders (fully unit-tested).
//! - [`client`] — `AiBackend` trait + Ollama / OpenAI / Anthropic / custom
//!   implementations.
//! - [`AiState`] — the main-thread state that holds pending requests and
//!   completed results. Background `tokio` tasks communicate with `AiState`
//!   via a `crossbeam-channel`, matching the existing `FindWorker` pattern.
//!
//! When v1.6 lands, wire `AiState` into the palette / block view via the
//! `spawn_command_gen` + `poll` entry points; the internals should need no
//! change.

pub mod client;
pub mod prompt;

use std::sync::Arc;

use crossbeam_channel::{unbounded, Receiver, Sender};
use tracing::warn;

use weft_core::config::AiConfig;

use client::{build_backend, AiBackend, AiError, AiResult};

pub use prompt::{
    build_command_gen_messages, build_diagnose_messages, clean_command_output, CommandGenPrompt,
    DiagnosePrompt,
};

// Re-export the byte/history caps so the Settings UI / palette can hint at
// the limits without depending on `prompt` directly.
pub use prompt::{MAX_HISTORY_ENTRIES, MAX_OUTPUT_BYTES};

/// A pending or completed AI request. The `id` lets the caller correlate
/// results with the UI entry that issued them (so a stale palette entry
/// doesn't pick up a newer result).
#[derive(Debug)]
pub struct AiRequest {
    pub id: u64,
    pub kind: AiRequestKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiRequestKind {
    /// Natural-language → shell command. Issued from the palette.
    CommandGen,
    /// Failed-block diagnosis. Issued from the block action button.
    Diagnose,
}

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
    /// A request failed. The error string is suitable for direct display
    /// in the UI (palette status line / block diagnostic panel).
    Error { id: u64, message: String },
}

/// Main-thread AI state. Holds the config snapshot, the optional backend,
/// and the receiving end of the result channel.
///
/// Clone-cheap: the backend is behind an `Arc`, the channel receivers are
/// `crossbeam` (also cheap). The main `App` struct can hold this by value
/// without indirection.
pub struct AiState {
    /// Snapshot of the `[ai]` config at construction time. The Settings UI
    /// rebuilds `AiState` when the user changes provider / key / model.
    config: AiConfig,
    /// `None` when AI is not configured (`provider = None` or insufficient
    /// credentials). Callers should hide the "✨ Ask AI" UI in that case.
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
}

impl AiState {
    /// Build a new state from the current config. Returns a state with
    /// `backend = None` when AI is not configured (no error surfaced —
    /// the caller checks `is_configured()` to decide whether to show AI UI).
    pub fn new(config: AiConfig) -> Self {
        let backend = match build_backend(&config) {
            Ok(b) => b.map(|b| Arc::<dyn AiBackend>::from(b)),
            Err(e) => {
                warn!(error = %e, "AI backend construction failed; AI features disabled");
                None
            }
        };
        let (tx, rx) = unbounded();
        Self {
            config,
            backend,
            rx,
            tx,
            next_id: 1,
            in_flight: 0,
        }
    }

    /// True when AI features can be surfaced in the UI. Equivalent to
    /// `AiConfig::is_configured()` plus a successful backend build.
    pub fn is_configured(&self) -> bool {
        self.backend.is_some()
    }

    /// Reference to the active config snapshot (for the Settings UI to
    /// display the current provider/model without re-parsing the file).
    pub fn config(&self) -> &AiConfig {
        &self.config
    }

    /// Number of requests currently awaiting a response.
    pub fn in_flight(&self) -> usize {
        self.in_flight
    }

    /// Spawn a command-generation request. Returns the assigned id so the
    /// caller can store it on the palette entry and match it against the
    /// eventual `AiResultEvent::CommandGen { id, .. }`.
    ///
    /// Returns `None` when AI is not configured — the caller should hide
    /// the entry or show a "configure AI in Settings" hint.
    pub fn spawn_command_gen(&mut self, request: CommandGenPrompt) -> Option<u64> {
        let backend = self.backend.clone()?;
        let id = self.next_id;
        self.next_id += 1;
        self.in_flight += 1;
        let tx = self.tx.clone();
        let messages = build_command_gen_messages(&request);

        // Spawn on the current tokio runtime. The runtime is owned by the
        // App (see AppRuntime) and is multi-threaded, so a slow HTTP call
        // doesn't block the winit event loop.
        tokio::spawn(async move {
            let result = run_completion(&backend, &messages).await;
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
                Err(e) => AiResultEvent::Error {
                    id,
                    message: e.to_string(),
                },
            };
            let _ = tx.send(event);
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
        let tx = self.tx.clone();
        let messages = build_diagnose_messages(&request);

        tokio::spawn(async move {
            let result = run_completion(&backend, &messages).await;
            let event = match result {
                Ok(explanation) => AiResultEvent::Diagnose { id, explanation },
                Err(e) => AiResultEvent::Error {
                    id,
                    message: e.to_string(),
                },
            };
            let _ = tx.send(event);
        });

        Some(id)
    }

    /// Drain any completed results from the channel. Called by the redraw
    /// loop each frame (matching the FindWorker pattern). Decrements
    /// `in_flight` for each result so the "thinking…" indicator clears.
    pub fn poll(&mut self) -> Vec<AiResultEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.rx.try_recv() {
            self.in_flight = self.in_flight.saturating_sub(1);
            events.push(event);
        }
        events
    }
}

/// Shared runner that calls the backend. Kept as a free function so it can
/// be reused by both spawn paths and tested with a mock backend.
async fn run_completion(
    backend: &Arc<dyn AiBackend>,
    messages: &[prompt::ChatMessage],
) -> AiResult {
    // The trait method is sync (it blocks on the tokio runtime internally
    // via `Handle::try_current()` + `block_on`). To avoid blocking the
    // async runtime's worker thread, we offload to `tokio::task::spawn_blocking`.
    let backend = backend.clone();
    let messages: Vec<_> = messages.to_vec();
    tokio::task::spawn_blocking(move || backend.complete(&messages))
        .await
        .map_err(|e| AiError::Parse(format!("background task panicked: {e}")))?
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
        fn provider_id(&self) -> &'static str {
            "mock"
        }
        fn complete(&self, _messages: &[prompt::ChatMessage]) -> AiResult {
            Ok(self.response.clone())
        }
    }

    fn state_with_mock(response: &str) -> AiState {
        let mut state = AiState::new(AiConfig::default());
        state.backend = Some(Arc::new(MockBackend {
            response: response.to_string(),
        }));
        state
    }

    #[test]
    fn new_state_with_default_config_has_no_backend() {
        let state = AiState::new(AiConfig::default());
        assert!(!state.is_configured());
        assert_eq!(state.in_flight(), 0);
    }

    #[test]
    fn spawn_returns_none_when_not_configured() {
        // No tokio runtime needed here: spawn_command_gen exits early when
        // backend is None, before reaching tokio::spawn.
        let mut state = AiState::new(AiConfig::default());
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
        assert_eq!(state.in_flight(), 1);
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
            if state.poll().len() >= 2 {
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
                assert_eq!(state.in_flight(), 0);
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
}
