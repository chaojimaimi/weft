//! v1.7.2: Concrete completion providers.
//!
//! Each provider wraps the existing pure matching logic from
//! `crate::complete` in the `CompletionProvider` trait.

use super::{CancelToken, CompletionCandidate, CompletionRequest, CompletionSource};
use crate::complete::{self, CompletePosition};

/// History provider — whole-line prefix matches from command history.
pub struct HistoryProvider;

impl super::CompletionProvider for HistoryProvider {
    fn complete(
        &self,
        request: &CompletionRequest,
        _cancel: &CancelToken,
    ) -> Vec<CompletionCandidate> {
        if request.position != CompletePosition::Command {
            return Vec::new();
        }
        complete::history_matches(request.prefix, request.history)
            .into_iter()
            .map(|m| CompletionCandidate {
                label: m.label,
                insert: m.insert,
                source: CompletionSource::History,
                is_dir: false,
                match_quality: 1,
            })
            .collect()
    }
}

/// PATH executable provider — prefix matches from cached $PATH binaries.
pub struct PathExecutableProvider;

impl super::CompletionProvider for PathExecutableProvider {
    fn complete(
        &self,
        request: &CompletionRequest,
        _cancel: &CancelToken,
    ) -> Vec<CompletionCandidate> {
        if request.position != CompletePosition::Command {
            return Vec::new();
        }
        complete::command_matches(request.prefix, request.path_bins)
            .into_iter()
            .map(|m| CompletionCandidate {
                label: m.label,
                insert: m.insert,
                source: CompletionSource::PathExecutable,
                is_dir: false,
                match_quality: m.match_quality,
            })
            .collect()
    }
}

/// Filesystem provider — directory/file path completions.
pub struct FilesystemProvider;

impl super::CompletionProvider for FilesystemProvider {
    fn complete(
        &self,
        request: &CompletionRequest,
        cancel: &CancelToken,
    ) -> Vec<CompletionCandidate> {
        let matches = complete::path_matches(request.prefix, request.cwd);
        // Check cancellation after filesystem I/O.
        if cancel.is_cancelled() {
            return Vec::new();
        }
        matches
            .into_iter()
            .map(|m| CompletionCandidate {
                label: m.label,
                insert: m.insert,
                source: CompletionSource::Filesystem,
                is_dir: m.is_dir,
                match_quality: 1,
            })
            .collect()
    }
}

/// Workflow provider — searches workflow names and commands.
/// Requires a list of (name, command) pairs from the WorkflowStore.
pub struct WorkflowProvider {
    /// (name, command_template) pairs from the WorkflowStore.
    pub workflows: Vec<(String, String)>,
}

impl WorkflowProvider {
    pub fn new(workflows: Vec<(String, String)>) -> Self {
        Self { workflows }
    }
}

impl super::CompletionProvider for WorkflowProvider {
    fn complete(
        &self,
        request: &CompletionRequest,
        _cancel: &CancelToken,
    ) -> Vec<CompletionCandidate> {
        if request.position != CompletePosition::Command {
            return Vec::new();
        }
        self.workflows
            .iter()
            .filter(|(name, _)| {
                name.starts_with(request.prefix) && name.len() > request.prefix.len()
            })
            .map(|(name, cmd)| CompletionCandidate {
                label: name.clone(),
                insert: cmd.clone(),
                source: CompletionSource::Workflow,
                is_dir: false,
                match_quality: 1,
            })
            .collect()
    }
}

/// Workspace command provider — searches saved workspace commands.
/// Stub for v1.7.2; will be wired to workspace data in a future batch.
pub struct WorkspaceCommandProvider {
    pub commands: Vec<String>,
}

impl WorkspaceCommandProvider {
    pub fn new(commands: Vec<String>) -> Self {
        Self { commands }
    }
}

