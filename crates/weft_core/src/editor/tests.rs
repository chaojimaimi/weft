use super::*;

#[test]
fn new_is_one_empty_line_cursor_origin() {
    let b = EditorBuffer::new();
    assert_eq!(b.lines, vec!["".to_string()]);
    assert_eq!(b.cursor, (0, 0));
    assert_eq!(b.text(), "");
}

#[test]
fn insert_char_appends_and_advances_cursor() {
    let mut b = EditorBuffer::new();
    b.insert_char('a');
    b.insert_char('b');
    assert_eq!(b.text(), "ab");
    assert_eq!(b.cursor, (0, 2));
}

#[test]
fn insert_char_in_middle() {
    let mut b = EditorBuffer::new();
    b.insert_char('a');
    b.insert_char('c');
    b.cursor.1 = 1; // between a and c
    b.insert_char('b');
    assert_eq!(b.text(), "abc");
    assert_eq!(b.cursor, (0, 2));
}

#[test]
fn backspace_deletes_char_behind_cursor() {
    let mut b = EditorBuffer::new();
    for c in "ab".chars() {
        b.insert_char(c);
    }
    b.delete_backspace();
    assert_eq!(b.text(), "a");
    assert_eq!(b.cursor, (0, 1));
}

#[test]
fn backspace_at_line_start_merges_lines() {
    let mut b = EditorBuffer::new();
    b.lines = vec!["ab".to_string(), "cd".to_string()];
    b.cursor = (1, 0);
    b.delete_backspace();
    assert_eq!(b.text(), "abcd");
    assert_eq!(b.cursor, (0, 2));
    assert_eq!(b.line_count(), 1);
}

#[test]
fn backspace_at_origin_is_noop() {
    let mut b = EditorBuffer::new();
    b.delete_backspace();
    assert_eq!(b.cursor, (0, 0));
    assert_eq!(b.text(), "");
}

#[test]
fn move_left_within_line() {
    let mut b = EditorBuffer::new();
    for c in "ab".chars() {
        b.insert_char(c);
    }
    b.move_left();
    assert_eq!(b.cursor, (0, 1));
    b.move_left();
    assert_eq!(b.cursor, (0, 0));
}

#[test]
fn move_left_at_line_start_goes_to_prev_line_end() {
    let mut b = EditorBuffer::new();
    b.lines = vec!["ab".to_string(), "cd".to_string()];
    b.cursor = (1, 0);
    b.move_left();
    assert_eq!(b.cursor, (0, 2));
}

#[test]
fn move_right_within_and_across_lines() {
    let mut b = EditorBuffer::new();
    b.lines = vec!["ab".to_string(), "cd".to_string()];
    b.cursor = (0, 2); // end of line 0
    b.move_right(); // → line 1, col 0
    assert_eq!(b.cursor, (1, 0));
    b.move_right();
    b.move_right(); // → end of line 1
    b.move_right(); // clamps, stays
    assert_eq!(b.cursor, (1, 2));
}

#[test]
fn delete_forward_removes_char_at_cursor() {
    let mut b = EditorBuffer::new();
    b.lines = vec!["ab".to_string()];
    b.cursor = (0, 0);
    b.delete_forward();
    assert_eq!(b.text(), "b");
    assert_eq!(b.cursor, (0, 0));
}

#[test]
fn delete_forward_at_line_end_merges_next_line() {
    let mut b = EditorBuffer::new();
    b.lines = vec!["ab".to_string(), "cd".to_string()];
    b.cursor = (0, 2);
    b.delete_forward();
    assert_eq!(b.text(), "abcd");
    assert_eq!(b.cursor, (0, 2));
}

#[test]
fn move_home_and_end_within_line() {
    let mut b = EditorBuffer::new();
    b.lines = vec!["abc".to_string()];
    b.cursor = (0, 1);
    b.move_line_end();
    assert_eq!(b.cursor, (0, 3));
    b.move_line_home();
    assert_eq!(b.cursor, (0, 0));
}

