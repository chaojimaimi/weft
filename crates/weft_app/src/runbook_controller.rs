use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::{App, PaletteEntry};

const MAX_RUNBOOK_FILE_BYTES: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub(crate) enum RunbookInteractionError {
    #[error("runbook selection cancelled")]
    Cancelled,
    #[error(transparent)]
    Panel(#[from] crate::macos_file_dialog::FilePanelError),
    #[error("runbook I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("runbook exceeds {MAX_RUNBOOK_FILE_BYTES} bytes")]
    TooLarge,
    #[error("runbook path is not a regular file")]
    NotRegularFile,
    #[error("runbook is not valid UTF-8")]
    InvalidUtf8,
    #[error("runbook contains no fenced shell commands")]
    Empty,
}

fn read_runbook(
    path: &Path,
) -> Result<Vec<weft_core::runbook::RunbookEntry>, RunbookInteractionError> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(RunbookInteractionError::NotRegularFile);
    }
    if metadata.len() > MAX_RUNBOOK_FILE_BYTES as u64 {
        return Err(RunbookInteractionError::TooLarge);
    }
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_RUNBOOK_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_RUNBOOK_FILE_BYTES {
        return Err(RunbookInteractionError::TooLarge);
    }
    let markdown = std::str::from_utf8(&bytes).map_err(|_| RunbookInteractionError::InvalidUtf8)?;
    let entries = weft_core::runbook::parse_runbook(markdown);
    if entries.is_empty() {
        return Err(RunbookInteractionError::Empty);
    }
    Ok(entries)
}

pub(crate) struct RunbookWorker {
    request_tx: crossbeam_channel::Sender<(u64, PathBuf)>,
    request_rx: crossbeam_channel::Receiver<(u64, PathBuf)>,
    result_rx:
        crossbeam_channel::Receiver<(u64, Result<Vec<weft_core::runbook::RunbookEntry>, String>)>,
    generation: Arc<AtomicU64>,
}

impl RunbookWorker {
    pub(crate) fn new(waker: impl Fn() + Send + Sync + 'static) -> Self {
        let (request_tx, request_rx) = crossbeam_channel::bounded::<(u64, PathBuf)>(1);
        let (result_tx, result_rx) = crossbeam_channel::bounded(2);
        let worker_rx = request_rx.clone();
        let stale_result_rx = result_rx.clone();
        let waker = Arc::new(waker);
        std::thread::Builder::new()
            .name("weft-runbook-loader".to_string())
            .spawn(move || {
                while let Ok((generation, path)) = worker_rx.recv() {
                    let result = read_runbook(&path).map_err(|error| error.to_string());
                    while stale_result_rx.try_recv().is_ok() {}
                    let _ = result_tx.try_send((generation, result));
                    waker();
                }
            })
            .expect("spawn runbook loader");
        Self {
            request_tx,
            request_rx,
            result_rx,
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    fn submit(&self, path: PathBuf) {
        while self.request_rx.try_recv().is_ok() {}
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.request_tx.try_send((generation, path));
    }

    fn try_recv(&self) -> Option<Result<Vec<weft_core::runbook::RunbookEntry>, String>> {
        while let Ok((generation, result)) = self.result_rx.try_recv() {
            if generation == self.generation.load(Ordering::SeqCst) {
                return Some(result);
            }
        }
        None
    }
}

impl App {
    pub(super) fn import_runbook_interactive(&mut self) -> Result<(), RunbookInteractionError> {
        let mtm = objc2_foundation::MainThreadMarker::new()
            .ok_or(crate::macos_file_dialog::FilePanelError::NotMainThread)?;
        let path = crate::macos_file_dialog::pick_runbook_open_path(mtm)?
            .ok_or(RunbookInteractionError::Cancelled)?;
        self.runbook_worker.submit(path);
        Ok(())
    }

    pub(super) fn poll_runbook_results(&mut self) {
        let Some(result) = self.runbook_worker.try_recv() else {
            return;
        };
        match result {
            Ok(entries) => {
                self.palette.runbook_entries = entries;
                self.palette.query.clear();
                self.palette.selection = 0;
                self.palette.results = self
                    .palette
                    .runbook_entries
                    .iter()
                    .cloned()
                    .map(PaletteEntry::Runbook)
                    .collect();
            }
            Err(error) => {
                tracing::warn!(%error, "runbook load failed");
                self.surface_config_error(&error);
            }
        }
        self.request_redraw();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_shell_fences_without_executing() {
        let path = std::env::temp_dir().join(format!(
            "weft-runbook-test-{}-{}.md",
            std::process::id(),
            std::thread::current().name().unwrap_or("thread")
        ));
        std::fs::write(&path, "Deploy\n\n```sh\necho safe\n```\n").unwrap();
        let entries = read_runbook(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].command, "echo safe");
    }
}
