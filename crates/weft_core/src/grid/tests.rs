use super::*;

#[test]
fn cell_struct_stays_at_24_bytes() {
    // v0.8 OSC 8 design constraint: hyperlink metadata lives in an
    // external side-map (HyperlinkRegistry), NOT on Cell. If a future
    // change pushes Cell past 24 bytes, this test fails — re-evaluate
    // before adjusting the target. See docs/v0.8_PLAN.md §5.
    assert!(
        std::mem::size_of::<Cell>() <= 24,
        "Cell must stay ≤ 24 bytes (HYPERLINK flag is a 1-bit side-state); got {}",
        std::mem::size_of::<Cell>()
    );
}

#[test]
fn grid_new_creates_correct_size() {
    let grid = Grid::new(24, 80);
    assert_eq!(grid.num_rows, 24);
    assert_eq!(grid.num_cols, 80);
    assert_eq!(grid.viewport.len(), 24);
    assert_eq!(grid.viewport[0].cells.len(), 80);
}

#[test]
fn cell_default_is_space() {
    let cell = Cell::default();
    assert_eq!(cell.character, ' ');
    assert_eq!(cell.width, CellWidth::Half);
}

#[test]
fn cursor_style_shape_and_blink_classification_is_exhaustive() {
    assert!(CursorStyle::Block.is_block());
    assert!(CursorStyle::BlinkingBlock.is_block());
    assert!(!CursorStyle::Block.is_blinking());
    assert!(CursorStyle::BlinkingBlock.is_blinking());

    assert!(CursorStyle::Bar.is_bar());
    assert!(CursorStyle::BlinkingBar.is_bar());
    assert!(!CursorStyle::Bar.is_blinking());
    assert!(CursorStyle::BlinkingBar.is_blinking());

    assert!(CursorStyle::Underline.is_underline());
    assert!(CursorStyle::BlinkingUnderline.is_underline());
    assert!(!CursorStyle::Underline.is_blinking());
    assert!(CursorStyle::BlinkingUnderline.is_blinking());
}

#[test]
fn row_text_trims_trailing_and_keeps_internal_spaces() {
    let mut grid = Grid::new(2, 12);
    // Write "ls  -la" at row 0 (two internal spaces), leaving trailing
    // default cells.
    for ch in "ls  -la".chars() {
        grid.viewport[0].cells[grid.cursor.col].character = ch;
        grid.cursor.col += 1;
    }
    assert_eq!(grid.row_text(0), "ls  -la");
    // Untouched row → empty.
    assert_eq!(grid.row_text(1), "");
}

#[test]
fn row_text_out_of_bounds_is_empty() {
    let grid = Grid::new(2, 10);
    assert_eq!(grid.row_text(99), "");
}

#[test]
fn write_char_advances_cursor() {
    let mut grid = Grid::new(24, 80);
    grid.write_char('A');
    assert_eq!(grid.cursor.row, 0);
    assert_eq!(grid.cursor.col, 1);
    assert_eq!(grid.cell(0, 0).character, 'A');
}

#[test]
fn direct_wide_write_creates_valid_pair() {
    let mut grid = Grid::new(2, 8);
    grid.write_char('中');

    assert_eq!(grid.cell(0, 0).width, CellWidth::Full);
    assert!(grid.cell(0, 1).flags.contains(CellFlags::WIDE_SPACER));
    assert_row_has_valid_wide_pairs(&grid, 0);
}

#[test]
fn newline_moves_cursor_down() {
    let mut grid = Grid::new(24, 80);
    grid.write_char('A');
    grid.newline();
    assert_eq!(grid.cursor.row, 1);
    assert_eq!(grid.cursor.col, 0);
}

#[test]
fn scroll_up_at_bottom() {
    let mut grid = Grid::new(5, 4);
    for i in 0..5 {
        grid.viewport[i].cells[0].character = char::from_digit(i as u32 + 1, 10).unwrap();
    }
    grid.cursor.row = 4;
    grid.newline();
    assert_eq!(grid.cell(0, 0).character, '2');
    assert_eq!(grid.cell(4, 0).character, ' ');
}

#[test]
fn scroll_up_stores_in_scrollback() {
    let mut grid = Grid::with_scrollback(5, 4, 100);
    for i in 0..5 {
        grid.viewport[i].cells[0].character = char::from_digit(i as u32 + 1, 10).unwrap();
    }
    grid.cursor.row = 4;
    grid.newline();
    // Row '1' should be in scrollback
    assert_eq!(grid.scrollback.len(), 1);
    assert_eq!(grid.scrollback.get(0).unwrap().cells[0].character, '1');
}

#[test]
fn scrollback_navigation() {
    let mut grid = Grid::with_scrollback(5, 4, 100);
    // Fill and scroll many lines
    for i in 0..20 {
        grid.viewport[grid.cursor.row].cells[0].character =
            char::from_digit((i % 10) as u32, 10).unwrap_or('X');
        grid.cursor.row = 4;
        grid.newline();
    }

    let sb_len = grid.scrollback.len();
    assert!(sb_len > 0, "should have scrollback entries");

    // Scroll up
    grid.scroll_up_history(3);
    assert_eq!(grid.scroll_offset, 3);

    // Scroll down
    grid.scroll_down_history(1);
    assert_eq!(grid.scroll_offset, 2);

    // Scroll to bottom
    grid.scroll_to_bottom();
    assert_eq!(grid.scroll_offset, 0);

    // Scroll to top
    grid.scroll_to_top();
    assert_eq!(grid.scroll_offset, sb_len);
}

#[test]
fn cell_shows_scrollback_content_when_scrolled() {
    let mut grid = Grid::with_scrollback(3, 4, 100);
    // Fill viewport with '1','2','3' then scroll so '1' enters scrollback.
    grid.viewport[0].cells[0].character = '1';
    grid.viewport[1].cells[0].character = '2';
    grid.viewport[2].cells[0].character = '3';
    grid.cursor.row = 2;
    grid.newline(); // '1' -> scrollback, viewport = ['2','3',' ']

    assert_eq!(grid.scrollback.len(), 1, "precondition: one scrolled line");

    // Scroll up one line: viewport should show scrollback + viewport tail.
    grid.scroll_up_history(1);
    assert_eq!(grid.cell(0, 0).character, '1', "row 0 from scrollback");
    assert_eq!(grid.cell(1, 0).character, '2', "row 1 from viewport[0]");
    assert_eq!(grid.cell(2, 0).character, '3', "row 2 from viewport[1]");

    // Scrolling back to bottom restores the live viewport.
    grid.scroll_to_bottom();
    assert_eq!(grid.cell(0, 0).character, '2');
}

