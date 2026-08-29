//! Background provider-based editor completion.

use crossbeam_channel::{bounded, Receiver, Sender};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use weft_core::complete::{CompletePosition, Match, MatchKind};
use weft_core::completion::{
    complete_with_providers_budgeted, dedupe_candidates, sort_candidates, CancelToken,
    CompletionProvider, CompletionRequest, CompletionSource, FilesystemProvider, HistoryProvider,
    PathExecutableProvider, WorkflowProvider, WorkspaceCommandProvider,
};

const COMPLETION_BUDGET: Duration = Duration::from_millis(40);

struct WorkerRequest {
    generation: u64,
    pane_session_id: u64,
    line: String,
    cursor_col: usize,
    word_start: usize,
    word_end: usize,
    prefix: String,
    cwd: String,
    history: Vec<String>,
    path_bins: Vec<String>,
    workflows: Vec<(String, String)>,
    workspace_commands: Vec<String>,
    position: CompletePosition,
    cancel: CancelToken,
}

pub(crate) struct CompletionResult {
    pub(crate) generation: u64,
    pub(crate) pane_session_id: u64,
    pub(crate) line: String,
    pub(crate) cursor_col: usize,
    pub(crate) word_start: usize,
    pub(crate) word_end: usize,
    pub(crate) matches: Vec<Match>,
}

pub(crate) struct CompletionSubmit {
    pub(crate) pane_session_id: u64,
    pub(crate) line: String,
    pub(crate) cursor_col: usize,
    pub(crate) word_start: usize,
    pub(crate) word_end: usize,
    pub(crate) prefix: String,
    pub(crate) cwd: String,
    pub(crate) history: Vec<String>,
    pub(crate) path_bins: Vec<String>,
    pub(crate) workflows: Vec<(String, String)>,
    pub(crate) workspace_commands: Vec<String>,
    pub(crate) position: CompletePosition,
}

pub(crate) struct CompletionWorker {
    request_tx: Sender<WorkerRequest>,
    request_rx: Receiver<WorkerRequest>,
    result_rx: Receiver<CompletionResult>,
    generation: Arc<AtomicU64>,
    active_cancel: Option<CancelToken>,
}

impl CompletionWorker {
    pub(crate) fn new(waker: impl Fn() + Send + Sync + 'static) -> Self {
        let (request_tx, request_rx) = bounded::<WorkerRequest>(1);
        let (result_tx, result_rx) = bounded::<CompletionResult>(4);
        let generation = Arc::new(AtomicU64::new(0));
        let worker_rx = request_rx.clone();
        let stale_result_rx = result_rx.clone();
        let waker = Arc::new(waker);
        std::thread::Builder::new()
            .name(String::from("weft-completion"))
            .spawn(move || run_worker(worker_rx, result_tx, stale_result_rx, waker))
            .expect("spawn completion worker");
        Self {
            request_tx,
            request_rx,
            result_rx,
            generation,
            active_cancel: None,
        }
    }

    pub(crate) fn submit(&mut self, submit: CompletionSubmit) -> u64 {
        if let Some(cancel) = self.active_cancel.take() {
            cancel.cancel();
        }
        while self.request_rx.try_recv().is_ok() {}
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let cancel = CancelToken::new();
        self.active_cancel = Some(cancel.clone());
        let _ = self.request_tx.try_send(WorkerRequest {
            generation,
            pane_session_id: submit.pane_session_id,
            line: submit.line,
            cursor_col: submit.cursor_col,
            word_start: submit.word_start,
            word_end: submit.word_end,
            prefix: submit.prefix,
            cwd: submit.cwd,
            history: submit.history,
            path_bins: submit.path_bins,
            workflows: submit.workflows,
            workspace_commands: submit.workspace_commands,
            position: submit.position,
            cancel,
        });
        generation
    }

    pub(crate) fn try_recv(&self) -> Option<CompletionResult> {
        self.result_rx.try_recv().ok()
    }

    pub(crate) fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }
}

