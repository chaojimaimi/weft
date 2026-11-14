//! VT100/VT520 escape sequence parser.
//!
//! Wraps the `vte` crate with a `Terminal` struct that implements
//! `vte::Perform` to translate escape sequences into Grid operations.
// v1.13.8 S2 (zero-behavior file-budget split): `vte::Perform` trait impl
// stays a single block in perform.rs; the osc/csi dispatch bodies and the
// DEC-mode / SGR / print method families live in child modules as `impl
// Terminal` cross-file blocks (screen_exit / kitty_keyboard precedent).
// v1.20.3 hygiene WP1 (1C): the accessor getter/setter family moved verbatim
// to accessors.rs (same-crate child `impl Terminal` block; #[path] is
// relative to vt/ because this file is a mod.rs).
#[path = "accessors.rs"]
mod accessors;
mod actions_csi;
mod actions_osc;
mod actions_print;
mod actions_sgr;
mod attrs;
mod capability;
mod capture_cursor;
// PLAN_B Phase 0 (docs/PLAN_B_phase0.md P0-2): crate-internal differential
// equivalence channel — declared test-only so production builds never see it.
#[cfg(test)]
mod capture_diff;
mod grapheme;
pub(crate) mod kitty_keyboard;
mod modes;
mod osc;
mod osc_guard;
mod perform;
mod render_mode;
mod replies;
mod screen_exit;
// v1.11.3 (PLAN_v1113 §2.1): colon-form SGR group handlers — extracted so
// the facade stays within its architecture-gate budget.
mod sgr_underline;
mod staging;
mod ui_events;
pub use attrs::{Attrs, ShellMarker};
pub use capability::{ScreenOwner, SettleState};
pub use render_mode::TuiRenderMode;
pub use screen_exit::{
    TuiColsKind, PRIMARY_HISTORY_SNAPSHOT_INTERVAL, PRIMARY_SCREEN_EXIT_SETTLE_DELAY,
};
// v1.11.5 (PLAN_v1115 §M1): app-facing events drained via
// `Terminal::take_ui_events()` at the same point as `take_response()`, plus
// the shared caps the app layer re-checks (notify texts, OSC 52 replies).
pub use ui_events::{
    DockProgress, UiEvent, NOTIFY_BODY_MAX, NOTIFY_TITLE_MAX, OSC52_MAX_BYTES, OSC52_READ_REPLY_MAX,
};
// v1.11.5 (PLAN_v1115 §M3): OSC 52 read-answer byte builder (None = too
// large, answer as deny) — app-side writes it straight to the pane PTY.
pub use osc::osc52_read_reply;
// Test-only re-export: lib code reaches the hysteresis via `tui_cols_kind`,
// the vt unit tests reach the pure decision + threshold directly.
#[cfg(test)]
pub(crate) use screen_exit::{sustained_alt_cols_kind, SUSTAINED_ALT_COLS_MS};

use std::time::SystemTime;