#[test]
fn delete_word_back_eats_trailing_word() {
    let mut b = EditorBuffer::new();
    b.lines = vec!["foo bar".to_string()];
    b.cursor = (0, 7);
    b.delete_word_back();
    assert_eq!(b.text(), "foo ");
    assert_eq!(b.cursor, (0, 4));
}

#[test]
fn delete_word_back_eats_preceding_whitespace() {
    let mut b = EditorBuffer::new();
    b.lines = vec!["foo  ".to_string()];
    b.cursor = (0, 5);
    b.delete_word_back();
    // Eats trailing spaces then the word "foo".
    assert_eq!(b.text(), "");
}

#[test]
fn clear_line_empties_current_line() {
    let mut b = EditorBuffer::new();
    b.lines = vec!["abc".to_string()];
    b.cursor = (0, 2);
    b.clear_line();
    assert_eq!(b.text(), "");
    assert_eq!(b.cursor, (0, 0));
}

#[test]
fn delete_to_end_truncates_after_cursor() {
    let mut b = EditorBuffer::new();
    b.lines = vec!["abcdef".to_string()];
    b.cursor = (0, 2);
    b.delete_to_end();
    assert_eq!(b.text(), "ab");
    assert_eq!(b.cursor, (0, 2));
}

#[test]
fn split_newline_breaks_line_at_cursor() {
    let mut b = EditorBuffer::new();
    b.lines = vec!["abcd".to_string()];
    b.cursor = (0, 2);
    b.split_newline();
    assert_eq!(b.text(), "ab\ncd");
    assert_eq!(b.cursor, (1, 0));
    assert_eq!(b.line_count(), 2);
}

// ── F2 P0-1: ensure_cursor_visible ──────────────────────────────────

#[test]
fn ensure_cursor_visible_no_scroll_when_lines_fit() {
    let mut b = EditorBuffer::new();
    b.lines = (0..3).map(|_| "x".into()).collect();
    b.cursor = (2, 0);
    b.ensure_cursor_visible(5);
    assert_eq!(b.scroll_offset, 0);
}

#[test]
fn ensure_cursor_visible_scrolls_down_when_cursor_below_window() {
    let mut b = EditorBuffer::new();
    b.lines = (0..10).map(|_| "x".into()).collect();
    b.cursor = (7, 0); // cursor on line 7
    b.scroll_offset = 0;
    b.ensure_cursor_visible(3); // window [0, 3)
    assert_eq!(b.scroll_offset, 5); // → window [5, 8), cursor visible
}

#[test]
fn ensure_cursor_visible_scrolls_up_when_cursor_above_window() {
    let mut b = EditorBuffer::new();
    b.lines = (0..10).map(|_| "x".into()).collect();
    b.cursor = (2, 0);
    b.scroll_offset = 6; // window [6, 9)
    b.ensure_cursor_visible(3);
    assert_eq!(b.scroll_offset, 2); // → window [2, 5)
}

#[test]
fn ensure_cursor_visible_keeps_offset_when_cursor_in_window() {
    let mut b = EditorBuffer::new();
    b.lines = (0..10).map(|_| "x".into()).collect();
    b.cursor = (4, 0);
    b.scroll_offset = 3; // window [3, 6)
    b.ensure_cursor_visible(3);
    assert_eq!(b.scroll_offset, 3); // unchanged
}

#[test]
fn ensure_cursor_visible_resets_to_zero_when_all_fit() {
    let mut b = EditorBuffer::new();
    b.lines = (0..4).map(|_| "x".into()).collect();
    b.cursor = (3, 0);
    b.scroll_offset = 2;
    b.ensure_cursor_visible(10);
    assert_eq!(b.scroll_offset, 0);
}

#[test]
fn ensure_cursor_visible_cursor_at_last_line_scrolls_to_end() {
    let mut b = EditorBuffer::new();
    b.lines = (0..10).map(|_| "x".into()).collect();
    b.cursor = (9, 0);
    b.scroll_offset = 0;
    b.ensure_cursor_visible(3); // window [0, 3)
    assert_eq!(b.scroll_offset, 7); // → window [7, 10)
}

