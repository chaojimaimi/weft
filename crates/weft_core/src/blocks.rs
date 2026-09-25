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
use std::time::{Duration, Instant, SystemTime};

pub mod annotations;
mod continuation;
mod cursor_capture;
pub mod export;
#[cfg(test)]
#[path = "blocks/live_styled_tests.rs"]
mod live_styled_tests;
#[cfg(test)]
mod orphan_finalize_tests;
mod output_capture;
pub mod retention;
#[cfg(test)]
mod retention_tests;
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
    compat_underline_style, encode_underline_style, AttributeSpan, CapturedStyle, CapturedStyleRun,
    ColorSpan, ForegroundSpan, LinkSpan, StyledLine, StyledOutput, ANSI_ATTRIBUTE_MASK,
    MAX_STYLE_RUNS_PER_BLOCK, MAX_STYLE_RUNS_PER_LINE,
};

/// Default cap on captured output to bound memory for commands like
/// `cat huge.log`. Beyond this the capture stops and the block is marked
/// truncated. The grid already holds the full output for display; this only
/// guards the detached snapshot used by search / persistence.
/// v0.9 fix: raised from 64 KiB to 1 MiB (64 KiB was too aggressive for
/// real-world commands, e.g. `for i in $(seq 1 20000); do echo ...; done`
/// hits it at ~3000 lines); a 100-block session = 100 MiB worst case,
/// acceptable for a desktop terminal. Block view layout is independently
/// capped at 2000 visible lines per block in the renderer, so raising this
/// doesn't affect rendering perf.
///
/// PLAN_v11217 §3.5 (T4): configurable per tab via `[blocks] output_cap_mib`
/// (1..=64 MiB). This constant is the DEFAULT and the fallback for paths
/// without tracker access (integration-test mirrors, `OutputCapture::default`
/// metadata); the live value lives in [`BlockTracker::output_cap`].
/// Amplifying it scales per-block memory, live-styled rebuild cost, and
/// SQLite write amplification linearly — see the PLAN_v11217 §3.5 guardrail.
pub const DEFAULT_OUTPUT_CAP: usize = 1024 * 1024;

/// PLAN_v11217 §3.5 (T4): legal `[blocks] output_cap_mib` range. Clamped at
/// BOTH the config-load normalization layer AND
/// [`BlockTracker::set_output_cap`] (the entry point is reachable from
/// live-reload and profile switches, not only the normalized load path —
/// same double-clamp precedent as `apply_scrollback_to_all_panes`).
pub const OUTPUT_CAP_MIN_MIB: usize = 1;
pub const OUTPUT_CAP_MAX_MIB: usize = 64;

/// PLAN_v11217 §3.5 (T4): default `[blocks] output_cap_mib` value — the MiB
/// reading of [`DEFAULT_OUTPUT_CAP`]. Kept as a named constant so the config
/// default and the byte default cannot drift.
pub const OUTPUT_CAP_DEFAULT_MIB: usize = 1;

/// One mebibyte — the unit step for [`OUTPUT_CAP_MIN_MIB`]..=[`OUTPUT_CAP_MAX_MIB`].
const MIB: usize = 1024 * 1024;

/// FIX_LIVE_STYLED_OUTPUT: live styled snapshots are rebuilt at most at this
/// interval; per-char rebuilds would be O(text) on every printed byte.
const LIVE_STYLED_PUBLISH_MIN_INTERVAL: Duration = Duration::from_millis(100);

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
    /// FIX_LIVE_STYLED_OUTPUT: on the plain capture path this is the 100ms-
    /// throttled streaming snapshot (cleared when a rewrite removes all
    /// styles); on screen-owned paths it is the caller-supplied snapshot.
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
    /// M6-a (PLAN_M6 §A-1): shared handle to the capture's rewrite watermark.
    /// Both `LiveLayoutCache::sync` call sites must `take_min_write_offset()`
    /// here and pass the value as the sync's authoritative append guard —
    /// the watermark proves "no byte below the synced boundary was rewritten
    /// since the last consumption". Public like the other fields (detached
    /// test/bench handles use [`InFlightBlock::detached_watermark`]).
    pub min_write_offset: &'a std::sync::atomic::AtomicUsize,
}

impl<'a> InFlightBlock<'a> {
    /// Read and reset the rewrite watermark of the backing capture (take
    /// semantics: the value is consumed and the capture is "pure append"
    /// again). Must be called exactly once per `LiveLayoutCache::sync`, by
    /// the caller passing the value in.
    pub fn take_min_write_offset(&self) -> usize {
        use std::sync::atomic::Ordering;
        self.min_write_offset.swap(usize::MAX, Ordering::Relaxed)
    }