use crate::blocks::{BlockTracker, OutputCapture};
use crate::editor::Editor;
use crate::grid::{Color, Cursor, CursorStyle, Grid};
use crate::hyperlink::HyperlinkRegistry;
use crate::input::{
    build_submit_bytes, effective_mode, InputMode, MouseProtocol, MouseSuppressFlag,
};
pub const SYNCHRONIZED_OUTPUT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);
/// The terminal: owns a Grid, vte::Parser, and current attributes.
/// Implements `vte::Perform` to translate escape sequences into Grid mutations.
pub struct Terminal {
    grid: Grid,
    parser: vte::Parser,
    attrs: Attrs,
    title: String,
    shell_markers: Vec<ShellMarker>,
    /// Command-block state machine driven by OSC 133 and printed output.
    block_tracker: BlockTracker,
    editor: Editor,
    cwd: Option<String>,
    git_branch: Option<String>,
    /// Editor command awaiting `133;B`; keeps the preexec window passthrough.
    command_from_editor: Option<String>,
    /// v1.20.9 (PLAN_v1209 §1 D1): wall-clock instant of the editor submit.
    /// Lives exactly as long as `command_from_editor` (set together in
    /// `submit_command`, cleared at `133;B` / the `133;A` fallback /
    /// consumed by the orphan `133;D` arm) so an orphan synthesis can date
    /// the block from the Enter keypress instead of the `133;D` arrival —
    /// a PS2 escape (`Ctrl+C` at a continuation prompt, no preexec ever
    /// fired) otherwise reports 0ms for a minute-long wait.
    command_submitted_at: Option<SystemTime>,
    /// FIX_ORPHAN_PARSE_ERROR_OUTPUT: bytes printed between editor submission
    /// and `133;B` (ZLE repaints, or a whole parse-error report when zsh
    /// rejects the line before preexec). Routed here because the in-flight
    /// capture gate requires `phase == CommandExecuting`, which does not hold
    /// before `133;B`. A normal `133;B` discards it (the ZLE echo must never
    /// enter a block); an orphan `133;D` (B missing — parse error) becomes a
    /// synthesized block's output via `BlockTracker::on_orphan_command_end`.
    preexec_staging: OutputCapture,
    pub bracketed_paste: bool,
    /// Origin mode (DECOM, CSI ?6h/l).
    origin_mode: bool,
    /// Cursor visibility (DECTCEM, CSI ?25h/l).
    pub cursor_visible: bool,
    /// Cursor style (DECSCUSR, CSI <n> q).
    pub cursor_style: CursorStyle,
    /// Start of a DEC 2026 atomic update; stale frames expire automatically.
    synchronized_output_started: Option<std::time::Instant>,
    synchronized_frame_cleared_rows: usize,
    /// 256-color palette (indexed colors for SGR 38;5 / 48;5).
    palette: [Color; 256],
    /// v1.10.12: Theme background, answered on OSC 11 queries.
    background_color: Color,
    /// Alternate screen buffer, swapped with `grid` for DEC 1049/47.
    alt_grid: Grid,
    /// Stashed primary cursor, restored on alt-screen exit (DEC 1049).
    saved_cursor: Option<Cursor>,
    /// Bytes written back for DA/DSR/size and capability queries.
    pending_output: Vec<u8>,
    /// v1.11.5 (PLAN_v1115 §M1): app-facing events discovered during parsing
    /// (OSC 52 clipboard / OSC 9+777 notify / OSC 9;4 progress). Drained by
    /// `take_ui_events()` at the same drain point as `take_response()`.
    ui_events: Vec<UiEvent>,
    /// Active OSC 8 hyperlink; printed cells are tagged in the side-map.
    active_hyperlink_id: Option<u32>,
    /// OSC 8 hyperlink registry — maps cell coords → id → URL. External to
    /// the `Cell` struct so Cell stays at 24 bytes (only the 1-bit HYPERLINK
    /// flag lives on the cell).
    hyperlinks: HyperlinkRegistry,
    parser_in_ground_state: bool, // gates the printable-ASCII fast path
    suppress_joined_scalar: bool,
    /// Single source of truth for capability + primary-screen lifecycle state.
    /// See `capability.rs` for the field-by-field rationale.
    pub(in crate::vt) capabilities: capability::CapabilityFlags,
    /// v1.10.26 Batch D (D-2): monotonic DEC 1049/47 alt-screen flip counter.
    /// The app layer (tab/lifecycle.rs) diffs this across a `process()` batch
    /// to detect alt toggles — a batch-internal h→l pair nets the `alt_active`
    /// boolean to zero but still counts as two flips here, so the debounce /
    /// burst windows can't be silently skipped by even-count batches.
    alt_flip_count: u64,
    /// In-flight DCS query collector (`DCS + q ...` XTGETTCAP, `DCS $ q
    /// ...` DECRQSS since v1.11.3, PLAN_v1113 §2.2), payload capped at 1KiB
    /// so an introducer without ST cannot grow memory without bound
    /// (FIX_TERMINAL_CAPABILITY_HARDENING, review M1).
    dcs_query: replies::DcsQueryCollector,
    /// v1.11.2 X1 (PLAN_v1112 §3): OSC accumulation guard observer state.
    /// Shadow-mirrors vte's OscString state across `process()` batches so an
    /// unterminated OSC can be cut off at [`osc_guard::OSC_GUARD_PAYLOAD_CAP`]
    /// instead of growing vte's internal buffer forever.
    osc_watch: osc_guard::OscWatch,
    /// "Last Ground byte was ESC" bit for [`Self::osc_watch`]. Separate field
    /// per the plan's two-field state layout.
    osc_watch_prev_esc: bool,
    /// Guard enable switch. Default ON; test-only off mode is the
    /// pre-guard behavior regression anchor (differential harness in
    /// fuzz_lite.rs).
    osc_guard_enabled: bool,
    /// v1.11.4 (PLAN_v1114 §1): kitty keyboard-protocol negotiation state —
    /// per-screen (main/alt) flag stacks; `keyboard_protocol_flags()` feeds
    /// the app's InputHandler encoder.
    kitty: kitty_keyboard::KittyKeyboardState,
    /// v1.11.4 (PLAN_v1114 §3): master switch — `[compat] kitty_keyboard`
    /// (default true). `false` swallows all four `CSI ...u` ops AND forces
    /// `keyboard_protocol_flags()` to 0 — a one-click rollback.
    kitty_protocol_enabled: bool,
    /// v1.11.7 (PLAN_v1117_SHADOW_BLOCK_VIEW §三 M1.2, D-d): primary-screen
    /// TUI render tier. `Terminal::new` defaults to [`TuiRenderMode::Classic`]
    /// so the ~40 existing `show_block_view` assertions and the 2750-test
    /// baseline stay green untouched; the app injects the config's
    /// `[experimental] tui_render_mode` (serde default `noninteractive`) at
    /// every construction site (P2-3).
    tui_render_mode: TuiRenderMode,
    /// v1.19.0 (D10): live block draws from grid row regions (app injects; core default false).
    live_block_grid_render: bool,
    /// v1.11.15 (FIX A, PLAN_v11115 §1.2): the pane's reader-side mouse
    /// suppression flag. The reader sets it on the first mouse-disable byte;
    /// THIS parser is the authoritative undo (h and l alike). `None` until
    /// the app injects it — clears are then no-ops.
    mouse_suppress: Option<MouseSuppressFlag>,
    /// WP1 D1: test-only fast-lane kill switch (plan §3 — no production
    /// flag; prod builds fold `print_fast_lanes_enabled()` to `true`).
    #[cfg(test)]
    print_fast_lanes_for_test: bool,
    /// WP1 D1: shadow of vte's out-of-band UTF-8 collector (utf8parse
    /// 0.2.2), `(remaining, next_min, next_max)`. While collecting, vte
    /// SWALLOWS every aborting byte (one U+FFFD, never reprocessed) — both
    /// print lanes must disengage or an ASCII byte after a truncated
    /// sequence would print instead (a live v1.0 ASCII-lane divergence the
    /// WP1 harness caught). See `note_slow_path_utf8_byte` in
    /// actions_print.rs; `remaining == 0` = idle, lanes may engage.
    utf8_collect_rem: u8,
    utf8_collect_min: u8,
    utf8_collect_max: u8,
}

