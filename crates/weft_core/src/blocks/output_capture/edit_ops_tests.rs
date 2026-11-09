// ICH/DCH/ECH 捕获镜像三原语（edit_ops）的单元测试。
// 语义模板 = erase_line（v1.13.12 FIX-E：含光标格、char 空间、行内受限、入口水印）。
// 维度：行中 / 行尾 / 超尾截断 / 空行 / CJK 相邻与跨界 / cap 部分插入 / 水印单调 /
// 样式 run 完整性（insert 后移、delete 前移含跨行）。PLAN_v1188 §5.1。

use super::*;
use crate::blocks::DEFAULT_OUTPUT_CAP;
use crate::grid::{CellColor, CellFlags};

fn owned(fg: CellColor, flags: CellFlags, bg: CellColor) -> CapturedStyle {
    CapturedStyle {
        fg,
        bg,
        flags,
        ansi_owned: true,
        ..CapturedStyle::default()
    }
}

fn fg_style(palette: u8) -> CapturedStyle {
    owned(
        CellColor::Palette(palette),
        CellFlags::empty(),
        CellColor::Default,
    )
}

fn run(start_char: u32, end_char: u32, style: CapturedStyle) -> CapturedStyleRun {
    CapturedStyleRun {
        start_char,
        end_char,
        style,
    }
}

/// 单行 `text`，cursor 定位到第 `col` 个 char（char 边界）。
fn char_space(text: &str, col: usize) -> OutputCapture {
    let mut output = OutputCapture::default();
    output.print_str(text, CapturedStyle::default(), DEFAULT_OUTPUT_CAP);
    let mut cursor = 0usize;
    for _ in 0..col {
        cursor += output.text[cursor..]
            .chars()
            .next()
            .map_or(0, char::len_utf8);
    }
    output.cursor = cursor;
    output.char_cursor = col as u32;
    output
}

// ── ICH：insert_blank ────────────────────────────────────────────────

#[test]
fn insert_blank_mid_line_shifts_tail() {
    let mut output = char_space("abcdef", 3);
    output.insert_blank(2, DEFAULT_OUTPUT_CAP);
    assert_eq!(output.as_str(), "abc  def");
    // cursor / char_cursor 不动，ledger 只增实加数。
    assert_eq!(output.char_cursor, 3);
    assert_eq!(output.text_char_count, 8);
}

#[test]
fn insert_blank_at_line_end_appends_spaces() {
    let mut output = char_space("abc", 3);
    output.insert_blank(2, DEFAULT_OUTPUT_CAP);
    assert_eq!(output.as_str(), "abc  ");
}

#[test]
fn insert_blank_partial_under_cap_sets_truncated() {
    let mut output = char_space("abc", 1);
    // cap=6：已有 3 字节，预算 3 → 只插前缀 3 个空格（want 5）。
    output.insert_blank(5, 6);
    assert_eq!(output.as_str(), "a   bc");
    assert!(
        output.truncated,
        "cap 部分插入必须置 truncated（goto 先例）"
    );
    assert_eq!(output.text_char_count, 6);
}

#[test]
fn insert_blank_zero_is_noop() {
    let mut output = char_space("abc", 1);
    output.insert_blank(0, 2);
    assert_eq!(output.as_str(), "abc");
    assert!(!output.truncated);
}

#[test]
fn insert_blank_on_empty_line() {
    let mut output = OutputCapture::default();
    output.insert_blank(3, DEFAULT_OUTPUT_CAP);
    assert_eq!(output.as_str(), "   ");
}

#[test]
fn insert_blank_after_cjk_at_char_boundary() {
    let mut output = char_space("你好world", 2);
    output.insert_blank(1, DEFAULT_OUTPUT_CAP);
    assert_eq!(output.as_str(), "你好 world");
    assert_eq!(output.char_cursor, 2);
}

#[test]
fn insert_blank_aligns_misaligned_cursor_to_char_boundary() {
    // 防御分支：cursor 落在「你」中间（正常操作序列不可达）。
    let mut output = char_space("你好x", 0);
    output.cursor = 1; // 「你」的第二个字节
    output.char_cursor = 1;
    output.insert_blank(1, DEFAULT_OUTPUT_CAP);
    assert_eq!(output.as_str(), " 你好x");
    assert_eq!(output.char_cursor, 0);
}

// ── DCH：delete_chars ────────────────────────────────────────────────

#[test]
fn delete_chars_mid_line() {
    let mut output = char_space("abcDEFghi", 3);
    output.delete_chars(3);
    assert_eq!(output.as_str(), "abcghi");
    // cursor 不动；后续内容在 char 空间整体前移。
    assert_eq!(output.char_cursor, 3);
    assert_eq!(output.text_char_count, 6);
}