fn editor_with_history(history: &[&str]) -> Editor {
    let mut e = Editor::new();
    e.load_history(history.iter().map(|s| s.to_string()).collect());
    e
}

#[test]
fn history_prev_fills_buffer_and_clamps() {
    let mut e = editor_with_history(&["first", "second"]);
    e.history_prev();
    assert_eq!(e.buffer.text(), "second"); // newest first
    e.history_prev();
    assert_eq!(e.buffer.text(), "first");
    e.history_prev(); // clamps at oldest
    assert_eq!(e.buffer.text(), "first");
}

#[test]
fn history_next_returns_to_empty() {
    let mut e = editor_with_history(&["only"]);
    e.history_prev();
    e.history_next();
    assert_eq!(e.buffer.text(), "");
}

#[test]
fn push_history_adds_commands_newest_first() {
    let mut e = Editor::new();
    assert!(e.history().is_empty());
    e.push_history("ls");
    e.push_history("git status");
    e.push_history("pwd");
    // newest-first
    assert_eq!(e.history(), &["pwd", "git status", "ls"]);
}

#[test]
fn push_history_skips_empty_and_dedup_newest() {
    let mut e = Editor::new();
    e.push_history("ls");
    e.push_history(""); // skipped
    e.push_history("   "); // skipped (whitespace only)
    e.push_history("ls"); // dedup against newest
    assert_eq!(e.history(), &["ls"]);
    e.push_history("pwd");
    assert_eq!(e.history(), &["pwd", "ls"]);
}

#[test]
fn submit_command_pushes_to_history() {
    // Verify that the Terminal wiring records submitted commands.
    use crate::vt::Terminal;
    let mut t = Terminal::new(24, 80);
    // Simulate typing a command into the editor.
    for c in "ls -la".chars() {
        t.editor_mut().buffer.insert_char(c);
    }
    t.submit_command();
    assert_eq!(t.editor().history(), &["ls -la"]);
    // ↑ should now recall it.
    t.editor_mut().history_prev();
    assert_eq!(t.editor().text(), "ls -la");
}

#[test]
fn search_finds_subsequence_match() {
    let mut e = editor_with_history(&["git status", "git push", "ls -la"]);
    e.search_start();
    e.search_input('g');
    e.search_input('p'); // query "gp" matches "git push" only
    assert_eq!(e.search_selected_text(), Some("git push".to_string()));
}

#[test]
fn search_next_cycles_through_matches() {
    let mut e = editor_with_history(&["git status", "git push"]);
    e.search_start();
    e.search_input('g');
    e.search_input('i');
    e.search_input('t'); // "git" matches both
    assert_eq!(e.search_selected_text(), Some("git push".to_string())); // newest
    e.search_next();
    assert_eq!(e.search_selected_text(), Some("git status".to_string()));
}

#[test]
fn search_accept_fills_buffer() {
    let mut e = editor_with_history(&["git push"]);
    e.search_start();
    e.search_input('g');
    e.search_accept();
    assert_eq!(e.buffer.text(), "git push");
    assert!(!e.is_searching());
}

#[test]
fn search_cancel_restores_original() {
    let mut e = editor_with_history(&["git push"]);
    e.buffer.insert_char('x');
    e.search_start();
    e.search_input('g');
    e.search_cancel();
    assert_eq!(e.buffer.text(), "x");
    assert!(!e.is_searching());
}

#[test]
fn search_no_match_keeps_empty_selection() {
    let mut e = editor_with_history(&["ls"]);
    e.search_start();
    e.search_input('z');
    assert_eq!(e.search_selected_text(), None);
}

#[test]
fn insert_char_rejects_control_chars() {
    let mut b = EditorBuffer::new();
    b.insert_char('a');
    b.insert_char('\x1b'); // ESC — spoofing/injection vector, rejected
    b.insert_char('\x00'); // NUL
    b.insert_char('b');
    assert_eq!(b.text(), "ab");
    assert_eq!(b.cursor, (0, 2));
}

