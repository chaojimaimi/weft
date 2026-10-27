//! Inline tests for `pane.rs`, extracted to keep the production file
//! under the 800-line architecture gate (repo convention: `pty.rs`
//! -> `pty/tests.rs`).

use super::*;

impl Pane {
    /// Empty pane — terminal/pty stay None. v1.14.2 (B3): no longer a
    /// production fallback (a failed fork now closes the owning tab —
    /// see `spawn`); the base of the test-only `Tab::empty` constructor.
    /// Test-only since v1.12.23 audit batch 2 precedent (Tab::empty twin).
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        let (msg_tx, msg_rx) = crossbeam_channel::bounded(1024);
        Self {
            pane_session_id: NEXT_PANE_SESSION.fetch_add(1, Ordering::Relaxed),
            input_seq: NEXT_INPUT_SEQ.fetch_add(1, Ordering::Relaxed),
            terminal: None,
            pty: None,
            // T10 P2 (D2): PTY spawn failure — no receiver ⇒ no parse worker.
            writer: None,
            had_output: Arc::new(AtomicBool::new(false)),
            mouse_suppress: new_flag(),
            msg_rx,
            msg_tx,
            input_handler: InputHandler::new(),
            selection_handler: SelectionHandler::new(),
            ime_preedit: String::new(),
            ime_preedit_cursor: None,
            pending_pty_resize: None,
            close_deadline: None,
            close_settled: false,
            last_resize_commit: None,
            last_sent_winsize: None,
            block_scroll_anchor: BlockScrollAnchor::FollowBottom,
            block_scroll_fraction: 0.0,
            grid_scroll_px: 0.0,
            restored_snapshot: None,
            restored_cwd: None,
            restored_cwd_set: false,
            pending_tui_scroll: None,
            tui_scroll_deadline: None,
            tui_scroll_wake_scheduled: false,
            primary_history_refresh: PrimaryHistoryRefresh::default(),
            alt_peek_gate: crate::alt_peek::PeekEntryGate::default(),
            pending_spawn: None,
            pending_stdin: Vec::new(),
            child_pid: None,
            reconcile_memo: None,
        }
    }

    /// Build a pane with a live Terminal but no PTY — used by snapshot
    /// roundtrip tests (`tab_with_terminal`). The pane has a fresh
    /// `pane_session_id` and the given scrollback depth.
    #[cfg(test)]
    pub(crate) fn with_terminal_only(scrollback_lines: usize) -> Self {
        let (msg_tx, msg_rx) = crossbeam_channel::bounded(1024);
        Self {
            pane_session_id: NEXT_PANE_SESSION.fetch_add(1, Ordering::Relaxed),
            input_seq: NEXT_INPUT_SEQ.fetch_add(1, Ordering::Relaxed),
            terminal: Some(Arc::new(FairMutex::new(Terminal::with_scrollback(
                24,
                80,
                scrollback_lines,
            )))),
            pty: None,
            writer: None,
            had_output: Arc::new(AtomicBool::new(false)),
            mouse_suppress: new_flag(),
            msg_rx,
            msg_tx,
            input_handler: InputHandler::new(),
            selection_handler: SelectionHandler::new(),
            ime_preedit: String::new(),
            ime_preedit_cursor: None,
            pending_pty_resize: None,
            close_deadline: None,
            close_settled: false,
            last_resize_commit: None,
            last_sent_winsize: None,
            block_scroll_anchor: BlockScrollAnchor::FollowBottom,
            block_scroll_fraction: 0.0,
            grid_scroll_px: 0.0,
            restored_snapshot: None,
            restored_cwd: None,
            restored_cwd_set: false,
            pending_tui_scroll: None,
            tui_scroll_deadline: None,
            tui_scroll_wake_scheduled: false,
            primary_history_refresh: PrimaryHistoryRefresh::default(),
            alt_peek_gate: crate::alt_peek::PeekEntryGate::default(),
            pending_spawn: None,
            pending_stdin: Vec::new(),
            child_pid: None,
            reconcile_memo: None,
        }
    }

    /// Test-only terminal injection for the former `pane.terminal = Some(…)`
    /// direct-field assignments.
    #[cfg(test)]
    pub(crate) fn set_terminal_for_test(&mut self, terminal: Terminal) {
        self.terminal = Some(Arc::new(FairMutex::new(terminal)));
    }

    /// Test-only Arc clone of this pane's terminal — lets the REAL parse
    /// worker thread be wired to a pane without a real PTY (the worker e2e
    /// test). D9 rule 1 stands for production consumers: nothing outside
    /// the test builds may obtain the Arc.
    #[cfg(test)]
    pub(crate) fn terminal_arc_for_test(&self) -> Option<Arc<FairMutex<Terminal>>> {
        self.terminal.clone()
    }
}

