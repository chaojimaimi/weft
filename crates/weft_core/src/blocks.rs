//! Command blocks — the metadata layer (v0.4 "Fabric", phase 1).
//!
//! A [`Block`] binds one shell command to its output, exit code, and timing.
//! Blocks are driven by OSC 133 shell-integration markers (see
//! [`crate::vt::ShellMarker`] and `crate::shell`): the shell emits `133;A`
//! (prompt start), `133;B` (command start / preexec), `133;C` (output start),
//! and `133;D;<exit>` (command end). [`BlockTracker`] is the state machine
//! that turns that marker stream into finished [`Block`]s.
//!
//! **Design: detached content.** A Block owns its `command` and `output` as
//! plain `String`s — snapshotted at marker time, not anchored to live grid
//! rows. The grid has no stable row identity across scroll / reflow /
//! clear_scrollback, so anchoring would be fragile. Capturing the text as it
//! streams (and the command line at `133;B`) makes a Block immune to every
//! later grid mutation. This is the low-risk foundation for history, search,
//! and persistence; in-terminal fold (which does need live row anchoring) is
//! deferred to v0.5 and will extend `Block` additively.
//!
//! This module is pure logic — no grid / terminal / fs dependency — so it is
//! fully unit-testable in isolation. The `Terminal` (phase 2) drives a
//! `BlockTracker` from its `osc_dispatch` / `print` paths.

use std::time::SystemTime;

/// Hard cap on captured output to bound memory for commands like
/// `cat huge.log`. Beyond this the capture stops and the block is marked
/// truncated. The grid already holds the full output for display; this only
/// guards the detached snapshot used by search / persistence.
/// v0.9 fix: raised from 64 KiB to 1 MiB. The 64 KiB cap was too aggressive
/// for real-world commands (e.g. `for i in $(seq 1 20000); do echo ...; done`
/// hits it at ~3000 lines). 1 MiB covers typical log dumps / build outputs
/// while keeping memory bounded (a 100-block session = 100 MiB worst case,
/// acceptable for a desktop terminal). Block view layout is independently
/// capped at 2000 visible lines per block in the renderer, so raising this
/// doesn't affect rendering perf.
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// Monotonically-increasing block identifier. Assigned by [`BlockTracker`],
/// reused as the SQLite primary key (phase 3).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockId(pub u64);

/// One executed command + its captured output + metadata.
#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub id: BlockId,
    /// The command line, extracted from the grid row at `133;B` (preexec).
    /// Best-effort (~80% accurate): it is the whole prompt row trimmed, so it
    /// may include a prompt glyph. Used as a title, not authoritative.
    pub command: String,
    /// Working directory at command start (from OSC 7). Shown in the block
    /// header (`<cwd> (<duration>)`) Warp-style. `None` for blocks loaded
    /// from older DB rows that predate the field.
    pub cwd: Option<String>,
    /// Output captured between `133;B` and `133;D` (detached snapshot).
    pub output: String,
    /// Exit code from `133;D;<exit>`. `None` when the command was interrupted
    /// (shell re-prompted via `133;A` without a preceding `133;D`).
    pub exit_code: Option<i32>,
    /// `133;B` time. `SystemTime` (not `Instant`) so it serializes to SQLite.
    pub started_at: SystemTime,
    /// `133;D` time. `None` only transiently while in flight.
    pub finished_at: Option<SystemTime>,
    /// Panel-local collapse state (v0.4 sidebar). v0.5 will also drive
    /// in-terminal fold via an added `live_range` field — additive, no rewrite.
    pub collapsed: bool,
}

/// A view of the currently-running command (borrowed from [`BlockTracker`]),
/// for the renderer's live block during CommandExecuting.
pub struct InFlightBlock<'a> {
    pub command: &'a str,
    pub cwd: Option<&'a str>,
    pub output: &'a str,
}

/// Shell-phase state machine, driven by OSC 133. See
/// `docs/DECISION-input-architecture.md` (§123-204). v0.4 uses it only to
/// gate output capture and mark integration readiness; the derived
/// `InputMode` (AtPrompt → editor) is a v0.5 concern.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum ShellPhase {
    /// No integration detected yet (boot / SSH / non-integrated shell).
    #[default]
    NotIntegrated,
    /// After `133;A`, before `133;B` — a prompt is showing.
    AtPrompt,
    /// `133;B` .. `133;D` — a command is running; output is being captured.
    CommandExecuting,
}