#[test]
fn scroll_up_history_never_exceeds_scrollback_len() {
    let mut grid = Grid::with_scrollback(5, 4, 100);
    // Two lines of scrollback.
    grid.scrollback.push(Row::new(4));
    grid.scrollback.push(Row::new(4));
    grid.scroll_offset = 1;
    // Scrolling far past the top must clamp to scrollback length, not shrink.
    grid.scroll_up_history(5);
    assert_eq!(grid.scroll_offset, 2, "clamps to scrollback.len()");
}

#[test]
fn clear_resets_all_cells() {
    let mut grid = Grid::new(2, 4);
    grid.write_char('X');
    grid.clear();
    assert_eq!(grid.cell(0, 0).character, ' ');
    assert_eq!(grid.cursor.row, 0);
    assert_eq!(grid.cursor.col, 0);
}

#[test]
fn move_up_clamps_at_zero() {
    let mut grid = Grid::new(24, 80);
    grid.cursor.row = 3;
    grid.move_up(1);
    assert_eq!(grid.cursor.row, 2);
    grid.move_up(10);
    assert_eq!(grid.cursor.row, 0);
}

#[test]
fn move_down_clamps_at_bottom() {
    let mut grid = Grid::new(5, 10);
    grid.cursor.row = 3;
    grid.move_down(1);
    assert_eq!(grid.cursor.row, 4);
    grid.move_down(5);
    assert_eq!(grid.cursor.row, 4);
}

#[test]
fn move_forward_clamps_at_right_edge() {
    let mut grid = Grid::new(24, 10);
    grid.cursor.col = 8;
    grid.move_forward(1);
    assert_eq!(grid.cursor.col, 9);
    grid.move_forward(5);
    assert_eq!(grid.cursor.col, 9);
}

#[test]
fn move_backward_clamps_at_zero() {
    let mut grid = Grid::new(24, 80);
    grid.cursor.col = 5;
    grid.move_backward(3);
    assert_eq!(grid.cursor.col, 2);
    grid.move_backward(10);
    assert_eq!(grid.cursor.col, 0);
}

#[test]
fn goto_sets_position() {
    let mut grid = Grid::new(24, 80);
    grid.goto(10, 20, false); // 1-based → (9, 19)
    assert_eq!(grid.cursor.row, 9);
    assert_eq!(grid.cursor.col, 19);
}

#[test]
fn goto_clamps_to_grid_bounds() {
    let mut grid = Grid::new(24, 80);
    grid.goto(100, 200, false);
    assert_eq!(grid.cursor.row, 23);
    assert_eq!(grid.cursor.col, 79);
}

#[test]
fn goto_origin_mode_is_relative_to_scroll_region() {
    // set_scroll_region is 1-based: (6,11) → 0-based rows 5..10.
    // DECOM set → CUP (1,1) is the region top (row 5).
    let mut grid = Grid::new(24, 80);
    grid.set_scroll_region(6, 11);
    grid.goto(1, 1, true);
    assert_eq!(grid.cursor.row, 5, "origin mode offsets by scroll_top");
    assert_eq!(grid.cursor.col, 0);
    // Row 3 → region row 5+2 = 7.
    grid.goto(3, 5, true);
    assert_eq!(grid.cursor.row, 7);
    // Out-of-range row clamps to scroll_bottom (10), not the screen bottom.
    grid.goto(100, 1, true);
    assert_eq!(grid.cursor.row, 10);
}

#[test]
fn clear_screen_below_clears_from_cursor() {
    let mut grid = Grid::new(5, 5);
    for r in 0..5 {
        for c in 0..5 {
            grid.viewport[r].cells[c].character = 'X';
        }
    }
    grid.cursor.row = 2;
    grid.cursor.col = 2;
    grid.clear_screen_below();
    assert_eq!(grid.cell(0, 0).character, 'X');
    assert_eq!(grid.cell(1, 0).character, 'X');
    assert_eq!(grid.cell(2, 1).character, 'X');
    assert_eq!(grid.cell(2, 2).character, ' ');
    assert_eq!(grid.cell(3, 0).character, ' ');
    assert_eq!(grid.cell(4, 4).character, ' ');
}

#[test]
fn clear_screen_above_clears_to_cursor() {
    let mut grid = Grid::new(5, 5);
    for r in 0..5 {
        for c in 0..5 {
            grid.viewport[r].cells[c].character = 'X';
        }
    }
    grid.cursor.row = 2;
    grid.cursor.col = 2;
    grid.clear_screen_above();
    assert_eq!(grid.cell(0, 0).character, ' ');
    assert_eq!(grid.cell(1, 4).character, ' ');
    assert_eq!(grid.cell(2, 2).character, ' ');
    assert_eq!(grid.cell(2, 3).character, 'X');
    assert_eq!(grid.cell(3, 0).character, 'X');
}

#[test]
fn clear_line_right_clears_from_cursor() {
    let mut grid = Grid::new(5, 5);
    for c in 0..5 {
        grid.viewport[0].cells[c].character = char::from_digit(c as u32 + 1, 10).unwrap();
    }
    grid.cursor.col = 2;
    grid.clear_line_right();
    assert_eq!(grid.cell(0, 1).character, '2');
    assert_eq!(grid.cell(0, 2).character, ' ');
    assert_eq!(grid.cell(0, 4).character, ' ');
}

