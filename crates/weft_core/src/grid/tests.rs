use super::*;

#[test]
fn cell_struct_stays_at_24_bytes() {
    // v1.11.3 (PLAN_v1113 §1.1): budget tightened from `<= 24` to `== 24`
    // — raw = char4 + fg5 + bg5 + flags2 + width1 + style1 + color5 = 23B,
    // one padding byte remains; adding another >1B field must trigger an
    // explicit budget re-evaluation (v0.8_PLAN §5) rather than silently
    // growing into 28B. The `Option<CellColor>` 5B niche is pinned by
    // `option_cell_color_is_5_bytes` in this file.
    assert_eq!(
        std::mem::size_of::<Cell>(),
        24,
        "Cell must stay exactly 24 bytes (PLAN_v1113 §1.1); got {}",
        std::mem::size_of::<Cell>()
    );
}

#[test]
fn option_cell_color_is_5_bytes() {
    // PLAN_v1113 §1.1 (audit S1): the `#[repr(u8)]` niche makes the Option
    // wrapper 5B instead of 6B — the Cell 24B budget depends on it. This
    // test locks that compiler-optimization contract explicitly.
    // (CellColor itself is 5B: 1B repr(u8) tag + 4B Rgb(Color) payload.)
    assert_eq!(std::mem::size_of::<CellColor>(), 5);
    assert_eq!(std::mem::size_of::<Option<CellColor>>(), 5);
    assert_eq!(std::mem::size_of::<UnderlineStyle>(), 1);
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
fn displayed_row_text_follows_scrollback_window() {
    let mut grid = Grid::new(2, 10);
    for ch in "old".chars() {
        grid.write_char(ch);
    }
    grid.newline();
    grid.carriage_return();
    for ch in "new".chars() {
        grid.write_char(ch);
    }
    grid.newline();
    grid.carriage_return();
    for ch in "latest".chars() {
        grid.write_char(ch);
    }
    assert_eq!(grid.row_text(0), "new");
    grid.scroll_up_history(1);
    assert_eq!(grid.displayed_row_text(0), "old");
    assert_eq!(grid.displayed_row_text(1), "new");
    assert_eq!(grid.displayed_row_text(99), "");
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

/// AUDIT_v1.10.39 fuzz-lite finding: a full-width char on a 1-column grid
/// used to recurse forever in `write_char_with_attrs` (the wrap arm could
/// not change any state once the cursor sat on the bottom row of a 1-col
/// grid). Post-fix the wrap walk is bounded: the cursor descends to the
/// bottom row, one scroll_up fires, then the guard stops the recursion and
/// the char lands truncated in the single column of the bottom row.
#[test]
fn wide_char_on_single_column_grid_writes_truncated_without_hanging() {
    let mut grid = Grid::new(4, 1);
    grid.write_char_with_attrs(
        '中',
        CellColor::Default,
        CellColor::Default,
        CellFlags::empty(),
    );

    // Terminated, and the char was written into the only column of the
    // bottom row (after the bounded wrap walk), not dropped silently.
    assert_eq!(grid.cell(3, 0).character, '中');
    assert_eq!(grid.cell(3, 0).width, CellWidth::Full);
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
    grid.set_scroll_offset(1);
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
    // dimension-only resize leaves each row in place. v1.10.26 B-2: narrowing
    // does NOT physically truncate viewport rows anymore — the row keeps its
    // 8 cells and the right half is merely clipped from the render window.
    grid.resize_dims(3, 4);
    assert_eq!(grid.num_cols, 4);
    // Row 0 keeps its first 4 chars in place — no relocation.
    assert_eq!(grid.cell(0, 0).character, 'A');
    assert_eq!(grid.cell(0, 1).character, 'B');
    assert_eq!(grid.cell(0, 2).character, 'C');
    assert_eq!(grid.cell(0, 3).character, 'D');
    // The tail "EFGH" survives at the row's own width (only-grow); the
    // renderer / `cell()` reads are bounded by `num_cols`.
    assert_eq!(
        grid.viewport[0].cells.len(),
        8,
        "viewport row is not truncated"
    );
    assert_eq!(grid.viewport[0].cells[4].character, 'E');
    // Row 1 still starts with 'X'.
    assert_eq!(grid.cell(1, 0).character, 'X');

    // Widen back to 8 — cells were never shrunk, so no loss: 'E' is back in
    // view exactly where it was, not padded as a blank.
    grid.resize_dims(3, 8);
    assert_eq!(grid.cell(0, 0).character, 'A');
    assert_eq!(grid.cell(0, 3).character, 'D');
    assert_eq!(grid.cell(0, 4).character, 'E'); // kept, NOT padded blank
}

#[test]
fn resize_dims_keeps_scrollback_rows_at_renderable_width() {
    let mut grid = Grid::with_scrollback(2, 4, 10);
    for (col, ch) in "ABCD".chars().enumerate() {
        grid.viewport[0].cells[col].character = ch;
    }
    grid.scroll_up(1);

    grid.resize_dims(2, 5);
    grid.scroll_to_top();
    // T2: flat history rows materialize at their wrapped width (4) — the
    // old grow-only padding is replaced by the `cell()` blank fallback, the
    // read-side twin of the same guarantee.
    assert_eq!(grid.scrollback.get(0).unwrap().cells.len(), 4);
    assert_eq!(grid.cell(0, 3).character, 'D');
    assert_eq!(grid.cell(0, 4).character, ' ');

    // v1.10.23 (FIX_OMP_CONTENT_LOSS): narrowing must NOT truncate history
    // rows — a TUI cannot repaint rows already scrolled out of the viewport,
    // so truncation lost the right half of every history line irreversibly.
    // The history row keeps its original width. v1.10.26 B-2 extends the same
    // only-grow strategy to the VIEWPORT rows (they repaint on SIGWINCH, but
    // the transient residual frame is clipped rather than destroyed).
    grid.resize_dims(2, 3);
    grid.scroll_to_top();
    assert_eq!(
        grid.scrollback.get(0).unwrap().cells.len(),
        4,
        "history rows keep their wrapped width when narrowing"
    );
    assert_eq!(grid.cell(0, 2).character, 'C');
    assert_eq!(
        grid.cell(0, 3).character,
        'D',
        "right half survives in history"
    );
    assert_eq!(grid.cell(0, 4).character, ' ');
    assert_eq!(
        grid.document_text_from(0),
        "ABCD",
        "the snapshot contains the complete history row"
    );
    assert_eq!(
        grid.viewport[0].cells.len(),
        5,
        "viewport rows also keep their width when narrowing (B-2 only-grow)"
    );
}

#[test]
fn narrowing_resize_keeps_scrollback_rows_at_original_width() {
    // v1.10.23 (FIX_OMP_CONTENT_LOSS): a narrowing dimension-only resize must
    // leave scrollback rows at their original width with their full text —
    // the TUI cannot repaint scrolled-out rows, so the old truncation lost
    // streamed content irreversibly (omp paragraphs missing their right half
    // after a resize).
    let mut grid = Grid::with_scrollback(2, 8, 16);
    for (col, ch) in "ABCDEFGH".chars().enumerate() {
        grid.viewport[0].cells[col].character = ch;
    }
    grid.scroll_up(1);
    assert_eq!(grid.scrollback.get(0).unwrap().cells.len(), 8);

    grid.resize_dims(2, 4);
    let row = grid.scrollback.get(0).unwrap();
    assert_eq!(row.cells.len(), 8, "history row keeps its original width");
    let text: String = row.cells.iter().map(|c| c.character).collect();
    assert_eq!(text, "ABCDEFGH", "the right half survives the narrowing");
    assert_eq!(
        grid.document_text_from(0),
        "ABCDEFGH",
        "the snapshot contains the complete history row"
    );
    // cell() stays readable within the new viewport width.
    grid.scroll_to_top();
    assert_eq!(grid.cell(0, 0).character, 'A');
    assert_eq!(grid.cell(0, 3).character, 'D');

    // Widen again: history rows pad to the new width, no unwrapping.
    grid.resize_dims(2, 8);
    grid.scroll_to_top();
    assert_eq!(grid.scrollback.get(0).unwrap().cells.len(), 8);
    assert_eq!(grid.cell(0, 7).character, 'H');
}

#[test]
fn scrollback_row_cell_access_is_bounds_safe() {
    // v1.10.23 (FIX_OMP_CONTENT_LOSS): defensive — `cell()` must not panic
    // when a history row is narrower than the requested column. The resize
    // invariant normally keeps history rows at least `num_cols` wide; the
    // guard makes the invariant non-load-bearing.
    let mut grid = Grid::with_scrollback(2, 8, 8);
    let mut row = Row::new(5);
    row.cells[0].character = 'x';
    grid.scrollback.push(row);
    grid.set_scroll_offset(1);

    assert_eq!(grid.cell(0, 0).character, 'x');
    let out_of_range = grid.cell(0, 7);
    assert_eq!(
        out_of_range.character, ' ',
        "out-of-range column returns a blank cell"
    );
    assert_eq!(out_of_range.width, CellWidth::Half);
}

#[test]
fn resize_dims_narrowing_keeps_wide_pair_intact() {
    let mut grid = Grid::with_scrollback(2, 4, 10);
    for row in &mut grid.viewport {
        row.cells[2].character = '中';
        row.cells[2].width = CellWidth::Full;
        row.cells[3].flags.insert(CellFlags::WIDE_SPACER);
    }
    grid.scroll_up(1);

    grid.resize_dims(2, 3);
    // v1.10.26 B-2: the viewport row is NO LONGER truncated when narrowing —
    // it keeps its 4-cell width, so the wide pair at cols 2-3 stays intact
    // (the renderer, bounded by num_cols=3, just clips col 3 until the TUI
    // repaints or a full-line erase normalizes the row back to num_cols).
    assert_eq!(grid.viewport[0].cells[2].character, '中');
    assert_eq!(grid.viewport[0].cells[2].width, CellWidth::Full);
    assert!(
        grid.viewport[0].cells[3]
            .flags
            .contains(CellFlags::WIDE_SPACER),
        "the wide pair stays intact in the viewport row (B-2 only-grow)"
    );
    grid.scroll_to_top();
    // v1.10.23 (FIX_OMP_CONTENT_LOSS): the history row is NOT truncated
    // anymore — its wide pair at cols 2-3 stays intact (the row keeps its
    // original 4-cell width), so the glyph survives in history/snapshot.
    assert_eq!(grid.cell(0, 2).character, '中');
    assert_eq!(grid.cell(0, 2).width, CellWidth::Full);
    assert!(
        grid.cell(0, 3).flags.contains(CellFlags::WIDE_SPACER),
        "the wide pair stays intact in history"
    );
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

/// v1.10.26 Batch B (FIX_WRAP_EPOCH_AND_VIEWPORT_KEEP B-2): a narrowing
/// dimension-only resize must NOT physically truncate VIEWPORT rows — same as
/// scrollback, viewport rows only ever GROW. The TUI repaints on SIGWINCH; the
/// transient residual frame is clipped at the right edge rather than having
/// its right half deleted irreversibly.
#[test]
fn resize_dims_narrowing_keeps_viewport_cells() {
    let mut grid = Grid::new(2, 8);
    for (col, ch) in "ABCDEFGH".chars().enumerate() {
        grid.viewport[0].cells[col].character = ch;
    }

    grid.resize_dims(2, 4);
    assert_eq!(grid.num_cols, 4);
    assert_eq!(
        grid.viewport[0].cells.len(),
        8,
        "viewport row keeps its original width when narrowing"
    );
    // Readers bounded by num_cols (renderer / cell()) see the clipped view.
    assert_eq!(grid.cell(0, 0).character, 'A');
    assert_eq!(grid.cell(0, 3).character, 'D');
    // The right half survives: widening again shows it instead of a blank.
    assert_eq!(grid.viewport[0].cells[4].character, 'E');
    assert_eq!(grid.viewport[0].cells[7].character, 'H');

    // Widening again: cells were already 8 wide — no reflow, no loss.
    grid.resize_dims(2, 8);
    assert_eq!(grid.cell(0, 0).character, 'A');
    assert_eq!(grid.cell(0, 4).character, 'E');
    assert_eq!(grid.cell(0, 7).character, 'H');
}

/// v1.10.26 Batch B: after a narrowing resize kept a viewport row wide, a
/// full-line erase (CSI 2 K) re-establishes the row at `num_cols` — the TUI's
/// rewrite normalizes the line back to the visible width. The stale right half
/// a narrowing resize preserved momentarily is dropped once the line is
/// erased in full.
#[test]
fn el_erase_then_rewrite_normalizes_row_width() {
    let mut grid = Grid::new(2, 8);
    for (col, ch) in "ABCDEFGH".chars().enumerate() {
        grid.viewport[0].cells[col].character = ch;
    }
    grid.resize_dims(2, 4);
    assert_eq!(
        grid.viewport[0].cells.len(),
        8,
        "narrowing keeps the row width (B-2 precondition)"
    );

    // CSI 2 K — clear the whole line.
    grid.cursor.col = 0;
    grid.clear_line_all();
    assert_eq!(
        grid.viewport[0].cells.len(),
        4,
        "full-line erase normalizes the row width to num_cols"
    );
    assert!(grid.viewport[0].cells.iter().all(|c| c.character == ' '));
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

/// Fix B2 (v1.11.16): `write_char_with_attrs` deferred wrap on a non-last row
/// must mark the row the cursor left as `wrapped`.
#[test]
fn write_char_wrap_marks_previous_row_on_normal_newline() {
    use super::CellFlags;
    let mut grid = Grid::with_scrollback(5, 20, 100);
    let row_before = 1;
    grid.cursor.row = row_before;
    grid.cursor.col = 19; // last column
    grid.cursor.wrap_pending = true;
    grid.write_char_with_attrs(
        'x',
        CellColor::Default,
        CellColor::Default,
        CellFlags::empty(),
    );
    assert_eq!(grid.cursor.row, row_before + 1, "cursor should advance");
    // 'x' is half-width: after the wrap to col 0 the write advances col to 1.
    assert_eq!(grid.cursor.col, 1, "cursor sits after the written 'x'");
    assert!(
        grid.viewport[row_before].wrapped,
        "row {} (the row the cursor left) must be marked wrapped",
        row_before
    );
}

/// Fix B2 (v1.11.16): `write_char_with_attrs` deferred wrap at the last
/// physical row outside the scroll region must NOT mark any row `wrapped`
/// (the old code mismarked the unrelated row above).
#[test]
fn write_char_wrap_no_mark_outside_scroll_region() {
    use super::CellFlags;
    let mut grid = Grid::with_scrollback(5, 20, 100);
    // Scroll region bottom = row index 1; full region would be 4.
    grid.set_scroll_region(1, 2); // 1-based → top 0, bottom 1
    let bottom = grid.scroll_region().1;
    assert_eq!(bottom, 1);
    grid.cursor.row = 4; // last row, outside the scroll region
    grid.cursor.col = 19; // last column
    grid.cursor.wrap_pending = true;
    grid.write_char_with_attrs(
        'x',
        CellColor::Default,
        CellColor::Default,
        CellFlags::empty(),
    );
    assert_eq!(grid.cursor.row, 4, "cursor must stay put at the last row");
    assert!(
        (0..grid.num_rows).all(|r| !grid.viewport[r].wrapped),
        "no row may be marked wrapped when the cursor cannot advance"
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

// ── PLAN_audit_fix_batch3 3B: reflow clone-cost measurement (Phase A/C) ──

/// Counting global allocator for the weft_core unit-test binary (cfg(test)
/// only — the production binary keeps the system allocator from main.rs).
/// Cumulative counters, no dealloc subtraction: a measurement snapshots the
/// counters before/after the target call and reports the DELTA, so frees and
/// allocator reuse inside the measured window cannot distort the numbers.
mod measure_alloc {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub static ALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);
    pub static ALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);

    pub struct CountingAllocator;

    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let ptr = System.alloc(layout);
            if !ptr.is_null() {
                ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
                ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
            }
            ptr
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let ptr = System.alloc_zeroed(layout);
            if !ptr.is_null() {
                ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
                ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
            }
            ptr
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            System.dealloc(ptr, layout)
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let new_ptr = System.realloc(ptr, layout, new_size);
            if !new_ptr.is_null() {
                ALLOC_BYTES.fetch_add(new_size, Ordering::Relaxed);
                ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
            }
            new_ptr
        }
    }

    #[global_allocator]
    static GLOBAL: CountingAllocator = CountingAllocator;

    /// (cumulative allocated bytes, cumulative allocation calls).
    pub fn snapshot() -> (usize, usize) {
        (
            ALLOC_BYTES.load(Ordering::Relaxed),
            ALLOC_CALLS.load(Ordering::Relaxed),
        )
    }
}

/// Serialize the measurement tests so sibling `#[ignore]` tests running on
/// another harness thread cannot pollute the allocator deltas.
fn measure_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 3B fixture (PLAN scenario): 60×200 grid driven to 10k scrollback lines
/// through the real VT print path. ASCII lines interleave with CJK lines
/// and every 4th line is long enough to soft-wrap, so merge/continuation
/// grouping and wide-pair rewrap carry real weight in the measurement.
fn fill_mixed_scrollback_60x200() -> Grid {
    let mut grid = Grid::with_scrollback(60, 200, 10_000);
    let cjk = "汉字终端重排测量";
    for i in 0..10_000usize {
        let line = match i % 4 {
            0 => format!("long wrapped line {i:05} {}", "x".repeat(240)),
            2 => format!("CJK wrapped 行{i:05} {}", cjk.repeat(24)),
            3 => format!("CJK 行{i:05} {cjk}"),
            _ => format!("line {i:05} ascii resize payload"),
        };
        for ch in line.chars() {
            // write_char_with_attrs is the wrapping writer (write_char is
            // the test-injection variant that clips at num_cols); soft-wrap
            // weight is part of the PLAN scenario.
            grid.write_char_with_attrs(
                ch,
                CellColor::Default,
                CellColor::Default,
                CellFlags::empty(),
            );
        }
        if i + 1 < 10_000 {
            grid.newline();
        }
    }
    grid
}

/// 3B Phase A gate — single-step cost. Decision protocol (locked): run in
/// RELEASE via
/// `cargo test -p weft_core --lib --release -- --ignored --test-threads=1 --nocapture measure`
/// (debug numbers are recorded for reference only). Byte reference points:
/// one Cell is exactly 24 bytes (pinned above), so ~4M merged cells ≈ ~96MB
/// if the rewrap path cloned every cell.
#[test]
#[ignore]
fn measure_reflow_single_step_61x200() {
    let _guard = measure_lock();
    let mut grid = fill_mixed_scrollback_60x200();
    let (bytes_before, calls_before) = measure_alloc::snapshot();
    let started = std::time::Instant::now();
    grid.resize(61, 200);
    let elapsed = started.elapsed();
    let (bytes, calls) = measure_alloc::snapshot();
    println!(
        "MEASURE single resize(61,200) [10k scrollback]: time={elapsed:?} alloc_bytes={} alloc_calls={}",
        bytes - bytes_before,
        calls - calls_before
    );
    assert_eq!(grid.num_rows, 61);
    assert_eq!(grid.num_cols, 200);
}

/// 3B Phase A gate — 60-step ±1 column resize storm (drag simulation),
/// same fixture and release protocol as the single-step measurement.
#[test]
#[ignore]
fn measure_reflow_storm_60_steps() {
    let _guard = measure_lock();
    let mut grid = fill_mixed_scrollback_60x200();
    let (bytes_before, calls_before) = measure_alloc::snapshot();
    let started = std::time::Instant::now();
    for step in 0..60 {
        let cols = if step % 2 == 0 { 199 } else { 200 };
        grid.resize(60, cols);
    }
    let elapsed = started.elapsed();
    let (bytes, calls) = measure_alloc::snapshot();
    println!(
        "MEASURE storm 60x ±1 col [10k scrollback]: time={elapsed:?} alloc_bytes={} alloc_calls={}",
        bytes - bytes_before,
        calls - calls_before
    );
    assert_eq!(grid.num_cols, 200);
}

/// 3B protocol, FIX_DRAG_RESIZE_STUTTER β acceptance: steady-state tier of
/// the oscillating drag. The storm test reports the 60-step total; this one
/// isolates the FIRST step (realloc-heavy by design: rows built at the old
/// width grow into the new capacity) and the mean of the LAST 10 steps of a
/// 20-step ±1-col oscillation (buffer identity has fully cycled through the
/// pool — the tier the ~52MB/step target measures). Also records a deep
/// narrow step (200→80: heavy wrapping exhausts the pool, forcing fresh-row
/// fallbacks) as its own tier. Same release protocol:
/// `cargo test -p weft_core --lib --release -- --ignored --test-threads=1
/// --nocapture measure_oscillation`.
#[test]
#[ignore]
fn measure_reflow_oscillation_steady_state() {
    let _guard = measure_lock();
    let mut grid = fill_mixed_scrollback_60x200();
    let mut step_bytes: Vec<usize> = Vec::new();
    for step in 0..20 {
        let cols = if step % 2 == 0 { 199 } else { 200 };
        let (before_b, _) = measure_alloc::snapshot();
        grid.resize(60, cols);
        let (after_b, _) = measure_alloc::snapshot();
        step_bytes.push(after_b - before_b);
    }
    let first = step_bytes[0];
    let tail = &step_bytes[10..];
    let steady = tail.iter().sum::<usize>() / tail.len();
    println!(
        "MEASURE oscillation steady-state [10k scrollback, 20 steps ±1 col]: \
         first_step_bytes={first} steady_mean_bytes_per_step_last10={steady} all_steps={step_bytes:?}"
    );
    // Deep-narrow tier (200→80): wrapped rows far exceed the old row count,
    // so the pool runs dry and `Row::new` fallbacks dominate.
    let (before_b, _) = measure_alloc::snapshot();
    grid.resize(60, 80);
    let (after_b, _) = measure_alloc::snapshot();
    println!(
        "MEASURE deep narrow 200->80 [10k scrollback]: alloc_bytes={}",
        after_b - before_b
    );
    assert_eq!(grid.num_cols, 80);
}

// ── PLAN_audit_fix_batch3 3B: move-reflow content equivalence (red line) ──

/// Plain text of one physical row: WIDE_SPACER halves skipped, trailing
/// blank cells (space + empty flags — the same predicate reflow's
/// `content_end` uses) trimmed. Works for scrollback rows regardless of
/// their only-grow width.
fn row_plain_text(row: &Row) -> String {
    let last = row
        .cells
        .iter()
        .rposition(|c| c.character != ' ' || !c.flags.is_empty())
        .map_or(0, |i| i + 1);
    row.cells[..last]
        .iter()
        .filter(|c| !c.flags.contains(CellFlags::WIDE_SPACER))
        .map(|c| c.character)
        .collect()
}

/// Full document (scrollback ++ viewport) as logical lines: consecutive
/// rows joined while the previous row carries `wrapped`, all-blank lines
/// dropped (reflow's flush_line drops the same set).
fn logical_lines(grid: &Grid) -> Vec<String> {
    let mut rows: Vec<&Row> = Vec::with_capacity(grid.scrollback.len() + grid.num_rows);
    // T2: flat history materializes owned rows — park them locally and lend
    // them to the same `&Row` walk as the viewport.
    let history: Vec<Row> = (0..grid.scrollback.len())
        .map(|i| grid.scrollback.get(i).expect("history index in bounds"))
        .collect();
    rows.extend(history.iter());
    rows.extend(grid.viewport.iter());
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for row in rows {
        let text = row_plain_text(row);
        current.push_str(&text);
        if !row.wrapped && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// 3B red line: the move-semantics reflow must preserve logical lines
/// byte-for-byte across narrowing, widening, and a full round trip.
///
/// The fixture leaves the cursor on the LAST (short) line without a trailing
/// newline — the real resize scenario (shell sitting at the prompt). Grid
/// resize anchors the viewport window at the cursor and discards physical
/// rows below it by design, so a dangling cursor on a blank row below the
/// content would measure the discard path instead of reflow preservation.
#[test]
fn resize_move_reflow_preserves_logical_lines() {
    let mut grid = Grid::with_scrollback(6, 12, 64);
    let lines = [
        "short ascii",
        "a long ascii line that must soft wrap at twelve columns",
        "汉字宽字符",
        "中文内容需要换行的长行包含宽字符测量",
        "abcdefghijkl",
        "tail 8",
    ];
    for (i, line) in lines.iter().enumerate() {
        for ch in line.chars() {
            grid.write_char_with_attrs(
                ch,
                CellColor::Default,
                CellColor::Default,
                CellFlags::empty(),
            );
        }
        if i + 1 < lines.len() {
            grid.newline();
        }
    }
    let expected: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
    assert_eq!(
        logical_lines(&grid),
        expected,
        "fixture itself must read back as the written lines"
    );

    grid.resize(4, 10);
    assert_eq!(
        logical_lines(&grid),
        expected,
        "narrowing reflow must preserve logical lines"
    );
    grid.resize(6, 20);
    assert_eq!(
        logical_lines(&grid),
        expected,
        "widening reflow must preserve logical lines"
    );
    grid.resize(6, 12);
    assert_eq!(
        logical_lines(&grid),
        expected,
        "round-trip reflow must preserve logical lines"
    );
}

/// 3B red line: scrollback rows must survive the Phase-1 wholesale take and
/// re-emerge intact in the Phase-4 redistribution (a regression here would
/// silently empty history instead of merely cloning it). The cursor stays on
/// the last content line (no trailing newline) — the bottom-anchored resize
/// scenario — so the Phase-4 window covers the document tail.
#[test]
fn resize_move_keeps_scrollback_distribution_intact() {
    let mut grid = Grid::with_scrollback(3, 8, 32);
    for i in 1..=8usize {
        for ch in format!("line-{i:02}").chars() {
            grid.write_char_with_attrs(
                ch,
                CellColor::Default,
                CellColor::Default,
                CellFlags::empty(),
            );
        }
        if i < 8 {
            grid.newline();
        }
    }
    let expected: Vec<String> = (1..=8).map(|i| format!("line-{i:02}")).collect();
    assert_eq!(logical_lines(&grid), expected);

    grid.resize(4, 20);
    assert!(
        !grid.scrollback.is_empty(),
        "history must survive the resize"
    );
    let mut actual: Vec<String> = Vec::new();
    for i in 0..grid.scrollback.len() {
        actual.push(row_plain_text(&grid.scrollback.get(i).unwrap()));
    }
    for row in &grid.viewport {
        let text = row_plain_text(row);
        if !text.is_empty() {
            actual.push(text);
        }
    }
    assert_eq!(
        actual, expected,
        "history + viewport must rebuild the document in order"
    );
}

/// 3B red line, Phase-4 grow branch (FIX_DRAG_RESIZE_STUTTER β, review
/// P3-3): when the rewrapped document fits the new height
/// (`total <= new_rows`), the viewport is padded with fresh rows. The
/// padding must be indistinguishable from `Row::new(new_cols)` — a recycled
/// stale row leaking into the padding would render ghost content below the
/// document.
#[test]
fn resize_row_growth_pads_viewport_with_fresh_blank_rows() {
    let mut grid = Grid::with_scrollback(2, 10, 16);
    for ch in "hi".chars() {
        grid.write_char_with_attrs(
            ch,
            CellColor::Default,
            CellColor::Default,
            CellFlags::empty(),
        );
    }
    grid.resize(5, 10);
    assert_eq!(grid.num_rows, 5);
    assert_eq!(logical_lines(&grid), vec!["hi"]);
    let fresh = Row::new(10);
    for (i, row) in grid.viewport.iter().enumerate().skip(1) {
        assert!(!row.wrapped, "padding row {i} must be unwrapped");
        assert!(
            row.extras.is_empty(),
            "padding row {i} must carry no extras"
        );
        for (col, (a, b)) in row.cells.iter().zip(fresh.cells.iter()).enumerate() {
            assert_eq!(a.character, b.character, "padding row {i} col {col}");
            assert_eq!(a.flags, b.flags, "padding row {i} col {col}");
            assert_eq!(a.width, b.width, "padding row {i} col {col}");
        }
    }
}

// ── PLAN_audit_fix_batch3 3B: Scrollback::into_rows unit tests ───────────

fn row_with_label(label: char) -> Row {
    let mut row = Row::new(2);
    row.cells[0].character = label;
    row
}

#[test]
fn scrollback_into_rows_moves_rotated_ring_in_logical_order() {
    let mut sb = Scrollback::new(4);
    for label in ['a', 'b', 'c', 'd', 'e', 'f'] {
        sb.push(row_with_label(label));
    }
    // The ring retains the newest 4 rows (c..f); into_rows consumes the
    // buffer and must unwrap them to logical order.
    let rows = sb.into_rows();
    let text: String = rows.iter().map(|r| r.cells[0].character).collect();
    assert_eq!(text, "cdef", "rotated ring must unwrap to logical order");
    assert!(
        Scrollback::new(4).into_rows().is_empty(),
        "empty ring yields no rows"
    );
}

#[test]
fn scrollback_into_rows_moves_unwrapped_buffer_in_order() {
    let mut sb = Scrollback::new(8);
    for label in ['a', 'b', 'c'] {
        sb.push(row_with_label(label));
    }
    let rows = sb.into_rows();
    let text: String = rows.iter().map(|r| r.cells[0].character).collect();
    assert_eq!(text, "abc");
}