impl Terminal {
    pub fn new(rows: usize, cols: usize) -> Self {
        Self::with_scrollback(rows, cols, 10_000)
    }
    /// Construct with a configured scrollback capacity (lines). The alternate
    /// screen always has zero scrollback.
    pub fn with_scrollback(rows: usize, cols: usize, scrollback_lines: usize) -> Self {
        Self {
            grid: Grid::with_scrollback(rows, cols, scrollback_lines),
            parser: vte::Parser::new(),
            attrs: Attrs::default(),
            title: String::new(),
            shell_markers: Vec::new(),
            block_tracker: BlockTracker::new(),
            editor: Editor::new(),
            cwd: None,
            git_branch: None,
            command_from_editor: None,
            command_submitted_at: None,
            preexec_staging: OutputCapture::default(),
            bracketed_paste: false,
            origin_mode: false,
            cursor_visible: true,
            cursor_style: CursorStyle::Block,
            synchronized_output_started: None,
            synchronized_frame_cleared_rows: 0,
            palette: Color::standard_palette(),
            background_color: Color::DEFAULT_BG,
            // Alt-screen apps manage their own scrolling and history.
            alt_grid: Grid::with_scrollback(rows, cols, 0),
            saved_cursor: None,
            pending_output: Vec::new(),
            ui_events: Vec::new(),
            active_hyperlink_id: None,
            hyperlinks: HyperlinkRegistry::new(),
            parser_in_ground_state: true,
            suppress_joined_scalar: false,
            capabilities: capability::CapabilityFlags::default(),
            alt_flip_count: 0,
            dcs_query: replies::DcsQueryCollector::default(),
            osc_watch: osc_guard::OscWatch::default(),
            osc_watch_prev_esc: false,
            // v1.11.2 X1: guard defaults ON for every production constructor.
            osc_guard_enabled: true,
            kitty: kitty_keyboard::KittyKeyboardState::default(),
            kitty_protocol_enabled: true,
            tui_render_mode: TuiRenderMode::Classic,
            live_block_grid_render: false,
            mouse_suppress: None,
            #[cfg(test)]
            print_fast_lanes_for_test: true,
            utf8_collect_rem: 0,
            utf8_collect_min: 0x80,
            utf8_collect_max: 0xBF,
        }
    }