#[test]
fn restored_cwd_fallback_is_set_once_and_visible_to_snapshot_logic() {
    let mut pane = Pane::with_terminal_only(1000);
    pane.set_restored_cwd_fallback(Some("/saved".into()));
    assert_eq!(pane.restored_cwd.as_deref(), Some("/saved"));
    assert_eq!(
        pane.restored_cwd_fallback(),
        Some("/saved"),
        "the fallback must be visible to the snapshot cwd logic"
    );
    assert!(
        pane.restored_snapshot.is_none(),
        "v1.8.9 no-op regression: the cwd fallback must not write a stub snapshot"
    );

    pane.set_restored_cwd_fallback(Some("/replacement".into()));
    assert_eq!(
        pane.restored_cwd.as_deref(),
        Some("/saved"),
        "a later fallback must not replace the original recovery state"
    );
}

// ── v1.10.19: PTY winsize dedup (resize-loop defense layer 1) ────────

#[test]
fn should_send_winsize_ioctl_skips_sizes_already_sent() {
    assert!(!Pane::should_send_winsize_ioctl(Some((24, 80)), (24, 80)));
    assert!(Pane::should_send_winsize_ioctl(Some((24, 80)), (24, 100)));
    assert!(Pane::should_send_winsize_ioctl(Some((24, 80)), (25, 80)));
    // Before any ioctl was ever sent, every request is a real send.
    assert!(Pane::should_send_winsize_ioctl(None, (24, 80)));
}

#[test]
fn apply_winsize_ioctl_dedups_and_records_the_last_sent_size() {
    // with_terminal_only has no PTY — the dedup decision still applies:
    // a re-request of the last-sent size must short-circuit before any
    // PTY access (Ok(false)), while a genuinely new size takes the
    // ioctl path and fails with the "no pty" error.
    let mut pane = Pane::with_terminal_only(1000);
    pane.last_sent_winsize = Some((24, 80));
    assert_eq!(pane.apply_winsize_ioctl(24, 80), Ok(false));
    assert_eq!(
        pane.last_sent_winsize,
        Some((24, 80)),
        "dedup does not touch last_sent_winsize"
    );
    assert!(
        pane.apply_winsize_ioctl(24, 100).is_err(),
        "a changed size must reach the ioctl path (no pty here → error)"
    );
    assert_eq!(
        pane.last_sent_winsize,
        Some((24, 80)),
        "failed ioctl must not record the requested size"
    );
}

// ── v1.10.36: restored-tab chdir target filter ──────────────────────

#[test]
fn restore_spawn_cwd_none_and_empty_inherit_weft_cwd() {
    assert_eq!(restore_spawn_cwd(None), None);
    assert_eq!(restore_spawn_cwd(Some("")), None);
}

#[test]
fn restore_spawn_cwd_honors_non_home_paths_verbatim() {
    assert_eq!(
        restore_spawn_cwd(Some("/Users/me")),
        Some("/Users/me".to_string())
    );
}

#[test]
fn restore_spawn_cwd_keeps_home_verbatim() {
    // Regression guard for the production bug: a saved cwd equal to
    // $HOME was filtered out, so a Finder-launched Weft (process cwd `/`)
    // restored home tabs at the filesystem root. The pure function takes
    // a literal so the guard runs even in a HOME-less CI environment.
    assert_eq!(
        restore_spawn_cwd(Some("/Users/andy")),
        Some("/Users/andy".to_string()),
        "a saved cwd equal to $HOME must be honored, not dropped"
    );
}