#[test]
fn set_text_replaces_and_positions_cursor_at_end() {
    let mut b = EditorBuffer::new();
    b.set_text("ab\ncd");
    assert_eq!(b.text(), "ab\ncd");
    assert_eq!(b.cursor, (1, 2));
    // Empty -> single empty line, cursor at origin.
    b.set_text("");
    assert_eq!(b.text(), "");
    assert_eq!(b.cursor, (0, 0));
}

#[test]
fn search_prev_navigates_backwards_and_wraps() {
    let mut e = editor_with_history(&["git status", "git push"]);
    e.search_start();
    for c in "git".chars() {
        e.search_input(c);
    }
    assert_eq!(e.search_selected_text(), Some("git push".to_string())); // newest
    e.search_prev();
    assert_eq!(e.search_selected_text(), Some("git status".to_string()));
    e.search_prev(); // wraps back to newest
    assert_eq!(e.search_selected_text(), Some("git push".to_string()));
}

// ── Tab completion ────────────────────────────────────────────────────

use crate::complete::{Match, MatchKind, MATCH_QUALITY_PREFIX};

fn m(label: &str, insert: &str) -> Match {
    Match {
        label: label.into(),
        kind: MatchKind::Command,
        insert: insert.into(),
        is_dir: false,
        match_quality: MATCH_QUALITY_PREFIX,
    }
}

#[test]
fn start_then_next_cycles() {
    let mut e = Editor::new();
    e.start_completion(vec![m("a", "a"), m("b", "b"), m("c", "c")], 0, 1);
    assert_eq!(e.completion_view().unwrap().1, 0);
    e.completion_next();
    assert_eq!(e.completion_view().unwrap().1, 1);
    e.completion_next();
    assert_eq!(e.completion_view().unwrap().1, 2);
    e.completion_next(); // wraps
    assert_eq!(e.completion_view().unwrap().1, 0);
}

#[test]
fn prev_wraps_to_last() {
    let mut e = Editor::new();
    e.start_completion(vec![m("a", "a"), m("b", "b")], 0, 1);
    e.completion_prev(); // from 0 -> wraps to last
    assert_eq!(e.completion_view().unwrap().1, 1);
}

#[test]
fn cancel_clears() {
    let mut e = Editor::new();
    e.start_completion(vec![m("a", "a")], 0, 1);
    assert!(e.is_completing());
    e.completion_cancel();
    assert!(!e.is_completing());
}

#[test]
fn accept_applies_insert_and_clears() {
    let mut e = Editor::new();
    e.buffer.lines = vec!["ls".to_string()];
    e.buffer.cursor = (0, 2); // cursor at end of "ls"
                              // replace the whole word "ls" (cols 0..2) with the selected insert "lsof"
    e.start_completion(vec![m("lsof", "lsof")], 0, 2);
    assert!(e.completion_accept());
    assert_eq!(e.buffer.lines[0], "lsof");
    assert_eq!(e.buffer.cursor.1, 4); // cursor after "lsof"
    assert!(!e.is_completing());
}

#[test]
fn accept_preserves_text_around_word() {
    let mut e = Editor::new();
    e.buffer.lines = vec!["echo ls more".to_string()];
    e.buffer.cursor = (0, 7); // cursor right after "ls"
                              // word "ls" occupies cols 5..7
    e.start_completion(vec![m("lsof", "lsof")], 5, 7);
    assert!(e.completion_accept());
    assert_eq!(e.buffer.lines[0], "echo lsof more");
}

#[test]
fn accept_returns_false_when_not_completing() {
    let mut e = Editor::new();
    assert!(!e.completion_accept());
}

#[test]
fn start_with_empty_matches_is_noop() {
    let mut e = Editor::new();
    e.start_completion(vec![], 0, 1);
    assert!(!e.is_completing());
}