#[test]
fn delete_chars_overrun_clamps_to_line_tail() {
    let mut output = char_space("abc", 1);
    output.delete_chars(10);
    assert_eq!(output.as_str(), "a");
    assert_eq!(output.text_char_count, 1);
}

#[test]
fn delete_chars_at_line_end_is_noop() {
    let mut output = char_space("abc", 3);
    output.delete_chars(2);
    assert_eq!(output.as_str(), "abc");
}

#[test]
fn delete_chars_never_crosses_newline() {
    // cursor 停在行尾（\n 处）：\n 与下一行必须原样保留。
    let mut output = OutputCapture::default();
    output.print_ascii(b"ab", CapturedStyle::default(), DEFAULT_OUTPUT_CAP);
    output.newline(DEFAULT_OUTPUT_CAP);
    output.print_ascii(b"cd", CapturedStyle::default(), DEFAULT_OUTPUT_CAP);
    output.cursor = 2;
    output.char_cursor = 2;
    output.delete_chars(5);
    assert_eq!(output.as_str(), "ab\ncd");
    assert_eq!(output.text_char_count, 5);
}

#[test]
fn delete_chars_on_empty_buffer_is_noop() {
    let mut output = OutputCapture::default();
    output.delete_chars(3);
    assert_eq!(output.as_str(), "");
}

// ── ECH：erase_chars ─────────────────────────────────────────────────

#[test]
fn erase_chars_mid_line_rewrites_spaces_in_place() {
    let mut output = char_space("abcDEF", 3);
    output.erase_chars(2);
    assert_eq!(output.as_str(), "abc  F");
    // 同 char 数替换：ledger 不动、cursor 不动（EL mode-1 技巧）。
    assert_eq!(output.char_cursor, 3);
    assert_eq!(output.text_char_count, 6);
}

#[test]
fn erase_chars_overrun_clamps_to_line_tail() {
    let mut output = char_space("abc", 1);
    output.erase_chars(10);
    // ECH 同 char 数替换不增长行（grid 版 end=min(col+count,num_cols)
    // 从不加格）：超尾截断只擦 [1,3) → "a  "，ledger 仍 3。
    assert_eq!(output.as_str(), "a  ");
    assert_eq!(output.text_char_count, 3);
}

#[test]
fn erase_chars_at_line_end_is_noop() {
    let mut output = char_space("abc", 3);
    output.erase_chars(1);
    assert_eq!(output.as_str(), "abc");
}

#[test]
fn erase_chars_cjk_replaces_one_char_with_one_space() {
    // "wide chars carry no filler"（erase_line mode-1 先例）：char 空间
    // 一个全角 = 一个空格，与 grid 的 2-cell 语义分歧备案。
    let mut output = char_space("你好x", 0);
    output.erase_chars(1);
    assert_eq!(output.as_str(), " 好x");
    assert_eq!(output.text_char_count, 3);
}

// ── 水印单调（M6-a）──────────────────────────────────────────────────

#[test]
fn edit_ops_record_min_write_offset_monotonically() {
    let mut output = char_space("abcdef", 3);
    output.delete_chars(1);
    assert_eq!(output.min_write_offset_value(), 3);
    // 更晚、更靠后的原地写不会把水印拉高（fetch_min 语义）。
    output.cursor = 4;
    output.char_cursor = 4;
    output.erase_chars(1);
    assert_eq!(output.min_write_offset_value(), 3);
    output.insert_blank(1, DEFAULT_OUTPUT_CAP);
    assert_eq!(output.min_write_offset_value(), 3);
}

#[test]
fn insert_blank_on_fresh_capture_records_watermark_zero() {
    let mut output = OutputCapture::default();
    output.insert_blank(2, DEFAULT_OUTPUT_CAP);
    assert_eq!(output.min_write_offset_value(), 0);
}

// ── 样式 run 完整性（shift_style_at）────────────────────────────────

#[test]
fn insert_blank_shifts_style_runs_after_cursor() {
    let mut output = char_space("abcdef", 3);
    output.style_runs = vec![run(1, 3, fg_style(1)), run(4, 6, fg_style(2))];
    output.insert_blank(2, DEFAULT_OUTPUT_CAP);
    // cursor 前的 run 不动；cursor 后的 run 整体后移 2。
    assert_eq!(output.style_runs[0].start_char, 1);
    assert_eq!(output.style_runs[0].end_char, 3);
    assert_eq!(output.style_runs[1].start_char, 6);
    assert_eq!(output.style_runs[1].end_char, 8);
}