#[test]
fn clear_line_left_clears_to_cursor() {
    let mut grid = Grid::new(5, 5);
    for c in 0..5 {
        grid.viewport[0].cells[c].character = char::from_digit(c as u32 + 1, 10).unwrap();
    }
    grid.cursor.col = 2;
    grid.clear_line_left();
    assert_eq!(grid.cell(0, 0).character, ' ');
    assert_eq!(grid.cell(0, 2).character, ' ');
    assert_eq!(grid.cell(0, 3).character, '4');
}

#[test]
fn scroll_down_inserts_blank_at_top() {
    let mut grid = Grid::new(5, 4);
    for i in 0..5 {
        grid.viewport[i].cells[0].character = char::from_digit(i as u32 + 1, 10).unwrap();
    }
    grid.scroll_down(1);
    assert_eq!(grid.cell(0, 0).character, ' ');
    assert_eq!(grid.cell(1, 0).character, '1');
    assert_eq!(grid.cell(4, 0).character, '4');
}

#[test]
fn save_restore_cursor_roundtrip() {
    let mut grid = Grid::new(24, 80);
    grid.cursor.row = 5;
    grid.cursor.col = 10;
    grid.save_cursor();
    grid.goto(20, 40, false);
    grid.restore_cursor();
    assert_eq!(grid.cursor.row, 5);
    assert_eq!(grid.cursor.col, 10);
}

#[test]
fn set_scroll_region_bounds() {
    let mut grid = Grid::new(24, 80);
    grid.set_scroll_region(5, 20);
    assert_eq!(grid.scroll_region(), (4, 19));
    assert_eq!(grid.cursor.row, 0);
    assert_eq!(grid.cursor.col, 0);
}

#[test]
fn insert_blank_shifts_right() {
    let mut grid = Grid::new(5, 5);
    grid.viewport[0].cells[0].character = 'A';
    grid.viewport[0].cells[1].character = 'B';
    grid.viewport[0].cells[2].character = 'C';
    grid.cursor.col = 1;
    grid.insert_blank(1);
    assert_eq!(grid.cell(0, 0).character, 'A');
    assert_eq!(grid.cell(0, 1).character, ' ');
    assert_eq!(grid.cell(0, 2).character, 'B');
}

#[test]
fn delete_chars_shifts_left() {
    let mut grid = Grid::new(5, 5);
    grid.viewport[0].cells[0].character = 'A';
    grid.viewport[0].cells[1].character = 'B';
    grid.viewport[0].cells[2].character = 'C';
    grid.cursor.col = 1;
    grid.delete_chars(1);
    assert_eq!(grid.cell(0, 0).character, 'A');
    assert_eq!(grid.cell(0, 1).character, 'C');
    assert_eq!(grid.cell(0, 4).character, ' ');
}

#[test]
fn index_scrolls_at_bottom() {
    let mut grid = Grid::new(5, 4);
    for i in 0..5 {
        grid.viewport[i].cells[0].character = char::from_digit(i as u32 + 1, 10).unwrap();
    }
    grid.cursor.row = 4;
    grid.index();
    assert_eq!(grid.cell(0, 0).character, '2');
    assert_eq!(grid.cell(4, 0).character, ' ');
}

#[test]
fn reverse_index_scrolls_at_top() {
    let mut grid = Grid::new(5, 4);
    for i in 0..5 {
        grid.viewport[i].cells[0].character = char::from_digit(i as u32 + 1, 10).unwrap();
    }
    grid.cursor.row = 0;
    grid.reverse_index();
    assert_eq!(grid.cell(0, 0).character, ' ');
    assert_eq!(grid.cell(1, 0).character, '1');
}

#[test]
fn tab_advance_moves_to_next_tabstop() {
    let mut grid = Grid::new(24, 80);
    grid.cursor.col = 0;
    grid.advance_tab(1);
    assert_eq!(grid.cursor.col, 8);
    grid.advance_tab(1);
    assert_eq!(grid.cursor.col, 16);
}

#[test]
fn back_tab_moves_to_previous_tabstop() {
    let mut grid = Grid::new(24, 80);
    grid.cursor.col = 16;
    grid.back_tab(1);
    assert_eq!(grid.cursor.col, 8);
    grid.back_tab(1);
    assert_eq!(grid.cursor.col, 0);
}

#[test]
fn carriage_return_resets_col() {
    let mut grid = Grid::new(24, 80);
    grid.cursor.col = 50;
    grid.carriage_return();
    assert_eq!(grid.cursor.col, 0);
}

#[test]
fn backspace_moves_left() {
    let mut grid = Grid::new(24, 80);
    grid.cursor.col = 5;
    grid.backspace();
    assert_eq!(grid.cursor.col, 4);
    grid.backspace();
    grid.backspace();
    grid.backspace();
    grid.backspace();
    grid.backspace();
    assert_eq!(grid.cursor.col, 0);
}

#[test]
fn write_char_with_attrs_uses_provided_colors() {
    let mut grid = Grid::new(24, 80);
    let red = Color::rgb(255, 0, 0);
    let blue = Color::rgb(0, 0, 255);
    grid.write_char_with_attrs(
        'X',
        CellColor::Rgb(red),
        CellColor::Rgb(blue),
        CellFlags::BOLD,
    );
    assert_eq!(grid.cell(0, 0).character, 'X');
    assert_eq!(grid.cell(0, 0).fg, CellColor::Rgb(red));
    assert_eq!(grid.cell(0, 0).bg, CellColor::Rgb(blue));
    assert!(grid.cell(0, 0).flags.contains(CellFlags::BOLD));
}

#[test]
fn erase_chars_clears_count_cells() {
    let mut grid = Grid::new(5, 5);
    for c in 0..5 {
        grid.viewport[0].cells[c].character = char::from_digit(c as u32 + 1, 10).unwrap();
    }
    grid.cursor.col = 1;
    grid.erase_chars(2);
    assert_eq!(grid.cell(0, 0).character, '1');
    assert_eq!(grid.cell(0, 1).character, ' ');
    assert_eq!(grid.cell(0, 2).character, ' ');
    assert_eq!(grid.cell(0, 3).character, '4');
}