fn run_worker(
    request_rx: Receiver<WorkerRequest>,
    result_tx: Sender<CompletionResult>,
    stale_result_rx: Receiver<CompletionResult>,
    waker: Arc<dyn Fn() + Send + Sync>,
) {
    let filesystem_in_flight = Arc::new(std::sync::atomic::AtomicBool::new(false));
    while let Ok(request) = request_rx.recv() {
        let started = std::time::Instant::now();
        let history = HistoryProvider;
        let path = PathExecutableProvider;
        let workflow = WorkflowProvider::new(request.workflows.clone());
        let workspace = WorkspaceCommandProvider::new(request.workspace_commands.clone());
        let providers: [&dyn CompletionProvider; 4] = [&history, &path, &workflow, &workspace];
        let completion_request = CompletionRequest {
            prefix: &request.prefix,
            cwd: &request.cwd,
            history: &request.history,
            path_bins: &request.path_bins,
            position: request.position,
        };
        let mut candidates = complete_with_providers_budgeted(
            &providers,
            &completion_request,
            &request.cancel,
            COMPLETION_BUDGET,
        );
        let remaining = COMPLETION_BUDGET.saturating_sub(started.elapsed());
        let claimed_filesystem = filesystem_in_flight
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        // 2.2 gate: only list files at an Argument position or with a
        // path-like prefix (`./`, `/usr/...`). A bare command prefix
        // ("pnp") must not surface file entries.
        if !remaining.is_zero()
            && !request.cancel.is_cancelled()
            && claimed_filesystem
            && (request.position == CompletePosition::Argument || request.prefix.contains('/'))
        {
            let (filesystem_tx, filesystem_rx) = std::sync::mpsc::sync_channel(1);
            let prefix = request.prefix.clone();
            let cwd = request.cwd.clone();
            let position = request.position;
            let cancel = request.cancel.clone();
            let filesystem_done = Arc::clone(&filesystem_in_flight);
            let spawned = std::thread::Builder::new()
                .name(String::from("weft-completion-fs"))
                .spawn(move || {
                    let provider = FilesystemProvider;
                    let completion_request = CompletionRequest {
                        prefix: &prefix,
                        cwd: &cwd,
                        history: &[],
                        path_bins: &[],
                        position,
                    };
                    let result = provider.complete(&completion_request, &cancel);
                    let _ = filesystem_tx.send(result);
                    filesystem_done.store(false, Ordering::SeqCst);
                });
            if spawned.is_err() {
                filesystem_in_flight.store(false, Ordering::SeqCst);
            } else if let Ok(filesystem) = filesystem_rx.recv_timeout(remaining) {
                candidates.extend(filesystem);
                sort_candidates(&mut candidates);
                candidates = dedupe_candidates(candidates);
            }
        } else if claimed_filesystem {
            filesystem_in_flight.store(false, Ordering::SeqCst);
        }
        let matches = candidates.into_iter().map(candidate_to_match).collect();
        if request.cancel.is_cancelled() {
            continue;
        }
        while stale_result_rx.try_recv().is_ok() {}
        let _ = result_tx.try_send(CompletionResult {
            generation: request.generation,
            pane_session_id: request.pane_session_id,
            line: request.line,
            cursor_col: request.cursor_col,
            word_start: request.word_start,
            word_end: request.word_end,
            matches,
        });
        waker();
    }
}

fn candidate_to_match(candidate: weft_core::completion::CompletionCandidate) -> Match {
    let kind = match candidate.source {
        CompletionSource::History => MatchKind::History,
        CompletionSource::Filesystem => MatchKind::Path,
        CompletionSource::PathExecutable
        | CompletionSource::Workflow
        | CompletionSource::WorkspaceCommand => MatchKind::Command,
    };
    Match {
        label: candidate.label,
        kind,
        insert: candidate.insert,
        is_dir: candidate.is_dir,
        match_quality: candidate.match_quality,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_request_cancels_and_replaces_pending_request() {
        let (wake_tx, wake_rx) = std::sync::mpsc::channel();
        let mut worker = CompletionWorker::new(move || {
            let _ = wake_tx.send(());
        });
        let submit = |prefix: &str| CompletionSubmit {
            pane_session_id: 1,
            line: prefix.to_string(),
            cursor_col: prefix.len(),
            word_start: 0,
            word_end: prefix.len(),
            prefix: prefix.to_string(),
            cwd: "/definitely/not/real".to_string(),
            history: vec!["cargo test".to_string()],
            path_bins: vec!["cargo".to_string()],
            workflows: Vec::new(),
            workspace_commands: Vec::new(),
            position: CompletePosition::Command,
        };
        worker.submit(submit("x"));
        let generation = worker.submit(submit("ca"));
        // v1.11.8 (PLAN_v1118 M-D, candidate 1): the worker may legitimately
        // win the race and complete gen-1 before the main thread cancels it
        // (F17) — its result then sits in the queue ahead of gen-2's, so a
        // single recv+try_recv reads gen-1 and flakes (1 != 2). Poll for the
        // generation-2 result specifically, within a 2s TOTAL budget
        // (architect P2-6: worst case = two serial budget periods, aligned
        // with the old single recv_timeout). gen-1 results are legal
        // transient states under production race semantics — filtered, not
        // failed.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let result = 'outer: loop {
            let remaining = deadline
                .checked_duration_since(std::time::Instant::now())
                .expect("flaky-test timeout: gen-2 result did not arrive within 2s");
            wake_rx
                .recv_timeout(remaining)
                .expect("worker did not wake");
            while let Some(r) = worker.try_recv() {
                if r.generation == generation {
                    break 'outer r;
                }
            }
        };
        assert_eq!(result.generation, generation);
        assert!(result
            .matches
            .iter()
            .any(|candidate| candidate.label == "cargo"));
    }
}