/// Turns an OSC 133 marker stream into finished [`Block`]s.
///
/// Owned by `Terminal` (phase 2). The shell-integration markers and printed
/// characters are fed in via the `on_*` methods in stream order; the tracker
/// keeps the resulting [`Block`]s in `blocks` (the in-memory history the panel
/// reads) and a parallel `unpersisted` queue that the app drains into SQLite.
#[derive(Debug)]
pub struct BlockTracker {
    phase: ShellPhase,
    /// True once the first `133;A` arrives → the shell sourced our hook.
    bootstrap_ready: bool,
    /// Finished blocks, newest last (the panel / search read this).
    blocks: Vec<Block>,
    /// Finished-but-not-yet-persisted blocks; drained by the app into SQLite.
    unpersisted: Vec<Block>,
    /// Next [`BlockId`] to assign.
    next_id: u64,
    // ── in-flight command (between 133;B and 133;D) ──
    pending_command: Option<String>,
    pending_started: Option<SystemTime>,
    /// cwd snapshotted at 133;B (from `current_cwd`), stamped onto the block.
    pending_cwd: Option<String>,
    /// Latest cwd from OSC 7; snapshotted into each new block.
    current_cwd: Option<String>,
    output_buf: String,
    output_truncated: bool,
    /// Index in `blocks` where the current session's blocks begin. Blocks
    /// before this were loaded from SQLite on startup (history search only —
    /// NOT shown in the main block view, which is session-scoped).
    session_start: usize,
}

impl Default for BlockTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockTracker {
    pub fn new() -> Self {
        Self {
            phase: ShellPhase::NotIntegrated,
            bootstrap_ready: false,
            blocks: Vec::new(),
            unpersisted: Vec::new(),
            next_id: 1,
            pending_command: None,
            pending_started: None,
            pending_cwd: None,
            current_cwd: None,
            output_buf: String::new(),
            output_truncated: false,
            session_start: 0,
        }
    }

    /// Current working directory (updated from OSC 7). Snapshotted into the
    /// next block at `133;B` so each block carries the dir it ran in.
    pub fn set_cwd(&mut self, cwd: Option<String>) {
        self.current_cwd = cwd;
    }

    // ── ShellPhase accessors ─────────────────────────────────────────────

    pub fn phase(&self) -> ShellPhase {
        self.phase
    }

    pub fn bootstrap_ready(&self) -> bool {
        self.bootstrap_ready
    }

    /// True while a command is executing and output should be captured.
    /// The `Terminal` print path gates on this (plus an alt-screen check).
    pub fn is_capturing(&self) -> bool {
        self.phase == ShellPhase::CommandExecuting
    }

    /// All finished blocks (newest last). The history panel / search read this
    /// (includes blocks loaded from SQLite on startup).
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// Blocks created THIS session only (excludes the startup-hydrated SQLite
    /// history). The main Warp-style block view reads this so it doesn't show
    /// phantom pre-session history; the panel still uses [`blocks`].
    pub fn session_blocks(&self) -> &[Block] {
        let start = self.session_start.min(self.blocks.len());
        &self.blocks[start..]
    }

    /// Take the finished-but-unpersisted blocks. The app calls this after each
    /// PTY batch and inserts them into SQLite; the blocks remain in [`blocks`].
    pub fn drain_unpersisted(&mut self) -> Vec<Block> {
        std::mem::take(&mut self.unpersisted)
    }

    /// Toggle the fold (collapse/expand) state of a block by id. Session-only
    /// (not persisted to SQLite in the MVP — the insert-time `collapsed` is).
    pub fn toggle_collapse(&mut self, id: BlockId) {
        if let Some(b) = self.blocks.iter_mut().find(|b| b.id == id) {
            b.collapsed = !b.collapsed;
        }
    }

