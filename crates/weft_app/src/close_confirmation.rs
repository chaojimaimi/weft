//! Close-time protection for foreground commands.

use weft_core::blocks::ShellPhase;
use winit::event_loop::ActiveEventLoop;

use crate::tab::Tab;
use crate::{App, AppEvent, Effect};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CloseScope {
    Window,
    Tab,
    Pane,
    Workspace,
}

impl CloseScope {
    fn prompt_title(self) -> &'static str {
        match self {
            Self::Window => "Close window?",
            Self::Tab => "Close tab?",
            Self::Pane => "Close pane?",
            Self::Workspace => "Replace workspace?",
        }
    }
}

fn foreground_process_group(master_fd: std::os::fd::RawFd) -> Option<i32> {
    // SAFETY: tcgetpgrp only reads the foreground process group associated
    // with this valid PTY master descriptor. Failure is reported as -1.
    let pid = unsafe { nix::libc::tcgetpgrp(master_fd) };
    (pid > 0).then_some(pid)
}

fn command_is_running(
    shell_pid: Option<i32>,
    foreground_pgid: Option<i32>,
    phase: ShellPhase,
) -> bool {
    let foreground_job =
        matches!((shell_pid, foreground_pgid), (Some(shell), Some(fg)) if shell != fg);
    foreground_job || phase == ShellPhase::CommandExecuting
}