#[test]
fn insert_blank_splits_style_run_straddling_cursor() {
    let mut output = char_space("abcdef", 3);
    output.style_runs = vec![run(2, 5, fg_style(2))];
    output.insert_blank(2, DEFAULT_OUTPUT_CAP);
    // [2,5) 跨 cursor=3：拆成 [2,3) + [5,7)，插入的空格保持默认样式。
    assert_eq!(output.style_runs.len(), 2);
    assert_eq!(output.style_runs[0].start_char, 2);
    assert_eq!(output.style_runs[0].end_char, 3);
    assert_eq!(output.style_runs[1].start_char, 5);
    assert_eq!(output.style_runs[1].end_char, 7);
}

#[test]
fn delete_chars_shifts_style_runs_forward_across_lines() {
    // B2 钉子：删除点之后的 run —— 无论是否跨行 —— 在 char 空间全局前移。
    let mut output = OutputCapture::default();
    output.print_ascii(b"abcd", fg_style(0), DEFAULT_OUTPUT_CAP);
    output.newline(DEFAULT_OUTPUT_CAP);
    output.print_ascii(b"efgh", fg_style(0), DEFAULT_OUTPUT_CAP);
    output.cursor = 2;
    output.char_cursor = 2;
    output.style_runs = vec![
        run(1, 3, fg_style(1)), // "bc"，跨删除区间 [2,4)
        run(3, 5, fg_style(2)), // "de"，起于删除区间内
        run(6, 8, fg_style(3)), // 下一行 "fg"，全局前移
    ];
    output.delete_chars(2);
    assert_eq!(output.as_str(), "ab\nefgh");
    // [1,3) → 头 [1,2) 存活；[3,5) → 尾 [2,3) 回拉；[6,8) → [4,6)。
    assert_eq!(output.style_runs.len(), 3);
    assert_eq!(output.style_runs[0].start_char, 1);
    assert_eq!(output.style_runs[0].end_char, 2);
    assert_eq!(output.style_runs[1].start_char, 2);
    assert_eq!(output.style_runs[1].end_char, 3);
    assert_eq!(output.style_runs[2].start_char, 4);
    assert_eq!(output.style_runs[2].end_char, 6);
}

#[test]
fn delete_chars_drops_style_run_fully_inside_removed_range() {
    let mut output = char_space("abcd", 1);
    output.style_runs = vec![run(1, 3, fg_style(1))];
    output.delete_chars(2);
    assert_eq!(output.as_str(), "ad");
    assert!(
        output.style_runs.is_empty(),
        "[1,3) 完全落入 [1,3) 移除区间"
    );
}

#[test]
fn erase_chars_clears_style_over_wiped_range() {
    let mut output = char_space("abcdef", 1);
    output.style_runs = vec![run(1, 3, fg_style(1))];
    output.erase_chars(2);
    // 擦 [1,3)('b','c')：同 char 数替换 → "a  def"，'c' 已被改写。
    assert_eq!(output.as_str(), "a  def");
    assert!(output.style_runs.is_empty(), "擦除区间改写为默认样式的空格");
}

#[test]
fn delete_chars_coalesces_style_runs_reunited_by_removal() {
    // "aaXbb"：两段同 style 被 X（默认样式）隔开；删掉中间的 X 后两段在
    // char 空间重新相邻 → 合并回一个 run（镜像 splice_style 的合并 pass；
    // insert 拆分出的头尾段隔着默认样式的插入区，永不触发合并）。
    let mut output = char_space("aaXbb", 2);
    let same = fg_style(4);
    output.style_runs = vec![run(0, 2, same), run(3, 5, same)];
    output.delete_chars(1);
    assert_eq!(output.as_str(), "aabb");
    assert_eq!(output.style_runs.len(), 1);
    assert_eq!(output.style_runs[0].start_char, 0);
    assert_eq!(output.style_runs[0].end_char, 4);
}

#[test]
fn sudo_su_inline_edit_shape_inserts_before_tail() {
    // 端到端形状钉子（grid 对齐）：`ls -al | grep ""` ← ×8 后插 8 个 s，
    // 捕获必须保留完整行尾 —— sudo su bug 的最小捕获级复现。
    let mut output = OutputCapture::default();
    output.print_str(
        "sh-3.2$ ls -al | grep \"\"",
        CapturedStyle::default(),
        DEFAULT_OUTPUT_CAP,
    );
    output.set_cursor_column(16, DEFAULT_OUTPUT_CAP);
    for _ in 0..8 {
        output.insert_blank(1, DEFAULT_OUTPUT_CAP);
        output.print('s', CapturedStyle::default(), DEFAULT_OUTPUT_CAP);
    }
    assert_eq!(
        output.as_str(),
        "sh-3.2$ ls -al |ssssssss grep \"\"",
        "修复前：print 覆写导致行尾 grep \"\" 丢失"
    );
}