    /// v1.11.2 X1 (PLAN_v1112 §3.2): construct with an explicit OSC guard
    /// switch. `false` yields the exact pre-guard behavior — the regression
    /// anchor used by the fuzz_lite differential harness.
    pub fn with_osc_guard(
        rows: usize,
        cols: usize,
        scrollback_lines: usize,
        enabled: bool,
    ) -> Self {
        let mut term = Self::with_scrollback(rows, cols, scrollback_lines);
        term.osc_guard_enabled = enabled;
        term
    }

    /// Toggle the OSC accumulation guard at runtime (tests / rollback path).
    pub fn set_osc_guard(&mut self, enabled: bool) {
        self.osc_guard_enabled = enabled;
    }

    /// v1.11.2 X4 (PLAN_v1112 §1.2): configure this terminal's block-history
    /// retention cap (`[blocks] retained_limit`). `0` disables retention.
    pub fn set_blocks_retained_limit(&mut self, limit: usize) {
        self.block_tracker_mut().set_retained_limit(limit);
    }

    /// Whether DEC synchronized-output mode (`CSI ?2026h`) is active.
    pub fn synchronized_output(&self) -> bool {
        self.synchronized_output_at(std::time::Instant::now())
    }
    fn synchronized_output_at(&self, now: std::time::Instant) -> bool {
        self.synchronized_output_started.is_some_and(|started| {
            now.saturating_duration_since(started) < SYNCHRONIZED_OUTPUT_TIMEOUT
        })
    }

    /// Coarse classification of who owns the viewport right now — a diagnostic
    /// snapshot suitable for tracing. See `capability::ScreenOwner`.
    pub fn screen_owner(&self) -> capability::ScreenOwner {
        self.capabilities.screen_owner(self.block_tracker.phase())
    }

    /// Lifecycle phase of a deferred primary-screen exit — a diagnostic
    /// snapshot suitable for tracing. See `capability::SettleState`.
    pub fn settle_state(&self) -> capability::SettleState {
        self.capabilities.settle_state(std::time::Instant::now())
    }

