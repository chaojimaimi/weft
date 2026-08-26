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
        // v1.11.0 (P1-5, AUDIT_v1.10.39): position gate — at a command
        // position (cursor inside the first word) with a bare-word prefix
        // (e.g. "pnp"), file candidates must NOT surface: the user is
        // typing a command, and history/PATH-executable sources already
        // cover that slot. A path-like prefix ("./x", "/usr/...", "~/d")
        // or an argument position keeps the previous behavior.
        if request.position == CompletePosition::Command && !request.prefix.contains('/') {
            return Vec::new();
        }
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
        // v1.11.0: prefix contains '/' so it passes the command-position
        // gate (P1-5) — this test keeps exercising the invalid-cwd path,
        // not the gate.
        let r = req(
            "/weft_complete_nonexistent/x",
            "/nonexistent/path/weft_test",
            &[],
            &[],
        );
        let cancel = CancelToken::new();
        let candidates = FilesystemProvider.complete(&r, &cancel);
        assert!(candidates.is_empty());
    }

    // ── v1.11.0 P1-5: command-position filesystem gate ────────────────

    /// A scratch cwd dir unique to this test process (see complete.rs for
    /// the same pattern).
    fn scratch_dir(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut d = std::env::temp_dir();
        d.push(format!(
            "weft_providers_{}_{name}_{}",
            std::process::id(),
            id
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn filesystem_provider_hides_files_at_command_position_with_bare_prefix() {
        // Cursor in the first word, prefix has no '/' → the user is typing
        // a command; file candidates must NOT surface (AUDIT_v1.10.39 P1-5).
        // The cwd is a real scratch dir with a matching file, so an empty
        // result can only come from the gate.
        let d = scratch_dir("fs_gate_bare");
        std::fs::write(d.join("pnux"), "").unwrap();
        let r = req("pn", d.to_str().unwrap(), &[], &[]);
        let cancel = CancelToken::new();
        assert!(FilesystemProvider.complete(&r, &cancel).is_empty());
    }

    #[test]
    fn filesystem_provider_lists_paths_at_command_position_with_slash_prefix() {
        // Command position but the prefix is path-like (contains '/') →
        // filesystem completion stays enabled (e.g. `./re` or `/usr/lo`).
        let d = scratch_dir("fs_gate_slash");
        std::fs::write(d.join("report.md"), "").unwrap();
        let abs_prefix = format!("{}/re", d.to_str().unwrap());
        let r = req(&abs_prefix, d.to_str().unwrap(), &[], &[]);
        let cancel = CancelToken::new();
        let candidates = FilesystemProvider.complete(&r, &cancel);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].label, "report.md");
        assert_eq!(candidates[0].source, CompletionSource::Filesystem);
    }

    #[test]
    fn filesystem_provider_lists_files_at_argument_position() {
        // After `cd ` (argument position) a bare prefix still completes
        // files — the gate only applies at command position.
        let d = scratch_dir("fs_gate_arg");
        std::fs::write(d.join("notes.txt"), "").unwrap();
        let r = CompletionRequest {
            prefix: "not",
            cwd: d.to_str().unwrap(),
            history: &[],
            path_bins: &[],
            position: CompletePosition::Argument,
        };
        let cancel = CancelToken::new();
        let candidates = FilesystemProvider.complete(&r, &cancel);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].label, "notes.txt");
        assert_eq!(candidates[0].source, CompletionSource::Filesystem);
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
        // FilesystemProvider checks cancellation after I/O. v1.11.0: prefix
        // "/" passes the command-position gate (P1-5) so this still
        // exercises the cancellation check, not the gate.
        let r = req("/", "/tmp", &[], &[]);
        let candidates = FilesystemProvider.complete(&r, &cancel);
        assert!(candidates.is_empty());
    }
}
