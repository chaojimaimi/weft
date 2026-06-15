//! VT100/VT520 escape sequence parser.
//!
//! Wraps the `vte` crate with a `Terminal` struct that implements
//! `vte::Perform` to translate escape sequences into Grid operations.

use crate::grid::{CellFlags, CellWidth, Color, Cursor, CursorStyle, Grid};
use crate::input::MouseProtocol;

/// Current text attributes applied to newly printed characters.
/// Updated by SGR (CSI m) sequences, consumed by `print()`.
#[derive(Clone, Debug)]
pub struct Attrs {
    pub fg: Color,
    pub bg: Color,
    pub flags: CellFlags,
}

impl Default for Attrs {
    fn default() -> Self {
        Self {
            fg: Color::DEFAULT_FG,
            bg: Color::DEFAULT_BG,
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
}

impl Terminal {
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            grid: Grid::new(rows, cols),
            parser: vte::Parser::new(),
            attrs: Attrs::default(),
            title: String::new(),
            shell_markers: Vec::new(),
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
        }
    }

    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    pub fn grid_mut(&mut self) -> &mut Grid {
        &mut self.grid
    }

    /// Whether the alternate screen buffer is currently active.
    pub fn is_alt_screen_active(&self) -> bool {
        self.alt_active
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
        } else {
            std::mem::swap(&mut self.grid, &mut self.alt_grid);
            if save_cursor_and_clear {
                if let Some(c) = self.saved_cursor.take() {
                    self.grid.cursor = c;
                }
            }
            self.alt_active = false;
        }
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