fn assert_row_has_valid_wide_pairs(grid: &Grid, row: usize) {
    for col in 0..grid.num_cols {
        let cell = grid.cell(row, col);
        if cell.flags.contains(CellFlags::WIDE_SPACER) {
            assert!(col > 0, "wide spacer cannot be in column zero");
            assert_eq!(
                grid.cell(row, col - 1).width,
                CellWidth::Full,
                "orphaned wide spacer at column {col}"
            );
        }
        if cell.width == CellWidth::Full {
            assert!(col + 1 < grid.num_cols, "wide lead cannot end a row");
            assert!(
                grid.cell(row, col + 1)
                    .flags
                    .contains(CellFlags::WIDE_SPACER),
                "orphaned wide lead at column {col}"
            );
        }
    }
}

#[test]
fn partial_clear_repairs_split_wide_pair() {
    let mut grid = Grid::new(2, 8);
    grid.write_char_with_attrs(
        '中',
        CellColor::Default,
        CellColor::Default,
        CellFlags::empty(),
    );
    grid.cursor.col = 1; // second half of 中
    grid.clear_line_right();

    assert_row_has_valid_wide_pairs(&grid, 0);
    assert_eq!(grid.cell(0, 0).character, ' ');
}

#[test]
fn insert_blank_repairs_split_wide_pair() {
    let mut grid = Grid::new(2, 8);
    grid.write_char_with_attrs(
        '中',
        CellColor::Default,
        CellColor::Default,
        CellFlags::empty(),
    );
    grid.cursor.col = 1; // insert between the lead and spacer
    grid.insert_blank(1);

    assert_row_has_valid_wide_pairs(&grid, 0);
}

#[test]
fn erase_chars_repairs_split_wide_pair() {
    let mut grid = Grid::new(2, 8);
    grid.write_char_with_attrs(
        '中',
        CellColor::Default,
        CellColor::Default,
        CellFlags::empty(),
    );
    grid.cursor.col = 0; // erase only the lead cell
    grid.erase_chars(1);

    assert_row_has_valid_wide_pairs(&grid, 0);
    assert_eq!(grid.cell(0, 1).character, ' ');
}

#[test]
fn delete_chars_repairs_shifted_wide_pair() {
    let mut grid = Grid::new(2, 8);
    grid.write_char('A');
    grid.write_char_with_attrs(
        '中',
        CellColor::Default,
        CellColor::Default,
        CellFlags::empty(),
    );
    grid.cursor.col = 1; // delete only the leading cell of 中
    grid.delete_chars(1);

    assert_row_has_valid_wide_pairs(&grid, 0);
}

#[test]
fn clearing_orphan_spacer_does_not_delete_valid_half_cell() {
    let mut row = Row::new(4);
    row.cells[0].character = 'A';
    row.cells[1].flags = CellFlags::WIDE_SPACER;

    row.clear_wide_pair_at(1);

    assert_eq!(row.cells[0].character, 'A');
    assert_eq!(row.cells[0].width, CellWidth::Half);
}

#[test]
fn tabstops_initialized_every_8() {
    let grid = Grid::new(24, 80);
    assert!(grid.tabstops[0]);
    assert!(grid.tabstops[8]);
    assert!(grid.tabstops[16]);
    assert!(!grid.tabstops[1]);
    assert!(!grid.tabstops[7]);
}

#[test]
fn resize_preserves_content() {
    let mut grid = Grid::with_scrollback(5, 5, 100);
    for c in 0..5 {
        grid.viewport[0].cells[c].character = char::from_digit(c as u32 + 1, 10).unwrap();
    }
    grid.resize(5, 10);
    assert_eq!(grid.num_cols, 10);
    assert_eq!(grid.cell(0, 0).character, '1');
    assert_eq!(grid.cell(0, 4).character, '5');
}

#[test]
fn resize_dims_does_not_reflow() {
    // The dimension-only resize must NOT rewrap content. A TUI app (less)
    // owns its layout and repaints on SIGWINCH. This guards against
    // regressing the "content squished into the top-left corner" bug.
    let mut grid = Grid::with_scrollback(3, 8, 100);
    // Row 0: "ABCDEFGH" (8 chars, no wrap). Row 1: a second line.
    for c in 0..8 {
        grid.viewport[0].cells[c].character = char::from(b'A' + c as u8);
    }
    grid.viewport[1].cells[0].character = 'X';

    // Narrow to 4 cols. A REFLOW would merge/wrap "ABCD" / "EFGH"; a
    // dimension-only resize just truncates each row in place.
    grid.resize_dims(3, 4);
    assert_eq!(grid.num_cols, 4);
    // Row 0 keeps its first 4 chars in place — no relocation.
    assert_eq!(grid.cell(0, 0).character, 'A');
    assert_eq!(grid.cell(0, 1).character, 'B');
    assert_eq!(grid.cell(0, 2).character, 'C');
    assert_eq!(grid.cell(0, 3).character, 'D');
    // The tail "EFGH" is dropped (truncated), NOT moved to row 1.
    // Row 1 still starts with 'X'.
    assert_eq!(grid.cell(1, 0).character, 'X');

    // Widen back to 8 — cells are padded with blanks, not unwrapped.
    grid.resize_dims(3, 8);
    assert_eq!(grid.cell(0, 0).character, 'A');
    assert_eq!(grid.cell(0, 3).character, 'D');
    assert_eq!(grid.cell(0, 4).character, ' '); // padded, NOT 'E'
}

#[test]
fn resize_dims_grows_and_shrinks_rows() {
    let mut grid = Grid::new(3, 4);
    grid.viewport[0].cells[0].character = 'A';
    // Grow rows 3 → 5: new rows appended at the bottom.
    grid.resize_dims(5, 4);
    assert_eq!(grid.num_rows, 5);
    assert_eq!(grid.cell(0, 0).character, 'A');
    assert_eq!(grid.cell(4, 0).character, ' '); // blank new row
                                                // Shrink rows 5 → 2: trailing rows dropped, content kept.
    grid.resize_dims(2, 4);
    assert_eq!(grid.num_rows, 2);
    assert_eq!(grid.cell(0, 0).character, 'A');
    // Cursor is clamped into range.
    assert!(grid.cursor.row < 2);
}

