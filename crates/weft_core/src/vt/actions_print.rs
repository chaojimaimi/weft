//! `print write path (`deferred_wrap_newline` / `print_ascii_run`)` bodies for the Terminal facade. vt/mod.rs keeps the struct,
//! `process()`, and core accessors (v1.13.8 S2 zero-behavior file-budget
//! split; `impl Terminal` cross-file blocks per the screen_exit /
//! kitty_keyboard precedent). Bodies moved verbatim.
use super::Terminal;
use crate::blocks::CapturedStyle;
use crate::blocks::ShellPhase;
use crate::grid::{terminal_char_width, CellFlags, CellWidth};

impl Terminal {
    /// v1.11.16 (Fix B2): shared deferred-wrap newline. Clears `wrap_pending`,
    /// moves to the next row (scrolling at the region bottom), and marks the
    /// previous row `wrapped` ONLY when the cursor actually advanced or a
    /// scroll occurred. At the last physical row outside the scroll region
    /// (DECSTBM status-line layout) neither arm fires — the cursor stays put
    /// and NO row may be marked (the old code mismarked `row-1` as a
    /// continuation of unrelated content).
    pub(super) fn deferred_wrap_newline(&mut self) {
        self.grid.cursor.wrap_pending = false;
        self.grid.cursor.col = 0;
        let (_, bottom) = self.grid.scroll_region();
        let mut advanced = false;
        if self.grid.cursor.row == bottom {
            self.scroll_grid_up(1);
            advanced = true; // content moved up; cursor.row-1 is the overflowed row
        } else if self.grid.cursor.row < self.grid.num_rows - 1 {
            self.grid.cursor.row += 1;
            advanced = true;
        }
        if advanced && self.grid.cursor.row > 0 {
            self.grid.viewport[self.grid.cursor.row - 1].wrapped = true;
        }
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
    pub(super) fn print_ascii_run(&mut self, bytes: &[u8]) {
        debug_assert!(!bytes.is_empty());
        let phase = self.block_tracker.phase();
        // Snap back to live viewport for new content — same gate as print().
        // Skipped while the user is browsing primary-screen TUI history so a
        // redraw cannot yank the viewport back to the live bottom.
        if phase != ShellPhase::AtPrompt && !self.primary_history_view() {
            self.grid.set_scroll_offset(0);
        }
        let num_cols = self.grid.num_cols;
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
                self.deferred_wrap_newline();
            }

            let col = self.grid.cursor.col;

            // v1.0 P1.5-C2: resize race — cursor.col may be >= num_cols after
            // a narrowing resize. Reset to col 0 and advance row (same as the
            // old per-char bounds check, but done once per row boundary).
            let col = if col >= num_cols {
                self.deferred_wrap_newline();
                self.grid.cursor.col
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

            // Batch capture to the active sink — one push instead of N pushes.
            // v1.7.0-A: capture the current VT SGR attrs for the whole ASCII
            // run — all bytes share one style since the fast path only fires
            // when no SGR change occurred mid-run.
            // FIX_ORPHAN_PARSE_ERROR_OUTPUT: sink selection lives in
            // staging.rs — in-flight capture while CommandExecuting, preexec
            // staging between editor submit and 133;B.
            // WP1 D2: one capture_style() feeds both sinks — neither mutates
            // `self.attrs`, so the second snapshot was byte-identical.
            {
                let style = self.capture_style();
                self.capture_print_ascii_run(chunk, style);
                self.capture_primary_screen_interrupt_ascii(chunk, style);
            }

            // The contiguous ASCII overwrite can only split a pre-existing
            // wide glyph at its two boundaries. Pairs fully inside the range
            // are overwritten together, so repairing the first and last cell
            // preserves the row invariant without adding per-cell overhead.
            self.grid.viewport[row].clear_wide_pair_at(col);
            self.grid.viewport[row].clear_wide_pair_at(col + count - 1);

            // v1.13.10 FIX (audit P0): stale-hyperlink unlink used to collect
            // positions into a fixed `[usize; 128]` inside the write loop and
            // indexed past the end on wide terminals (cols > 128 with a
            // >=129-cell stale-link row) — an index-out-of-bounds panic on the
            // PTY parse hot path. Unlink BEFORE the overwrite instead: the
            // write resets flags, so stale positions can't be recovered after
            // it, and unlinking only touches the registry + extras (never
            // cells), so doing it eagerly per cell is equivalent within the
            // frame. The branch is entered whenever no OSC 8 link is active
            // and the hyperlink cell_map is non-empty (a stale link anywhere
            // on screen); whether THIS row segment has stale links is decided
            // per cell by the flags check below.
            if need_unlink_check {
                for c in col..col + count {
                    if self.grid.viewport[row].cells[c]
                        .flags
                        .contains(CellFlags::HYPERLINK)
                    {
                        self.hyperlinks.unlink_cell(row, c);
                        self.grid.viewport[row].extras.set_hyperlink(c, None);
                    }
                }
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
                        // v1.11.3 (PLAN_v1113 §1.1): carry underline
                        // style/color (cells are field-assigned, not
                        // default-constructed — stale values would survive).
                        cells[c].underline_style = self.attrs.underline_style;
                        cells[c].underline_color = self.attrs.underline_color;
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
                } else {
                    // Fast path: no active OSC 8 link. (Stale links were
                    // unlinked above when need_unlink_check was set — v1.13.10
                    // FIX merged the old bounded-array branch into this path.)
                    for (i, &b) in chunk.iter().enumerate() {
                        let c = col + i;
                        cells[c].character = b as char;
                        cells[c].fg = fg;
                        cells[c].bg = bg;
                        cells[c].flags = base_flags;
                        cells[c].width = CellWidth::Half;
                        // v1.11.3 (PLAN_v1113 §1.1): carry underline
                        // style/color (see hyperlink path).
                        cells[c].underline_style = self.attrs.underline_style;
                        cells[c].underline_color = self.attrs.underline_color;
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
            // v1.15.3 P1a: the whole segment [col, col + count) was written
            // content — one O(1) occupancy extension per row segment.
            debug_assert!(count >= 1);
            self.grid.viewport[row].cell_written(col + count - 1);

            self.grid.cursor.col += count;
            offset += count;

            // Handle end-of-row wrap.
            if self.grid.cursor.col >= num_cols {
                self.grid.cursor.wrap_pending = true;
                self.grid.cursor.col = num_cols - 1;
            }
        }
    }

    /// WP1 D1: UTF-8 wide-char fast lane — bulk-write a run of consecutive
    /// D3-admitted width-2 chars (see [`Self::scan_wide_run`]) directly to
    /// the grid, bypassing vte's per-byte state machine for the dominant
    /// CJK case. Engaged by `Terminal::process` under the same guards as the
    /// ASCII lane (osc_guard Ground + parser_in_ground_state +
    /// !suppress_joined_scalar).
    ///
    /// Per-char `print()` preamble work collapses to run level:
    /// - scroll snap once per run (R2-P1: run-level, same shape as
    ///   `print_ascii_run`'s head — `set_scroll_offset(0)` is idempotent and
    ///   the gate inputs cannot change mid-run because any ESC/C0/OSC byte
    ///   terminates the scan);
    /// - capture sinks once per run (run-level style: no SGR change is
    ///   possible inside the run by lane definition);
    /// - suppress / RI / width-0 branches are unreachable (D3 admission);
    ///   the entry state is re-asserted below.
    pub(super) fn print_wide_run(&mut self, s: &str) {
        debug_assert!(!s.is_empty());
        debug_assert!(!self.suppress_joined_scalar);
        let phase = self.block_tracker.phase();
        // Snap back to the live viewport for new content — same gate and
        // shape as print_ascii_run (and per-char print()).
        if phase != ShellPhase::AtPrompt && !self.primary_history_view() {
            self.grid.set_scroll_offset(0);
        }
        // Run-level capture: both sink families, one style, char-aware
        // batching (WP1 P0: never the byte/char-conflated ASCII path).
        {
            let style = self.capture_style();
            self.capture_print_wide_run(s, style);
            self.capture_primary_screen_interrupt_str(s, style);
        }
        for c in s.chars() {
            // D3 admission guarantees width 2; the shared core derives the
            // same CellWidth the slow path would for this char.
            self.print_char_write(c, terminal_char_width(c));
        }
    }

    /// WP1 D1/D3: scan a run of consecutive chars admissible to the wide
    /// lane. Returns the admitted prefix as a `&str` — the scanner is the
    /// single UTF-8 validation point and its borrow carries that evidence to
    /// `print_wide_run` (rust-review P2-1: no re-validation, no expect).
    /// `None` when the first position is not admitted.
    ///
    /// Termination is conservative: a char failing the D3 set, a malformed
    /// byte, or an incomplete trailing sequence ends the scan WITHOUT
    /// consuming it — the vte slow path owns malformed and cross-batch
    /// partial-sequence handling, and the lane must never swallow a byte the
    /// parser would need (run-interrupt chars stay verbatim for the slow
    /// path).
    pub(super) fn scan_wide_run(bytes: &[u8]) -> Option<&str> {
        // Bound the candidate first, WITHOUT validation: admitted chars are
        // 3-4 byte sequences (width-2 scalars live at U+1100 and above —
        // 2-byte leads are width 0/1, overlong 0xC0/0xC1 and invalid
        // 0xF5..=0xFF leads excluded), so the walk jumps sequence-by-sequence
        // and stops at the first byte that cannot belong to one (ASCII, C0,
        // 2-byte leads, stray continuations, 0xF5+). Jumping keeps the walk
        // O(run) even on hostile continuation-byte floods (immediate break).
        let mut bound = 0usize;
        while bound < bytes.len() {
            let len = match bytes[bound] {
                0xE0..=0xEF => 3,
                0xF0..=0xF4 => 4,
                _ => break,
            };
            bound = (bound + len).min(bytes.len());
        }
        if bound == 0 {
            return None;
        }
        // The scanner is the single UTF-8 validation point: validate the
        // bounded candidate once and lend the `&str` onward. A malformed
        // byte or truncated tail shrinks the domain to the valid prefix
        // (valid_up_to is a char boundary of a valid prefix by definition,
        // so the second conversion cannot fail; `.ok()?` keeps it
        // expect-free nonetheless) — the remainder stays verbatim with the
        // slow path.
        let valid = match std::str::from_utf8(&bytes[..bound]) {
            Ok(s) => s,
            Err(e) => std::str::from_utf8(&bytes[..e.valid_up_to()]).ok()?,
        };
        // Admission walk over the already-validated text: stop at the first
        // char outside the D3 set.
        let mut end = 0usize;
        for c in valid.chars() {
            if !Self::wide_run_char(c) {
                break;
            }
            end += c.len_utf8();
        }
        if end == 0 {
            return None;
        }
        valid.get(..end)
    }

    /// WP1 D3 admission predicate: terminal width exactly 2, Regional
    /// Indicators excluded (pair-extension logic stays slow-path). Every
    /// other exemption family — emoji modifier, ZWJ, VS, combining marks,
    /// zero-width, width-1 multibyte — measures 0 or 1 via
    /// `terminal_char_width` and is rejected by the width test itself.
    fn wide_run_char(c: char) -> bool {
        !matches!(c, '\u{1f1e6}'..='\u{1f1ff}') && terminal_char_width(c) == 2
    }

    /// WP1 D1: the `print()` write body from the deferred-wrap handling
    /// onward (perform.rs), extracted verbatim so the vte slow path and the
    /// wide-char fast lane share one implementation. `scalar_width` is
    /// computed by the caller's preamble (print() needs it for the width-0
    /// branch; the fast lane recomputes it per char).
    pub(super) fn print_char_write(&mut self, c: char, scalar_width: usize) {
        // Handle deferred wrap
        if self.grid.cursor.wrap_pending {
            self.deferred_wrap_newline();
        }

        // v1.0 perf: ASCII fast path — all ASCII chars are width 1.
        // This skips the unicode_width lookup for the common case (terminal
        // output is predominantly ASCII: digits, letters, punctuation).
        // For 58KB of `seq` output, this saves ~58000 lookup calls.
        let width = if scalar_width > 1 {
            CellWidth::Full
        } else {
            CellWidth::Half
        };

        let num_cols = self.grid.num_cols;
        let row = self.grid.cursor.row;
        let col = self.grid.cursor.col;

        // Wide char straddling line boundary → wrap first
        if width == CellWidth::Full && col + 1 >= num_cols {
            self.deferred_wrap_newline();
            let new_row = self.grid.cursor.row;
            let new_col = self.grid.cursor.col;
            self.prepare_primary_screen_exit_row_overwrite();
            self.include_primary_screen_viewport_row(new_row);
            // T3 review P1: bisected far-half registry accounting (see the
            // standard site) — the fresh pair below re-tags new_col and
            // new_col+1; only a slot beyond them can keep a stale entry.
            let next_bisected = new_col + 2 < num_cols
                && self.grid.viewport[new_row].cells[new_col + 1].width == CellWidth::Full
                && self.grid.viewport[new_row].cells[new_col + 2]
                    .flags
                    .contains(CellFlags::WIDE_SPACER);
            self.grid.viewport[new_row].clear_wide_pair_at(new_col);
            self.grid.viewport[new_row].clear_wide_pair_at(new_col + 1);
            // v1.6.0 review C1: clear orphaned grapheme extras for wide-char overwrite.
            self.grid.viewport[new_row].extras.clear_grapheme(new_col);
            if new_col + 1 < num_cols {
                self.grid.viewport[new_row]
                    .extras
                    .clear_grapheme(new_col + 1);
            }
            {
                let cells = &mut self.grid.viewport[new_row].cells;
                let cell = &mut cells[new_col];
                cell.character = c;
                cell.fg = self.attrs.fg;
                cell.bg = self.attrs.bg;
                cell.flags = self.attrs.flags | CellFlags::DIRTY;
                cell.width = CellWidth::Full;
                // v1.11.3 (§1.1): carry underline fields (field-assigned).
                cell.underline_style = self.attrs.underline_style;
                cell.underline_color = self.attrs.underline_color;
                // OSC 8: tag the wrapped wide-char cell with the active hyperlink.
                if self.active_hyperlink_id.is_some() {
                    cell.flags |= CellFlags::HYPERLINK;
                }

                if new_col + 1 < num_cols {
                    let spacer = &mut cells[new_col + 1];
                    spacer.character = ' ';
                    spacer.flags = CellFlags::WIDE_SPACER;
                    spacer.width = CellWidth::Half;
                }
            }
            // v1.6.1: update hyperlink registry + RowExtras after releasing the
            // `cells` borrow (same pattern as the standard print path above).
            if let Some(id) = self.active_hyperlink_id {
                self.hyperlinks.link_cell(new_row, new_col, id);
                self.grid.viewport[new_row]
                    .extras
                    .set_hyperlink(new_col, Some(id));
            } else {
                self.hyperlinks.unlink_cell(new_row, new_col);
                self.grid.viewport[new_row]
                    .extras
                    .set_hyperlink(new_col, None);
            }
            // T3 (P3-2 ruling): the spacer half carries the lead's hyperlink
            // so viewport rows hover like materialized history rows (both
            // halves of the glyph are clickable). Plain assignment above
            // already dropped any stale HYPERLINK flag; registry/extras are
            // cleaned symmetrically with the lead.
            if new_col + 1 < num_cols {
                if let Some(id) = self.active_hyperlink_id {
                    self.hyperlinks.link_cell(new_row, new_col + 1, id);
                    self.grid.viewport[new_row]
                        .extras
                        .set_hyperlink(new_col + 1, Some(id));
                    self.grid.viewport[new_row].cells[new_col + 1].flags |= CellFlags::HYPERLINK;
                } else {
                    self.hyperlinks.unlink_cell(new_row, new_col + 1);
                    self.grid.viewport[new_row]
                        .extras
                        .set_hyperlink(new_col + 1, None);
                }
            }
            if next_bisected {
                self.hyperlinks.unlink_cell(new_row, new_col + 2);
            }
            self.grid.viewport[new_row].mark_dirty(new_col);
            if new_col + 1 < num_cols {
                self.grid.viewport[new_row].mark_dirty(new_col + 1);
            }
            // v1.15.3 P1a: content write funnels — lead cell + spacer both
            // hold non-default content (spacer carries WIDE_SPACER).
            self.grid.viewport[new_row].cell_written(new_col);
            if new_col + 1 < num_cols {
                self.grid.viewport[new_row].cell_written(new_col + 1);
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
            self.deferred_wrap_newline();
            row = self.grid.cursor.row;
            col = 0;
        }
        self.prepare_primary_screen_exit_row_overwrite();
        self.include_primary_screen_viewport_row(row);

        // T3 review P1: a narrow write that bisects a wide pair clears the
        // far half's cell + extras (inside clear_wide_pair_at) — its
        // registry entry must follow. Detected before the clears mutate the
        // row.
        let lead_bisected = col > 0
            && self.grid.viewport[row].cells[col]
                .flags
                .contains(CellFlags::WIDE_SPACER)
            && self.grid.viewport[row].cells[col - 1].width == CellWidth::Full;
        let spacer_bisected = width == CellWidth::Half
            && col + 1 < num_cols
            && self.grid.viewport[row].cells[col].width == CellWidth::Full
            && self.grid.viewport[row].cells[col + 1]
                .flags
                .contains(CellFlags::WIDE_SPACER);

        {
            self.grid.viewport[row].clear_wide_pair_at(col);
            if width == CellWidth::Full {
                self.grid.viewport[row].clear_wide_pair_at(col + 1);
            }
            // v1.6.0 review C1: clear orphaned grapheme extras before overwriting.
            // A non-combining scalar replaces the cell's content; any multi-scalar
            // grapheme cluster stored in extras becomes stale and must be removed
            // (preserving hyperlink_id, which is handled below).
            self.grid.viewport[row].extras.clear_grapheme(col);
            if width == CellWidth::Full {
                self.grid.viewport[row].extras.clear_grapheme(col + 1);
            }
            // v1.0 fix (CJK splat): before writing a new char, clear any
            // existing wide-char pair that this write would bisect. Without
            // this, overwriting part of a double-width CJK char (or its
            // WIDE_SPACER second cell) leaves an orphaned half that manifests
            // as merged fragments and phantom spaces after a TUI app (vim)
            // scrolls via IL/DL and reprints shorter/different content. This
            // is the standard "wide splat" handling xterm/Alacritty perform.
            {
                let cells = &mut self.grid.viewport[row].cells;
                let cell = &mut cells[col];
                cell.character = c;
                cell.fg = self.attrs.fg;
                cell.bg = self.attrs.bg;
                cell.flags = self.attrs.flags | CellFlags::DIRTY;
                cell.width = width;
                // v1.11.3 (§1.1): carry underline style/color (see wrap site).
                cell.underline_style = self.attrs.underline_style;
                cell.underline_color = self.attrs.underline_color;

                // OSC 8: tag the cell with HYPERLINK and record its (row,col)→id
                // in the registry side-map. When the active hyperlink is None
                // (overwriting a previously tagged cell), drop the side-map entry
                // and clear the flag so the underline disappears.
                if self.active_hyperlink_id.is_some() {
                    cell.flags |= CellFlags::HYPERLINK;
                } else if cell.flags.contains(CellFlags::HYPERLINK) {
                    cell.flags.remove(CellFlags::HYPERLINK);
                }

                if width == CellWidth::Full && col + 1 < num_cols {
                    let spacer = &mut cells[col + 1];
                    spacer.character = ' ';
                    spacer.flags = CellFlags::WIDE_SPACER;
                    spacer.width = CellWidth::Half;
                }
            }
            // v1.6.1: update hyperlink registry + RowExtras after releasing the
            // `cells` borrow so we don't violate the borrow checker (cells and
            // extras are different fields of the same Row, but the borrow
            // checker can't prove non-aliasing through `&mut self.grid.viewport[row]`).
            if let Some(id) = self.active_hyperlink_id {
                self.hyperlinks.link_cell(row, col, id);
                self.grid.viewport[row].extras.set_hyperlink(col, Some(id));
            } else {
                self.hyperlinks.unlink_cell(row, col);
                self.grid.viewport[row].extras.set_hyperlink(col, None);
            }
            // T3 (P3-2 ruling): spacer carries the lead's hyperlink — see the
            // wrap-first site above. `spacer.flags = WIDE_SPACER` in the write
            // above already cleared a stale HYPERLINK flag; the id/registry
            // follow the lead symmetrically.
            if width == CellWidth::Full && col + 1 < num_cols {
                if let Some(id) = self.active_hyperlink_id {
                    self.hyperlinks.link_cell(row, col + 1, id);
                    self.grid.viewport[row]
                        .extras
                        .set_hyperlink(col + 1, Some(id));
                    self.grid.viewport[row].cells[col + 1].flags |= CellFlags::HYPERLINK;
                } else {
                    self.hyperlinks.unlink_cell(row, col + 1);
                    self.grid.viewport[row].extras.set_hyperlink(col + 1, None);
                }
            }
            // T3 review P1: registry entries of bisected wide-pair halves die
            // with the pair (extras already handled by clear_wide_pair_at).
            if lead_bisected {
                self.hyperlinks.unlink_cell(row, col - 1);
            }
            if spacer_bisected {
                self.hyperlinks.unlink_cell(row, col + 1);
            }
            self.grid.viewport[row].mark_dirty(col);
            if width == CellWidth::Full && col + 1 < num_cols {
                self.grid.viewport[row].mark_dirty(col + 1);
            }
            // v1.15.3 P1a: content write funnels — the printed cell (and the
            // spacer half of a wide glyph) are non-default content.
            self.grid.viewport[row].cell_written(col);
            if width == CellWidth::Full && col + 1 < num_cols {
                self.grid.viewport[row].cell_written(col + 1);
            }
        }
        self.grid.cursor.col += width as usize;

        if self.grid.cursor.col >= num_cols {
            self.grid.cursor.wrap_pending = true;
            self.grid.cursor.col = num_cols - 1;
        }
    }
}

impl Terminal {
    /// Current capture style — the batch-sink twin of the per-char sites
    /// in perform.rs (v1.11.3: includes underline style/color carriers).
    pub(crate) fn capture_style(&self) -> CapturedStyle {
        CapturedStyle::from_attrs(
            self.attrs.fg,
            self.attrs.bg,
            self.attrs.flags,
            self.attrs.underline_style,
            self.attrs.underline_color,
        )
    }

    /// WP1 D1: advance the UTF-8 collector shadow for the byte vte just
    /// consumed on the slow path. Transitions mirror utf8parse 0.2.2 exactly
    /// (lead ranges, the E0/ED/F0/F4 restricted second bytes, and the abort
    /// rule: any byte outside the expected range prints U+FFFD and is
    /// swallowed). Runs only for slow-path bytes — the lanes engage solely
    /// when `utf8_collect_rem == 0`, so a lane-consumed byte cannot perturb
    /// the collector.
    pub(super) fn note_slow_path_utf8_byte(&mut self, b: u8) {
        if self.utf8_collect_rem > 0 {
            if b >= self.utf8_collect_min && b <= self.utf8_collect_max {
                self.utf8_collect_rem -= 1;
                self.utf8_collect_min = 0x80;
                self.utf8_collect_max = 0xBF;
            } else {
                // Abort: one U+FFFD printed, `b` swallowed, back to Ground.
                self.utf8_collect_rem = 0;
            }
            return;
        }
        match b {
            0xC2..=0xDF => {
                self.utf8_collect_rem = 1;
                self.utf8_collect_min = 0x80;
                self.utf8_collect_max = 0xBF;
            }
            0xE0 => {
                self.utf8_collect_rem = 2;
                self.utf8_collect_min = 0xA0;
                self.utf8_collect_max = 0xBF;
            }
            0xED => {
                self.utf8_collect_rem = 2;
                self.utf8_collect_min = 0x80;
                self.utf8_collect_max = 0x9F;
            }
            0xE1..=0xEC | 0xEE..=0xEF => {
                self.utf8_collect_rem = 2;
                self.utf8_collect_min = 0x80;
                self.utf8_collect_max = 0xBF;
            }
            0xF0 => {
                self.utf8_collect_rem = 3;
                self.utf8_collect_min = 0x90;
                self.utf8_collect_max = 0xBF;
            }
            0xF4 => {
                self.utf8_collect_rem = 3;
                self.utf8_collect_min = 0x80;
                self.utf8_collect_max = 0x8F;
            }
            0xF1..=0xF3 => {
                self.utf8_collect_rem = 3;
                self.utf8_collect_min = 0x80;
                self.utf8_collect_max = 0xBF;
            }
            _ => {}
        }
    }

    /// WP1 D1: fast-lane enable switch. Constant `true` in production (the
    /// compiler folds it away together with the `#[cfg(test)]` field); test
    /// builds can force the pure-vte slow path for the differential harness
    /// (plan §3 — the forcing mechanism is test-build-only).
    #[cfg(test)]
    pub(super) fn print_fast_lanes_enabled(&self) -> bool {
        self.print_fast_lanes_for_test
    }

    #[cfg(not(test))]
    pub(super) fn print_fast_lanes_enabled(&self) -> bool {
        true
    }

    /// WP1 D1 (test-only): force both print fast lanes (ASCII + wide) off so
    /// a terminal exercises the pure vte per-byte path — the differential
    /// baseline the fuzz harness compares against.
    #[cfg(test)]
    pub(crate) fn force_slow_path_for_test(&mut self) {
        self.print_fast_lanes_for_test = false;
    }
}
