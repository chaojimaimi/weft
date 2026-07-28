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

impl CompletionCandidate {
    pub fn new(label: String, insert: String, source: CompletionSource) -> Self {
        Self {
            label,
            insert,
            source,
            is_dir: false,
        }
    }
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
    let mut all = Vec::new();
    for provider in providers {
        if cancel.is_cancelled() {
            break;
        }
        all.extend(provider.complete(request, cancel));
    }
    sort_candidates(&mut all);
    dedupe_candidates(all)
}