    /// Always-fresh watermark for detached handles (tests/benches construct
    /// an `InFlightBlock` without a backing capture). Swapping `usize::MAX`
    /// for MAX is a no-op, so sharing one static across tests is safe.
    pub fn detached_watermark() -> &'static std::sync::atomic::AtomicUsize {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static DETACHED: AtomicUsize = AtomicUsize::new(usize::MAX);
        debug_assert_eq!(DETACHED.load(Ordering::Relaxed), usize::MAX);
        &DETACHED
    }
}

/// v1.10.34: combined block copy ("Copy Block" context action) — format a
/// block's cwd + command + output as one paste-ready snippet:
///
/// ```text
/// ~/code/weft
/// $ cargo test
/// test result: ok. 2317 passed
/// ```
///
/// Empty parts are skipped (no blank filler lines); the output is appended
/// verbatim. Returns an empty string when all three parts are empty, letting
/// callers decide whether an empty clipboard write makes sense (it doesn't —
/// the caller keeps `clipboard_text` as `None`).
pub fn format_block_for_copy(cwd: Option<&str>, command: &str, output: &str) -> String {
    let mut out = String::new();
    if let Some(cwd) = cwd.filter(|c| !c.is_empty()) {
        out.push_str(cwd);
        out.push('\n');
    }
    if !command.is_empty() {
        out.push_str("$ ");
        out.push_str(command);
        if !output.is_empty() {
            out.push('\n');
        }
    }
    out.push_str(output);
    out
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
    /// FIX_LIVE_STYLED_OUTPUT review M1: the streaming snapshot lives in its
    /// OWN field, never in `styled_output` — that field's only writers remain
    /// the screen-snapshot paths plus finalize, so the finalize fallback
    /// `styled_output.take()` can never resurrect a stale live snapshot (and
    /// persist it) for blocks whose styles were cleared mid-stream. Consumed
    /// exclusively via [`in_flight`](Self::in_flight).
    live_styled_snapshot: Option<Arc<StyledOutput>>,
    /// FIX_LIVE_STYLED_OUTPUT: last instant a live styled snapshot was
    /// published — the throttle anchor for [`Self::maybe_publish_live_styled`].
    /// Reset at command start so the 100ms interval never leaks across
    /// commands.
    last_live_styled_publish: Option<Instant>,
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
    /// v1.11.2 X4 (PLAN_v1112 §1.2): in-memory cap; 0 disables. See retention.rs.
    retained_limit: usize,
    /// PLAN_v11217 §3.5 (T4): per-tracker retained-output cap in BYTES
    /// (default [`DEFAULT_OUTPUT_CAP`], configurable via
    /// `[blocks] output_cap_mib`, legal range OUTPUT_CAP_MIN_MIB..=MAX MiB).
    /// Single source for every bounding site of user-visible retained text:
    /// the in-flight capture prints, the style-rewrite `replace` paths, the
    /// preexec staging / interrupt tails, the screen-history frame drop, and
    /// the derived grid-snapshot text budget. Synced into
    /// `OutputCapture::cap_bytes` by [`Self::set_output_cap`] so the finalize
    /// truncation marker reports the configured value.
    output_cap: usize,
    /// v1.11.2 X4: ids evicted by [`Self::enforce_retention`] — folded into
    /// `session_produced_block_ids()` so eviction never changes restore.
    evicted_ids: HashSet<u64>,
    /// v1.11.2 X4 review Minor-4: ids the USER explicitly paged back in via
    /// the panel's "load older" path. `enforce_retention` never evicts them
    /// (otherwise the next command finalize would immediately drop the page
    /// the user just asked for). Startup hydrate (`load_blocks`) does NOT
    /// pin — only explicit user action grows this set, bounding memory at
    /// `retained_limit + user_pinned` by construction.
    user_pinned_ids: HashSet<u64>,
    /// v1.11.7 (PLAN_v1117_SHADOW_BLOCK_VIEW §三 M1.4, P0-2): the deferred
    /// screen-exit settle window. `defer_screen_command_end` flips `phase` to
    /// AtPrompt immediately (so marker accounting stays coherent), which
    /// would make `in_flight()` return None and hide the live block for the
    /// 200ms settle window — worse than today's flash. Set by
    /// `defer_screen_command_end`, cleared by `finish_deferred_screen_command`
    /// (and by `resume_screen_command`: a nested 133;B cancels the pending
    /// exit), so `in_flight()` keeps serving the live block during the
    /// defer→settle window. `is_capturing()` is unaffected (it still
    /// requires `screen_document_start.is_some()` — false while settling).
    settling: bool,
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
            live_styled_snapshot: None,
            last_live_styled_publish: None,
            live_output_version: 0,
            screen_document_start: None,
            screen_owned_blocks: HashSet::new(),
            continuation_candidate: None,
            continuation_base: None,
            session_start: 0,
            loaded_ids: HashSet::new(),
            dirty_blocks: HashSet::new(),
            retained_limit: retention::DEFAULT_BLOCKS_RETAINED_LIMIT,
            output_cap: DEFAULT_OUTPUT_CAP,
            evicted_ids: HashSet::new(),
            user_pinned_ids: HashSet::new(),
            settling: false,
        }
    }

    /// Current working directory (updated from OSC 7). Snapshotted into the
    /// next block at `133;B` so each block carries the dir it ran in.
    pub fn set_cwd(&mut self, cwd: Option<String>) {
        self.current_cwd = cwd;
    }

    /// v1.12.19 (PLAN_v11217 §3.8 T13b): the configured in-memory retention
    /// cap (0 = retention disabled). Read-side twin of
    /// `set_retained_limit` — the all-tab walk test asserts the config
    /// value reached every pane's tracker.
    pub fn retained_limit(&self) -> usize {
        self.retained_limit
    }

    /// PLAN_v11217 §3.5 (T4): the configured retained-output cap in bytes.
    /// Every bounding site of user-visible retained text reads this — never a
    /// global constant — so `[blocks] output_cap_mib` takes effect per tab.
    pub fn output_cap(&self) -> usize {
        self.output_cap
    }

    /// PLAN_v11217 §3.5 (T4): configure the retained-output cap. `cap` is in
    /// BYTES and is clamped into
    /// `[OUTPUT_CAP_MIN_MIB * MIB, OUTPUT_CAP_MAX_MIB * MIB]` here (the entry
    /// point is reachable from live-reload and profile switches, not only the
    /// normalized config-load path — review P2a double-clamp). Also syncs the
    /// capture's metadata field so the finalize truncation marker reports the
    /// configured value instead of a stale default.
    pub fn set_output_cap(&mut self, cap: usize) {
        self.output_cap = cap.clamp(OUTPUT_CAP_MIN_MIB * MIB, OUTPUT_CAP_MAX_MIB * MIB);
        self.output.set_cap_bytes(self.output_cap);
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
            // FIX_LIVE_STYLED_OUTPUT review m2: a plain-phase snapshot must not
            // survive the screen takeover — the cleared capture no longer
            // matches its line indices, and finalize's screen-snapshot fallback
            // would otherwise persist it.
            self.live_styled_snapshot = None;
            self.last_live_styled_publish = None;
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
    ///
    /// v1.11.7 (PLAN_v1117_SHADOW_BLOCK_VIEW §三 M1.4, P0-2): also `Some`
    /// during the deferred-exit settle window (`settling`), so the live block
    /// never vanishes for the 200ms between `defer_screen_command_end` and
    /// `finish_deferred_screen_command` — the phase already flipped to
    /// AtPrompt at defer time, and without the widened gate the renderer
    /// would flash a naked grid exactly when the handle is closing.
    pub fn in_flight(&self) -> Option<InFlightBlock<'_>> {
        if self.phase != ShellPhase::CommandExecuting && !self.settling {
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
            // FIX_LIVE_STYLED_OUTPUT: plain-path commands read the throttled
            // streaming snapshot; screen-owned commands keep reading the
            // caller-supplied screen-snapshot field (publishes never fire
            // while `is_capturing()` is false, so neither can clobber the
            // other).
            styled_output: self
                .live_styled_snapshot
                .as_ref()
                .or(self.styled_output.as_ref()),
            version: self.live_output_version,
            screen_origin: self.screen_document_start.is_some(),
            min_write_offset: self.output.min_write_offset_handle(),
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
        self.live_styled_snapshot = None;
        // FIX_LIVE_STYLED_OUTPUT: the throttle must not leak across commands —
        // the next command's first printed byte publishes immediately.
        self.last_live_styled_publish = None;
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
            self.output.print(c, style, self.output_cap);
            self.live_output_version = self.live_output_version.wrapping_add(1);
            self.maybe_publish_live_styled(Instant::now());
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
        self.output.print_ascii(bytes, style, self.output_cap);
        self.live_output_version = self.live_output_version.wrapping_add(1);
        self.maybe_publish_live_styled(Instant::now());
    }

    /// Append a newline to the in-flight block's output. No-op unless a command
    /// is executing.
    pub fn on_newline(&mut self) {
        if self.is_capturing() {
            self.output.newline(self.output_cap);
            self.live_output_version = self.live_output_version.wrapping_add(1);
            // Line terminators are rewrite boundaries (spinner frame → "done"
            // line): a snapshot-category flip here publishes immediately, so
            // a de-styled rewrite can't end the stream with stale colors
            // still on the live view.
            self.boundary_publish_live_styled();
        }
    }

    pub fn on_carriage_return(&mut self) {
        if self.is_capturing() {
            self.output.carriage_return();
            self.live_output_version = self.live_output_version.wrapping_add(1);
            self.boundary_publish_live_styled();
        }
    }

    pub fn on_backspace(&mut self) {
        if self.is_capturing() {
            self.output.backspace();
            self.live_output_version = self.live_output_version.wrapping_add(1);
            self.boundary_publish_live_styled();
        }
    }

    pub fn on_erase_line(&mut self, mode: u16) {
        if self.is_capturing() {
            self.output.erase_line(mode);
            self.live_output_version = self.live_output_version.wrapping_add(1);
            self.boundary_publish_live_styled();
        }
    }

    /// Mirror CSI A/B/E/F (cursor up/down) into the capture buffer so
    /// multi-line progress bars (`ollama pull`, `brew upgrade`) repaint
    /// in place instead of appending new rows on every update.
    pub fn on_move_cursor_rows(&mut self, delta: isize) {
        if self.is_capturing() {
            self.output.move_cursor_rows(delta);
            self.live_output_version = self.live_output_version.wrapping_add(1);
            self.maybe_publish_live_styled(Instant::now());
        }
    }

    // ── Live styled publish (FIX_LIVE_STYLED_OUTPUT) ─────────────────────

    /// Whether enough time has elapsed since the last live styled publish to
    /// rebuild the snapshot. Pure — unit-anchored in live_styled_tests.rs.
    fn live_styled_publish_due(last: Option<Instant>, now: Instant) -> bool {
        !last.is_some_and(|t| now.duration_since(t) < LIVE_STYLED_PUBLISH_MIN_INTERVAL)
    }

    /// Publish a styled snapshot of the in-flight output so the live block
    /// renders program-emitted SGR attributes before finalize (Warp-parity:
    /// colors visible while streaming, not only after 133;D). Three states:
    /// runs present → fresh snapshot; run-cap overflow → freeze the LAST good
    /// snapshot instead of flickering to unstyled; runs cleared by an
    /// all-default rewrite (`\r` + reprint) → clear, so the live view drops
    /// stale colors immediately (review m1). Writes only
    /// `live_styled_snapshot` — never `styled_output` (review M1).
    fn publish_live_styled(&mut self) {
        match self.output.peek_styled() {
            Some(styled) => self.live_styled_snapshot = Some(Arc::new(styled)),
            None if self.output.style_overflow() => {}
            None => self.live_styled_snapshot = None,
        }
    }

    fn maybe_publish_live_styled(&mut self, now: Instant) {
        if Self::live_styled_publish_due(self.last_live_styled_publish, now) {
            self.last_live_styled_publish = Some(now);
            self.publish_live_styled();
        }
    }

    /// Boundary publish for rewrite events (`\r`, EL, backspace, newline).
    /// Review round 2 MAJOR: an UNTHROTTLED rebuild here re-introduced the
    /// O(text) cost on every newline — newlines are the highest-frequency
    /// event in streamed output, so a 1 MiB colored stream paid ~6.7s of
    /// main-thread rebuilds. The m1 guarantee only needs immediacy when the
    /// snapshot's CATEGORY flips (first colors appear / all colors cleared),
    /// and that flip is O(1)-detectable from the run counters; Some→Some
    /// refreshes stay on the 100ms throttle like the print paths.
    fn boundary_publish_live_styled(&mut self) {
        let peek_some = !(self.output.style_overflow() || self.output.style_runs_empty());
        let now = Instant::now();
        if peek_some != self.live_styled_snapshot.is_some()
            || Self::live_styled_publish_due(self.last_live_styled_publish, now)
        {
            self.last_live_styled_publish = Some(now);
            self.publish_live_styled();
        }
    }
}

#[cfg(test)]
mod tracker_tests;
