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
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

pub mod annotations;
pub mod continuation;
mod cursor_capture;
pub mod export;
// v1.14.2 (B1 备料, PLAN_v1142 §3): the InFlightBlock view moved
// byte-identical to the child module to buy file-budget headroom; the
// re-export keeps every `blocks::InFlightBlock` path unchanged.
mod in_flight;
// Live-styled publish family lives in the child module (v1.13.9
// file-budget split, pure code motion; `impl BlockTracker` cross-file
// block per the continuation.rs precedent).
mod live_styled_publish;
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
#[cfg(test)]
mod styled_window_tests;

pub use in_flight::InFlightBlock;
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
    /// v1.13.7 F3 (PLAN_v1137): batch-level clock, synced once per
    /// `Terminal::process` call. The live-styled publish throttle only needs
    /// ~ms precision; per-line `Instant::now()` was 2 clock_gettime syscalls
    /// per line over 2M-line floods (~3% of parse). Direct tracker callers
    /// outside `process` (tests, hydrate) see the stale construction clock —
    /// throttle decisions only, no semantic effect (PLAN_v1137 §1 F3).
    pub(crate) batch_clock: Instant,
    /// v1.10.23: live-output content version, exposed via
    /// [`in_flight`](Self::in_flight); bumped on every mutation.
    live_output_version: u64,
    /// v1.13.11 P1-1 (rust-reviewer): per-command generation, bumped at every
    /// [`on_command_start`](Self::on_command_start). The renderer's in-flight
    /// warm watermark keys on this — NOT on `InFlightBlock::started_at`,
    /// which continuation sessions surface from the BASE block and is
    /// therefore not command-unique. Chained commands (`a; b`) can finish
    /// command 1 and start command 2 inside one PTY batch with no
    /// in-between frame; a generation mismatch then forces the sentinel
    /// warm scan to restart at 0 instead of inheriting command 1's
    /// watermark (which would leave command 2's first lines forever
    /// unscanned).
    command_generation: u64,
    /// v1.13.13 FIX-D (PLAN_v1.13.13): bumped by every screen-snapshot
    /// REWRITE that replaces the in-flight capture's text wholesale. Three
    /// sites, one contract: `replace_screen_snapshot` (style.rs,
    /// primary-screen TUI snapshot path), `replace_screen_output` (style.rs,
    /// plain-phase snapshot rewrite; P2-1) and `split_screen_history`
    /// (screen_capture.rs, the >1MiB split branch). Those paths break the
    /// append-only contract the renderer's in-flight warm watermark relies
    /// on: rows below the old watermark may hold rewritten content whose
    /// glyphs would never be rescanned before finalize. The watermark
    /// identity folds this epoch in (atlas_warmup live identity), so a
    /// rewrite restarts the live scan at 0. Deliberately NOT bumped by the
    /// content-equality short-circuit in `replace_screen_snapshot` — idle
    /// TUIs re-publish the same document every 50ms, and an unconditional
    /// bump would restart the warm scan every frame. In-memory only (no
    /// persistence).
    screen_rewrite_epoch: u64,
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
    /// v1.12.24 review P1-2: ids paged in via the panel's GLOBAL "load
    /// older" query ([`load_older_to_front`]). Unlike `loaded_ids`
    /// (restore-owned history that N-3 keeps in the snapshot lineage),
    /// load-older blocks are browsed content — another tab's blocks — so
    /// they are excluded from `session_produced_block_ids` /
    /// `lineage_block_ids` and a restart can never absorb them into this
    /// tab's snapshot. Append-only: load-older pages are PINNED against
    /// eviction (rust-reviewer v1.11.2 Minor-4), so the eviction pop loop
    /// can never visit them.
    load_older_ids: HashSet<u64>,
    /// v1.0 P0-b Layer 2: Block ids whose rendering-relevant state changed
    /// (new block, collapse toggle). v1.12.23 audit batch 2: the polling
    /// accessors (`take_dirty_blocks`/`has_dirty_blocks`/`mark_block_dirty`)
    /// were removed as dead code — the set is now maintained purely by the
    /// inline insert protocol: every production mutation site inserts directly
    /// (`toggle_collapse`/`load_blocks` here, continuation/screen-capture/
    /// retention finalize paths), and retention pops remove ids on eviction.
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
    /// v1.14.2 (B1, PLAN_v1142 §3): generation counter for everything the
    /// TAB BAR TITLE renders from — `in_flight().command` and the OSC 7 cwd.
    /// Bumped at the title's exact input points: `on_command_start`
    /// (in-flight set), `finalize` (in-flight clear; continuation.rs — one
    /// bump covers 133;D, the interrupted-133;A path, the deferred screen
    /// exit AND the orphan path), and `set_cwd` (OSC 7 — without it a `cd`
    /// would freeze the title until the next command boundary). Deliberately
    /// NOT bumped by streaming output (the title never reads it); extra
    /// bumps are always conservative (one extra cached-entry rebuild).
    /// Read per frame by the app's tab-title cache (`tab_title_cache.rs`)
    /// to decide reuse vs rebuild; the counter itself is parsed-worker-
    /// owned (bumped under the tracker lock) so no cross-lock notification
    /// exists.
    title_generation: u64,
    /// v1.15.0 (T3, PLAN_v1150): the panel footer's persistent exhausted
    /// ruling — set by the app's "load older" effect when the store answers
    /// empty, cleared by [`retention::BlockTracker::enforce_retention`] on
    /// ANY eviction (an eviction moves the memory-front, so the old
    /// "no older block in the store" verdict is stale and SQLite history
    /// must become reachable from the footer again). Owned by the tracker
    /// (same object as the blocks it rules about) so multi-pane tabs are
    /// naturally per-pane. In-memory only; never persisted.
    footer_exhausted: bool,
}

// v1.15.2: `Default` + inherent `impl BlockTracker`, moved verbatim
// (zero-rewrite). The struct stays put: sibling impls access its fields.
mod tracker_impl;

#[cfg(test)]
mod tracker_tests;