impl super::CompletionProvider for WorkspaceCommandProvider {
    fn complete(
        &self,
        request: &CompletionRequest,
        _cancel: &CancelToken,
    ) -> Vec<CompletionCandidate> {
        if request.position != CompletePosition::Command {
            return Vec::new();
        }
        self.commands
            .iter()
            .filter(|cmd| cmd.starts_with(request.prefix) && cmd.len() > request.prefix.len())
            .map(|cmd| CompletionCandidate {
                label: cmd.clone(),
                insert: cmd.clone(),
                source: CompletionSource::WorkspaceCommand,
                is_dir: false,
                match_quality: 1,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::CompletionProvider;

    fn req<'a>(
        prefix: &'a str,
        cwd: &'a str,
        history: &'a [String],
        path_bins: &'a [String],
    ) -> CompletionRequest<'a> {
        CompletionRequest {
            prefix,
            cwd,
            history,
            path_bins,
            position: CompletePosition::Command,
        }
    }

    #[test]
    fn history_provider_returns_prefix_matches() {
        let history = vec![
            "ls -la".to_string(),
            "ls /tmp".to_string(),
            "cd".to_string(),
        ];
        let r = req("ls", "/tmp", &history, &[]);
        let cancel = CancelToken::new();
        let candidates = HistoryProvider.complete(&r, &cancel);
        assert_eq!(candidates.len(), 2);
        assert!(candidates
            .iter()
            .all(|c| c.source == CompletionSource::History));
    }

    #[test]
    fn history_provider_excludes_argument_position() {
        let history = vec!["ls -la".to_string()];
        let r = CompletionRequest {
            prefix: "ls",
            cwd: "/tmp",
            history: &history,
            path_bins: &[],
            position: CompletePosition::Argument,
        };
        let cancel = CancelToken::new();
        assert!(HistoryProvider.complete(&r, &cancel).is_empty());
    }

    #[test]
    fn path_executable_provider_returns_matches() {
        let bins = vec!["cargo".to_string(), "cat".to_string(), "ls".to_string()];
        let r = req("ca", "/tmp", &[], &bins);
        let cancel = CancelToken::new();
        let candidates = PathExecutableProvider.complete(&r, &cancel);
        assert_eq!(candidates.len(), 2);
        assert!(candidates
            .iter()
            .all(|c| c.source == CompletionSource::PathExecutable));
    }

    #[test]
    fn filesystem_provider_returns_empty_for_invalid_cwd() {
        let r = req("x", "/nonexistent/path/weft_test", &[], &[]);
        let cancel = CancelToken::new();
        let candidates = FilesystemProvider.complete(&r, &cancel);
        assert!(candidates.is_empty());
    }

    #[test]
    fn workflow_provider_matches_by_name() {
        let workflows = vec![
            ("deploy".to_string(), "kubectl deploy".to_string()),
            ("debug".to_string(), "lldb".to_string()),
        ];
        let provider = WorkflowProvider::new(workflows);
        // Prefix "dep" matches only "deploy" (not "debug"), giving exactly 1 candidate.
        let r = req("dep", "/tmp", &[], &[]);
        let cancel = CancelToken::new();
        let candidates = provider.complete(&r, &cancel);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].label, "deploy");
        assert_eq!(candidates[0].insert, "kubectl deploy");
        assert_eq!(candidates[0].source, CompletionSource::Workflow);
    }

    #[test]
    fn workspace_command_provider_returns_matches() {
        let commands = vec!["npm test".to_string(), "npm run".to_string()];
        let provider = WorkspaceCommandProvider::new(commands);
        let r = req("npm", "/tmp", &[], &[]);
        let cancel = CancelToken::new();
        let candidates = provider.complete(&r, &cancel);
        assert_eq!(candidates.len(), 2);
    }

    #[test]
    fn cancel_token_returns_early() {
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(cancel.is_cancelled());
        // FilesystemProvider checks cancellation after I/O.
        let r = req("x", "/tmp", &[], &[]);
        let candidates = FilesystemProvider.complete(&r, &cancel);
        assert!(candidates.is_empty());
    }
}