    /// R1-5: Whether the history-snapshot rate-limit window has elapsed,
    /// surfaced for the diagnostic tracing layer. `false` when history
    /// browsing is inactive. See `capability::CapabilityFlags::history_snapshot_due`.
    pub fn history_snapshot_due(&self) -> bool {
        self.capabilities
            .history_snapshot_due(std::time::Instant::now())
    }

    pub fn attrs(&self) -> &Attrs {
        &self.attrs
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn shell_markers(&self) -> &[ShellMarker] {
        &self.shell_markers
    }

    /// The command-block tracker (history of finished blocks + live phase).
    pub fn block_tracker(&self) -> &BlockTracker {
        &self.block_tracker
    }

    /// Mutable access to the block tracker (e.g. to drain unpersisted blocks
    /// or load history at startup).
    pub fn block_tracker_mut(&mut self) -> &mut BlockTracker {
        &mut self.block_tracker
    }

    /// Feed raw bytes from PTY through the vte parser.
    /// Each byte is advanced through the parser, which calls back
    /// into our Perform implementation.
    ///
    /// v1.0 perf: ASCII fast path scans runs of printable ASCII (0x20..=0x7E)
    /// and writes them directly via `print_ascii_run`, bypassing vte's
    /// per-byte state machine. Only ESC (0x1B) and C0 controls go through
    /// `parser.advance()`. See `parser_in_ground_state` and inline notes.
    /// WP1 D1: a second lane does the same for consecutive D3-admitted
    /// width-2 UTF-8 chars via `print_wide_run` — same-source guards,
    /// conservative scan that never consumes malformed or cross-batch
    /// partial sequences (see `scan_wide_run`).
    pub fn process(&mut self, bytes: &[u8]) {
        self.block_tracker.begin_batch(std::time::Instant::now());
        let mut parser = std::mem::take(&mut self.parser);
        let mut i = 0;
        while i < bytes.len() {
            // Fast path: only when parser is in ground state AND the OSC
            // guard observer is in Ground (v1.11.2 X1 — inside an OSC even
            // printable payload must reach the slow path so the guard can
            // count it). Scan a run of printable ASCII (0x20..=0x7E) that
            // doesn't start with an escape/C0 control. These bytes map 1:1 to
            // chars and are all width-1, so they bypass vte entirely.
            if self.print_fast_lanes_enabled()
                && self.osc_guard_fast_path_allowed()
                && self.parser_in_ground_state
                && !self.suppress_joined_scalar
                && self.utf8_collect_rem == 0
            {
                let run_start = i;
                while i < bytes.len() && bytes[i] >= 0x20 && bytes[i] <= 0x7E {
                    i += 1;
                }
                if i > run_start {
                    self.print_ascii_run(&bytes[run_start..i]);
                    continue;
                }
                // WP1 D1: UTF-8 wide-char lane — D3 conservative admission;
                // the scan never consumes malformed / batch-truncated bytes
                // (left verbatim for the slow path below). The scanner lends
                // its validated &str straight to the writer (single
                // validation point, rust-review P2-1).
                if let Some(s) = Terminal::scan_wide_run(&bytes[i..]) {
                    self.print_wide_run(s);
                    i += s.len();
                    continue;
                }
            }
            // Slow path: escape sequences, C0 controls, and UTF-8 multi-byte
            // sequences all go through vte byte-by-byte. vte handles partial
            // sequences internally via its state machine.
            if i < bytes.len() {
                let b = bytes[i];
                // v1.11.2 X1 (PLAN_v1112 §3.1): OSC guard decides BEFORE the
                // advance whether this byte reaches vte at all. Over-cap OSC
                // payload is withheld here; terminators always pass so both
                // machines resynchronize.
                if !self.osc_guard_forwards(b) {
                    i += 1;
                    continue;
                }
                // v1.0 P1.5-C3: Only ESC (0x1B) transitions vte OUT of ground
                // state. C0 controls (0x00-0x1F except 0x1B) are "execute"
                // actions that stay in ground per the VT500 state machine, so
                // the ASCII fast path can remain engaged after \n, \r, \t, etc.
                // Previously the fast path was disabled after EVERY \n, forcing
                // the first char of each line through vte's per-byte advance()+
                // print() — ~100k extra advance calls for `seq 1 100000`.
                // If we were already in an escape sequence (ground == false),
                // leaving the flag untouched is safe: vte's print callback
                // restores it to true when the sequence completes.
                if b == 0x1B {
                    self.parser_in_ground_state = false;
                }
                parser.advance(self, b);
                // WP1 D1: mirror the out-of-band UTF-8 collector so the fast
                // lanes know when vte is mid-sequence (see field docs).
                self.note_slow_path_utf8_byte(b);
                i += 1;
            }
        }
        self.parser = parser;
        self.note_primary_screen_exit_activity();
        // D1 sync point: streaming output mutates flat history (scroll-ups);
        // the tail re-materializes the ≤ num_rows window so the &self
        // observers (renderer, a11y) always see a fresh window.
        self.grid.sync_history_window();
    }

    /// v1.11.2 X1: printable-ASCII fast path is allowed only when the OSC
    /// observer sits in Ground (or the guard is off). Inside InOsc/Swallowing
    /// every byte — including plain ASCII payload — must take the slow path.
    fn osc_guard_fast_path_allowed(&self) -> bool {
        !self.osc_guard_enabled || matches!(self.osc_watch, osc_guard::OscWatch::Ground)
    }

    /// v1.11.2 X1: run `b` through the observer FSM; returns false when the
    /// byte must be withheld from vte (over-cap OSC payload only).
    fn osc_guard_forwards(&mut self, b: u8) -> bool {
        if !self.osc_guard_enabled {
            return true;
        }
        osc_guard::observe(&mut self.osc_watch, &mut self.osc_watch_prev_esc, b)
    }

    /// Queue bytes to write back to the PTY (terminal query responses).
    fn respond(&mut self, bytes: &[u8]) {
        self.pending_output.extend_from_slice(bytes);
    }

    /// Take any pending response bytes (DA/DSR/size reports). The app writes
    /// these to the PTY after each `process` batch.
    pub fn take_response(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending_output)
    }

