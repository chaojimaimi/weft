//! VT100/VT520 escape sequence parser.
//!
//! Wraps the `vte` crate with a `Terminal` struct that implements
//! `vte::Perform` to translate escape sequences into Grid operations.
mod attrs;
mod capability;
mod capture_cursor;
mod grapheme;
mod osc;
mod perform;
mod screen_exit;
pub use attrs::{Attrs, ShellMarker};
pub use capability::{ScreenOwner, SettleState};
pub use screen_exit::{
    TuiColsKind, PRIMARY_HISTORY_SNAPSHOT_INTERVAL, PRIMARY_SCREEN_EXIT_SETTLE_DELAY,
};

use crate::blocks::{BlockTracker, CapturedStyle, ShellPhase};
use crate::editor::Editor;
use crate::grid::{CellColor, CellFlags, CellWidth, Color, Cursor, CursorStyle, Grid};
use crate::hyperlink::HyperlinkRegistry;
use crate::input::{build_submit_bytes, effective_mode, InputMode, MouseProtocol};
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
            active_hyperlink_id: None,
            hyperlinks: HyperlinkRegistry::new(),
            parser_in_ground_state: true,
            suppress_joined_scalar: false,
            capabilities: capability::CapabilityFlags::default(),
            alt_flip_count: 0,
        }
    }

    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    pub fn grid_mut(&mut self) -> &mut Grid {
        &mut self.grid
    }

    /// The 256-color palette. Seeded from the theme, mutable by OSC 4/104.
    /// The renderer resolves `CellColor::Palette(i)` against this each frame,
    /// so theme switches and OSC edits recolor the screen on the next draw.
    pub fn palette(&self) -> &[Color; 256] {
        &self.palette
    }

    /// Reseed the whole palette (used on theme switch). Because cells store
    /// palette *indices*, this recolors every existing `Palette(i)` cell on
    /// the next render — OSC runtime overrides are discarded, matching
    /// "switch theme = reset palette".
    pub fn set_palette(&mut self, palette: [Color; 256]) {
        self.palette = palette;
    }

    pub fn set_background_color(&mut self, color: Color) {
        self.background_color = color;
    }

    pub fn editor(&self) -> &Editor {
        &self.editor
    }

    pub fn editor_mut(&mut self) -> &mut Editor {
        &mut self.editor
    }

    /// Borrow the OSC 8 hyperlink registry (read-only). The renderer uses
    /// this for Cmd+Click URL lookup.
    pub fn hyperlinks(&self) -> &HyperlinkRegistry {
        &self.hyperlinks
    }

    /// Drop all `(row, col) → URL` mappings. Called after a manual
    /// `grid.scroll_offset` change (e.g. FindInGrid scrolling to a match):
    /// the side-map is viewport-relative, so any scroll invalidates it.
    pub fn clear_hyperlink_cell_map(&mut self) {
        self.hyperlinks.clear_cell_map();
    }

    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// Current git branch (from OSC 9;git=<branch>), for the prompt header.
    pub fn git_branch(&self) -> Option<&str> {
        self.git_branch.as_deref()
    }

    /// Effective input routing right now (passthrough vs. editor).
    pub fn effective_input_mode(&self) -> InputMode {
        effective_mode(
            self.block_tracker.phase(),
            self.capabilities.alt_active,
            self.block_tracker.bootstrap_ready(),
            self.command_from_editor.is_some(),
        )
    }

    /// True between editor submission and the shell's OSC 133 `B` marker.
    /// Input arriving in this short window belongs to the launched command,
    /// not to the prompt/block view.
    pub fn command_from_editor_pending(&self) -> bool {
        self.command_from_editor.is_some()
    }

    /// Whether the alternate screen buffer is currently active.
    pub fn is_alt_screen_active(&self) -> bool {
        self.capabilities.alt_active
    }

    /// v1.10.26 Batch D (D-2): total DEC 1049/47 flips ever performed. The
    /// app layer diffs this before/after a `process()` batch to detect
    /// alt-screen toggles even when the batch nets the `alt_active` phase to
    /// zero (an h→l pair counts as two flips).
    pub fn alt_flip_count(&self) -> u64 {
        self.alt_flip_count
    }

    /// Active mouse reporting mode (DEC modes 9/1000/1002/1003).
    pub fn mouse_protocol(&self) -> MouseProtocol {
        self.capabilities.mouse_protocol
    }

    /// Whether SGR-1006 mouse-report encoding is selected (DEC mode 1006).
    pub fn sgr_mouse(&self) -> bool {
        self.capabilities.sgr_mouse
    }

    /// Application cursor key mode (DECCKM, CSI ?1h/l).
    pub fn app_cursor_keys(&self) -> bool {
        self.capabilities.app_cursor_keys
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

    /// Swap the primary and alternate screen buffers (DEC 1049/47).
    ///
    /// `save_cursor_and_clear` distinguishes the two modes:
    /// - `true` (1049): save the primary cursor, clear the alt screen on
    ///   entry, restore the cursor on exit.
    /// - `false` (47): plain swap, no cursor save/restore, no clear.
    ///
    /// Modelled on Alacritty's `swap_alt` (O(1) `mem::swap`) with the
    /// parameterised clear/restore semantics from Warp's `SwapScreen` mode.
    fn swap_alt(&mut self, save_cursor_and_clear: bool) {
        if !self.capabilities.alt_active {
            if save_cursor_and_clear {
                self.saved_cursor = Some(self.grid.cursor.clone());
                self.alt_grid.clear();
            }
            // Alternate screen starts fresh with the cursor at home (0,0).
            self.alt_grid.cursor = Cursor::default();
            std::mem::swap(&mut self.grid, &mut self.alt_grid);
            self.capabilities.alt_active = true;
            // OSC 8 state is viewport-relative — entering the alt screen
            // invalidates any cell_map entries from the primary grid.
            self.hyperlinks.clear_cell_map();
            self.active_hyperlink_id = None;
        } else {
            std::mem::swap(&mut self.grid, &mut self.alt_grid);
            if save_cursor_and_clear {
                if let Some(c) = self.saved_cursor.take() {
                    self.grid.cursor = c;
                }
            }
            self.capabilities.alt_active = false;
            // v1.10.12: leaving the alt screen ends any in-progress history
            // peek so show_block_view() reverts to the primary-screen rules
            // and the renderer shows the restored primary grid, not a stale
            // BlockView overlay.
            //
            // v1.10.19: kept unconditional — the resize feedback loop that
            // used to hammer this path (primary-screen TUI SIGWINCH repaints
            // toggling DEC 1049 every ~130ms, each toggle clearing the peek
            // and yanking a scrolled-up history back to the grid) is broken
            // at the source (stable cols + winsize dedup + rescale debounce),
            // so the only alt exits here are real ones, where clearing the
            // peek is correct (see alt_screen_exit_clears_history_peek).
            self.capabilities.alt_screen_history_peek = false;
            // Restoring the primary grid — alt-screen hyperlinks are gone.
            self.hyperlinks.clear_cell_map();
            self.active_hyperlink_id = None;
        }
        tracing::info!(
            active = self.capabilities.alt_active,
            rows = self.grid.num_rows,
            cols = self.grid.num_cols,
            "alt-screen toggled"
        );
        // v1.10.26 Batch D (D-2): count every real flip for the app-layer
        // batch-diff detection and burst-storm signature.
        self.alt_flip_count = self.alt_flip_count.wrapping_add(1);
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
    pub fn process(&mut self, bytes: &[u8]) {
        let mut parser = std::mem::take(&mut self.parser);
        let mut i = 0;
        while i < bytes.len() {
            // Fast path: only when parser is in ground state. Scan a run
            // of printable ASCII (0x20..=0x7E) that doesn't start with an
            // escape/C0 control. These bytes map 1:1 to chars and are all
            // width-1, so they bypass vte entirely.
            if self.parser_in_ground_state && !self.suppress_joined_scalar {
                let run_start = i;
                while i < bytes.len() && bytes[i] >= 0x20 && bytes[i] <= 0x7E {
                    i += 1;
                }
                if i > run_start {
                    self.print_ascii_run(&bytes[run_start..i]);
                    continue;
                }
            }
            // Slow path: escape sequences, C0 controls, and UTF-8 multi-byte
            // sequences all go through vte byte-by-byte. vte handles partial
            // sequences internally via its state machine.
            if i < bytes.len() {
                let b = bytes[i];
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
                i += 1;
            }
        }
        self.parser = parser;
        self.note_primary_screen_exit_activity();
    }

    /// v1.0 perf: Bulk-write a run of printable ASCII bytes (0x20..=0x7E)
    /// directly to the grid, bypassing vte's per-byte state machine.
    ///
    /// v1.0 P1.5-C2: Rewritten to process one row at a time instead of
    /// per-char, eliminating several sources of per-char overhead:
    /// - **Hyperlink check**: skipped entirely when no active hyperlink AND
    ///   the cell_map is empty (99.9% of output). Old code called
    ///   `unlink_cell` (HashMap::remove) for every char.
    /// - **Dirty marking**: once per row segment, not per cell.
    /// - **Bounds check**: computed once per row (`remaining_in_row`), not
    ///   checked per char.
    /// - **Block capture**: batched via `on_print_ascii_run` (one
    ///   `push_str` instead of N `push` calls).
    /// - **Cursor access**: `cursor.col` updated once per row, not per char.
    fn print_ascii_run(&mut self, bytes: &[u8]) {
        debug_assert!(!bytes.is_empty());
        let phase = self.block_tracker.phase();
        // Snap back to live viewport for new content — same gate as print().
        // Skipped while the user is browsing primary-screen TUI history so a
        // redraw cannot yank the viewport back to the live bottom.
        if phase != ShellPhase::AtPrompt && !self.primary_history_view() {
            self.grid.scroll_offset = 0;
        }
        let capturing = !self.capabilities.alt_active && phase == ShellPhase::CommandExecuting;
        let num_cols = self.grid.num_cols;
        let num_rows = self.grid.num_rows;
        let fg = self.attrs.fg;
        let bg = self.attrs.bg;
        let base_flags = self.attrs.flags | CellFlags::DIRTY;

        // v1.0 P1.5-C2: Determine hyperlink handling mode once.
        // - `has_hyperlink`: active OSC 8 → every cell gets linked.
        // - `need_unlink_check`: no active link, but old cells might have
        //   HYPERLINK flag → need to check + unlink (rare).
        // - Neither: skip ALL hyperlink logic (fast path, 99.9% of output).
        let has_hyperlink = self.active_hyperlink_id.is_some();
        let need_unlink_check = !has_hyperlink && !self.hyperlinks.cell_map_is_empty();

        let mut offset = 0;
        while offset < bytes.len() {
            // Handle deferred wrap (same as print()) — once per row boundary.
            if self.grid.cursor.wrap_pending {
                self.grid.cursor.wrap_pending = false;
                self.grid.cursor.col = 0;
                let (_, bottom) = self.grid.scroll_region();
                if self.grid.cursor.row == bottom {
                    self.scroll_grid_up(1);
                } else if self.grid.cursor.row < num_rows - 1 {
                    self.grid.cursor.row += 1;
                }
                if self.grid.cursor.row > 0 {
                    self.grid.viewport[self.grid.cursor.row - 1].wrapped = true;
                }
            }

            let col = self.grid.cursor.col;

            // v1.0 P1.5-C2: resize race — cursor.col may be >= num_cols after
            // a narrowing resize. Reset to col 0 and advance row (same as the
            // old per-char bounds check, but done once per row boundary).
            let col = if col >= num_cols {
                self.grid.cursor.wrap_pending = false;
                self.grid.cursor.col = 0;
                let (_, bottom) = self.grid.scroll_region();
                if self.grid.cursor.row == bottom {
                    self.scroll_grid_up(1);
                } else if self.grid.cursor.row < num_rows - 1 {
                    self.grid.cursor.row += 1;
                }
                let new_row = self.grid.cursor.row;
                if new_row > 0 {
                    self.grid.viewport[new_row - 1].wrapped = true;
                }
                0
            } else {
                col
            };
            // Read row AFTER the col adjustment (cursor.row may have changed).
            let row = self.grid.cursor.row;
            self.prepare_primary_screen_exit_row_overwrite();
            self.include_primary_screen_viewport_row(row);

            // How many bytes fit in the current row? No per-char bounds check.
            let remaining_in_row = num_cols - col;
            let remaining_bytes = bytes.len() - offset;
            let count = remaining_in_row.min(remaining_bytes);
            let chunk = &bytes[offset..offset + count];

            // Batch capture to block tracker — one push_str instead of N pushes.
            // v1.7.0-A: capture the current VT SGR attrs for the whole ASCII
            // run — all bytes share one style since the fast path only fires
            // when no SGR change occurred mid-run.
            if capturing {
                let style =
                    CapturedStyle::from_attrs(self.attrs.fg, self.attrs.bg, self.attrs.flags);
                self.block_tracker.on_print_ascii_run(chunk, style);
            }
            {
                let style =
                    CapturedStyle::from_attrs(self.attrs.fg, self.attrs.bg, self.attrs.flags);
                self.capture_primary_screen_interrupt_ascii(chunk, style);
            }

            // The contiguous ASCII overwrite can only split a pre-existing
            // wide glyph at its two boundaries. Pairs fully inside the range
            // are overwritten together, so repairing the first and last cell
            // preserves the row invariant without adding per-cell overhead.
            self.grid.viewport[row].clear_wide_pair_at(col);
            self.grid.viewport[row].clear_wide_pair_at(col + count - 1);

            // Write cells — tight inner loop, no per-cell wrap/bounds check.
            {
                let cells = &mut self.grid.viewport[row].cells;
                if has_hyperlink {
                    let id = self.active_hyperlink_id.unwrap();
                    let link_flags = base_flags | CellFlags::HYPERLINK;
                    for (i, &b) in chunk.iter().enumerate() {
                        let c = col + i;
                        cells[c].character = b as char;
                        cells[c].fg = fg;
                        cells[c].bg = bg;
                        cells[c].flags = link_flags;
                        cells[c].width = CellWidth::Half;
                    }
                    // Batch-link all cells at once (borrow released).
                    for i in 0..count {
                        self.hyperlinks.link_cell(row, col + i, id);
                    }
                    // v1.6.1: also write to RowExtras for persistence/scrollback.
                    // Hyperlinks are rare (OSC 8 active), so per-cell BTreeMap
                    // insert is acceptable here — the hot ASCII fast path below
                    // never touches extras.
                    // v1.6.0 review C1: also clear orphaned grapheme extras for
                    // the overwritten range (ASCII overwrites don't extend clusters).
                    {
                        let extras = &mut self.grid.viewport[row].extras;
                        extras.clear_grapheme_range(col, col + count);
                        for i in 0..count {
                            extras.set_hyperlink(col + i, Some(id));
                        }
                    }
                } else if need_unlink_check {
                    // Slow path: some cells might have old hyperlinks to clean.
                    // Collect positions first, then unlink after writing.
                    let mut to_unlink: [usize; 128] = [0; 128];
                    let mut unlink_n = 0;
                    for (i, &b) in chunk.iter().enumerate() {
                        let c = col + i;
                        if cells[c].flags.contains(CellFlags::HYPERLINK) {
                            to_unlink[unlink_n] = c;
                            unlink_n += 1;
                        }
                        cells[c].character = b as char;
                        cells[c].fg = fg;
                        cells[c].bg = bg;
                        cells[c].flags = base_flags;
                        cells[c].width = CellWidth::Half;
                    }
                    for &c in to_unlink.iter().take(unlink_n) {
                        self.hyperlinks.unlink_cell(row, c);
                    }
                    // v1.6.1: clear extras for unlinked cells too.
                    // v1.6.0 review C1: also clear orphaned grapheme extras
                    // for the full overwritten range (not just unlinked cells).
                    {
                        let extras = &mut self.grid.viewport[row].extras;
                        for &c in to_unlink.iter().take(unlink_n) {
                            extras.set_hyperlink(c, None);
                        }
                        extras.clear_grapheme_range(col, col + count);
                    }
                } else {
                    // Fast path: no hyperlink logic at all.
                    for (i, &b) in chunk.iter().enumerate() {
                        let c = col + i;
                        cells[c].character = b as char;
                        cells[c].fg = fg;
                        cells[c].bg = bg;
                        cells[c].flags = base_flags;
                        cells[c].width = CellWidth::Half;
                    }
                    // v1.6.0 review C1: clear orphaned grapheme extras for the
                    // overwritten range. Common case: extras is empty (no
                    // multi-scalar clusters on this row) → single is_empty()
                    // check, zero per-cell cost.
                    let extras = &mut self.grid.viewport[row].extras;
                    if !extras.is_empty() {
                        extras.clear_grapheme_range(col, col + count);
                    }
                }
            }

            // Mark dirty once for the whole row segment — was per-cell.
            self.grid.viewport[row].mark_dirty(col + count - 1);

            self.grid.cursor.col += count;
            offset += count;

            // Handle end-of-row wrap.
            if self.grid.cursor.col >= num_cols {
                self.grid.cursor.wrap_pending = true;
                self.grid.cursor.col = num_cols - 1;
            }
        }
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
        *self = Self::new(rows, cols);
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
        // v1.0: Empty Enter — zsh's preexec hook does NOT fire on an empty
        // command, so no 133;B/D markers arrive. We synthesize an empty
        // block here (Warp-style spacer) so the block view preserves a
        // visual gap for blank Enter, matching the UX of history blocks.
        if command.is_empty() {
            self.block_tracker.on_command_start(String::new());
            self.block_tracker.on_command_end(0);
        }
        self.command_from_editor = Some(command.clone());
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

    // ── SGR helpers ──────────────────────────────────────────────

    /// Handle SGR (Select Graphic Rendition) — CSI m.
    ///
    /// vte parses `CSI 38;5;196m` as three separate param groups:
    ///   iter → [38], [5], [196]
    /// We flatten all sub-param first-values into a `Vec<u16>`, then walk it
    /// with an index so we can consume 1–4 values for color sequences.
    fn handle_sgr(&mut self, params: &vte::Params) {
        if params.is_empty() {
            self.attrs = Attrs::default();
            return;
        }

        // v1.0 perf: Use a stack-allocated array instead of Vec<u16>.
        // SGR sequences rarely exceed 16 params (truecolor: 38;2;R;G;B = 5).
        // The old `Vec<u16>::collect()` allocated on every SGR dispatch —
        // a hot path for colored output (e.g. `ls --color`, `rg`).
        const MAX_SGR_PARAMS: usize = 32;
        let mut buf = [0u16; MAX_SGR_PARAMS];
        let mut len = 0usize;
        for sub in params.iter() {
            if len >= MAX_SGR_PARAMS {
                break;
            }
            buf[len] = sub.first().copied().unwrap_or(0);
            len += 1;
        }
        let vals: &[u16] = &buf[..len];

        let mut i = 0;
        while i < vals.len() {
            let v = vals[i];
            match v {
                0 => self.attrs = Attrs::default(),
                1 => self.attrs.flags.insert(CellFlags::BOLD),
                2 => self.attrs.flags.insert(CellFlags::DIM),
                3 => self.attrs.flags.insert(CellFlags::ITALIC),
                4 => self.attrs.flags.insert(CellFlags::UNDERLINE),
                7 => self.attrs.flags.insert(CellFlags::REVERSE),
                8 => self.attrs.flags.insert(CellFlags::HIDDEN),
                9 => self.attrs.flags.insert(CellFlags::STRIKETHROUGH),
                21 => self.attrs.flags.remove(CellFlags::BOLD),
                22 => self.attrs.flags.remove(CellFlags::BOLD | CellFlags::DIM),
                23 => self.attrs.flags.remove(CellFlags::ITALIC),
                24 => {
                    self.attrs
                        .flags
                        .remove(CellFlags::UNDERLINE | CellFlags::DOUBLE_UNDER);
                }
                27 => self.attrs.flags.remove(CellFlags::REVERSE),
                28 => self.attrs.flags.remove(CellFlags::HIDDEN),
                29 => self.attrs.flags.remove(CellFlags::STRIKETHROUGH),
                // Standard foreground 30-37
                30..=37 => {
                    self.attrs.fg = CellColor::Palette((v - 30) as u8);
                }
                // 256-color / truecolor foreground
                38 => {
                    if let Some((color, skip)) = self.parse_sgr_color(vals, i + 1) {
                        self.attrs.fg = color;
                        i += skip;
                    }
                }
                // Default foreground
                39 => self.attrs.fg = CellColor::Default,
                // Standard background 40-47
                40..=47 => {
                    self.attrs.bg = CellColor::Palette((v - 40) as u8);
                }
                // 256-color / truecolor background
                48 => {
                    if let Some((color, skip)) = self.parse_sgr_color(vals, i + 1) {
                        self.attrs.bg = color;
                        i += skip;
                    }
                }
                // Default background
                49 => self.attrs.bg = CellColor::Default,
                // Bright foreground 90-97
                90..=97 => {
                    self.attrs.fg = CellColor::Palette((v - 90 + 8) as u8);
                }
                // Bright background 100-107
                100..=107 => {
                    self.attrs.bg = CellColor::Palette((v - 100 + 8) as u8);
                }
                _ => {
                    tracing::trace!(v, "unhandled SGR param");
                }
            }
            i += 1;
        }
    }

    /// Parse SGR color starting after the 38/48 marker.
    /// Returns `(CellColor, skip_count)` where skip_count is how many extra
    /// values (beyond the 38/48) were consumed. Stores the *origin* (palette
    /// index or explicit RGB) rather than resolving against the palette, so a
    /// theme/palette change can recolor already-written cells.
    fn parse_sgr_color(&self, vals: &[u16], start: usize) -> Option<(CellColor, usize)> {
        let kind = vals.get(start).copied()?;
        match kind {
            // Indexed 256-color: 38;5;N
            5 => {
                let idx = vals.get(start + 1).copied()?.min(255) as u8;
                Some((CellColor::Palette(idx), 2))
            }
            // Truecolor: 38;2;R;G;B
            2 => {
                let r = vals.get(start + 1).copied()? as u8;
                let g = vals.get(start + 2).copied()? as u8;
                let b = vals.get(start + 3).copied()? as u8;
                Some((CellColor::Rgb(Color::rgb(r, g, b)), 4))
            }
            _ => None,
        }
    }

    /// Handle DEC private mode set/reset (CSI ? <n> h/l).
    fn handle_dec_private_mode(&mut self, mode: u16, set: bool) {
        match mode {
            1 => self.capabilities.app_cursor_keys = set, // DECCKM
            6 => {
                // DECOM: CUP is relative to the scroll region.
                self.origin_mode = set;
                tracing::debug!(set, "DECOM origin mode toggled");
            }
            7 => { /* DECAWM — auto wrap mode, always on */ }
            25 => {
                self.cursor_visible = set; // DECTCEM — cursor show/hide
            }
            47 | 1049 => {
                // Swap only on a real state change.
                if set != self.capabilities.alt_active {
                    self.swap_alt(mode == 1049);
                }
                if !set {
                    self.synchronized_output_started = None;
                }
            }
            2004 => self.bracketed_paste = set, // Bracketed paste
            2026 => {
                if set {
                    self.begin_primary_screen_synchronized_frame();
                    self.synchronized_output_started
                        .get_or_insert_with(std::time::Instant::now);
                } else {
                    self.finish_primary_screen_synchronized_frame();
                    self.synchronized_output_started = None;
                }
                tracing::debug!(set, "DEC synchronized output toggled");
            }
            9 => {
                self.capabilities.mouse_protocol = if set {
                    MouseProtocol::X10
                } else {
                    MouseProtocol::Off
                }
            }
            1000 => {
                self.capabilities.mouse_protocol = if set {
                    MouseProtocol::Normal
                } else {
                    MouseProtocol::Off
                }
            }
            1002 => {
                self.capabilities.mouse_protocol = if set {
                    MouseProtocol::ButtonEvent
                } else {
                    MouseProtocol::Off
                }
            }
            1003 => {
                self.capabilities.mouse_protocol = if set {
                    MouseProtocol::AnyEvent
                } else {
                    MouseProtocol::Off
                }
            }
            // v1.0 fix: SGR-1006 mouse ENCODING (selects the format of mouse
            // reports, independent of whether reporting is on). vim/tmux/htop
            // enable this together with 1000/1002/1003. Previously ignored →
            // we always emitted SGR format, corrupting apps that expected
            // legacy encoding and leaking bytes as visible text (vim `~@k`).
            1006 => self.capabilities.sgr_mouse = set,
            // SGR pixel-mode (1015) and urxvt-mode (1015): not implemented;
            // apps that request them fall back to our default (SGR-1006 when
            // sgr_mouse, legacy otherwise).
            _ => tracing::trace!(mode, set, "unhandled DEC private mode"),
        }
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