    /// Feed raw bytes from PTY through the vte parser.
    /// Each byte is advanced through the parser, which calls back
    /// into our Perform implementation.
    pub fn process(&mut self, bytes: &[u8]) {
        // We need to temporarily move the parser out to avoid double &mut self.
        // Take ownership, advance, then put it back.
        let mut parser = std::mem::replace(&mut self.parser, vte::Parser::new());
        for &byte in bytes {
            parser.advance(self, byte);
        }
        self.parser = parser;
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

    // ── Palette initialization ───────────────────────────────────

    /// Initialize the xterm-256color palette.
    fn init_palette() -> [Color; 256] {
        let mut palette = [Color::DEFAULT_FG; 256];

        // Standard 16 colors
        let standard = [
            (0, 0, 0),       // 0 Black
            (205, 0, 0),     // 1 Red
            (0, 205, 0),     // 2 Green
            (205, 205, 0),   // 3 Yellow
            (0, 0, 238),     // 4 Blue
            (205, 0, 205),   // 5 Magenta
            (0, 205, 205),   // 6 Cyan
            (229, 229, 229), // 7 White
            (127, 127, 127), // 8 Bright Black
            (255, 0, 0),     // 9 Bright Red
            (0, 255, 0),     // 10 Bright Green
            (255, 255, 0),   // 11 Bright Yellow
            (92, 92, 255),   // 12 Bright Blue
            (255, 0, 255),   // 13 Bright Magenta
            (0, 255, 255),   // 14 Bright Cyan
            (255, 255, 255), // 15 Bright White
        ];
        for (i, (r, g, b)) in standard.iter().enumerate() {
            palette[i] = Color::rgb(*r, *g, *b);
        }

        // 16-231: 6x6x6 color cube
        let cube_values = [0, 95, 135, 175, 215, 255];
        let mut idx = 16;
        for r in &cube_values {
            for g in &cube_values {
                for b in &cube_values {
                    palette[idx] = Color::rgb(*r, *g, *b);
                    idx += 1;
                }
            }
        }

        // 232-255: grayscale ramp
        for i in 0u8..24 {
            let v = 8 + i * 10;
            palette[232 + i as usize] = Color::rgb(v, v, v);
        }

        palette
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

        // Flatten each param group's first value into a single list.
        // Semicolons → separate groups, colons → sub-params within one group.
        let vals: Vec<u16> = params
            .iter()
            .map(|sub| sub.first().copied().unwrap_or(0))
            .collect();

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
                    self.attrs.fg = self.palette[v as usize - 30];
                }
                // 256-color / truecolor foreground
                38 => {
                    if let Some((color, skip)) = self.parse_sgr_color(&vals, i + 1) {
                        self.attrs.fg = color;
                        i += skip;
                    }
                }
                // Default foreground
                39 => self.attrs.fg = Color::DEFAULT_FG,
                // Standard background 40-47
                40..=47 => {
                    self.attrs.bg = self.palette[v as usize - 40];
                }
                // 256-color / truecolor background
                48 => {
                    if let Some((color, skip)) = self.parse_sgr_color(&vals, i + 1) {
                        self.attrs.bg = color;
                        i += skip;
                    }
                }
                // Default background
                49 => self.attrs.bg = Color::DEFAULT_BG,
                // Bright foreground 90-97
                90..=97 => {
                    self.attrs.fg = self.palette[v as usize - 90 + 8];
                }
                // Bright background 100-107
                100..=107 => {
                    self.attrs.bg = self.palette[v as usize - 100 + 8];
                }
                _ => {
                    tracing::trace!(v, "unhandled SGR param");
                }
            }
            i += 1;
        }
    }

    /// Parse SGR color starting after the 38/48 marker.
    /// Returns `(Color, skip_count)` where skip_count is how many extra
    /// values (beyond the 38/48) were consumed.
    fn parse_sgr_color(&self, vals: &[u16], start: usize) -> Option<(Color, usize)> {
        let kind = vals.get(start).copied()?;
        match kind {
            // Indexed 256-color: 38;5;N
            5 => {
                let idx = vals.get(start + 1).copied()?.min(255) as usize;
                Some((self.palette[idx], 2))
            }
            // Truecolor: 38;2;R;G;B
            2 => {
                let r = vals.get(start + 1).copied()? as u8;
                let g = vals.get(start + 2).copied()? as u8;
                let b = vals.get(start + 3).copied()? as u8;
                Some((Color::rgb(r, g, b), 4))
            }
            _ => None,
        }
    }

    /// Handle DEC private mode set/reset (CSI ? <n> h/l).
    fn handle_dec_private_mode(&mut self, mode: u16, set: bool) {
        match mode {
            1 => self.app_cursor_keys = set, // DECCKM
            6 => self.origin_mode = set,     // DECOM
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
        // New PTY output: snap back to the live viewport so fresh content is
        // visible instead of a stale scrollback view.
        self.grid.scroll_offset = 0;

        // Handle deferred wrap
        if self.grid.cursor.wrap_pending {
            self.grid.cursor.wrap_pending = false;
            self.grid.cursor.col = 0;
            let (_, bottom) = self.grid.scroll_region();
            if self.grid.cursor.row == bottom {
                self.grid.scroll_up(1);
            } else if self.grid.cursor.row < self.grid.num_rows - 1 {
                self.grid.cursor.row += 1;
            }
            // Mark the previous row as wrapped so reflow can merge it
            // back when the terminal widens.
            if self.grid.cursor.row > 0 {
                self.grid.viewport[self.grid.cursor.row - 1].wrapped = true;
            }
        }

        let width = if unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) > 1 {
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
                self.grid.scroll_up(1);
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

        if col < num_cols {
            let cell = &mut self.grid.viewport[row].cells[col];
            cell.character = c;
            cell.fg = self.attrs.fg;
            cell.bg = self.attrs.bg;
            cell.flags = self.attrs.flags | CellFlags::DIRTY;
            cell.width = width;

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
            0x09 => self.grid.advance_tab(1),
            0x0A..=0x0C => {
                // LF, VT, FF → move to next line (CR+LF on Unix terminals).
                // The raw VT `index()` only moves down; Unix terminals
                // treat LF as newline (carriage return + index).
                self.grid.carriage_return();
                self.grid.index();
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
                self.grid.goto(row, col);
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

            // Scrolling
            'S' => {
                let n = param(params, 0, 1);
                self.grid.scroll_up(if n == 0 { 1 } else { n as usize });
            }
            'T' => {
                if intermediates.is_empty() {
                    self.grid.scroll_down(param(params, 0, 1) as usize);
                }
            }

            // Scroll region
            'r' => {
                if params.is_empty() {
                    self.grid.reset_scroll_region();
                } else {
                    let top = param(params, 0, 1) as usize;
                    let bottom = param(params, 1, 0) as usize;
                    if bottom == 0 {
                        self.grid.reset_scroll_region();
                    } else {
                        self.grid.set_scroll_region(top, bottom);
                    }
                }
            }

            // Cursor save/restore (SCO style)
            's' => self.grid.save_cursor(),
            'u' => self.grid.restore_cursor(),

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
            (&[], 0x44) => self.grid.index(),          // IND
            (&[], 0x4D) => self.grid.reverse_index(),  // RI
            (&[], 0x45) => {
                // NEL
                self.grid.index();
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
            "133" => {
                if params.len() > 1 {
                    match params[1] {
                        b"A" => self.shell_markers.push(ShellMarker::PromptStart),
                        b"B" => self.shell_markers.push(ShellMarker::CommandStart),
                        b"C" => self.shell_markers.push(ShellMarker::CommandOutputStart),
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
        assert_eq!(t.grid().cell(0, 0).fg, t.palette[1]); // red
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
        assert_eq!(t.grid().cell(0, 0).fg, t.palette[1]);
        assert_eq!(t.grid().cell(0, 1).fg, Color::DEFAULT_FG);
    }

    #[test]
    fn sgr_empty_params_means_reset() {
        let mut t = term();
        t.process(b"\x1b[1;31mA\x1b[mB");
        assert!(t.grid().cell(0, 0).flags.contains(CellFlags::BOLD));
        assert!(!t.grid().cell(0, 1).flags.contains(CellFlags::BOLD));
        assert_eq!(t.grid().cell(0, 1).fg, Color::DEFAULT_FG);
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

    // ── Colors ───────────────────────────────────────────────────

    #[test]
    fn truecolor_fg() {
        let mut t = term();
        t.process(b"\x1b[38;2;255;128;0mX");
        assert_eq!(t.grid().cell(0, 0).fg, Color::rgb(255, 128, 0));
    }

    #[test]
    fn indexed_256_color() {
        let mut t = term();
        t.process(b"\x1b[38;5;196mX");
        assert_eq!(t.grid().cell(0, 0).fg, t.palette[196]);
    }

    #[test]
    fn bright_foreground() {
        let mut t = term();
        t.process(b"\x1b[91mX");
        assert_eq!(t.grid().cell(0, 0).fg, t.palette[9]);
    }

    #[test]
    fn background_color() {
        let mut t = term();
        t.process(b"\x1b[44mX");
        assert_eq!(t.grid().cell(0, 0).bg, t.palette[4]);
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

    // ── Scroll region ────────────────────────────────────────────

    #[test]
    fn scroll_region_set_and_reset() {
        let mut t = term();
        t.process(b"\x1b[5;20r");
        assert_eq!(t.grid().scroll_region(), (4, 19));
        t.process(b"\x1b[r"); // reset
        assert_eq!(t.grid().scroll_region(), (0, 23));
    }
}