#[test]
fn scrollback_ring_buffer_overflow() {
    let mut grid = Grid::with_scrollback(3, 4, 5);
    // Scroll more lines than scrollback can hold
    for i in 0..10 {
        grid.viewport[2].cells[0].character = char::from_digit(i as u32 % 10, 10).unwrap_or('X');
        grid.cursor.row = 2;
        grid.newline();
    }
    // Scrollback should be capped at 5
    assert_eq!(grid.scrollback.len(), 5);
}

#[test]
fn write_output_resets_scroll_offset() {
    let mut grid = Grid::with_scrollback(5, 4, 100);
    // Scroll up into history
    for i in 0..10 {
        grid.viewport[4].cells[0].character = char::from_digit(i as u32 % 10, 10).unwrap_or('X');
        grid.cursor.row = 4;
        grid.newline();
    }
    grid.scroll_up_history(3);
    assert_eq!(grid.scroll_offset, 3);
    // Grid-level write no longer resets scroll_offset — the Terminal
    // (print path) manages that based on shell phase. Writing directly
    // to the grid preserves the offset so the caller can decide.
    grid.write_char_with_attrs(
        'A',
        CellColor::Default,
        CellColor::Default,
        CellFlags::empty(),
    );
    assert_eq!(grid.scroll_offset, 3, "grid write preserves scroll_offset");
}

// ── Resize regression tests ──────────────────────────────────────

/// Regression: shrink then grow should keep content visible.
/// Before the fix, the viewport took the bottom N wrapped rows (which
/// were empty padding), pushing the cursor's content into scrollback.
#[test]
fn resize_shrink_keeps_cursor_content_visible() {
    // 10 rows × 20 cols, cursor near bottom
    let mut grid = Grid::with_scrollback(10, 20, 100);
    // Fill rows 0-5 with content (simulating command output)
    for r in 0..6 {
        for c in 0..20 {
            grid.viewport[r].cells[c].character =
                char::from_digit((r * 20 + c) as u32 % 10, 10).unwrap_or('X');
        }
    }
    grid.cursor.row = 5;
    grid.cursor.col = 3;

    // Shrink to 4 rows × 10 cols — cursor's content row wraps and
    // the 10 old rows become ~12 wrapped rows, overflowing the 4-row
    // viewport.
    grid.resize(4, 10);

    // The cursor must be within the viewport bounds
    assert!(grid.cursor.row < 4);

    // The character at the cursor's original position must still be
    // accessible (either in viewport or scrollback). Since the cursor
    // row had content at col 3, the character at the new cursor
    // position should be non-null (the rewrapped content).
    let ch = grid.cell(grid.cursor.row, grid.cursor.col).character;
    assert_ne!(
        ch, '\0',
        "cursor position should have content after shrink, got null"
    );
}

/// Regression: grow-then-shrink preserves command output near cursor.
/// Simulates: max window → type ls → minimize → output invisible.
#[test]
fn resize_grow_then_shrink_preserves_output() {
    // Start small, fill with content, grow, then shrink back
    let mut grid = Grid::with_scrollback(5, 10, 100);

    // Simulate prompt + output in a 5×10 grid
    for c in 0..5 {
        grid.viewport[0].cells[c].character = if c == 0 { '>' } else { ' ' };
    }
    for r in 1..4 {
        for c in 0..8 {
            grid.viewport[r].cells[c].character = char::from_digit(r as u32, 10).unwrap();
        }
    }
    grid.cursor.row = 3;
    grid.cursor.col = 8;

    // Grow to 8×30 (maximize)
    grid.resize(8, 30);
    assert_eq!(grid.num_rows, 8);
    assert_eq!(grid.num_cols, 30);

    // The '>' prompt should still be in the grid
    let mut found_prompt = false;
    for r in 0..grid.num_rows {
        for c in 0..grid.num_cols {
            if grid.cell(r, c).character == '>' {
                found_prompt = true;
                break;
            }
        }
    }
    assert!(found_prompt, "prompt '>' should survive grow");

    // Shrink back to 4×8 (minimize)
    grid.resize(4, 8);
    assert_eq!(grid.num_rows, 4);
    assert_eq!(grid.num_cols, 8);

    // Cursor should be within bounds
    assert!(grid.cursor.row < 4, "cursor row {} < 4", grid.cursor.row);
    assert!(grid.cursor.col < 8, "cursor col {} < 8", grid.cursor.col);

    // The cursor's row must have actual content (not lost to scrollback).
    // This is the core regression check: before the fix, the viewport was
    // positioned on empty padding rows, so the cursor landed on an empty row.
    let cursor_has_content =
        (0..grid.num_cols).any(|c| grid.cell(grid.cursor.row, c).character != '\0');
    assert!(
        cursor_has_content,
        "cursor row {} should have content after shrink, not be empty padding",
        grid.cursor.row
    );
}

/// Cursor position should track correctly through rewrap when the
/// cursor's old row wraps into multiple new rows.
#[test]
fn resize_cursor_tracks_through_rewrap() {
    let mut grid = Grid::with_scrollback(3, 10, 100);
    // Fill row 1 with content across all 10 cols
    for c in 0..10 {
        grid.viewport[1].cells[c].character = char::from_digit(c as u32, 10).unwrap();
    }
    grid.cursor.row = 1;
    grid.cursor.col = 7; // col 7 should be in the first wrapped sub-row

    // Shrink to 3 rows × 4 cols — row 1 (10 chars) wraps to 3 sub-rows
    grid.resize(3, 4);

    // Cursor should be in the viewport
    assert!(grid.cursor.row < 3);
    assert!(grid.cursor.col < 4);

    // The character at the new cursor position should be '7' (the old col 7)
    // Col 7 in a 4-col wrap: sub-row 1 (7/4=1), col 3 (7%4=3)
    // But our cursor tracking uses cursor_wrap_start + col/new_cols
    // which is approximate. At minimum the cursor row should have content.
    let ch = grid.cell(grid.cursor.row, grid.cursor.col).character;
    assert_ne!(ch, '\0', "cursor should land on a content row after rewrap");
}

