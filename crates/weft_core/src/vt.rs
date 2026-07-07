//! VT100/VT520 escape sequence parser.
//!
//! Wraps the `vte` crate with a `Terminal` struct that implements
//! `vte::Perform` to translate escape sequences into Grid operations.

use crate::blocks::{BlockTracker, ShellPhase};
use crate::editor::Editor;
use crate::grid::{CellColor, CellFlags, CellWidth, Color, Cursor, CursorStyle, Grid};
use crate::hyperlink::HyperlinkRegistry;
use crate::input::{build_submit_bytes, effective_mode, InputMode, MouseProtocol};

/// Current text attributes applied to newly printed characters.
/// Updated by SGR (CSI m) sequences, consumed by `print()`.
#[derive(Clone, Debug)]
pub struct Attrs {
    pub fg: CellColor,
    pub bg: CellColor,
    pub flags: CellFlags,
}

impl Default for Attrs {
    fn default() -> Self {
        Self {
            fg: CellColor::Default,
            bg: CellColor::Default,
            flags: CellFlags::empty(),
        }
    }
}

/// Shell integration markers (OSC 133).
/// Stored during v0.1 for future use in v0.5 block parsing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellMarker {
    PromptStart,
    CommandStart,
    CommandOutputStart,
    CommandEnd { exit_code: i32 },
}

/// The terminal: owns a Grid, vte::Parser, and current attributes.
/// Implements `vte::Perform` to translate escape sequences into Grid mutations.
pub struct Terminal {
    grid: Grid,
    parser: vte::Parser,
    attrs: Attrs,
    /// Window title (from OSC 0/2).
    title: String,
    /// Shell integration markers collected during parsing.
    shell_markers: Vec<ShellMarker>,
    /// Command-block state machine: consumes the OSC 133 marker stream and the
    /// printed output to build finished [`Block`](crate::blocks::Block)s.
    block_tracker: BlockTracker,
    editor: Editor,
    /// cwd reported by the shell via OSC 7.
    cwd: Option<String>,
    /// Git branch reported by the shell hook via OSC 9;git=<branch>.
    git_branch: Option<String>,
    /// Set on editor submit, consumed by `133;B`. While `Some`, input passes
    /// through (Enter→preexec window) and the command source is the editor.
    command_from_editor: Option<String>,
    /// Application cursor key mode (DECCKM, CSI ?1h/l).
    pub app_cursor_keys: bool,
    /// Bracketed paste mode (CSI ?2004h/l).
    pub bracketed_paste: bool,
    /// Origin mode (DECOM, CSI ?6h/l).
    origin_mode: bool,
    /// Cursor visibility (DECTCEM, CSI ?25h/l).
    pub cursor_visible: bool,
    /// Cursor style (DECSCUSR, CSI <n> q).
    pub cursor_style: CursorStyle,
    /// Mouse protocol mode.
    pub mouse_protocol: MouseProtocol,
    /// 256-color palette (indexed colors for SGR 38;5 / 48;5).
    palette: [Color; 256],
    /// Alternate screen buffer for full-screen apps (DEC 1049/47).
    /// Swapped with `grid` on enter/exit; main content survives in `alt_grid`.
    alt_grid: Grid,
    /// True while the alternate screen is active.
    alt_active: bool,
    /// Stashed primary cursor, restored on alt-screen exit (DEC 1049).
    saved_cursor: Option<Cursor>,
    /// Bytes to write back to the PTY in response to terminal queries
    /// (DA1/DA2 device attributes, DSR cursor-position report, text-area size
    /// report). Modern TUIs probe these to detect capabilities and engage their
    /// full UI; weft must answer or they degrade (claude falls back to a basic
    /// line mode with an unconstrained, roaming cursor).
    pending_output: Vec<u8>,
    /// Active OSC 8 hyperlink (None = no link). Set by `OSC 8;params;URI ST`,
    /// cleared by `OSC 8;; ST`. All cells printed while this is `Some(id)`
    /// are tagged with `CellFlags::HYPERLINK` and linked to `id` in the
    /// registry's side-map.
    active_hyperlink_id: Option<u32>,
    /// OSC 8 hyperlink registry — maps cell coords → id → URL. External to
    /// the `Cell` struct so Cell stays at 24 bytes (only the 1-bit HYPERLINK
    /// flag lives on the cell).
    hyperlinks: HyperlinkRegistry,
    /// v1.0 perf: tracks whether vte's parser is in the ground state (not
    /// inside an escape sequence). Set true in `print()` (vte only calls
    /// print() in ground state), cleared on escape (0x1B) / C0 controls.
    /// Used by `process()` to gate the ASCII fast path — only when in ground
    /// state can a run of printable ASCII bypass the per-byte state machine.
    parser_in_ground_state: bool,
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
            app_cursor_keys: false,
            bracketed_paste: false,
            origin_mode: false,
            cursor_visible: true,
            cursor_style: CursorStyle::Block,
            mouse_protocol: MouseProtocol::Off,
            palette: Self::init_palette(),
            // Alt screen has no scrollback: full-screen apps manage their own
            // scrolling and history should not leak across invocations.
            alt_grid: Grid::with_scrollback(rows, cols, 0),
            alt_active: false,
            saved_cursor: None,
            pending_output: Vec::new(),
            active_hyperlink_id: None,
            hyperlinks: HyperlinkRegistry::new(),
            parser_in_ground_state: true,
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

    /// Scroll the grid up by `n` rows and invalidate the OSC 8 cell_map.
    /// Required because the side-map keys are viewport-relative `(row, col)`
    /// and shift on every scroll — leaving stale keys would make clicks
    /// resolve to the wrong URL.
    fn scroll_grid_up(&mut self, n: usize) {
        self.grid.scroll_up(n);
        // v1.0 P1.5-C2: skip the HashMap clear when no hyperlinks are active
        // (the common case — terminal output rarely has OSC 8 links).
        if !self.hyperlinks.cell_map_is_empty() {
            self.hyperlinks.clear_cell_map();
        }
    }