    /// The currently-running command (between `133;B` and `133;D`), for the
    /// renderer's live block during CommandExecuting (e.g. an interactive
    /// `sudo su`). `None` when nothing is in flight.
    pub fn in_flight(&self) -> Option<InFlightBlock<'_>> {
        if !self.is_capturing() {
            return None;
        }
        let command = self.pending_command.as_deref()?;
        Some(InFlightBlock {
            command,
            cwd: self.pending_cwd.as_deref(),
            output: self.output_buf.as_str(),
        })
    }

    /// Load previously-persisted blocks (e.g. on startup from SQLite). They go
    /// straight into the history list (already persisted, so NOT into
    /// `unpersisted`), and `next_id` is advanced past the highest loaded id.
    pub fn load_blocks(&mut self, blocks: Vec<Block>) {
        for b in &blocks {
            if b.id.0 >= self.next_id {
                self.next_id = b.id.0 + 1;
            }
        }
        self.blocks.extend(blocks);
        // Everything loaded so far is pre-session history; session blocks
        // (appended after this) begin at the new length.
        self.session_start = self.blocks.len();
    }

    // ── Marker-driven state transitions ──────────────────────────────────

    /// `133;A` — prompt start. Marks integration ready. If a command was
    /// mid-flight without a closing `133;D` (interrupted, e.g. Ctrl+C),
    /// finalize it with no exit code so it still appears in history.
    pub fn on_prompt_start(&mut self) {
        self.bootstrap_ready = true;
        if self.phase == ShellPhase::CommandExecuting {
            self.finalize(None);
        }
        self.phase = ShellPhase::AtPrompt;
    }

    /// `133;B` — command start (preexec). `command` is the prompt-row text the
    /// caller extracted from the grid. Begins output capture.
    pub fn on_command_start(&mut self, command: String) {
        self.pending_command = Some(command);
        self.pending_started = Some(SystemTime::now());
        self.pending_cwd = self.current_cwd.clone();
        self.output_buf.clear();
        self.output_truncated = false;
        self.phase = ShellPhase::CommandExecuting;
    }

    /// `133;C` — output start. Capture already began at `133;B`; this is a
    /// no-op for v0.4 (kept for symmetry and future marker-aware splitting).
    pub fn on_command_output_start(&mut self) {
        // intentionally empty
    }

    /// `133;D;<exit>` — command end. Finalizes the in-flight block.
    pub fn on_command_end(&mut self, exit_code: i32) {
        self.finalize(Some(exit_code));
        self.phase = ShellPhase::AtPrompt;
    }

    // ── Output capture (called from Terminal print / LF paths) ───────────

    /// Append a printed character to the in-flight block's output. No-op unless
    /// a command is executing.
    pub fn on_print(&mut self, c: char) {
        if self.is_capturing() {
            self.push_capped(c);
        }
    }

    /// Append a newline to the in-flight block's output. No-op unless a command
    /// is executing.
    pub fn on_newline(&mut self) {
        if self.is_capturing() {
            self.push_capped('\n');
        }
    }

    fn push_capped(&mut self, c: char) {
        if self.output_truncated {
            return;
        }
        if self.output_buf.len() + c.len_utf8() > MAX_OUTPUT_BYTES {
            self.output_truncated = true;
            return;
        }
        self.output_buf.push(c);
    }

    /// Finalize the in-flight command into a [`Block`], appending it to both
    /// the history list and the unpersisted queue. A stray `133;D` with no
    /// preceding `133;B` (no pending command) is discarded — it produces no
    /// garbage block.
    fn finalize(&mut self, exit_code: Option<i32>) {
        let Some(command) = self.pending_command.take() else {
            self.pending_started = None;
            self.output_buf.clear();
            self.output_truncated = false;
            return;
        };
        let started_at = self.pending_started.take().unwrap_or_else(SystemTime::now);
        let cwd = self.pending_cwd.take();
        let mut output = std::mem::take(&mut self.output_buf);
        if self.output_truncated {
            output.push_str("\n…(output truncated, >1 MiB)");
        }
        self.output_truncated = false;
        // Mask secrets capture-side so the stored block (history / search /
        // future AI context) never holds a credential. The live grid stays raw.
        output = crate::secrets::mask(&output);

        let block = Block {
            id: BlockId(self.next_id),
            command,
            cwd,
            output,
            exit_code,
            started_at,
            finished_at: Some(SystemTime::now()),
            collapsed: false,
        };
        self.next_id += 1;
        self.blocks.push(block.clone());
        self.unpersisted.push(block);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a tracker through one command and return the resulting block.
    fn run_one<'a>(
        tracker: &'a mut BlockTracker,
        command: &str,
        output: &str,
        exit: i32,
    ) -> &'a Block {
        let before = tracker.blocks().len();
        tracker.on_prompt_start();
        tracker.on_command_start(command.to_string());
        for line in output.split('\n') {
            for ch in line.chars() {
                tracker.on_print(ch);
            }
            tracker.on_newline();
        }
        tracker.on_command_end(exit);
        assert_eq!(tracker.blocks().len(), before + 1, "expected one new block");
        tracker.blocks().last().unwrap()
    }

    #[test]
    fn starts_not_integrated() {
        let t = BlockTracker::new();
        assert_eq!(t.phase(), ShellPhase::NotIntegrated);
        assert!(!t.bootstrap_ready());
        assert!(t.blocks().is_empty());
        assert!(!t.is_capturing());
    }

    #[test]
    fn bootstrap_sets_on_first_prompt_start() {
        let mut t = BlockTracker::new();
        t.on_prompt_start();
        assert!(t.bootstrap_ready());
        assert_eq!(t.phase(), ShellPhase::AtPrompt);
    }

    #[test]
    fn full_lifecycle_produces_one_block() {
        let mut t = BlockTracker::new();
        let block = run_one(&mut t, "ls -la", "file_a\nfile_b", 0);

        assert_eq!(block.command, "ls -la");
        // run_one splits on '\n' and appends a newline after each segment,
        // so "file_a\nfile_b" → "file_a\nfile_b\n".
        assert_eq!(block.output, "file_a\nfile_b\n");
        assert_eq!(block.exit_code, Some(0));
        assert!(block.finished_at.is_some());
        assert!(block.finished_at.unwrap() >= block.started_at);
        assert!(!block.collapsed);
        assert_eq!(block.id, BlockId(1));
        assert_eq!(t.phase(), ShellPhase::AtPrompt);
    }

    #[test]
    fn command_end_without_command_start_is_noop() {
        let mut t = BlockTracker::new();
        t.on_prompt_start();
        t.on_command_end(0); // no pending command
        assert!(t.blocks().is_empty());
        assert_eq!(t.phase(), ShellPhase::AtPrompt);
    }

    #[test]
    fn multiple_commands_accumulate_with_rising_ids() {
        let mut t = BlockTracker::new();
        run_one(&mut t, "echo a", "a", 0);
        run_one(&mut t, "echo b", "b", 0);

        let blocks = t.blocks();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].id, BlockId(1));
        assert_eq!(blocks[1].id, BlockId(2));
        assert_eq!(blocks[0].command, "echo a");
        assert_eq!(blocks[1].command, "echo b");
    }

    #[test]
    fn capture_only_while_executing() {
        let mut t = BlockTracker::new();
        // Printing before any command must not be captured.
        t.on_prompt_start();
        t.on_print('x');
        t.on_newline();
        assert!(!t.is_capturing());

        t.on_command_start("cmd".to_string());
        assert!(t.is_capturing());
        t.on_print('h');
        t.on_print('i');
        t.on_command_end(0);

        let block = t.blocks().last().unwrap();
        assert_eq!(block.output, "hi");
    }

    #[test]
    fn newline_becomes_output_newline() {
        let mut t = BlockTracker::new();
        t.on_prompt_start();
        t.on_command_start("c".to_string());
        t.on_print('a');
        t.on_newline();
        t.on_print('b');
        t.on_command_end(0);
        assert_eq!(t.blocks().last().unwrap().output, "a\nb");
    }

    #[test]
    fn failed_command_records_nonzero_exit() {
        let mut t = BlockTracker::new();
        let block = run_one(&mut t, "false", "", 1);
        assert_eq!(block.exit_code, Some(1));
    }

    #[test]
    fn re_prompt_without_end_finalizes_interrupted() {
        let mut t = BlockTracker::new();
        t.on_prompt_start();
        t.on_command_start("sleep 100".to_string());
        t.on_print('z');
        // User hits Ctrl+C: shell re-prompts with 133;A, no 133;D.
        t.on_prompt_start();

        assert_eq!(t.blocks().len(), 1);
        let block = &t.blocks()[0];
        assert_eq!(block.command, "sleep 100");
        assert_eq!(block.output, "z");
        assert_eq!(block.exit_code, None, "interrupted → no exit code");
        assert_eq!(t.phase(), ShellPhase::AtPrompt);
    }

    #[test]
    fn output_cap_truncates() {
        let mut t = BlockTracker::new();
        t.on_prompt_start();
        t.on_command_start("cat huge".to_string());
        // Push well past the cap.
        for _ in 0..(MAX_OUTPUT_BYTES + 1024) {
            t.on_print('a');
        }
        t.on_command_end(0);

        let block = t.blocks().last().unwrap();
        assert!(
            block.output.ends_with("(output truncated, >1 MiB)"),
            "expected truncation marker, got tail: …{}",
            &block.output[block.output.len().saturating_sub(40)..]
        );
        assert!(
            block.output.len() <= MAX_OUTPUT_BYTES + 64,
            "captured output must not exceed the cap by more than the marker"
        );
    }

    #[test]
    fn drain_unpersisted_then_empty() {
        let mut t = BlockTracker::new();
        run_one(&mut t, "a", "", 0);
        run_one(&mut t, "b", "", 0);

        let drained = t.drain_unpersisted();
        assert_eq!(drained.len(), 2);
        // Blocks remain in history; only the unpersisted queue is drained.
        assert_eq!(t.blocks().len(), 2);
        assert!(t.drain_unpersisted().is_empty());
    }

    #[test]
    fn load_blocks_advances_next_id() {
        let mut t = BlockTracker::new();
        let loaded = vec![
            Block {
                id: BlockId(7),
                command: "old".into(),
                cwd: None,
                output: String::new(),
                exit_code: Some(0),
                started_at: SystemTime::UNIX_EPOCH,
                finished_at: Some(SystemTime::UNIX_EPOCH),
                collapsed: false,
            },
            Block {
                id: BlockId(3),
                command: "older".into(),
                cwd: None,
                output: String::new(),
                exit_code: Some(0),
                started_at: SystemTime::UNIX_EPOCH,
                finished_at: Some(SystemTime::UNIX_EPOCH),
                collapsed: false,
            },
        ];
        t.load_blocks(loaded);
        assert_eq!(t.blocks().len(), 2);

        // Next freshly-detected block must not collide with loaded id 7.
        run_one(&mut t, "new", "", 0);
        assert_eq!(t.blocks().last().unwrap().id, BlockId(8));
    }

    #[test]
    fn session_blocks_exclude_loaded_history() {
        let mut t = BlockTracker::new();
        // Pre-session blocks loaded from SQLite on startup.
        t.load_blocks(vec![Block {
            id: BlockId(1),
            command: "old".into(),
            cwd: None,
            output: String::new(),
            exit_code: Some(0),
            started_at: SystemTime::UNIX_EPOCH,
            finished_at: Some(SystemTime::UNIX_EPOCH),
            collapsed: false,
        }]);
        assert_eq!(t.blocks().len(), 1);
        // The main block view is session-scoped: loaded history is excluded.
        assert!(t.session_blocks().is_empty());

        // A command run this session becomes a session block.
        run_one(&mut t, "ls", "", 0);
        assert_eq!(t.blocks().len(), 2);
        assert_eq!(t.session_blocks().len(), 1);
        assert_eq!(t.session_blocks()[0].command, "ls");
    }

    #[test]
    fn cwd_is_stamped_from_set_cwd_at_command_start() {
        let mut t = BlockTracker::new();
        t.set_cwd(Some("/Users/me/proj".to_string()));
        let b = run_one(&mut t, "pwd", "/Users/me/proj", 0);
        assert_eq!(b.cwd.as_deref(), Some("/Users/me/proj"));
    }

    #[test]
    fn command_output_start_is_harmless_noop() {
        let mut t = BlockTracker::new();
        t.on_prompt_start();
        t.on_command_start("c".to_string());
        t.on_command_output_start();
        t.on_print('x');
        t.on_command_end(0);
        assert_eq!(t.blocks().last().unwrap().output, "x");
    }
}
