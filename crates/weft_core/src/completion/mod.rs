//! v1.7.2: Completion provider trait and types.
//!
//! V17_IMPLEMENTATION_PLAN §4: defines a `CompletionProvider` trait so
//! completion sources (history, PATH, filesystem, workflow, workspace) can
//! be added independently. Each provider returns candidates with source
//! metadata; the aggregator sorts and dedupes.
//!
//! The existing `crate::complete` module remains the home of the pure
//! matching logic (`history_matches`, `command_matches`, `path_matches`).
//! This module wraps that logic in the provider trait and adds pure
//! sort/dedupe functions.

mod providers;
mod sort;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub use providers::{
    FilesystemProvider, HistoryProvider, PathExecutableProvider, WorkflowProvider,
    WorkspaceCommandProvider,
};
pub use sort::{dedupe_candidates, sort_candidates};

/// v1.7.2: Where a completion candidate came from. Drives priority sorting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompletionSource {
    History,
    PathExecutable,
    Filesystem,
    Workflow,
    WorkspaceCommand,
}

impl CompletionSource {
    /// Lower = higher priority. History first, then Filesystem, then
    /// PathExecutable, then Workflow, then WorkspaceCommand.
    pub fn priority(self) -> u8 {
        match self {
            Self::History => 0,
            Self::Filesystem => 1,
            Self::PathExecutable => 2,
            Self::Workflow => 3,
            Self::WorkspaceCommand => 4,
        }
    }
}

/// v1.7.2: A completion candidate — the unified type across all providers.
///
/// Replaces `crate::complete::Match` for new provider-based code. Each
/// candidate records its source (for priority sorting), the text to insert,
/// the display label, and whether it's a directory (for 📁/📄 rendering).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionCandidate {
    /// Display text shown in the dropdown.
    pub label: String,
    /// Text to replace the prefix with.
    pub insert: String,
    /// Where this candidate came from.
    pub source: CompletionSource,
    /// True for filesystem directories (Path source only).
    pub is_dir: bool,
}

/// v1.7.2: A completion request. Replaces `CompleteCtx` for provider code.
pub struct CompletionRequest<'a> {
    pub prefix: &'a str,
    pub cwd: &'a str,
    pub history: &'a [String],
    pub path_bins: &'a [String],
    pub position: crate::complete::CompletePosition,
}

/// v1.7.2: Cancellation token. Providers should check `is_cancelled()`
/// periodically and return early if true.
#[derive(Clone)]
pub struct CancelToken {
    cancelled: Arc<AtomicBool>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

/// v1.7.2: The provider trait. Each provider produces candidates from one source.
pub trait CompletionProvider: Send + Sync {
    fn complete(
        &self,
        request: &CompletionRequest,
        cancel: &CancelToken,
    ) -> Vec<CompletionCandidate>;
}

/// v1.7.2: Run multiple providers and merge results with sort + dedupe.
pub fn complete_with_providers(
    providers: &[&dyn CompletionProvider],
    request: &CompletionRequest,
    cancel: &CancelToken,
) -> Vec<CompletionCandidate> {
    complete_with_providers_budgeted(providers, request, cancel, std::time::Duration::MAX)
}

/// Run providers until the shared wall-clock budget expires. Slow providers
/// still run off the UI thread; the budget prevents subsequent providers from
/// extending a stale request indefinitely.
pub fn complete_with_providers_budgeted(
    providers: &[&dyn CompletionProvider],
    request: &CompletionRequest,
    cancel: &CancelToken,
    budget: std::time::Duration,
) -> Vec<CompletionCandidate> {
    let started = std::time::Instant::now();
    let mut all = Vec::new();
    for provider in providers {
        if cancel.is_cancelled() || started.elapsed() >= budget {
            break;
        }
        all.extend(provider.complete(request, cancel));
    }
    sort_candidates(&mut all);
    dedupe_candidates(all)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::complete::CompletePosition;

    #[test]
    fn aggregator_sorts_dedupes_and_honors_cancellation() {
        let history = vec!["cargo test".to_string()];
        let path_bins = vec!["cargo".to_string(), "cat".to_string()];
        let request = CompletionRequest {
            prefix: "ca",
            cwd: "/definitely/not/a/real/path",
            history: &history,
            path_bins: &path_bins,
            position: CompletePosition::Command,
        };
        let history_provider = HistoryProvider;
        let path_provider = PathExecutableProvider;
        let providers: [&dyn CompletionProvider; 2] = [&path_provider, &history_provider];
        let cancel = CancelToken::new();

        let candidates = complete_with_providers(&providers, &request, &cancel);
        assert_eq!(candidates[0].source, CompletionSource::History);
        assert_eq!(candidates[0].label, "cargo test");
        assert_eq!(candidates[1].label, "cargo");

        cancel.cancel();
        assert!(complete_with_providers(&providers, &request, &cancel).is_empty());
    }

    #[test]
    fn zero_budget_does_not_run_any_provider() {
        let request = CompletionRequest {
            prefix: "c",
            cwd: "/tmp",
            history: &[],
            path_bins: &["cargo".to_string()],
            position: CompletePosition::Command,
        };
        let provider = PathExecutableProvider;
        let providers: [&dyn CompletionProvider; 1] = [&provider];
        assert!(complete_with_providers_budgeted(
            &providers,
            &request,
            &CancelToken::new(),
            std::time::Duration::ZERO,
        )
        .is_empty());
    }
}