    /// Same as [`scroll_grid_up`](Self::scroll_grid_up) for scroll-down.
    fn scroll_grid_down(&mut self, n: usize) {
        self.grid.scroll_down(n);
        if !self.hyperlinks.cell_map_is_empty() {
            self.hyperlinks.clear_cell_map();
        }
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
            self.alt_active,
            self.block_tracker.bootstrap_ready(),
            self.command_from_editor.is_some(),
        )
    }

    /// Whether the alternate screen buffer is currently active.
    pub fn is_alt_screen_active(&self) -> bool {
        self.alt_active
    }

    /// True when the Warp-style block view should render (integrated shell, not
    /// in an alt-screen app). Covers both AtPrompt (editor + input box) and
    /// CommandExecuting (blocks overlaid above the live grid) — the renderer
    /// distinguishes them via the prompt / shell phase.
    pub fn show_block_view(&self) -> bool {
        self.block_tracker.bootstrap_ready() && !self.alt_active
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
        if !self.alt_active {
            if save_cursor_and_clear {
                self.saved_cursor = Some(self.grid.cursor.clone());
                self.alt_grid.clear();
            }
            // Alternate screen starts fresh with the cursor at home (0,0).
            self.alt_grid.cursor = Cursor::default();
            std::mem::swap(&mut self.grid, &mut self.alt_grid);
            self.alt_active = true;
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
            self.alt_active = false;
            // Restoring the primary grid — alt-screen hyperlinks are gone.
            self.hyperlinks.clear_cell_map();
            self.active_hyperlink_id = None;
        }
        tracing::info!(
            active = self.alt_active,
            rows = self.grid.num_rows,
            cols = self.grid.num_cols,
            "alt-screen toggled"
        );
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
    /// v1.0 perf: ASCII fast path — scan for runs of printable ASCII
    /// (0x20..=0x7E) and write them directly to the grid via `print_ascii_run`,
    /// bypassing vte's per-byte state machine. Only escape (0x1B) and C0
    /// control bytes (< 0x20, except 0x07/0x08/0x09/0x0A/0x0D handled by
    /// `execute`) go through `parser.advance()`. For `seq 1 100000` (~580KB
    /// of ASCII digits + newlines), this reduces vte state-machine calls
    /// from ~580000 to ~10000 (just the newlines), a ~58x reduction.
    ///
    /// Important: the fast path only triggers when vte's parser is in the
    /// ground state (no escape sequence in progress). We track this via
    /// `parser_in_ground_state` — set true initially, cleared ONLY on ESC
    /// (0x1B) which is the sole byte that transitions vte out of ground.
    /// C0 controls (\n, \r, \t, BEL, …) are "execute" actions that stay in
    /// ground, so the fast path remains engaged after them. v1.0 P1.5-C3:
    /// the previous code also cleared the flag on every C0 control, which
    /// forced the first char of each line through vte's per-byte path —
    /// ~100k wasted advance() calls for `seq 1 100000`.
    pub fn process(&mut self, bytes: &[u8]) {
        let mut parser = std::mem::take(&mut self.parser);
        let mut i = 0;
        while i < bytes.len() {
            // Fast path: only when parser is in ground state. Scan a run
            // of printable ASCII (0x20..=0x7E) that doesn't start with an
            // escape/C0 control. These bytes map 1:1 to chars and are all
            // width-1, so they bypass vte entirely.
            if self.parser_in_ground_state {
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
        if phase != ShellPhase::AtPrompt {
            self.grid.scroll_offset = 0;
        }
        let capturing = !self.alt_active && phase == ShellPhase::CommandExecuting;
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

            // How many bytes fit in the current row? No per-char bounds check.
            let remaining_in_row = num_cols - col;
            let remaining_bytes = bytes.len() - offset;
            let count = remaining_in_row.min(remaining_bytes);
            let chunk = &bytes[offset..offset + count];

            // Batch capture to block tracker — one push_str instead of N pushes.
            if capturing {
                self.block_tracker.on_print_ascii_run(chunk);
            }

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
    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.grid.resize(rows, cols);
        self.alt_grid.resize(rows, cols);
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

    // ── Palette initialization ───────────────────────────────────

    /// Initialize the xterm-256color palette (delegates to the shared
    /// `Color::standard_palette`).
    fn init_palette() -> [Color; 256] {
        Color::standard_palette()
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
            1 => self.app_cursor_keys = set, // DECCKM
            6 => {
                // DECOM (origin mode): CUP becomes relative to the scroll
                // region. Full-screen TUIs (vim, claude) rely on this.
                self.origin_mode = set;
                tracing::debug!(set, "DECOM origin mode toggled");
            }
            7 => { /* DECAWM — auto wrap mode, always on */ }
            25 => self.cursor_visible = set, // DECTCEM — cursor show/hide
            47 | 1049 => {
                // DEC alternate screen buffer: only swap on a real state
                // change, matching Warp's idempotent enter/exit guards.
                if set != self.alt_active {
                    self.swap_alt(mode == 1049);
                }
            }
            2004 => self.bracketed_paste = set, // Bracketed paste
            9 => {
                self.mouse_protocol = if set {
                    MouseProtocol::X10
                } else {
                    MouseProtocol::Off
                }
            }
            1000 => {
                self.mouse_protocol = if set {
                    MouseProtocol::Normal
                } else {
                    MouseProtocol::Off
                }
            }
            1002 => {
                self.mouse_protocol = if set {
                    MouseProtocol::ButtonEvent
                } else {
                    MouseProtocol::Off
                }
            }
            1003 => {
                self.mouse_protocol = if set {
                    MouseProtocol::AnyEvent
                } else {
                    MouseProtocol::Off
                }
            }
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
        .unwrap_or(default)
}

impl vte::Perform for Terminal {
    fn print(&mut self, c: char) {
        // v1.0 perf: vte only calls print() in ground state, so mark it.
        self.parser_in_ground_state = true;
        // v1.0 perf: cache phase once per print() call. The phase doesn't
        // change within a single print() — it only transitions on OSC 133
        // markers, which arrive via osc_dispatch, not print.
        let phase = self.block_tracker.phase();

        // Snap back to the live viewport for new content — EXCEPT when idle at
        // an integrated prompt (AtPrompt). In that state the shell may emit
        // prompt re-renders or async segments that would otherwise destroy
        // the user's scroll position while they're reading history. During
        // CommandExecuting (output streaming) and in non-integrated mode
        // (plain grid terminal), new output always resets scroll.
        if phase != ShellPhase::AtPrompt {
            self.grid.scroll_offset = 0;
        }

        // Feed the printed char to the active command block's output capture.
        // v1.0 perf: check is_capturing() here to skip the function call
        // overhead when not capturing (e.g. AtPrompt, NotIntegrated).
        if !self.alt_active && phase == ShellPhase::CommandExecuting {
            self.block_tracker.on_print(c);
        }

        // Handle deferred wrap
        if self.grid.cursor.wrap_pending {
            self.grid.cursor.wrap_pending = false;
            self.grid.cursor.col = 0;
            let (_, bottom) = self.grid.scroll_region();
            if self.grid.cursor.row == bottom {
                self.scroll_grid_up(1);
            } else if self.grid.cursor.row < self.grid.num_rows - 1 {
                self.grid.cursor.row += 1;
            }
            // Mark the previous row as wrapped so reflow can merge it
            // back when the terminal widens.
            if self.grid.cursor.row > 0 {
                self.grid.viewport[self.grid.cursor.row - 1].wrapped = true;
            }
        }

        // v1.0 perf: ASCII fast path — all ASCII chars are width 1.
        // This skips the unicode_width lookup for the common case (terminal
        // output is predominantly ASCII: digits, letters, punctuation).
        // For 58KB of `seq` output, this saves ~58000 lookup calls.
        let width = if c.is_ascii() {
            CellWidth::Half
        } else if unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) > 1 {
            CellWidth::Full
        } else {
            CellWidth::Half
        };

        let num_cols = self.grid.num_cols;
        let row = self.grid.cursor.row;
        let col = self.grid.cursor.col;

        // Wide char straddling line boundary → wrap first
        if width == CellWidth::Full && col + 1 >= num_cols {
            self.grid.cursor.wrap_pending = false;
            self.grid.cursor.col = 0;
            let (_, bottom) = self.grid.scroll_region();
            if self.grid.cursor.row == bottom {
                self.scroll_grid_up(1);
            } else if self.grid.cursor.row < self.grid.num_rows - 1 {
                self.grid.cursor.row += 1;
            }
            // Mark the previous row as wrapped for reflow
            if self.grid.cursor.row > 0 {
                self.grid.viewport[self.grid.cursor.row - 1].wrapped = true;
            }
            // Write on new line
            let new_row = self.grid.cursor.row;
            let new_col = self.grid.cursor.col;
            let cell = &mut self.grid.viewport[new_row].cells[new_col];
            cell.character = c;
            cell.fg = self.attrs.fg;
            cell.bg = self.attrs.bg;
            cell.flags = self.attrs.flags | CellFlags::DIRTY;
            cell.width = CellWidth::Full;
            // OSC 8: tag the wrapped wide-char cell with the active hyperlink.
            if let Some(id) = self.active_hyperlink_id {
                cell.flags |= CellFlags::HYPERLINK;
                self.hyperlinks.link_cell(new_row, new_col, id);
            } else {
                self.hyperlinks.unlink_cell(new_row, new_col);
            }
            self.grid.viewport[new_row].mark_dirty(new_col);

            if new_col + 1 < num_cols {
                let spacer = &mut self.grid.viewport[new_row].cells[new_col + 1];
                spacer.character = ' ';
                spacer.flags = CellFlags::WIDE_SPACER;
                spacer.width = CellWidth::Half;
                self.grid.viewport[new_row].mark_dirty(new_col + 1);
            }

            self.grid.cursor.col = new_col + 2;
            if self.grid.cursor.col >= num_cols {
                self.grid.cursor.wrap_pending = true;
                self.grid.cursor.col = num_cols - 1;
            }
            return;
        }

        // Write the character. Normally `col < num_cols` because the deferred-
        // wrap / wide-char logic above keeps the cursor in bounds, but during a
        // resize the grid narrows immediately while the PTY SIGWINCH is still
        // in flight — the shell keeps emitting at the old width and the cursor
        // can momentarily point past the last column. Instead of silently
        // dropping those characters, wrap to the next line so nothing is lost.
        // The shell will repaint correctly once it learns the new width.
        let mut row = row;
        let mut col = col;
        if col >= num_cols {
            self.grid.cursor.wrap_pending = false;
            self.grid.cursor.col = 0;
            let (_, bottom) = self.grid.scroll_region();
            if self.grid.cursor.row == bottom {
                self.scroll_grid_up(1);
            } else if self.grid.cursor.row < self.grid.num_rows - 1 {
                self.grid.cursor.row += 1;
            }
            row = self.grid.cursor.row;
            col = 0;
            if row > 0 {
                self.grid.viewport[row - 1].wrapped = true;
            }
        }

        {
            let cell = &mut self.grid.viewport[row].cells[col];
            cell.character = c;
            cell.fg = self.attrs.fg;
            cell.bg = self.attrs.bg;
            cell.flags = self.attrs.flags | CellFlags::DIRTY;
            cell.width = width;

            // OSC 8: tag the cell with HYPERLINK and record its (row,col)→id
            // in the registry side-map. When the active hyperlink is None
            // (overwriting a previously tagged cell), drop the side-map entry
            // and clear the flag so the underline disappears.
            if let Some(id) = self.active_hyperlink_id {
                cell.flags |= CellFlags::HYPERLINK;
                self.hyperlinks.link_cell(row, col, id);
            } else {
                if cell.flags.contains(CellFlags::HYPERLINK) {
                    cell.flags.remove(CellFlags::HYPERLINK);
                }
                self.hyperlinks.unlink_cell(row, col);
            }

            self.grid.viewport[row].mark_dirty(col);
            self.grid.cursor.col += width as usize;

            if width == CellWidth::Full && col + 1 < num_cols {
                let spacer = &mut self.grid.viewport[row].cells[col + 1];
                spacer.character = ' ';
                spacer.flags = CellFlags::WIDE_SPACER;
                spacer.width = CellWidth::Half;
                self.grid.viewport[row].mark_dirty(col + 1);
            }
        }

        if self.grid.cursor.col >= num_cols {
            self.grid.cursor.wrap_pending = true;
            self.grid.cursor.col = num_cols - 1;
        }
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x07 => { /* BEL — bell, ignored in v0.1 */ }
            0x08 => self.grid.backspace(),
            0x09 => {
                // Tab is a C0 control, so the print path never sees it — but
                // columnar tools (ls, etc.) separate fields with tabs. Mirror
                // the cursor's tab advance into the captured block output as
                // spaces, otherwise the block view concatenates the fields.
                let prev_col = self.grid.cursor.col;
                self.grid.advance_tab(1);
                // v1.0 perf: skip on_print calls when not capturing.
                if !self.alt_active && self.block_tracker.is_capturing() {
                    let advanced = self.grid.cursor.col.saturating_sub(prev_col);
                    for _ in 0..advanced {
                        self.block_tracker.on_print(' ');
                    }
                }
            }
            0x0A..=0x0C => {
                // LF, VT, FF → move to next line (CR+LF on Unix terminals).
                // The raw VT `index()` only moves down; Unix terminals
                // treat LF as newline (carriage return + index).
                // v1.0 perf: skip on_newline call when not capturing.
                if !self.alt_active && self.block_tracker.is_capturing() {
                    self.block_tracker.on_newline();
                }
                self.grid.carriage_return();
                if self.grid.index() {
                    self.hyperlinks.clear_cell_map();
                }
            }
            0x0D => self.grid.carriage_return(),
            _ => tracing::trace!(byte, "unhandled execute"),
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        // DEC private mode: CSI ? <params> h/l
        if intermediates == [b'?'] {
            let set = action == 'h';
            for sub in params.iter() {
                if let &[mode] = sub {
                    self.handle_dec_private_mode(mode, set);
                }
            }
            return;
        }

        // Diagnostic: trace cursor-moving CSIs to pin down TUI cursor desync.
        if matches!(
            action,
            'A' | 'B' | 'C' | 'D' | 'E' | 'F' | 'H' | 'f' | 'G' | 'd'
        ) {
            tracing::debug!(
                action = %action,
                p0 = param(params, 0, 1),
                p1 = param(params, 1, 1),
                before_row = self.grid.cursor.row,
                before_col = self.grid.cursor.col,
                origin = self.origin_mode,
                "csi-move"
            );
        }

        match action {
            // Cursor movement
            'A' => self.grid.move_up(param(params, 0, 1) as usize),
            'B' => self.grid.move_down(param(params, 0, 1) as usize),
            'C' => self.grid.move_forward(param(params, 0, 1) as usize),
            'D' => self.grid.move_backward(param(params, 0, 1) as usize),
            'E' => {
                let n = param(params, 0, 1) as usize;
                self.grid.move_down(n);
                self.grid.carriage_return();
            }
            'F' => {
                let n = param(params, 0, 1) as usize;
                self.grid.move_up(n);
                self.grid.carriage_return();
            }

            // Cursor position
            'H' | 'f' => {
                let row = param(params, 0, 1) as usize;
                let col = param(params, 1, 1) as usize;
                self.grid.goto(row, col, self.origin_mode);
                tracing::debug!(
                    req_row = row,
                    req_col = col,
                    origin = self.origin_mode,
                    cur_row = self.grid.cursor.row,
                    cur_col = self.grid.cursor.col,
                    "CUP"
                );
            }
            'G' => {
                let col = param(params, 0, 1) as usize;
                self.grid.set_cursor_col(col.saturating_sub(1));
            }
            'd' => {
                let row = param(params, 0, 1) as usize;
                self.grid.set_cursor_row(row.saturating_sub(1));
            }

            // Erase
            'J' => {
                let mode = param(params, 0, 0);
                match mode {
                    0 => self.grid.clear_screen_below(),
                    1 => self.grid.clear_screen_above(),
                    2 => self.grid.clear_screen_all(),
                    3 => self.grid.clear_scrollback(),
                    _ => {}
                }
            }
            'K' => {
                let mode = param(params, 0, 0);
                match mode {
                    0 => self.grid.clear_line_right(),
                    1 => self.grid.clear_line_left(),
                    2 => self.grid.clear_line_all(),
                    _ => {}
                }
            }
            'X' => {
                let count = param(params, 0, 1) as usize;
                self.grid.erase_chars(count);
            }

            // SGR
            'm' => self.handle_sgr(params),

            // ── Terminal queries (must respond, else TUIs degrade) ──
            // DA1 / primary device attributes (CSI c).
            'c' if intermediates.is_empty() => {
                // VT220-class with ANSI color — a response any TUI accepts.
                self.respond(b"\x1b[?62;1;2;4;6;9;15;22c");
            }
            // DA2 / secondary device attributes (CSI > c).
            'c' if intermediates == [b'>'] => {
                // "weft", version 0, ROM 0.
                self.respond(b"\x1b[>0;276;0c");
            }
            // DSR — device status / cursor-position report (CSI n).
            'n' => match param(params, 0, 0) {
                5 => self.respond(b"\x1b[0n"), // terminal OK
                6 => {
                    // CPR: cursor position, 1-based.
                    let r = self.grid.cursor.row + 1;
                    let c = self.grid.cursor.col + 1;
                    self.respond(format!("\x1b[{r};{c}R").as_bytes());
                }
                _ => {}
            },
            // Text-area size report (CSI t). 18 = size in chars, 14/16 = pixels.
            't' => match param(params, 0, 0) {
                18 | 19 => {
                    let rows = self.grid.num_rows;
                    let cols = self.grid.num_cols;
                    self.respond(format!("\x1b[8;{rows};{cols}t").as_bytes());
                }
                _ => {}
            },

            // Scrolling
            'S' => {
                let n = param(params, 0, 1);
                self.scroll_grid_up(if n == 0 { 1 } else { n as usize });
            }
            'T' => {
                if intermediates.is_empty() {
                    self.scroll_grid_down(param(params, 0, 1) as usize);
                }
            }

            // Scroll region
            'r' => {
                if params.is_empty() {
                    self.grid.reset_scroll_region();
                    tracing::debug!("scroll region reset");
                } else {
                    let top = param(params, 0, 1) as usize;
                    let bottom = param(params, 1, 0) as usize;
                    if bottom == 0 {
                        self.grid.reset_scroll_region();
                        tracing::debug!("scroll region reset (bottom=0)");
                    } else {
                        self.grid.set_scroll_region(top, bottom);
                        tracing::debug!(top, bottom, "scroll region set");
                    }
                }
            }

            // Cursor save/restore (SCO style)
            's' => {
                self.grid.save_cursor();
                tracing::debug!(
                    row = self.grid.cursor.row,
                    col = self.grid.cursor.col,
                    "DECSC save"
                );
            }
            'u' => {
                self.grid.restore_cursor();
                tracing::debug!(
                    row = self.grid.cursor.row,
                    col = self.grid.cursor.col,
                    "DECRC restore"
                );
            }

            // Insert/delete
            '@' => self.grid.insert_blank(param(params, 0, 1) as usize),
            'P' => self.grid.delete_chars(param(params, 0, 1) as usize),
            'L' => self.grid.insert_blank_lines(param(params, 0, 1) as usize),
            'M' => self.grid.delete_lines(param(params, 0, 1) as usize),

            // Tab stops
            'I' => self.grid.advance_tab(param(params, 0, 1) as usize),
            'Z' => self.grid.back_tab(param(params, 0, 1) as usize),
            'g' => {
                let mode = param(params, 0, 0);
                match mode {
                    0 => self.grid.clear_tabstop(),
                    3 => self.grid.clear_all_tabstops(),
                    _ => {}
                }
            }

            // Cursor style (DECSCUSR — CSI <n> q)
            'q' => {
                if intermediates.is_empty() {
                    // Only handle as DECSCUSR if it looks like "CSI N q"
                    // (not a regular CSI q which is rare)
                    let style = param(params, 0, 0);
                    self.cursor_style = match style {
                        0 | 1 => CursorStyle::BlinkingBlock,
                        2 => CursorStyle::Block,
                        3 => CursorStyle::BlinkingUnderline,
                        4 => CursorStyle::Underline,
                        5 => CursorStyle::BlinkingBar,
                        6 => CursorStyle::Bar,
                        _ => CursorStyle::Block,
                    };
                }
            }

            _ => {
                tracing::trace!(?params, ?intermediates, action = ?action, "unhandled CSI");
            }
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        match (intermediates, byte) {
            (&[], 0x37) => self.grid.save_cursor(),    // DECSC
            (&[], 0x38) => self.grid.restore_cursor(), // DECRC
            (&[], 0x44) => {
                // IND — index. May scroll the region; if so, OSC 8 cell_map
                // entries (viewport-relative) go stale.
                if self.grid.index() {
                    self.hyperlinks.clear_cell_map();
                }
            }
            (&[], 0x4D) => {
                // RI — reverse index. Same scroll invalidation as IND.
                if self.grid.reverse_index() {
                    self.hyperlinks.clear_cell_map();
                }
            }
            (&[], 0x45) => {
                // NEL — next line. Same as CR+IND.
                if self.grid.index() {
                    self.hyperlinks.clear_cell_map();
                }
                self.grid.carriage_return();
            }
            (&[], 0x48) => self.grid.set_tabstop(), // HTS
            (&[], 0x63) => self.reset(),            // RIS
            (&[], 0x3D) | (&[], 0x3E) => { /* DECKPAM/DECKPNM — ignored */ }
            _ => {
                tracing::trace!(?intermediates, byte, "unhandled ESC");
            }
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if params.is_empty() {
            return;
        }

        let code = std::str::from_utf8(params[0]).unwrap_or("");

        match code {
            "0" | "2" => {
                if params.len() > 1 {
                    self.title = String::from_utf8_lossy(params[1]).into_owned();
                }
            }
            "4" => {
                if params.len() >= 3 {
                    if let Ok(idx) = std::str::from_utf8(params[1])
                        .unwrap_or("")
                        .parse::<usize>()
                    {
                        if idx < 256 {
                            if let Some(color) = parse_x11_color(params[2]) {
                                self.palette[idx] = color;
                            }
                        }
                    }
                }
            }
            "7" => {
                if params.len() > 1 {
                    if let Some(path) = parse_osc7_cwd(params[1]) {
                        self.cwd = Some(path.clone());
                        // Mirror into the block tracker so each block is stamped
                        // with the dir it ran in (for the block-view header).
                        self.block_tracker.set_cwd(Some(path));
                    }
                }
            }
            "9" => {
                // Custom OSC 9;git=<branch> — shell hook reports the git branch.
                if let Some(payload) = params.get(1) {
                    if let Ok(s) = std::str::from_utf8(payload) {
                        if let Some(branch) = s.strip_prefix("git=") {
                            self.git_branch = Some(branch.to_string());
                        }
                    }
                }
            }
            "8" => {
                // OSC 8 ; params ; URI ST — hyperlink.
                //   `OSC 8 ; ; URI ST`     → start hyperlink to URI (no id).
                //   `OSC 8 ; id=K ; URI ST`→ start hyperlink with id K (kitty
                //                            extension; we dedup by URL anyway).
                //   `OSC 8 ; ; ST`         → end hyperlink (empty URI).
                // We dedup URLs in the registry and remember the active id;
                // `print()` stamps cells with HYPERLINK while this is Some.
                if params.len() >= 3 {
                    let uri = std::str::from_utf8(params[2]).unwrap_or("");
                    if uri.is_empty() {
                        self.active_hyperlink_id = None;
                    } else {
                        let id = self.hyperlinks.register(uri.to_string());
                        self.active_hyperlink_id = Some(id);
                    }
                } else {
                    // OSC 8 ;; ST (no URI field) — clear.
                    self.active_hyperlink_id = None;
                }
            }
            "133" => {
                if params.len() > 1 {
                    match params[1] {
                        b"A" => {
                            self.shell_markers.push(ShellMarker::PromptStart);
                            self.block_tracker.on_prompt_start();
                            // Clear the git branch: the precmd hook re-emits
                            // OSC 9;git= if (and only if) the cwd is still a git
                            // repo. Without this, leaving a repo keeps the stale
                            // branch label forever (the hook sends nothing in a
                            // non-git dir, so git_branch was never cleared).
                            self.git_branch = None;
                            // Clear any stale submit flag so a missing 133;B
                            // (crashed / non-integrated sub-shell) can't pin the
                            // editor in passthrough forever.
                            self.command_from_editor = None;
                        }
                        b"B" => {
                            self.shell_markers.push(ShellMarker::CommandStart);
                            // 133;B (preexec): if the editor submitted the
                            // command, record that; otherwise snapshot the grid
                            // row (real shells emit a newline first, so the
                            // cursor may sit below the command —
                            // `snapshot_command_line` walks up).
                            let command = if let Some(c) = self.command_from_editor.take() {
                                c
                            } else {
                                self.snapshot_command_line()
                            };
                            self.block_tracker.on_command_start(command);
                        }
                        b"C" => {
                            self.shell_markers.push(ShellMarker::CommandOutputStart);
                            self.block_tracker.on_command_output_start();
                        }
                        b"D" => {
                            let exit_code = if params.len() > 2 {
                                std::str::from_utf8(params[2])
                                    .ok()
                                    .and_then(|s| s.parse::<i32>().ok())
                                    .unwrap_or(0)
                            } else {
                                0
                            };
                            self.shell_markers
                                .push(ShellMarker::CommandEnd { exit_code });
                            self.block_tracker.on_command_end(exit_code);
                        }
                        _ => {}
                    }
                }
            }
            _ => {
                tracing::trace!(code, "unhandled OSC");
            }
        }
    }

    fn hook(&mut self, _params: &vte::Params, _intermediates: &[u8], _ignore: bool, action: char) {
        tracing::trace!(action = ?action, "DCS hook (ignored in v0.1)");
    }

    fn put(&mut self, _byte: u8) {
        // DCS data — ignored in v0.1
    }

    fn unhook(&mut self) {
        // DCS end — ignored in v0.1
    }
}

/// Parse an OSC 7 payload `file://[host]/abs/path` → `/abs/path`.
fn parse_osc7_cwd(payload: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(payload).ok()?;
    let s = s.strip_prefix("file://")?;
    let path_start = s.find('/')?;
    Some(s[path_start..].to_string())
}

/// Parse an X11 color string (#RRGGBB or rgb:RR/GG/BB) into a Color.
fn parse_x11_color(bytes: &[u8]) -> Option<Color> {
    let s = std::str::from_utf8(bytes).ok()?;

    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() == 6 {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some(Color::rgb(r, g, b));
        }
    }

    if let Some(rest) = s.strip_prefix("rgb:") {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() == 3 {
            let r = u8::from_str_radix(parts[0], 16).ok()?;
            let g = u8::from_str_radix(parts[1], 16).ok()?;
            let b = u8::from_str_radix(parts[2], 16).ok()?;
            return Some(Color::rgb(r, g, b));
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::ShellPhase;

    fn term() -> Terminal {
        Terminal::new(24, 80)
    }

    // ── Print / basic ────────────────────────────────────────────

    #[test]
    fn print_ascii() {
        let mut t = term();
        t.process(b"Hello");
        assert_eq!(t.grid().cell(0, 0).character, 'H');
        assert_eq!(t.grid().cell(0, 1).character, 'e');
        assert_eq!(t.grid().cell(0, 4).character, 'o');
        assert_eq!(t.grid().cursor.col, 5);
    }

    #[test]
    fn new_output_resets_scroll_offset() {
        let mut t = Terminal::new(3, 8);
        // Scroll several lines into scrollback.
        t.process(b"row0\nrow1\nrow2\nrow3\nrow4\nrow5\n");
        assert!(
            t.grid().scrollback_len() > 0,
            "precondition: history exists"
        );

        // User views older history.
        t.grid_mut().scroll_up_history(2);
        assert!(t.grid().is_scrolled(), "precondition: scrolled up");

        // New PTY output must snap back to the live viewport.
        t.process(b"X");
        assert_eq!(t.grid().scroll_offset, 0, "new output resets offset");
        assert!(!t.grid().is_scrolled());
    }

    #[test]
    fn alt_screen_enter_clear_and_exit_restores() {
        let mut t = Terminal::new(5, 10);
        t.process(b"hello");
        assert_eq!(t.grid().cell(0, 0).character, 'h');

        // DEC 1049h: enter alternate screen, stash main, clear alt view.
        t.process(b"\x1b[?1049h");
        assert!(t.is_alt_screen_active());
        assert_eq!(t.grid().cell(0, 0).character, ' ', "alt screen cleared");

        // Writes land on the alternate screen only.
        t.process(b"world");
        assert_eq!(t.grid().cell(0, 0).character, 'w');

        // DEC 1049l: leave alternate screen, main content restored.
        t.process(b"\x1b[?1049l");
        assert!(!t.is_alt_screen_active());
        assert_eq!(t.grid().cell(0, 0).character, 'h', "main screen restored");
    }

    #[test]
    fn alt_screen_cursor_restored_on_exit() {
        let mut t = Terminal::new(5, 10);
        t.process(b"hello"); // cursor at col 5
        assert_eq!(t.grid().cursor.col, 5);

        t.process(b"\x1b[?1049h");
        // Move around inside the alternate screen.
        t.process(b"\x1b[3;3HXYZ");
        t.process(b"\x1b[?1049l");
        // Main cursor returns to where we left it.
        assert_eq!(t.grid().cursor.col, 5, "cursor restored to main position");
    }

    #[test]
    fn print_with_color() {
        let mut t = term();
        t.process(b"\x1b[31mX");
        assert_eq!(t.grid().cell(0, 0).character, 'X');
        assert_eq!(t.grid().cell(0, 0).fg, CellColor::Palette(1)); // red = palette[1]
    }

    #[test]
    fn print_with_bold_and_italic() {
        let mut t = term();
        t.process(b"\x1b[1;3mX");
        let flags = t.grid().cell(0, 0).flags;
        assert!(flags.contains(CellFlags::BOLD));
        assert!(flags.contains(CellFlags::ITALIC));
    }

    #[test]
    fn sgr_reset() {
        let mut t = term();
        t.process(b"\x1b[31mX\x1b[0mY");
        assert_eq!(t.grid().cell(0, 0).fg, CellColor::Palette(1));
        assert_eq!(t.grid().cell(0, 1).fg, CellColor::Default);
    }

    #[test]
    fn sgr_empty_params_means_reset() {
        let mut t = term();
        t.process(b"\x1b[1;31mA\x1b[mB");
        assert!(t.grid().cell(0, 0).flags.contains(CellFlags::BOLD));
        assert!(!t.grid().cell(0, 1).flags.contains(CellFlags::BOLD));
        assert_eq!(t.grid().cell(0, 1).fg, CellColor::Default);
    }

    // ── Cursor movement ──────────────────────────────────────────

    #[test]
    fn cursor_right() {
        let mut t = term();
        t.process(b"\x1b[5C");
        assert_eq!(t.grid().cursor.col, 5);
    }

    #[test]
    fn cursor_down() {
        let mut t = term();
        t.process(b"\x1b[3B");
        assert_eq!(t.grid().cursor.row, 3);
    }

    #[test]
    fn cursor_up() {
        let mut t = term();
        t.process(b"\x1b[3B\x1b[2A");
        assert_eq!(t.grid().cursor.row, 1);
    }

    #[test]
    fn cursor_position() {
        let mut t = term();
        t.process(b"\x1b[10;20H");
        assert_eq!(t.grid().cursor.row, 9);
        assert_eq!(t.grid().cursor.col, 19);
    }

    #[test]
    fn cursor_horizontal_absolute() {
        let mut t = term();
        t.process(b"\x1b[30G");
        assert_eq!(t.grid().cursor.col, 29);
    }

    #[test]
    fn cursor_vertical_absolute() {
        let mut t = term();
        t.process(b"\x1b[12d");
        assert_eq!(t.grid().cursor.row, 11);
    }

    // ── Clearing ─────────────────────────────────────────────────

    #[test]
    fn clear_screen_all() {
        let mut t = term();
        t.process(b"ABC\x1b[2J");
        assert_eq!(t.grid().cell(0, 0).character, ' ');
    }

    #[test]
    fn clear_line_right() {
        let mut t = term();
        t.process(b"ABCDE\x1b[1;1H"); // goto (0,0)
        t.grid_mut().cursor.col = 2;
        t.process(b"\x1b[K");
        assert_eq!(t.grid().cell(0, 1).character, 'B');
        assert_eq!(t.grid().cell(0, 2).character, ' ');
    }

    // ── Line feed / control ──────────────────────────────────────

    #[test]
    fn linefeed_moves_down() {
        let mut t = term();
        t.process(b"A\nB");
        assert_eq!(t.grid().cell(0, 0).character, 'A');
        assert_eq!(t.grid().cell(1, 0).character, 'B');
    }

    #[test]
    fn carriage_return() {
        let mut t = term();
        t.process(b"ABC\rX");
        assert_eq!(t.grid().cell(0, 0).character, 'X');
        assert_eq!(t.grid().cell(0, 1).character, 'B');
    }

    #[test]
    fn tab_advances() {
        let mut t = term();
        t.process(b"A\tB");
        assert_eq!(t.grid().cell(0, 0).character, 'A');
        assert_eq!(t.grid().cell(0, 8).character, 'B');
    }

    // ── Escape sequences ─────────────────────────────────────────

    #[test]
    fn esc_save_restore() {
        let mut t = term();
        t.process(b"\x1b[5;10H\x1b7\x1b[1;1H\x1b8");
        assert_eq!(t.grid().cursor.row, 4);
        assert_eq!(t.grid().cursor.col, 9);
    }

    #[test]
    fn esc_index() {
        let mut t = term();
        t.grid_mut().cursor.row = 3;
        t.process(b"\x1bD");
        assert_eq!(t.grid().cursor.row, 4);
    }

    #[test]
    fn esc_reverse_index() {
        let mut t = term();
        t.grid_mut().cursor.row = 3;
        t.process(b"\x1bM");
        assert_eq!(t.grid().cursor.row, 2);
    }

    #[test]
    fn print_wrap_at_bottom_row_stays_in_bounds() {
        // Regression: the deferred-wrap path in print() did `cursor.row += 1`
        // without a `num_rows - 1` guard. With a scroll region whose bottom is
        // not the last row, wrapping on the last row indexed past the viewport
        // (panic: index == len). The fix clamps like grid.rs does.
        let mut t = Terminal::new(3, 5);
        // Scroll region bottom = row index 1 (NOT the last row index 2).
        t.grid_mut().set_scroll_region(1, 2); // 1-based → top 0, bottom 1; resets cursor
        t.grid_mut().cursor.row = 2; // last row, outside the scroll region
        t.grid_mut().cursor.col = 4; // last column
        t.grid_mut().cursor.wrap_pending = true;
        t.process(b"x"); // triggers the deferred-wrap path
        assert!(
            t.grid().cursor.row < t.grid().num_rows,
            "cursor row {} escaped the viewport of {} rows",
            t.grid().cursor.row,
            t.grid().num_rows
        );
    }

    // ── Scroll ───────────────────────────────────────────────────

    #[test]
    fn scroll_up_csi() {
        let mut t = Terminal::new(5, 4);
        for i in 0..5 {
            t.grid_mut().viewport[i].cells[0].character =
                char::from_digit(i as u32 + 1, 10).unwrap();
        }
        t.process(b"\x1b[S");
        assert_eq!(t.grid().cell(0, 0).character, '2');
        assert_eq!(t.grid().cell(4, 0).character, ' ');
    }

    // ── OSC ──────────────────────────────────────────────────────

    #[test]
    fn osc_set_title() {
        let mut t = term();
        t.process(b"\x1b]0;mytitle\x07");
        assert_eq!(t.title(), "mytitle");
    }

    #[test]
    fn osc_133_marker() {
        let mut t = term();
        t.process(b"\x1b]133;A\x07");
        assert_eq!(t.shell_markers().len(), 1);
        assert_eq!(t.shell_markers()[0], ShellMarker::PromptStart);
    }

    #[test]
    fn osc_133_end_with_exit_code() {
        let mut t = term();
        t.process(b"\x1b]133;D;42\x07");
        assert_eq!(t.shell_markers().len(), 1);
        assert_eq!(
            t.shell_markers()[0],
            ShellMarker::CommandEnd { exit_code: 42 }
        );
    }

    // ── Command blocks (OSC 133 → BlockTracker) ───────────────────

    #[test]
    fn osc133_lifecycle_produces_block() {
        let mut t = term();
        // Prompt start → AtPrompt + integration ready.
        t.process(b"\x1b]133;A\x07");
        assert!(t.block_tracker().bootstrap_ready());
        assert_eq!(t.block_tracker().phase(), ShellPhase::AtPrompt);

        // Prompt + command render during AtPrompt → NOT captured as output.
        t.process(b"$ ls -la\r");
        // Command start (preexec): snapshot the command row.
        t.process(b"\x1b]133;B\x07");
        assert_eq!(t.block_tracker().phase(), ShellPhase::CommandExecuting);
        // Output start + streaming output.
        t.process(b"\x1b]133;C\x07");
        t.process(b"file1\nfile2\n");
        // Command end → finalize.
        t.process(b"\x1b]133;D;0\x07");

        let blocks = t.block_tracker().blocks();
        assert_eq!(blocks.len(), 1);
        let b = &blocks[0];
        assert_eq!(b.command, "$ ls -la", "command = prompt row at 133;B");
        assert_eq!(b.output, "file1\nfile2\n", "output captured B..D");
        assert_eq!(b.exit_code, Some(0));
        assert_eq!(t.block_tracker().phase(), ShellPhase::AtPrompt);
    }

    #[test]
    fn alt_screen_output_is_not_captured() {
        let mut t = term();
        t.process(b"\x1b]133;A\x07");
        t.process(b"$ run vim\r");
        t.process(b"\x1b]133;B\x07"); // CommandExecuting — capture active
                                      // Enter the alternate screen (DEC 1049): a full-screen app takes over.
        t.process(b"\x1b[?1049h");
        assert!(t.is_alt_screen_active());
        // This content belongs to the full-screen app — it must NOT leak into
        // the block's output snapshot.
        t.process(b"VIM FULLSCREEN CONTENT\nmore lines\n");
        // Leave the alternate screen and end the command.
        t.process(b"\x1b[?1049l");
        t.process(b"\x1b]133;D;0\x07");

        let b = &t.block_tracker().blocks()[0];
        assert!(
            !b.output.contains("VIM FULLSCREEN CONTENT"),
            "alt-screen content leaked into block output: {:?}",
            b.output
        );
        assert_eq!(b.exit_code, Some(0));
    }

    // ── Terminal query responses (DA/DSR/size) ──────────────────

    #[test]
    fn dsr_reports_cursor_position_1_based() {
        let mut t = term();
        t.grid_mut().cursor.row = 4;
        t.grid_mut().cursor.col = 9;
        t.process(b"\x1b[6n");
        // 1-based → row 5, col 10.
        assert_eq!(t.take_response(), b"\x1b[5;10R");
    }

    #[test]
    fn da1_and_text_area_size_responses() {
        let mut t = term(); // 24×80
        t.process(b"\x1b[c"); // DA1
        t.process(b"\x1b[18t"); // text-area size in chars
        let resp = t.take_response();
        let s = String::from_utf8_lossy(&resp);
        assert!(
            s.contains("\x1b[?62") && s.contains('c'),
            "DA1 missing: {s}"
        );
        assert!(s.contains("\x1b[8;24;80t"), "size report missing: {s}");
    }

    #[test]
    fn da2_secondary_device_attributes() {
        let mut t = term();
        t.process(b"\x1b[>c");
        let resp = t.take_response();
        let s = String::from_utf8_lossy(&resp);
        assert!(s.starts_with("\x1b[>") && s.ends_with('c'), "DA2: {s}");
    }

    // ── Colors ───────────────────────────────────────────────────

    #[test]
    fn truecolor_fg() {
        let mut t = term();
        t.process(b"\x1b[38;2;255;128;0mX");
        assert_eq!(
            t.grid().cell(0, 0).fg,
            CellColor::Rgb(Color::rgb(255, 128, 0))
        );
    }

    #[test]
    fn indexed_256_color() {
        let mut t = term();
        t.process(b"\x1b[38;5;196mX");
        assert_eq!(t.grid().cell(0, 0).fg, CellColor::Palette(196));
    }

    #[test]
    fn bright_foreground() {
        let mut t = term();
        t.process(b"\x1b[91mX");
        assert_eq!(t.grid().cell(0, 0).fg, CellColor::Palette(9)); // SGR 91 → palette[9]
    }

    #[test]
    fn background_color() {
        let mut t = term();
        t.process(b"\x1b[44mX");
        assert_eq!(t.grid().cell(0, 0).bg, CellColor::Palette(4)); // SGR 44 → palette[4]
    }

    // ── Insert/delete ────────────────────────────────────────────

    #[test]
    fn delete_chars_csi() {
        let mut t = term();
        t.process(b"ABCDE\x1b[1;1H\x1b[1P");
        assert_eq!(t.grid().cell(0, 0).character, 'B');
        assert_eq!(t.grid().cell(0, 1).character, 'C');
        assert_eq!(t.grid().cell(0, 4).character, ' ');
    }

    // ── Private modes ────────────────────────────────────────────

    #[test]
    fn dec_private_cursor_keys() {
        let mut t = term();
        t.process(b"\x1b[?1h");
        assert!(t.app_cursor_keys);
        t.process(b"\x1b[?1l");
        assert!(!t.app_cursor_keys);
    }

    #[test]
    fn bracketed_paste_mode() {
        let mut t = term();
        t.process(b"\x1b[?2004h");
        assert!(t.bracketed_paste);
        t.process(b"\x1b[?2004l");
        assert!(!t.bracketed_paste);
    }

    // ── Full reset ───────────────────────────────────────────────

    #[test]
    fn ris_full_reset() {
        let mut t = term();
        t.process(b"\x1b[31mX\x1b[?1h");
        assert!(t.app_cursor_keys);
        t.process(b"\x1bc");
        assert!(!t.app_cursor_keys);
        assert_eq!(t.grid().cell(0, 0).character, ' ');
    }

    // ── Palette ──────────────────────────────────────────────────

    #[test]
    fn palette_init_has_256_colors() {
        let t = term();
        assert_eq!(t.palette[0], Color::rgb(0, 0, 0));
        assert_eq!(t.palette[7], Color::rgb(229, 229, 229));
        assert_eq!(t.palette[16], Color::rgb(0, 0, 0));
        assert_eq!(t.palette[232], Color::rgb(8, 8, 8));
    }

    // ── Wrap behavior ────────────────────────────────────────────

    #[test]
    fn wrap_at_line_end() {
        let mut t = Terminal::new(5, 4);
        t.process(b"ABCD"); // fills 4 cols, sets wrap_pending
        assert!(t.grid().cursor.wrap_pending);
        t.process(b"E"); // should wrap to next line
        assert_eq!(t.grid().cursor.row, 1);
        assert_eq!(t.grid().cursor.col, 1);
        assert_eq!(t.grid().cell(1, 0).character, 'E');
    }

    #[test]
    fn print_after_narrowing_resize_does_not_drop_chars() {
        // Simulates the resize race: shell wrote a full-width row at 10 cols,
        // then the grid was narrowed to 5 while the PTY SIGWINCH is still in
        // flight. Force the cursor past the new last column and print more —
        // those chars must wrap onto the next line, not be discarded.
        let mut t = Terminal::new(5, 10);
        t.process(b"0123456789"); // fills row 0 at width 10
        assert!(t.grid().cursor.wrap_pending);
        // Narrow the grid (rewrap merges the single logical line into two).
        t.resize(5, 5);
        // The shell has NOT learned the new size yet and keeps printing at
        // the cursor position, which now points past the last column.
        // Force cursor to column 7 (past the new num_cols=5) as the old
        // shell output would, then print — must wrap, not drop.
        t.grid_mut().cursor.col = 7;
        t.grid_mut().cursor.wrap_pending = false;
        t.process(b"XY");
        // No character should be lost: both 'X' and 'Y' must appear.
        let found_x = (0..t.grid().num_rows)
            .any(|r| (0..t.grid().num_cols).any(|c| t.grid().cell(r, c).character == 'X'));
        let found_y = (0..t.grid().num_rows)
            .any(|r| (0..t.grid().num_cols).any(|c| t.grid().cell(r, c).character == 'Y'));
        assert!(found_x, "'X' must not be dropped on resize race");
        assert!(found_y, "'Y' must not be dropped on resize race");
    }

    // ── Scroll region ────────────────────────────────────────────

    #[test]
    fn scroll_region_set_and_reset() {
        let mut t = term();
        t.process(b"\x1b[5;20r");
        assert_eq!(t.grid().scroll_region(), (4, 19));
        t.process(b"\x1b[r"); // reset
        assert_eq!(t.grid().scroll_region(), (0, 23));
    }

    // ── v0.5 editor takeover: OSC 7 + effective mode + submit ──────

    use crate::input::{build_submit_bytes, InputMode};

    #[test]
    fn osc7_sets_cwd() {
        let mut t = Terminal::new(24, 80);
        t.process(b"\x1b]7;file://macbook.local/Users/me/proj\x1b\\");
        assert_eq!(t.cwd(), Some("/Users/me/proj"));
    }

    #[test]
    fn osc7_localhost_host_strips_correctly() {
        let mut t = Terminal::new(24, 80);
        t.process(b"\x1b]7;file://localhost/tmp\x1b\\");
        assert_eq!(t.cwd(), Some("/tmp"));
    }

    #[test]
    fn osc7_malformed_is_ignored() {
        let mut t = Terminal::new(24, 80);
        t.process(b"\x1b]7;not-a-uri\x1b\\");
        assert_eq!(t.cwd(), None);
    }

    #[test]
    fn osc8_hyperlink_tags_cells_and_resolves_url() {
        // OSC 8 ; ; URI ST → start hyperlink. Subsequent printed cells get
        // the HYPERLINK flag and resolve to URI via the registry.
        let mut t = Terminal::new(24, 80);
        t.process(b"\x1b]8;;https://weft.dev/a\x1b\\");
        t.process(b"link");
        t.process(b"\x1b]8;;\x1b\\"); // close
        t.process(b"plain");

        // The four cells of "link" should be tagged; "plain" should not.
        let g = t.grid();
        for (i, _) in "link".chars().enumerate() {
            assert!(
                g.cell(0, i)
                    .flags
                    .contains(crate::grid::CellFlags::HYPERLINK),
                "cell {i} of 'link' should be HYPERLINK"
            );
        }
        // 'p' of "plain" is at col 4 (after 4 chars of "link").
        assert!(
            !g.cell(0, 4)
                .flags
                .contains(crate::grid::CellFlags::HYPERLINK),
            "cell after link close should NOT be HYPERLINK"
        );

        // Cmd+Click resolution: cell (0, 0) → URL.
        assert_eq!(t.hyperlinks().url_at(0, 0), Some("https://weft.dev/a"));
        assert_eq!(t.hyperlinks().url_at(0, 3), Some("https://weft.dev/a"));
        // After close, the plain cell has no URL.
        assert_eq!(t.hyperlinks().url_at(0, 4), None);
    }

    #[test]
    fn osc8_dedups_identical_urls() {
        let mut t = Terminal::new(24, 80);
        // Two consecutive links to the same URL — registry dedups.
        t.process(b"\x1b]8;;https://weft.dev/x\x1b\\");
        t.process(b"a");
        t.process(b"\x1b]8;;\x1b\\");
        t.process(b"\x1b]8;;https://weft.dev/x\x1b\\");
        t.process(b"b");
        t.process(b"\x1b]8;;\x1b\\");

        assert_eq!(t.hyperlinks().url_at(0, 0), Some("https://weft.dev/x"));
        assert_eq!(t.hyperlinks().url_at(0, 1), Some("https://weft.dev/x"));
    }

    #[test]
    fn osc8_cell_map_clears_on_scroll() {
        // When content scrolls the viewport, the (row, col) → id map is
        // invalidated. The HYPERLINK flag stays on cells (visual underline
        // persists) but click resolution returns None — MVP trade-off.
        let mut t = Terminal::new(3, 80);
        t.process(b"\x1b]8;;https://weft.dev/s\x1b\\");
        t.process(b"link\n");
        t.process(b"\x1b]8;;\x1b\\");
        // Emit enough lines to force a scroll.
        t.process(b"line1\nline2\nline3");
        // After scrolling, no cells should resolve to URLs.
        for row in 0..3 {
            for col in 0..10 {
                assert!(
                    t.hyperlinks().url_at(row, col).is_none(),
                    "hyperlink at ({row},{col}) should be cleared after scroll"
                );
            }
        }
    }

    #[test]
    fn osc9_git_branch_set_then_cleared_on_prompt_start() {
        // Shell hook emits OSC 9;git=<branch> only inside a repo.
        let mut t = Terminal::new(24, 80);
        // First prompt inside a git repo: hook sends OSC 9;git=main.
        t.process(b"\x1b]9;git=main\x07");
        assert_eq!(t.git_branch(), Some("main"));
        // Next prompt: precmd runs again. PromptStart (133;A) must clear
        // the branch first; if the cwd is still a repo the hook re-emits,
        // but if it's now a non-git dir nothing arrives and the label
        // correctly disappears.
        t.process(b"\x1b]133;A\x07"); // no OSC 9 this time (non-git dir)
        assert_eq!(t.git_branch(), None, "branch cleared on PromptStart");
        // Returning to a git repo re-establishes it.
        t.process(b"\x1b]9;git=develop\x07");
        assert_eq!(t.git_branch(), Some("develop"));
    }

    #[test]
    fn editor_takes_over_only_after_bootstrap_at_prompt() {
        let mut t = Terminal::new(24, 80);
        // Not integrated yet.
        assert_eq!(t.effective_input_mode(), InputMode::Passthrough);
        // Bootstrap + AtPrompt.
        t.process(b"\x1b]133;A\x07");
        assert_eq!(t.effective_input_mode(), InputMode::Editor);
    }

    #[test]
    fn editor_hidden_in_alt_screen() {
        let mut t = Terminal::new(24, 80);
        t.process(b"\x1b]133;A\x07");
        assert_eq!(t.effective_input_mode(), InputMode::Editor);
        t.process(b"\x1b[?1049h"); // enter alt screen
        assert_eq!(t.effective_input_mode(), InputMode::Passthrough);
    }

    #[test]
    fn submit_command_builds_bytes_and_blocks_editor() {
        let mut t = Terminal::new(24, 80);
        t.process(b"\x1b]133;A\x07");
        for c in "ls -la".chars() {
            t.editor_mut().buffer.insert_char(c);
        }
        let bytes = t.submit_command();
        assert_eq!(bytes, build_submit_bytes("ls -la", false));
        // Editor cleared and blocked until 133;B.
        assert_eq!(t.editor().text(), "");
        assert_eq!(t.effective_input_mode(), InputMode::Passthrough);
    }

    #[test]
    fn tab_in_command_output_is_captured_as_spaces() {
        // Regression: macOS `ls` separates columns with tabs, which are C0
        // controls (handled by `execute`, not `print`). Without mirroring the
        // tab advance into the block's captured output, the block view showed
        // filenames concatenated (`Cargo.lockCargo.toml...`).
        let mut t = Terminal::new(24, 80);
        t.process(b"\x1b]133;A\x07"); // bootstrap + AtPrompt
        t.process(b"\x1b]133;B\x07"); // command start — capture on
        t.process(b"a\tb\tc");
        t.process(b"\x1b]133;D;0\x07"); // command end — finalize
        let blocks = t.block_tracker().blocks();
        assert_eq!(blocks.len(), 1);
        let out = &blocks[0].output;
        assert!(!out.contains('\t'), "tab leaked into output: {out:?}");
        let a = out.find('a').unwrap();
        let b = out.find('b').unwrap();
        assert!(
            b > a + 1,
            "a and b are adjacent (tabs not expanded): {out:?}"
        );
    }

    #[test]
    fn command_133b_uses_editor_command_not_grid_snapshot() {
        let mut t = Terminal::new(24, 80);
        t.process(b"\x1b]133;A\x07");
        for c in "real-cmd".chars() {
            t.editor_mut().buffer.insert_char(c);
        }
        t.submit_command();
        // Shell "executes": emits 133;B. The tracker should record the editor
        // command, not the (empty) grid prompt row.
        t.process(b"\x1b]133;B\x07");
        t.process(b"\x1b]133;D;0\x07");
        let blocks = t.block_tracker().blocks();
        assert_eq!(blocks.last().unwrap().command, "real-cmd");
    }

    #[test]
    fn passthrough_133b_uses_grid_snapshot() {
        let mut t = Terminal::new(24, 80);
        t.process(b"\x1b]133;A\x07");
        // No editor submit → passthrough path → command from grid snapshot.
        // Print a fake prompt+command line, then 133;B.
        t.process(b"$ echo hi");
        t.process(b"\x1b]133;B\x07");
        t.process(b"\x1b]133;D;0\x07");
        let cmd = t.block_tracker().blocks().last().unwrap().command.clone();
        assert!(cmd.contains("echo hi"), "got {cmd:?}");
    }
}

#[cfg(test)]
mod reflow_cjk_tests {
    use super::*;

    /// A full-width char that would straddle the right margin makes the print
    /// path wrap before placing it, leaving the last cell as a never-written
    /// default. Reflow must NOT bake that trailing blank into the logical line
    /// as a real space — otherwise each resize inserts a phantom space between
    /// CJK characters (compounding). See Grid::resize content_end trimming.
    #[test]
    fn wide_wrap_blank_is_not_baked_into_logical_line() {
        let mut t = Terminal::new(40, 94);
        // Long line with CJK that lands at a wrap boundary when narrowed.
        let echo = "andylee@host dir % cd /tmp && rm -f 待产手册_v1.0.md && touch 待产手册_v1.0.md && ls -lt 待产手册_v1.0.md\n";
        t.process(echo.as_bytes());
        let ls = "-rw-r--r--@ 1 user  wheel  0 Jun 16 12:00 待产手册_v1.0.md\n";
        t.process(ls.as_bytes());
        t.process(b"andylee@host /tmp % ");

        // Wrap (narrow) then unwrap (wide). CJK runs must stay contiguous —
        // no phantom space between characters.
        for w in [50, 64, 94, 40, 94, 55, 94] {
            t.resize(40, w);
        }

        let g = t.grid();
        for row in 0..g.num_rows {
            let mut run = String::new();
            let mut in_cjk = false;
            for col in 0..g.num_cols {
                let c = g.cell(row, col);
                if c.flags.contains(CellFlags::WIDE_SPACER) {
                    continue;
                }
                let wide = unicode_width::UnicodeWidthChar::width(c.character).unwrap_or(0) > 1;
                if wide {
                    run.push(c.character);
                    in_cjk = true;
                } else if in_cjk && c.character == ' ' {
                    // A space immediately after/within a CJK run is the bug.
                    panic!("phantom space in CJK run {run:?} at row {row}");
                } else if in_cjk {
                    break;
                }
            }
        }
    }
}

/// v1.0 P1.5: Performance benchmarks for the VT parse + grid write pipeline.
///
/// Run with: `cargo test -p weft_core --lib -- --ignored --nocapture`
///
/// These feed realistic PTY byte streams through `Terminal::process()` and
/// measure wall-clock time. They cover the scenarios from the v1.0 plan's
/// Phase 1.5 validation table so we can decide whether the optional C2
/// (custom VT parser) / C3 (multithreading) tasks are still needed after
/// the high-ROI B0-B3 + C1 optimizations.
///
/// All benchmarks use a 24×80 terminal (the v1.0 default) with 10K-line
/// scrollback, matching the plan's test conditions.
#[cfg(test)]
mod perf_benchmarks {
    use super::*;
    use std::time::Instant;

    /// Build a realistic `seq 1 N` byte stream, wrapped in OSC 133 shell-
    /// integration markers the way zsh would emit them. The marker prefix
    /// forces the parser through the escape path once per command, then the
    /// bulk numeric output hits the ASCII fast path.
    fn seq_output(n: u32) -> Vec<u8> {
        let mut buf = Vec::with_capacity(n as usize * 8);
        // 133;A (prompt start) + 133;B (command start) + 133;C (output start)
        buf.extend_from_slice(b"\x1b]133;A\x07andylee@host ~ % \x1b]133;B\x07seq 1 ");
        buf.extend_from_slice(n.to_string().as_bytes());
        buf.extend_from_slice(b"\n\x1b]133;C\x07");
        for i in 1..=n {
            buf.extend_from_slice(i.to_string().as_bytes());
            buf.push(b'\n');
        }
        // 133;D;0 (command end, exit 0) + 133;A (next prompt start)
        buf.extend_from_slice(b"\x1b]133;D;0\x07\x1b]133;A\x07andylee@host ~ % ");
        buf
    }

    /// Build a realistic `ls -la /usr/bin` byte stream: ~1000 entries with
    /// file-mode / owner / size / date / name columns. Mixes ASCII fast path
    /// (the columns) with occasional ANSI color escapes (like `ls --color`).
    fn ls_output(entry_count: usize) -> Vec<u8> {
        let mut buf = Vec::with_capacity(entry_count * 80);
        buf.extend_from_slice(
            b"\x1b]133;A\x07andylee@host ~ % \x1b]133;B\x07ls -la /usr/bin\n\x1b]133;C\x07",
        );
        buf.extend_from_slice(b"total 12345\n");
        for i in 0..entry_count {
            // Mode owner group size date name — printable ASCII bulk.
            // Insert a color SGR every 10 lines to exercise escape handling.
            if i % 10 == 0 {
                buf.extend_from_slice(b"\x1b[1;32m"); // bold green
            }
            let line = format!(
                "-rwxr-xr-x  1 root  wheel  {:>6} Jan  1 12:00 bin_tool_{:04}\n",
                10000 + i,
                i
            );
            buf.extend_from_slice(line.as_bytes());
            if i % 10 == 0 {
                buf.extend_from_slice(b"\x1b[0m"); // reset
            }
        }
        buf.extend_from_slice(b"\x1b]133;D;0\x07\x1b]133;A\x07andylee@host ~ % ");
        buf
    }

    /// Measure `Terminal::process` throughput for a given byte stream.
    /// Returns (elapsed_ms, bytes, rows_written).
    fn bench(label: &str, bytes: &[u8]) -> (f64, usize) {
        let mut t = Terminal::with_scrollback(24, 80, 10_000);
        let start = Instant::now();
        t.process(bytes);
        let elapsed = start.elapsed();
        let ms = elapsed.as_secs_f64() * 1000.0;
        let bytes_len = bytes.len();
        // Throughput in MB/s.
        let mbps = (bytes_len as f64 / 1_048_576.0) / (elapsed.as_secs_f64().max(1e-9));
        println!("  {label:<28} {ms:>8.2} ms  | {bytes_len:>8} bytes | {mbps:>7.1} MB/s");
        (ms, bytes_len)
    }

    /// `seq 1 10000` — plan target: < 50ms (Warp ~10ms).
    #[test]
    #[ignore]
    fn bench_seq_10000() {
        println!("\n=== Phase 1.5 benchmark: seq 1 10000 (target < 50ms) ===");
        let bytes = seq_output(10_000);
        let (ms, _) = bench("seq 1 10000", &bytes);
        assert!(ms < 50.0, "seq 1 10000 took {ms:.2}ms, target < 50ms");
    }

    /// `seq 1 100000` — plan target: < 300ms (Warp ~50ms).
    #[test]
    #[ignore]
    fn bench_seq_100000() {
        println!("\n=== Phase 1.5 benchmark: seq 1 100000 (target < 300ms) ===");
        let bytes = seq_output(100_000);
        let (ms, _) = bench("seq 1 100000", &bytes);
        assert!(ms < 300.0, "seq 1 100000 took {ms:.2}ms, target < 300ms");
    }

    /// `ls -la /usr/bin` style (~1000 entries) — plan target: < 20ms (Warp ~5ms).
    #[test]
    #[ignore]
    fn bench_ls_usr_bin() {
        println!("\n=== Phase 1.5 benchmark: ls -la /usr/bin (target < 20ms) ===");
        let bytes = ls_output(1000);
        let (ms, _) = bench("ls -la /usr/bin (1000 entries)", &bytes);
        assert!(ms < 20.0, "ls output took {ms:.2}ms, target < 20ms");
    }

    /// Pure ASCII bulk (no escapes) — measures the C1 fast-path ceiling.
    #[test]
    #[ignore]
    fn bench_pure_ascii_100k() {
        println!("\n=== Phase 1.5 benchmark: pure ASCII bulk (no escapes) ===");
        let bytes: Vec<u8> = (0..100_000)
            .flat_map(|i| format!("{i}\n").into_bytes())
            .collect();
        let (ms, _) = bench("pure ASCII 100k lines", &bytes);
        // No escape overhead at all — should be faster than seq_output which
        // has OSC 133 markers. Use as a ceiling reference.
        let _ = ms;
    }

    /// Color-heavy output (SGR every line) — measures escape-sequence overhead.
    #[test]
    #[ignore]
    fn bench_color_output() {
        println!("\n=== Phase 1.5 benchmark: colored output (SGR per line) ===");
        let mut bytes = Vec::with_capacity(80_000);
        bytes.extend_from_slice(b"\x1b]133;A\x07% \x1b]133;B\x07color-test\n\x1b]133;C\x07");
        for i in 0..5000 {
            // Alternate colors to exercise SGR parsing.
            let color = match i % 6 {
                0 => b"\x1b[31m", // red
                1 => b"\x1b[32m", // green
                2 => b"\x1b[33m", // yellow
                3 => b"\x1b[34m", // blue
                4 => b"\x1b[35m", // magenta
                _ => b"\x1b[36m", // cyan
            };
            bytes.extend_from_slice(color);
            bytes.extend_from_slice(format!("line {i:04} with color\n").as_bytes());
            bytes.extend_from_slice(b"\x1b[0m");
        }
        let (ms, _) = bench("colored 5k lines", &bytes);
        let _ = ms;
    }
}