    /// v1.11.5 (PLAN_v1115 §M1): queue an app-facing event discovered during
    /// parsing. Internal — `osc_dispatch` is the only producer.
    pub(crate) fn push_ui_event(&mut self, event: UiEvent) {
        self.ui_events.push(event);
    }

    /// Take any pending app-facing ui events (OSC 52 clipboard access,
    /// OSC 9/777 notifications, OSC 9;4 Dock progress). The app drains this
    /// at the same point as `take_response()`. `reset()` replaces the whole
    /// object, so RIS (ESC c) automatically clears the queue.
    pub fn take_ui_events(&mut self) -> Vec<UiEvent> {
        std::mem::take(&mut self.ui_events)
    }

    /// Resize the terminal grid (and alternate screen to match).
    ///
    /// The **active** grid (whichever is currently displayed, i.e. `self.grid`
    /// after any alt-screen swap) is resized **dimension-only** when an
    /// full-screen TUI app is running or a deferred screen transcript is still
    /// live. Before TUI ownership is confirmed, ordinary command output still
    /// reflows and its frozen candidate boundary is mapped through that reflow.
    /// A hidden owned primary grid is also kept dimension-only while an
    /// alternate screen is visible.
    ///
    /// Why dimension-only for the active TUI grid: apps like `less`, `vim`,
    /// `man` and Claude paint with absolute cursor positioning at a fixed width and
    /// repaint themselves on SIGWINCH. Reflowing (rewrapping) their content
    /// mid-drag — while SIGWINCH is still debounced and undelivered — moves
    /// their characters to wrong cells, producing the "content squished into
    /// the top-left corner" artifact that only resolves on mouse release (when
    /// SIGWINCH finally fires and the app repaints). Alacritty/Warp apply the
    /// same rule: never reflow the active screen during a TUI app's lifetime.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        let primary_screen_layout_owned = self.capabilities.primary_screen_cursor_ops > 0
            || self.block_tracker.screen_document_start().is_some();
        let primary_screen_candidate_pending =
            self.block_tracker.phase() == crate::blocks::ShellPhase::CommandExecuting;
        if self.capabilities.alt_active {
            // self.grid is the visible alternate screen and is always
            // TUI-owned. The hidden primary grid must also stay
            // dimension-only once primary-screen ownership evidence exists;
            // otherwise reflow resets its logical document positions while
            // the capture boundary still points into that document.
            self.grid.resize_dims(rows, cols);
            if primary_screen_layout_owned {
                self.resize_hidden_primary_screen_dims(rows, cols);
            } else if primary_screen_candidate_pending {
                self.reflow_primary_screen_candidate(rows, cols, true);
            } else {
                self.alt_grid.resize(rows, cols);
            }
        } else if primary_screen_layout_owned {
            // The visible primary grid is cursor-addressed or still owns a
            // deferred exit transcript. Preserve its row coordinates until
            // the app repaints or the snapshot settles.
            self.resize_visible_primary_screen_dims(rows, cols);
            self.alt_grid.resize(rows, cols);
        } else if primary_screen_candidate_pending {
            self.reflow_primary_screen_candidate(rows, cols, false);
            self.alt_grid.resize(rows, cols);
        } else {
            // self.grid IS the primary grid at a shell prompt — reflow so
            // scrollback rewraps. self.alt_grid is hidden (usually empty);
            // reflow is harmless and keeps it consistent.
            self.grid.resize(rows, cols);
            self.alt_grid.resize(rows, cols);
        }
    }

    /// Full terminal reset (RIS / ESC c).
    pub fn reset(&mut self) {
        let rows = self.grid.num_rows;
        let cols = self.grid.num_cols;
        // rust-reviewer v1.11.4 Major-1: RIS clears the negotiated keyboard
        // stacks (via the fresh struct) but must NOT resurrect the user's
        // `[compat] kitty_keyboard = false` switch — an app-sent `ESC c`
        // would otherwise override the config-level kill switch.
        let kitty_enabled = self.kitty_protocol_enabled;
        *self = Self::new(rows, cols);
        self.kitty_protocol_enabled = kitty_enabled;
    }

    /// Snapshot the command line at OSC 133;B (preexec). Real shells emit a
    /// `\n` after the user presses Enter, so the cursor sits on a fresh line
    /// *below* the command — walk up to the nearest non-empty row. This is
    /// best-effort (~80%): it returns the last line of the prompt+command and
    /// is used only as a block title. (Detached output capture, not this, is
    /// the authoritative record.)
    fn snapshot_command_line(&self) -> String {
        let mut row = self.grid.cursor.row;
        loop {
            let text = self.grid.row_text(row);
            if !text.is_empty() || row == 0 {
                return text;
            }
            row -= 1;
        }
    }

    /// Called by the app when the user submits the editor (Enter). Returns the
    /// bytes to write to the PTY. Sets `command_from_editor` so (a) further
    /// input passes through until `133;B`, and (b) `133;B` records the
    /// editor's command rather than the grid snapshot.
    pub fn submit_command(&mut self) -> Vec<u8> {
        let command = self.editor.text();
        // v1.20.7 (PLAN_v1207 §3 D1): prime the region begin anchor at
        // submit — the window between Enter and the shell's `133;B` marker
        // (PS2 continuations, slow shells) has no in-flight block, so the
        // region track renders a pseudo-block (live=None) from this anchor.
        // Sampled BEFORE the empty-Enter branch: the synthesized block's
        // on_command_start clears it again (an empty Enter must not leak an
        // anchor — no preexec fires, so no 133;B/D would ever clear it).
        // Primed UNCONDITIONALLY (`prime_command_begin_position`): the
        // idempotent guard would reject the sample while a screen-owned
        // `133;D` defer still holds `screen_document_start`, and after the
        // settle clears it the pseudo-block would lose its anchor entirely.
        // `133;B` clears both anchors and re-samples from the current
        // cursor (the primed value only matters for marker-less shells);
        // 133;D clears it back (AtPrompt stays anchor-free).
        self.block_tracker.prime_command_begin_position(
            self.grid
                .scrollback
                .position()
                .saturating_add(self.grid.cursor.row as u64),
        );
        // v1.0: Empty Enter — zsh's preexec hook does NOT fire on an empty
        // command, so no 133;B/D markers arrive. We synthesize an empty
        // block here (Warp-style spacer) so the block view preserves a
        // visual gap for blank Enter, matching the UX of history blocks.
        if command.is_empty() {
            self.block_tracker.on_command_start(String::new());
            self.block_tracker.on_command_end(0);
        }
        self.command_from_editor = Some(command.clone());
        // v1.20.9 (PLAN_v1209 §1 D1): same lifecycle as the flag above — the
        // orphan `133;D` arm dates its synthesized block from here (the
        // submit) rather than from the marker arrival, so a PS2 escape
        // reports the real wait instead of 0ms. An empty Enter leaves a
        // stale value behind, but its synthesized block closes its own
        // lifecycle and the staging buffer stays unarmed (a later orphan
        // arm never consumes it); the next `133;A` fallback or submit
        // overwrites it regardless.
        self.command_submitted_at = Some(SystemTime::now());
        let bytes = build_submit_bytes(&command, self.bracketed_paste);
        // Record into in-memory history so ↑/↓ navigation works. This is the
        // only place commands enter the editor's history — without it the
        // history vector stays empty and Up-arrow is a no-op.
        self.editor.push_history(&command);
        self.editor.clear();
        bytes
    }

    /// v1.3 AI integration: programmatically run `command`. Sets the editor
    /// buffer, submits it, and returns the PTY bytes. The AI palette uses
    /// this so an AI-suggested command enters the same block lifecycle as
    /// a user-typed one (133;B marker, BlockTracker, output capture).
    pub fn run_command(&mut self, command: &str) -> Vec<u8> {
        self.editor.buffer.set_text(command);
        self.submit_command()
    }
}