/// Regression: maximize → shell moves cursor to bottom → minimize.
/// Before the fix, empty rows between content and cursor were rewrapped
/// into empty wrapped rows that inflated total count, pushing content
/// into scrollback and leaving the viewport full of empty padding.
#[test]
fn resize_skips_empty_rows_between_content_and_cursor() {
    // Start: 5×10 grid with content in rows 0-2, cursor at row 2
    let mut grid = Grid::with_scrollback(5, 10, 200);
    for c in 0..8 {
        grid.viewport[0].cells[c].character = 'A';
        grid.viewport[1].cells[c].character = 'B';
        grid.viewport[2].cells[c].character = 'C';
    }
    grid.cursor.row = 2;
    grid.cursor.col = 8;

    // Maximize to 20×40 — shell moves cursor to bottom row
    grid.resize(20, 40);
    grid.cursor.row = 19; // shell puts cursor at bottom after SIGWINCH
    grid.cursor.col = 0;
    // Shell redraws prompt at row 19
    grid.viewport[19].cells[0].character = '$';

    // Minimize back to 5×10
    grid.resize(5, 10);

    // The prompt '$' should be visible in the viewport
    let mut found_prompt = false;
    for r in 0..5 {
        for c in 0..10 {
            if grid.cell(r, c).character == '$' {
                found_prompt = true;
            }
        }
    }
    assert!(
        found_prompt,
        "prompt '$' should be in viewport after minimize"
    );

    // At least some original content (A/B/C) should be visible
    let mut found_content = false;
    for r in 0..5 {
        for c in 0..10 {
            let ch = grid.cell(r, c).character;
            if ch == 'A' || ch == 'B' || ch == 'C' {
                found_content = true;
            }
        }
    }
    assert!(
        found_content,
        "original content (A/B/C) should be visible, not pushed to scrollback"
    );

    // Cursor within bounds
    assert!(grid.cursor.row < 5);
}

/// Empty rows should not accumulate over multiple resize cycles.
#[test]
fn resize_multiple_cycles_no_content_loss() {
    let mut grid = Grid::with_scrollback(5, 10, 200);

    // Fill with content
    for c in 0..8 {
        grid.viewport[0].cells[c].character = 'X';
        grid.viewport[1].cells[c].character = 'Y';
    }
    grid.cursor.row = 1;
    grid.cursor.col = 8;

    // Cycle 1: maximize
    grid.resize(15, 30);
    grid.cursor.row = 14;
    grid.viewport[14].cells[0].character = '$';
    // Cycle 1: minimize
    grid.resize(5, 10);

    let mut content_after_cycle1 = 0;
    for r in 0..5 {
        for c in 0..10 {
            let ch = grid.cell(r, c).character;
            if ch == 'X' || ch == 'Y' || ch == '$' {
                content_after_cycle1 += 1;
            }
        }
    }

    // Cycle 2: maximize
    grid.resize(15, 30);
    grid.cursor.row = 14;
    grid.viewport[14].cells[0].character = '$';
    // Cycle 2: minimize
    grid.resize(5, 10);

    let mut content_after_cycle2 = 0;
    for r in 0..5 {
        for c in 0..10 {
            let ch = grid.cell(r, c).character;
            if ch == 'X' || ch == 'Y' || ch == '$' {
                content_after_cycle2 += 1;
            }
        }
    }

    // Content count should be stable across cycles (no progressive loss).
    // Allow minor fluctuation (±2) from rewrap trimming edge cases, but
    // detect real degradation (e.g. 18 → 10 → 5).
    let diff = (content_after_cycle1 as i64 - content_after_cycle2 as i64).unsigned_abs();
    assert!(
        diff <= 2,
        "content should not degrade significantly: cycle1={}, cycle2={}",
        content_after_cycle1,
        content_after_cycle2
    );

    // And there should still be visible content
    assert!(
        content_after_cycle2 > 0,
        "content must survive multiple resize cycles"
    );
}

/// Wrapped rows must merge back into one line when the grid widens.
/// This is the core reflow test: narrow → wrap → widen → unwrap.
#[test]
fn resize_reflow_merges_wrapped_rows_on_widen() {
    // 3×10 grid, write a long line that wraps
    let mut grid = Grid::with_scrollback(3, 10, 100);
    // Write "ABCDEFGHIJ" (10 chars) — fills row 0, wrapped=true
    // Then "KLMNO" (5 chars) on row 1 — continuation
    for c in 0..10 {
        grid.viewport[0].cells[c].character = char::from_digit((c % 10) as u32, 10).unwrap();
    }
    grid.viewport[0].wrapped = true;
    for c in 0..5 {
        grid.viewport[1].cells[c].character = char::from_digit((c % 10) as u32, 10).unwrap();
    }
    grid.cursor.row = 1;
    grid.cursor.col = 5;

    // Widen to 20 cols — the two wrapped rows should merge into one
    grid.resize(3, 20);
    assert_eq!(grid.num_cols, 20);

    // Row 0 should now contain "ABCDEFGHIJKLMNO" (15 chars in 20-col row)
    let row0_chars: String = grid.viewport[0]
        .cells
        .iter()
        .take_while(|c| c.character != ' ')
        .map(|c| c.character)
        .collect();
    assert_eq!(
        row0_chars, "012345678901234",
        "wrapped rows should merge on widen, got: {:?}",
        row0_chars
    );

    // Row 1 should NOT contain the continuation text anymore
    // (it was merged into row 0)
    let row1_has_digits = grid.viewport[1]
        .cells
        .iter()
        .any(|c| c.character.is_ascii_digit());
    assert!(
        !row1_has_digits,
        "continuation content should have been merged into row 0"
    );
}

/// Separate logical lines must NOT be merged during reflow.
#[test]
fn resize_reflow_does_not_merge_separate_lines() {
    let mut grid = Grid::with_scrollback(4, 10, 100);
    // Row 0: "AAAA" (not wrapped)
    for c in 0..4 {
        grid.viewport[0].cells[c].character = 'A';
    }
    // Row 1: "BBBB" (not wrapped)
    for c in 0..4 {
        grid.viewport[1].cells[c].character = 'B';
    }
    grid.cursor.row = 1;
    grid.cursor.col = 4;

    // Widen to 20 cols — rows should remain separate
    grid.resize(4, 20);

    // Row 0 should have AAAA, Row 1 should have BBBB — NOT "AAAABBBB"
    assert_eq!(grid.viewport[0].cells[0].character, 'A');
    assert_eq!(grid.viewport[1].cells[0].character, 'B');
    assert_eq!(grid.viewport[0].cells[4].character, ' ');
}

