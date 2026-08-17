// Block metadata and its cohesive shell-integration state machine.
//! Command blocks — the metadata layer (v0.4 "Fabric", phase 1).
//!
//! A [`Block`] binds one shell command to its output, exit code, and timing.
//! OSC 133 shell-integration markers drive blocks: the shell emits `133;A`
//! (prompt start), `133;B` (command start / preexec), `133;C` (output start),
//! and `133;D;<exit>`; [`BlockTracker`] turns that stream into finished blocks.
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

use std::collections::HashSet;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::SystemTime;

pub mod annotations;
mod continuation;
mod cursor_capture;
pub mod export;
mod output_capture;
mod screen_capture;

#[cfg(test)]
mod screen_capture_tests;
mod semantic;
mod style;

pub(crate) use output_capture::OutputCapture;
pub(crate) use screen_capture::{
    extract_owned_pushed_rows, line_boundary_at_or_before, styled_lines_from, styled_lines_in,
};
pub use semantic::{
    classify_block, classify_line, OutputSemanticRole, SemanticLine, SemanticOutput, SemanticSpan,
    MAX_SEMANTIC_SPANS_PER_BLOCK, MAX_SEMANTIC_SPANS_PER_LINE,
};
pub use style::{
    AttributeSpan, CapturedStyle, CapturedStyleRun, ForegroundSpan, LinkSpan, StyledLine,
    StyledOutput, ANSI_ATTRIBUTE_MASK, MAX_STYLE_RUNS_PER_BLOCK, MAX_STYLE_RUNS_PER_LINE,
};

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
pub(crate) const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

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
    pub output: Arc<str>,
    pub styled_output: Option<Arc<StyledOutput>>,
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
    /// v1.10.26 (FIX_WRAP_EPOCH_AND_VIEWPORT_KEEP B-1): true when the block's
    /// output is a primary-screen TUI document (`screen_document_start` was
    /// `Some` during the command). Screen-origin lines are hard terminal rows:
    /// a narrower window CLIPS their right edge instead of soft-wrapping them
    /// (a TUI `|]` border must never fold onto the next line). Shell-output
    /// blocks keep soft-wrap semantics. v1.10.26 Batch B review closure (SF-1):
    /// now persisted to SQLite (`screen_origin INTEGER NOT NULL DEFAULT 0`) so
    /// a restored TUI block keeps clip-not-wrap across the Restore path —
    /// otherwise `|]` folding could resurrect after restart.
    pub screen_origin: bool,
}

/// A view of the currently-running command (borrowed from [`BlockTracker`]),
/// for the renderer's live block during CommandExecuting.
pub struct InFlightBlock<'a> {
    pub command: &'a str,
    pub cwd: Option<&'a str>,
    pub output: &'a str,
    /// v1.10.5: borrow the `Arc<StyledOutput>` itself (not the dereferenced
    /// `&StyledOutput`) so callers can cheap-clone the Arc instead of deep-
    /// copying the styled lines on the render hot path.
    pub styled_output: Option<&'a Arc<StyledOutput>>,
    /// v1.10.23: live-output content version (LiveLayoutCache key) — bumped
    /// on every mutation, so version equality ⇔ byte-identical output.
    pub version: u64,
    /// v1.10.26 Batch B review blocker (BL-1): whether the live output is a
    /// primary-screen TUI document (`screen_document_start` was set during the
    /// command). The renderer's live layout splits by this flag: screen-owned
    /// frames clip on a narrow window, ordinary streaming output soft-wraps —
    /// the initial Batch B "live is always screen-origin" clip was a functional
    /// regression for plain commands emitting long lines.
    pub screen_origin: bool,
}