/// Helper: extract a single parameter with a default value.
fn param(params: &vte::Params, idx: usize, default: u16) -> u16 {
    params
        .iter()
        .nth(idx)
        .and_then(|sub| sub.first().copied())
        // vte represents an omitted CSI parameter as zero. ECMA-48 count
        // parameters (CUU/CUD/IL/DL/ICH/DCH/ECH/SU/SD/CHT/CBT) define both
        // omitted Ps and Ps=0 as their command default, normally 1. Commands
        // where zero is meaningful pass `default == 0` and keep it unchanged.
        .filter(|&value| value != 0 || default == 0)
        .unwrap_or(default)
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[cfg(test)]
mod diagnostic_tests;

#[cfg(test)]
mod diagnostic_fixtures;

#[cfg(test)]
mod diagnostic_caret_tests;

// v1.15.3 P1a (PLAN_v1153 §2.2 防线 2): Row.occ property tests — randomized
// mixed streams (print/SGR/ICH/DCH/IL/DL/ED/EL/ECH/DECSTBM/wide/combining)
// asserting occ equals the reverse-scan oracle after every op.
#[cfg(test)]
mod occ_property_tests;

// WP1 (v1.17.0): UTF-8 wide-char fast lane — differential fuzz vs the
// forced-slow-path baseline (cfg(test) kill switch), capture equivalence,
// cap-boundary truncation, malformed UTF-8, resize race, D3 admission set,
// and the D4 full-domain width sweep.
#[cfg(test)]
mod wide_fastpath_tests;