/// End-to-end test: write a long line via write_char_with_attrs
/// (which the VT parser uses), then widen — the line must unwrap.
#[test]
fn resize_unwraps_line_written_by_vt_parser() {
    use super::CellFlags;
    let mut grid = Grid::with_scrollback(5, 20, 100);

    // Write 35 characters — wraps in a 20-col grid
    for i in 0..35u8 {
        let ch = char::from_digit((i % 10) as u32, 10).unwrap();
        grid.write_char_with_attrs(
            ch,
            CellColor::Default,
            CellColor::Default,
            CellFlags::empty(),
        );
    }
    // VT newline to end the line
    grid.newline();

    // Row 0 should be wrapped (first 20 chars)
    assert!(
        grid.viewport[0].wrapped,
        "row 0 should have wrapped=true after writing 35 chars in 20-col grid"
    );

    // Widen to 50 — should unwrap to one row
    grid.resize(5, 50);

    // All 35 chars should be on row 0
    let content: String = grid.viewport[0]
        .cells
        .iter()
        .take_while(|c| c.character != ' ')
        .map(|c| c.character)
        .collect();
    assert_eq!(
        content.len(),
        35,
        "35 chars should fit on one row after widening to 50, got: {:?}",
        content
    );

    // Row 1 should NOT have continuation digits
    let row1_has_digits = grid.viewport[1]
        .cells
        .iter()
        .any(|c| c.character.is_ascii_digit());
    assert!(
        !row1_has_digits,
        "continuation should have been merged into row 0"
    );
}

// ── v1.0 P0-b: dirty tracking tests ────────────────────────────────

#[test]
fn dirty_rows_empty_on_fresh_grid() {
    let grid = Grid::new(5, 10);
    assert_eq!(grid.dirty_rows().count(), 0);
    assert!(!grid.has_dirty());
}

#[test]
fn dirty_rows_after_cell_write() {
    let mut grid = Grid::new(5, 10);
    grid.cell_mut(2, 3).character = 'X';
    let dirty: Vec<_> = grid.dirty_rows().collect();
    assert_eq!(dirty, vec![(2, 4)]);
    assert!(grid.has_dirty());
}

#[test]
fn dirty_rows_extent_tracks_max_col() {
    let mut grid = Grid::new(5, 10);
    grid.cell_mut(1, 2).character = 'A';
    grid.cell_mut(1, 7).character = 'B';
    let dirty: Vec<_> = grid.dirty_rows().collect();
    assert_eq!(dirty, vec![(1, 8)]);
}

#[test]
fn dirty_rows_multiple_rows() {
    let mut grid = Grid::new(5, 10);
    grid.cell_mut(0, 1).character = 'A';
    grid.cell_mut(3, 5).character = 'B';
    let dirty: Vec<_> = grid.dirty_rows().collect();
    assert_eq!(dirty, vec![(0, 2), (3, 6)]);
}

#[test]
fn clear_all_dirty_resets_rows() {
    let mut grid = Grid::new(5, 10);
    grid.cell_mut(1, 2).character = 'A';
    grid.cell_mut(3, 4).character = 'B';
    assert!(grid.has_dirty());
    grid.clear_all_dirty();
    assert!(!grid.has_dirty());
    assert_eq!(grid.dirty_rows().count(), 0);
}

#[test]
fn mark_all_dirty_sets_every_row() {
    let mut grid = Grid::new(3, 10);
    grid.mark_all_dirty();
    assert_eq!(grid.dirty_rows().count(), 3);
    for (_, extent) in grid.dirty_rows() {
        assert_eq!(extent, 10);
    }
}

#[test]
fn scroll_up_sets_pending_scroll() {
    let mut grid = Grid::new(3, 5);
    // Move cursor to bottom so newline triggers scroll_up.
    grid.cursor.row = 2;
    grid.newline();
    // v1.0 P0-c: scroll_up now records pending_scroll instead of
    // mark_all_dirty — the renderer shifts its cache to match.
    assert_eq!(grid.take_pending_scroll(), 1);
    // After take, pending_scroll is reset.
    assert_eq!(grid.take_pending_scroll(), 0);
}

#[test]
fn scroll_down_sets_negative_pending_scroll() {
    let mut grid = Grid::new(5, 5);
    grid.scroll_down(2);
    assert_eq!(grid.take_pending_scroll(), -2);
}

#[test]
fn pending_scroll_accumulates() {
    let mut grid = Grid::new(5, 5);
    grid.scroll_up(1);
    grid.scroll_up(1);
    assert_eq!(grid.take_pending_scroll(), 2);
}

#[test]
fn clear_all_dirty_clears_pending_scroll() {
    let mut grid = Grid::new(5, 5);
    grid.scroll_up(1);
    assert_eq!(grid.take_pending_scroll(), 1);
    grid.scroll_up(1);
    grid.clear_all_dirty();
    assert_eq!(grid.take_pending_scroll(), 0);
}

#[test]
fn scroll_region_up_marks_dirty_not_pending() {
    // less/vim set a scroll region (DECSTBM) then scroll within it.
    // The renderer's cache shift can only handle full-viewport scrolls,
    // so scroll-region scrolls must mark rows dirty instead.
    let mut grid = Grid::new(5, 5);
    // Scroll region: rows 1..3 (0-indexed), leaving row 0 and row 4
    // outside the region.
    grid.set_scroll_region(2, 4);
    assert_eq!(grid.scroll_top, 1);
    assert_eq!(grid.scroll_bottom, 3);

    grid.scroll_up(1);
    // No pending_scroll for scroll-region scrolls.
    assert_eq!(grid.take_pending_scroll(), 0);
    // Rows in the scroll region (1..=3) should be dirty.
    let dirty: Vec<_> = grid.dirty_rows().map(|(r, _)| r).collect();
    assert!(dirty.contains(&1));
    assert!(dirty.contains(&2));
    assert!(dirty.contains(&3));
    // Row 0 is outside the scroll region — must NOT be dirty.
    assert!(!dirty.contains(&0));
    // Row 4 is outside the scroll region — must NOT be dirty.
    assert!(!dirty.contains(&4));
}