/// Shell-phase state machine driven by OSC 133. v0.4 uses it only to
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
    ids: crate::block_id_sequence::BlockIdSequence,
    // ── in-flight command (between 133;B and 133;D) ──
    pending_command: Option<String>,
    pending_started: Option<SystemTime>,
    /// cwd snapshotted at 133;B (from `current_cwd`), stamped onto the block.
    pending_cwd: Option<String>,
    /// Latest cwd from OSC 7; snapshotted into each new block.
    current_cwd: Option<String>,
    output: OutputCapture,
    styled_output: Option<Arc<StyledOutput>>,
    /// v1.10.23: live-output content version, exposed via
    /// [`in_flight`](Self::in_flight); bumped on every mutation.
    live_output_version: u64,
    /// Absolute document position where primary-screen output begins.
    screen_document_start: Option<u64>,
    /// Session-local primary-screen blocks eligible for replay continuation.
    screen_owned_blocks: HashSet<u64>,
    continuation_candidate: Option<BlockId>,
    continuation_base: Option<Block>,
    /// v1.7.5: Kept at 0 (never advanced after init). Previously this marked
    /// where the current session's blocks begin so `session_blocks()` could
    /// exclude startup-hydrated SQLite history. Restore now shows that history
    /// in the main view (Warp parity), so the field is effectively unused;
    /// `screen_owned_blocks` is the new authority for continuation scoping.
    session_start: usize,
    /// v1.7.6: IDs of blocks loaded via [`load_blocks`] (startup/Restore).
    /// Used by [`session_produced_block_ids`] to distinguish blocks produced
    /// THIS session (eligible for per-tab snapshot persistence) from loaded
    /// history. Without this, saving a tab would re-persist all loaded
    /// global history into the tab's `block_ids`, breaking per-tab isolation.
    loaded_ids: HashSet<u64>,
    /// v1.0 P0-b Layer 2: Block ids whose rendering-relevant state changed
    /// (new block, collapse toggle) since the last [`take_dirty_blocks`].
    /// The renderer can skip unchanged blocks when rebuilding vertices.
    dirty_blocks: HashSet<u64>,
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
            ids: crate::block_id_sequence::BlockIdSequence::new(),
            pending_command: None,
            pending_started: None,
            pending_cwd: None,
            current_cwd: None,
            output: OutputCapture::default(),
            styled_output: None,
            live_output_version: 0,
            screen_document_start: None,
            screen_owned_blocks: HashSet::new(),
            continuation_candidate: None,
            continuation_base: None,
            session_start: 0,
            loaded_ids: HashSet::new(),
            dirty_blocks: HashSet::new(),
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
        self.phase == ShellPhase::CommandExecuting && self.screen_document_start.is_none()
    }

    pub fn begin_screen_owned_output(&mut self, document_start: u64) {
        if self.phase == ShellPhase::CommandExecuting && self.screen_document_start.is_none() {
            self.output.clear();
            self.live_output_version = self.live_output_version.wrapping_add(1);
            self.screen_document_start = Some(document_start);
            self.activate_screen_continuation();
        }
    }

    pub fn screen_document_start(&self) -> Option<u64> {
        self.screen_document_start
    }

    /// All finished blocks (newest last). The history panel / search read this
    /// (includes blocks loaded from SQLite on startup).
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// All blocks visible in the main Warp-style block view. v1.7.5: this now
    /// includes blocks loaded from SQLite on startup/Restore (so users see
    /// their previous session's history), plus blocks created this session.
    /// `screen_owned_blocks` distinguishes the two for continuation logic;
    /// the panel / search use [`blocks`] directly.
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
            self.dirty_blocks.insert(id.0);
        }
    }

    /// v1.0 P0-b Layer 2: Mark a block as needing vertex rebuild.
    pub fn mark_block_dirty(&mut self, id: BlockId) {
        self.dirty_blocks.insert(id.0);
    }

    /// v1.0 P0-b Layer 2: Drain the set of dirty block ids. The renderer calls
    /// this after rebuilding vertices to reset the set for the next frame.
    pub fn take_dirty_blocks(&mut self) -> HashSet<u64> {
        std::mem::take(&mut self.dirty_blocks)
    }

    /// v1.0 P0-b Layer 2: Whether any block is dirty.
    pub fn has_dirty_blocks(&self) -> bool {
        !self.dirty_blocks.is_empty()
    }

    /// The currently-running command (between `133;B` and `133;D`), for the
    /// renderer's live block during CommandExecuting (e.g. an interactive
    /// `sudo su`). `None` when nothing is in flight.
    pub fn in_flight(&self) -> Option<InFlightBlock<'_>> {
        if self.phase != ShellPhase::CommandExecuting {
            return None;
        }
        let command = self
            .continuation_base
            .as_ref()
            .map_or(self.pending_command.as_deref()?, |block| {
                block.command.as_str()
            });
        Some(InFlightBlock {
            command,
            cwd: self
                .continuation_base
                .as_ref()
                .and_then(|block| block.cwd.as_deref())
                .or(self.pending_cwd.as_deref()),
            output: self.output.as_str(),
            styled_output: self.styled_output.as_ref(),
            version: self.live_output_version,
            screen_origin: self.screen_document_start.is_some(),
        })
    }

    /// Load previously-persisted blocks (e.g. on startup from SQLite). They go
    /// straight into the history list (already persisted, so NOT into
    /// `unpersisted`), and `next_id` is advanced past the highest loaded id.
    ///
    /// v1.7.5: 加载的历史 block 现在也通过 `session_blocks()` 暴露给主视图，
    /// 这样启动/Restore 后用户能看到上次的命令记录（与 Warp 行为一致）。
    /// `session_start` 保持为 0，让 `session_blocks()` 返回全部 blocks。
    /// `screen_owned_blocks` 集合保证 continuation 逻辑只匹配本会话产生的 block，
    /// 不会误判加载的历史 block。
    pub fn load_blocks(&mut self, blocks: Vec<Block>) {
        for b in &blocks {
            self.ids.observe(b.id.0);
            self.dirty_blocks.insert(b.id.0);
            self.loaded_ids.insert(b.id.0);
        }
        self.blocks.extend(blocks);
        // session_start 保持为初始值 0，让 session_blocks() 包含加载的历史。
        // 之前这里设为 self.blocks.len() 是为了"避免显示 phantom pre-session
        // history"，但用户期望 Restore 后能看到历史命令记录。
    }

    /// v1.7.6: IDs of blocks produced THIS session only (excludes blocks
    /// loaded via [`load_blocks`] on startup/Restore). Used by the app-layer
    /// `Tab::to_snapshot` to persist per-tab block ownership so each tab
    /// can restore only its own history on next launch.
    pub fn session_produced_block_ids(&self) -> Vec<u64> {
        self.blocks
            .iter()
            .filter(|b| !self.loaded_ids.contains(&b.id.0))
            .map(|b| b.id.0)
            .collect()
    }

    /// Share persisted IDs across tabs; isolated trackers remain local.
    pub fn use_shared_id_allocator(&mut self, next_id: Arc<AtomicU64>) {
        self.ids.share(next_id);
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

    /// v1.0 fix: Force-reset to AtPrompt after Ctrl+C flush.
    ///
    /// When `flush_pty_output()` discards stale PTY output, it may also
    /// discard the OSC 133;A marker the shell emits after an interrupted
    /// command. Without that marker, `phase` stays `CommandExecuting` and
    /// the editor/input box never reappears. This method synthesizes the
    /// `133;A` transition: finalize any in-flight block (no exit code) and
    /// return to `AtPrompt`.
    pub fn reset_to_prompt(&mut self) {
        if self.phase == ShellPhase::CommandExecuting {
            self.finalize(None);
        }
        self.phase = ShellPhase::AtPrompt;
    }

    /// `133;B` — command start (preexec). `command` is the prompt-row text the
    /// caller extracted from the grid. Begins output capture.
    pub fn on_command_start(&mut self, command: String) {
        self.prepare_screen_continuation(&command);
        self.pending_command = Some(command);
        self.pending_started = Some(SystemTime::now());
        self.pending_cwd = self.current_cwd.clone();
        self.output.clear();
        self.styled_output = None;
        self.screen_document_start = None;
        self.live_output_version = self.live_output_version.wrapping_add(1);
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
    /// a command is executing. v1.7.0-A: `style` captures the current VT SGR
    /// attributes so the block preserves program-emitted colors after the live
    /// grid scrolls away.
    pub fn on_print(&mut self, c: char, style: CapturedStyle) {
        if self.is_capturing() {
            self.output.print(c, style, MAX_OUTPUT_BYTES);
            self.live_output_version = self.live_output_version.wrapping_add(1);
        }
    }

    /// v1.0 P1.5-C2: Batch-append a run of printable ASCII bytes to the
    /// in-flight block's output. Avoids per-char `push_capped` method call
    /// overhead — one truncation check + one `extend_from_slice` instead of
    /// N individual `push` calls. No-op unless a command is executing.
    /// v1.7.0-A: `style` captures the current VT SGR attributes for the whole
    /// ASCII run — all bytes share one style since the fast path only fires
    /// when no SGR change occurred mid-run.
    pub fn on_print_ascii_run(&mut self, bytes: &[u8], style: CapturedStyle) {
        if !self.is_capturing() || bytes.is_empty() {
            return;
        }
        self.output.print_ascii(bytes, style, MAX_OUTPUT_BYTES);
        self.live_output_version = self.live_output_version.wrapping_add(1);
    }

    /// Append a newline to the in-flight block's output. No-op unless a command
    /// is executing.
    pub fn on_newline(&mut self) {
        if self.is_capturing() {
            self.output.newline(MAX_OUTPUT_BYTES);
            self.live_output_version = self.live_output_version.wrapping_add(1);
        }
    }

    pub fn on_carriage_return(&mut self) {
        if self.is_capturing() {
            self.output.carriage_return();
            self.live_output_version = self.live_output_version.wrapping_add(1);
        }
    }

    pub fn on_backspace(&mut self) {
        if self.is_capturing() {
            self.output.backspace();
            self.live_output_version = self.live_output_version.wrapping_add(1);
        }
    }

    pub fn on_erase_line(&mut self, mode: u16) {
        if self.is_capturing() {
            self.output.erase_line(mode);
            self.live_output_version = self.live_output_version.wrapping_add(1);
        }
    }

    /// Mirror CSI A/B/E/F (cursor up/down) into the capture buffer so
    /// multi-line progress bars (`ollama pull`, `brew upgrade`) repaint
    /// in place instead of appending new rows on every update.
    pub fn on_move_cursor_rows(&mut self, delta: isize) {
        if self.is_capturing() {
            self.output.move_cursor_rows(delta);
            self.live_output_version = self.live_output_version.wrapping_add(1);
        }
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
                tracker.on_print(ch, CapturedStyle::default());
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
        assert_eq!(block.output.as_ref(), "file_a\nfile_b\n");
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
        t.on_print('x', CapturedStyle::default());
        t.on_newline();
        assert!(!t.is_capturing());

        t.on_command_start("cmd".to_string());
        assert!(t.is_capturing());
        t.on_print('h', CapturedStyle::default());
        t.on_print('i', CapturedStyle::default());
        t.on_command_end(0);

        let block = t.blocks().last().unwrap();
        assert_eq!(block.output.as_ref(), "hi");
    }

    #[test]
    fn newline_becomes_output_newline() {
        let mut t = BlockTracker::new();
        t.on_prompt_start();
        t.on_command_start("c".to_string());
        t.on_print('a', CapturedStyle::default());
        t.on_newline();
        t.on_print('b', CapturedStyle::default());
        t.on_command_end(0);
        assert_eq!(t.blocks().last().unwrap().output.as_ref(), "a\nb");
    }

    #[test]
    fn failed_command_records_nonzero_exit() {
        let mut t = BlockTracker::new();
        let block = run_one(&mut t, "false", "", 1);
        assert_eq!(block.exit_code, Some(1));
    }

    /// v1.10.23 review-blocker B2: absolute/relative column moves mutate the
    /// captured bytes (padded spaces / wide-char overwrite) so they must bump
    /// the live output version — a stale `LiveLayoutCache` would slice the
    /// wrong byte range (non-char-boundary panic on multibyte output).
    #[test]
    fn cursor_column_moves_bump_live_output_version() {
        let mut t = BlockTracker::new();
        t.on_prompt_start();
        t.on_command_start("echo".to_string());
        t.on_print('你', CapturedStyle::default()); // 1 wide char (3 UTF-8 bytes)
        let v0 = t.in_flight().unwrap().version;
        // Column 1 lands inside the wide char → a space is emitted, the byte
        // contents change.
        t.on_set_cursor_column(1);
        assert_eq!(
            t.in_flight().unwrap().version,
            v0 + 1,
            "set_cursor_column mutates output"
        );
        t.on_move_cursor_columns(-1);
        assert_eq!(
            t.in_flight().unwrap().version,
            v0 + 2,
            "move_cursor_columns mutates output"
        );
        // Not capturing: no mutation, no bump.
        t.on_command_end(0);
        t.on_prompt_start();
        assert!(t.in_flight().is_none());
        t.on_set_cursor_column(2);
        t.on_move_cursor_columns(1);
        assert!(t.in_flight().is_none(), "no capture at prompt");
    }

    #[test]
    fn re_prompt_without_end_finalizes_interrupted() {
        let mut t = BlockTracker::new();
        t.on_prompt_start();
        t.on_command_start("sleep 100".to_string());
        t.on_print('z', CapturedStyle::default());
        // User hits Ctrl+C: shell re-prompts with 133;A, no 133;D.
        t.on_prompt_start();

        assert_eq!(t.blocks().len(), 1);
        let block = &t.blocks()[0];
        assert_eq!(block.command, "sleep 100");
        assert_eq!(block.output.as_ref(), "z");
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
            t.on_print('a', CapturedStyle::default());
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
                output: String::new().into(),
                styled_output: None,
                exit_code: Some(0),
                started_at: SystemTime::UNIX_EPOCH,
                finished_at: Some(SystemTime::UNIX_EPOCH),
                collapsed: false,
                screen_origin: false,
            },
            Block {
                id: BlockId(3),
                command: "older".into(),
                cwd: None,
                output: String::new().into(),
                styled_output: None,
                exit_code: Some(0),
                started_at: SystemTime::UNIX_EPOCH,
                finished_at: Some(SystemTime::UNIX_EPOCH),
                collapsed: false,
                screen_origin: false,
            },
        ];
        t.load_blocks(loaded);
        assert_eq!(t.blocks().len(), 2);

        // Next freshly-detected block must not collide with loaded id 7.
        run_one(&mut t, "new", "", 0);
        assert_eq!(t.blocks().last().unwrap().id, BlockId(8));
    }

    #[test]
    fn session_blocks_include_loaded_history_for_restore() {
        // v1.7.5: 启动/Restore 后主视图应显示上次会话的命令记录（与 Warp 一致）。
        // load_blocks 加载的历史 block 现在通过 session_blocks() 暴露给主视图。
        let mut t = BlockTracker::new();
        // Pre-session blocks loaded from SQLite on startup.
        t.load_blocks(vec![Block {
            id: BlockId(1),
            command: "old".into(),
            cwd: None,
            output: String::new().into(),
            styled_output: None,
            exit_code: Some(0),
            started_at: SystemTime::UNIX_EPOCH,
            finished_at: Some(SystemTime::UNIX_EPOCH),
            collapsed: false,
            screen_origin: false,
        }]);
        assert_eq!(t.blocks().len(), 1);
        // v1.7.5: session_blocks() 现在包含加载的历史（之前会排除）。
        assert_eq!(t.session_blocks().len(), 1);
        assert_eq!(t.session_blocks()[0].command, "old");

        // A command run this session is appended after loaded history.
        run_one(&mut t, "ls", "", 0);
        assert_eq!(t.blocks().len(), 2);
        assert_eq!(t.session_blocks().len(), 2);
        assert_eq!(t.session_blocks()[0].command, "old");
        assert_eq!(t.session_blocks()[1].command, "ls");
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
        t.on_print('x', CapturedStyle::default());
        t.on_command_end(0);
        assert_eq!(t.blocks().last().unwrap().output.as_ref(), "x");
    }

    // ── v1.0 P0-b: dirty_blocks tests ──────────────────────────────────

    #[test]
    fn dirty_blocks_empty_on_new_tracker() {
        let mut t = BlockTracker::new();
        assert!(!t.has_dirty_blocks());
        assert!(t.take_dirty_blocks().is_empty());
    }

    #[test]
    fn finalize_marks_block_dirty() {
        let mut t = BlockTracker::new();
        t.on_prompt_start();
        t.on_command_start("ls".to_string());
        t.on_command_end(0);
        assert!(t.has_dirty_blocks());
        let dirty = t.take_dirty_blocks();
        assert_eq!(dirty.len(), 1);
        assert!(dirty.contains(&1));
    }

    #[test]
    fn toggle_collapse_marks_dirty() {
        let mut t = BlockTracker::new();
        t.on_prompt_start();
        t.on_command_start("ls".to_string());
        t.on_command_end(0);
        t.take_dirty_blocks(); // clear
        assert!(!t.has_dirty_blocks());
        t.toggle_collapse(BlockId(1));
        assert!(t.has_dirty_blocks());
        let dirty = t.take_dirty_blocks();
        assert!(dirty.contains(&1));
    }

    #[test]
    fn mark_block_dirty_adds_id() {
        let mut t = BlockTracker::new();
        t.mark_block_dirty(BlockId(42));
        assert!(t.has_dirty_blocks());
        let dirty = t.take_dirty_blocks();
        assert!(dirty.contains(&42));
    }

    #[test]
    fn take_dirty_blocks_clears_set() {
        let mut t = BlockTracker::new();
        t.mark_block_dirty(BlockId(1));
        t.mark_block_dirty(BlockId(2));
        let dirty = t.take_dirty_blocks();
        assert_eq!(dirty.len(), 2);
        assert!(!t.has_dirty_blocks());
    }

    #[test]
    fn load_blocks_marks_all_loaded_dirty() {
        let mut t = BlockTracker::new();
        let b = Block {
            id: BlockId(5),
            command: "x".to_string(),
            cwd: None,
            output: "y".into(),
            styled_output: None,
            exit_code: Some(0),
            started_at: SystemTime::now(),
            finished_at: Some(SystemTime::now()),
            collapsed: false,
            screen_origin: false,
        };
        t.load_blocks(vec![b]);
        assert!(t.has_dirty_blocks());
        let dirty = t.take_dirty_blocks();
        assert!(dirty.contains(&5));
    }

    // ── T1: additional BlockTracker coverage ──────────────────────────

    #[test]
    fn multiple_tabs_block_isolation() {
        // Two independent BlockTrackers maintain their own block lists;
        // assigning an id in one must not collide with the other.
        let mut a = BlockTracker::new();
        let mut b = BlockTracker::new();
        run_one(&mut a, "ls", "a", 0);
        run_one(&mut b, "pwd", "b", 0);

        // Each tracker has exactly one block — no cross-contamination.
        assert_eq!(a.blocks().len(), 1);
        assert_eq!(b.blocks().len(), 1);
        // Both start their id sequence at 1 (independent state machines).
        assert_eq!(a.blocks()[0].id, BlockId(1));
        assert_eq!(b.blocks()[0].id, BlockId(1));
        // Commands don't leak across trackers.
        assert_eq!(a.blocks()[0].command, "ls");
        assert_eq!(b.blocks()[0].command, "pwd");

        // Adding a block to `a` doesn't change `b`.
        run_one(&mut a, "echo more", "more", 0);
        assert_eq!(a.blocks().len(), 2);
        assert_eq!(b.blocks().len(), 1);
    }

    #[test]
    fn clear_command_produces_empty_output_block() {
        // The shell's `clear` command typically produces no captured output
        // (it emits control sequences that the VT parser handles, not printable
        // chars). The block should still record the command name and exit code.
        // We drive the tracker directly (without run_one) so no stray newline
        // is appended to the output.
        let mut t = BlockTracker::new();
        t.on_prompt_start();
        t.on_command_start("clear".to_string());
        // No on_print / on_newline calls — clear produces no printable output.
        t.on_command_end(0);
        let block = t.blocks().last().unwrap();
        assert_eq!(block.command, "clear");
        assert!(block.output.is_empty(), "clear should produce no output");
        assert_eq!(block.exit_code, Some(0));
    }

    #[test]
    fn empty_command_produces_block() {
        // v1.0 fix: an empty command ("") still produces a block. The shell
        // may emit 133;B with an empty command line (e.g. user pressed Enter
        // on an empty prompt). The block should be recorded with an empty
        // command string rather than being silently dropped.
        let mut t = BlockTracker::new();
        let block = run_one(&mut t, "", "some output", 0);
        assert_eq!(block.command, "");
        assert_eq!(block.output.as_ref(), "some output\n");
        assert_eq!(block.exit_code, Some(0));
        assert_eq!(t.blocks().len(), 1);
    }

    /// v1.10.26 Batch B (FIX_WRAP_EPOCH_AND_VIEWPORT_KEEP B-1): a finalized
    /// block whose command took the primary screen (`screen_document_start`
    /// became Some) is marked `screen_origin` so the renderer clips its frame
    /// rows instead of soft-wrapping them. A plain shell command (no screen
    /// document) is not marked.
    #[test]
    fn finalize_marks_screen_origin_only_for_screen_documents() {
        let mut t = BlockTracker::new();

        // Plain shell command — capture never becomes screen-owned.
        t.on_prompt_start();
        t.on_command_start("echo hi".to_string());
        t.on_print_ascii_run(b"hi", CapturedStyle::default());
        t.on_command_end(0);
        assert!(
            !t.blocks().last().unwrap().screen_origin,
            "ordinary shell output must stay soft-wrappable (screen_origin = false)"
        );

        // A TUI command that takes the primary screen.
        t.on_prompt_start();
        t.on_command_start("omp".to_string());
        t.begin_screen_owned_output(0);
        t.replace_screen_output("banner\nsecond line");
        t.on_command_end(0);
        assert!(
            t.blocks().last().unwrap().screen_origin,
            "a screen-owned TUI block must be marked screen_origin"
        );
    }

    #[test]
    fn command_after_clear_does_not_affect_history() {
        // Running `clear` then another command should leave the history with
        // exactly two blocks — the clear block doesn't erase prior history
        // (that's the shell's job, not the BlockTracker's).
        let mut t = BlockTracker::new();
        run_one(&mut t, "echo first", "first\n", 0);
        run_one(&mut t, "clear", "", 0);
        run_one(&mut t, "echo second", "second\n", 0);

        assert_eq!(t.blocks().len(), 3);
        assert_eq!(t.blocks()[0].command, "echo first");
        assert_eq!(t.blocks()[1].command, "clear");
        assert_eq!(t.blocks()[2].command, "echo second");
        // IDs continue to rise monotonically across the clear.
        assert_eq!(t.blocks()[0].id, BlockId(1));
        assert_eq!(t.blocks()[1].id, BlockId(2));
        assert_eq!(t.blocks()[2].id, BlockId(3));
    }
}