fn bounded_command_summary(command: &str) -> String {
    const MAX_CHARS: usize = 120;
    command
        .trim()
        .chars()
        .take(MAX_CHARS)
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

pub(crate) fn run_if_confirmed<R>(confirmed: bool, operation: impl FnOnce() -> R) -> Option<R> {
    confirmed.then(operation)
}

/// Final shutdown pair (FIX_close_exit_after_modal): request loop exit, then
/// nudge the main runloop so that request is actually observed.
///
/// WHY the wake is load-bearing: `ActiveEventLoop::exit()` only sets winit's
/// exit flag (winit 0.30.13 app_state.rs:245-247); the flag is read by the
/// RunLoopObserver's `cleared()` callback (app_state.rs:394-398). When a modal
/// confirmation ran earlier in this same event handler, `runModal`'s nested
/// runloop returns into a main runloop with no pending event source — it never
/// turns another revolution, `cleared()` never runs, and the process stays
/// alive with the flag set (docs/FIX_close_exit_after_modal.md §一).
/// `EventLoopProxy::send_event` fires CFRunLoopSourceSignal + CFRunLoopWakeUp
/// (winit event_loop.rs:513-522), driving exactly that revolution; winit's own
/// `stop_app_immediately` pairs `stop:` with a dummy event the same way
/// (winit event_loop.rs:411-418). Closures instead of the real proxy keep the
/// exit→wake contract unit-testable — `EventLoopProxy` cannot be constructed
/// in tests.
fn exit_then_request_wake<E>(exit: impl FnOnce(), wake: impl FnOnce() -> Result<(), E>) {
    exit();
    // EventLoopClosed = loop already gone — silent by design, the repo-wide
    // convention for every wakeup-class send (app_runtime.rs:145-150).
    let _ = wake();
}

fn pane_running_command(pane: &crate::pane::Pane) -> Option<String> {
    let terminal = pane.terminal.as_ref()?;
    let shell_pid = pane.pty.as_ref().map(|pty| pty.child_pid().as_raw());
    let foreground_pgid = pane
        .pty
        .as_ref()
        .and_then(|pty| foreground_process_group(pty.master_fd()));
    if !command_is_running(shell_pid, foreground_pgid, terminal.block_tracker().phase()) {
        return None;
    }

    let command = terminal
        .block_tracker()
        .in_flight()
        .map(|block| block.command.trim())
        .filter(|command| !command.is_empty())
        .map(bounded_command_summary)
        .unwrap_or_else(|| {
            foreground_pgid.map_or_else(
                || "Foreground command".to_owned(),
                |pid| format!("Foreground process group {pid}"),
            )
        });
    Some(command)
}

fn tab_running_commands(tab: &Tab) -> Vec<String> {
    tab.panes()
        .filter_map(|(_, pane)| pane_running_command(pane))
        .collect()
}

impl App {
    fn confirm_running_commands(&self, scope: CloseScope, commands: &[String]) -> bool {
        if commands.is_empty() {
            return true;
        }
        let Some(mtm) = objc2_foundation::MainThreadMarker::new() else {
            tracing::warn!(
                ?scope,
                count = commands.len(),
                "close prompt unavailable off main thread"
            );
            return false;
        };
        crate::macos_alert::show_running_process_close_prompt(mtm, scope.prompt_title(), commands)
    }

    pub(super) fn confirm_window_close(&self) -> bool {
        let commands: Vec<String> = self
            .sessions
            .tabs()
            .iter()
            .flat_map(tab_running_commands)
            .collect();
        self.confirm_running_commands(CloseScope::Window, &commands)
    }

    pub(super) fn confirm_workspace_replace(&self) -> bool {
        let commands: Vec<String> = self
            .sessions
            .tabs()
            .iter()
            .flat_map(tab_running_commands)
            .collect();
        self.confirm_running_commands(CloseScope::Workspace, &commands)
    }

    /// Guard interactive workspace replacement before profile, window,
    /// index, session, or PTY state can be mutated.
    pub(super) fn restore_workspace_with_confirmation(
        &mut self,
        doc: &weft_core::workspace::WorkspaceDocument,
    ) -> Result<
        Option<crate::workspace_controller::WorkspaceRestoreOutcome>,
        crate::workspace_controller::WorkspaceRestoreError,
    > {
        let confirmed = self.confirm_workspace_replace();
        match run_if_confirmed(confirmed, || self.restore_workspace(doc)) {
            Some(outcome) => outcome.map(Some),
            None => Ok(None),
        }
    }

    pub(super) fn request_application_close(&mut self, event_loop: &ActiveEventLoop) {
        let confirmed = self.confirm_window_close();
        let _ = run_if_confirmed(confirmed, || self.perform_clean_shutdown(event_loop));
        if !confirmed {
            tracing::info!("application close cancelled: foreground command still running");
        }
    }

    fn perform_clean_shutdown(&mut self, event_loop: &ActiveEventLoop) {
        tracing::info!("application close confirmed");
        // DIAGNOSTIC step markers (hang-after-confirm, v1.12.5): the confirm
        // line is the LAST log the field shows -- these pinpoint which
        // teardown stage stops emitting.
        // FIX (field run, v1.12.6): release every pane's PTY first -- the
        // waitpid reaper threads live in the tokio blocking pool, and the
        // pool must be free before the runtime drops on main (see
        // Pane::release_pty for the sampled stall).
        for tab in self.sessions.tabs_mut() {
            for pane in tab.panes_mut() {
                pane.release_pty();
            }
        }
        let blocks = self.finish_all_pending_blocks();
        tracing::info!(
            count = blocks.len(),
            "teardown step 1: pending blocks finished"
        );
        let mut effects = Vec::new();
        if !blocks.is_empty() {
            effects.push(crate::effect::Effect::PersistBlocks { blocks });
        }
        effects.push(crate::effect::Effect::PersistTabs);
        self.drain_effects(effects);
        tracing::info!("teardown step 2: persistence effects drained");
        if let Some(workspace) = self.capture_workspace("recovery".into()) {
            // v1.10.31: Final snapshot write is dispatched to a background thread
            // ("weft-recovery-writer") and immediately followed by event_loop.exit(),
            // creating a race. This is acceptable because:
            // 1. The clean-shutdown marker (written below) supersedes the snapshot
            //    on the next launch — no recovery prompt is shown.
            // 2. If the write completes before exit, it's a best-effort final state.
            // 3. If the write is racing exit, the worst case is a stale snapshot that
            //    gets deleted by the clean marker anyway.
            // An Err here is a synchronous setup failure (ensure_root/to_yaml)
            // — the actionable diagnostic; keep the warn (review S3). The
            // background write's own failure is flagged via dispatch_failed
            // and retried by the next autosave tick (not reachable here).
            if let Err(error) = self.recovery.write_snapshot_if_changed(&workspace) {
                tracing::warn!(error = %error, "final recovery snapshot write failed");
            }
        }
        tracing::info!("teardown step 3: recovery snapshot handled");
        self.recovery.mark_clean_shutdown();
        tracing::info!("teardown step 4: clean marker written, requesting event-loop exit");
        // FIX_close_exit_after_modal: when the modal confirmation path ran in
        // this handler, exit() alone sets a flag no observer ever reads — pair
        // it with a Wake so the main runloop turns one more revolution and
        // winit's `cleared()` checkpoint sees the flag. Mechanism chain and
        // winit source anchors: `exit_then_request_wake` doc + FIX doc §一/§二.
        exit_then_request_wake(
            || event_loop.exit(),
            || self.proxy.send_event(AppEvent::Wake),
        );
    }

    fn confirm_tab_close(&self, index: usize) -> bool {
        let commands = self
            .sessions
            .tab(index)
            .map(tab_running_commands)
            .unwrap_or_default();
        self.confirm_running_commands(CloseScope::Tab, &commands)
    }

    pub(super) fn close_active_tab_with_confirmation(&mut self) -> Vec<Effect> {
        if !self.confirm_tab_close(self.sessions.active_idx()) {
            return Vec::new();
        }
        self.close_tab()
    }

    pub(super) fn confirm_background_tab_close(&self, index: usize) -> bool {
        self.confirm_tab_close(index)
    }

    pub(super) fn confirm_active_pane_close(&self) -> bool {
        let commands = self
            .sessions
            .active()
            .map(|tab| pane_running_command(tab.active()))
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        self.confirm_running_commands(CloseScope::Pane, &commands)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        bounded_command_summary, command_is_running, exit_then_request_wake,
        foreground_process_group, run_if_confirmed, CloseScope,
    };
    use weft_core::blocks::ShellPhase;

    #[test]
    fn empty_shell_prompt_does_not_require_confirmation() {
        assert!(!command_is_running(
            Some(100),
            Some(100),
            ShellPhase::AtPrompt
        ));
        assert!(!command_is_running(
            Some(100),
            Some(100),
            ShellPhase::NotIntegrated
        ));
    }

    #[test]
    fn foreground_job_or_integrated_command_requires_confirmation() {
        assert!(command_is_running(
            Some(100),
            Some(200),
            ShellPhase::AtPrompt
        ));
        assert!(command_is_running(
            Some(100),
            Some(100),
            ShellPhase::CommandExecuting
        ));
    }

    #[test]
    fn missing_pty_evidence_falls_back_to_shell_phase() {
        assert!(command_is_running(None, None, ShellPhase::CommandExecuting));
        assert!(!command_is_running(None, None, ShellPhase::AtPrompt));
    }

    #[test]
    fn every_close_scope_has_a_specific_title() {
        assert_eq!(CloseScope::Window.prompt_title(), "Close window?");
        assert_eq!(CloseScope::Tab.prompt_title(), "Close tab?");
        assert_eq!(CloseScope::Pane.prompt_title(), "Close pane?");
        assert_eq!(CloseScope::Workspace.prompt_title(), "Replace workspace?");
    }

    #[test]
    fn cancelled_operation_is_not_run_and_confirmed_operation_runs_once() {
        let mut calls = 0;
        assert_eq!(run_if_confirmed(false, || calls += 1), None);
        assert_eq!(calls, 0);
        assert_eq!(run_if_confirmed(true, || calls += 1), Some(()));
        assert_eq!(calls, 1);
    }

    #[test]
    fn clean_shutdown_exits_then_requests_wake_and_swallows_closed() {
        // FIX_close_exit_after_modal §三: the exit→wake ordering IS the fix —
        // exit() alone strands the flag after runModal starves the main
        // runloop, and EventLoopClosed (loop already gone) must stay silent.
        // Any deviation — wake before exit, missing wake, propagated/panicking
        // error — fails this assertion.
        let events = std::cell::RefCell::new(Vec::<&'static str>::new());
        exit_then_request_wake(
            || events.borrow_mut().push("exit"),
            || {
                events.borrow_mut().push("wake");
                Err(winit::event_loop::EventLoopClosed(crate::AppEvent::Wake))
            },
        );
        assert_eq!(*events.borrow(), ["exit", "wake"]);
    }

    #[test]
    fn cancelled_workspace_replace_keeps_profile_sessions_and_pty_state() {
        let mut state = ("base", 3, true);
        let result = run_if_confirmed(false, || state = ("other", 1, false));
        assert_eq!(result, None);
        assert_eq!(state, ("base", 3, true));

        let result = run_if_confirmed(true, || state = ("other", 1, false));
        assert_eq!(result, Some(()));
        assert_eq!(state, ("other", 1, false));
    }

    #[test]
    fn command_summary_is_bounded_and_removes_controls() {
        let summary = bounded_command_summary(&format!("  abc\r\ndef\t{}", "中".repeat(200)));
        assert!(!summary.chars().any(char::is_control));
        assert_eq!(summary.chars().count(), 120);
        assert!(summary.starts_with("abc  def "));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_pty_foreground_group_distinguishes_shell_and_external_job() {
        let pty = weft_core::pty::Pty::spawn("/bin/sh", (24, 80), || {}).unwrap();
        let shell = pty.child_pid().as_raw();
        let mut shell_seen = false;
        for _ in 0..50 {
            if foreground_process_group(pty.master_fd()) == Some(shell) {
                shell_seen = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(shell_seen, "idle PTY foreground group should be the shell");

        pty.write_sync(b"sleep 5\n").unwrap();
        let mut job_seen = false;
        for _ in 0..100 {
            if foreground_process_group(pty.master_fd()).is_some_and(|pgid| pgid != shell) {
                job_seen = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(job_seen, "external foreground job should own the PTY");
        assert!(pty.send_interrupt());
    }
}