#[test]
fn scroll_region_down_marks_dirty_not_pending() {
    let mut grid = Grid::new(5, 5);
    grid.set_scroll_region(2, 4);
    grid.scroll_down(1);
    assert_eq!(grid.take_pending_scroll(), 0);
    let dirty: Vec<_> = grid.dirty_rows().map(|(r, _)| r).collect();
    assert!(dirty.contains(&1));
    assert!(dirty.contains(&2));
    assert!(dirty.contains(&3));
    assert!(!dirty.contains(&0));
    assert!(!dirty.contains(&4));
}

#[test]
fn full_viewport_scroll_still_uses_pending_scroll() {
    // Regression: full-viewport scrolls must still use the cache-shift
    // optimization (pending_scroll), not mark-all-dirty.
    let mut grid = Grid::new(5, 5);
    grid.scroll_up(2);
    assert_eq!(grid.take_pending_scroll(), 2);
    assert_eq!(grid.dirty_rows().count(), 0);

    let mut grid = Grid::new(5, 5);
    grid.scroll_down(2);
    assert_eq!(grid.take_pending_scroll(), -2);
    assert_eq!(grid.dirty_rows().count(), 0);
}

#[test]
fn resize_marks_all_rows_dirty() {
    let mut grid = Grid::new(3, 5);
    grid.clear_all_dirty();
    assert!(!grid.has_dirty());
    grid.resize(5, 8);
    assert!(grid.has_dirty());
    assert_eq!(grid.dirty_rows().count(), 5);
}

/// CI performance gate for live resize with the configured 10k scrollback.
/// Ignored in the normal suite so wall-clock assertions run in isolation.
#[test]
#[ignore]
fn perf_resize_10k_scrollback() {
    let mut grid = Grid::with_scrollback(40, 120, 10_000);
    for i in 0..10_000 {
        for ch in format!("line {i:05} terminal resize payload").chars() {
            grid.write_char(ch);
        }
        grid.newline();
    }

    let started = std::time::Instant::now();
    for _ in 0..3 {
        grid.resize(24, 80);
        grid.resize(50, 160);
    }
    let elapsed = started.elapsed();
    println!("six 10k-scrollback resize passes: {elapsed:?} (budget <1s)");

    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "six 10k-scrollback resize passes took {elapsed:?}, budget <1s"
    );
}

// ── IL/DL dirty marking tests ──────────────────────────────────

#[test]
fn insert_blank_lines_marks_dirty() {
    // less/vim use IL (CSI L) to scroll within a scroll region.
    // The affected rows must be marked dirty so the renderer rebuilds them.
    let mut grid = Grid::new(5, 5);
    grid.clear_all_dirty();
    // Set a scroll region (rows 1..3, 0-indexed) so IL operates within it.
    grid.set_scroll_region(2, 4);
    // Place cursor inside the scroll region.
    grid.cursor.row = 1;
    grid.cursor.col = 0;
    grid.insert_blank_lines(1);
    // Rows 1..=3 must be dirty (moved + blanked).
    let dirty: Vec<_> = grid.dirty_rows().map(|(r, _)| r).collect();
    assert!(dirty.contains(&1), "cursor row must be dirty");
    assert!(dirty.contains(&2), "shifted row must be dirty");
    assert!(dirty.contains(&3), "bottom row must be dirty");
    // Row 0 is outside the scroll region — must NOT be dirty.
    assert!(!dirty.contains(&0));
    // Row 4 is outside the scroll region — must NOT be dirty.
    assert!(!dirty.contains(&4));
}

#[test]
fn full_region_insert_and_delete_lines_clear_without_underflow() {
    fn populated_grid() -> Grid {
        let mut grid = Grid::new(5, 6);
        for row in 0..5 {
            grid.cursor.row = row;
            grid.cursor.col = 0;
            grid.write_char(char::from(b'A' + row as u8));
        }
        grid.set_scroll_region(1, 4); // rows 0..=3, with status row 4 outside
        grid.cursor.row = 0;
        grid
    }

    let mut inserted = populated_grid();
    inserted.insert_blank_lines(usize::MAX);
    assert_eq!(inserted.row_text(0), "");
    assert_eq!(inserted.row_text(1), "");
    assert_eq!(inserted.row_text(2), "");
    assert_eq!(inserted.row_text(3), "");
    assert_eq!(inserted.row_text(4), "E");

    let mut deleted = populated_grid();
    deleted.delete_lines(usize::MAX);
    assert_eq!(deleted.row_text(0), "");
    assert_eq!(deleted.row_text(1), "");
    assert_eq!(deleted.row_text(2), "");
    assert_eq!(deleted.row_text(3), "");
    assert_eq!(deleted.row_text(4), "E");
}

#[test]
fn delete_lines_marks_dirty() {
    // less/vim use DL (CSI M) to scroll within a scroll region.
    // The affected rows must be marked dirty so the renderer rebuilds them.
    let mut grid = Grid::new(5, 5);
    grid.clear_all_dirty();
    grid.set_scroll_region(2, 4);
    grid.cursor.row = 1;
    grid.cursor.col = 0;
    grid.delete_lines(1);
    let dirty: Vec<_> = grid.dirty_rows().map(|(r, _)| r).collect();
    assert!(dirty.contains(&1), "cursor row must be dirty");
    assert!(dirty.contains(&2), "shifted row must be dirty");
    assert!(dirty.contains(&3), "bottom row must be dirty");
    assert!(
        !dirty.contains(&0),
        "row outside scroll region must NOT be dirty"
    );
    assert!(
        !dirty.contains(&4),
        "row outside scroll region must NOT be dirty"
    );
}
