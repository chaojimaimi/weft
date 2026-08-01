//! v1.10 Smart Select 集成测试：验证 matcher 与 `Grid::row_text` 的协同。
//!
//! 这不是新的产品代码——而是证明 V110_PLAN §3.2 的调用契约：
//! "调用方把 Grid 一行 cell 经 `row_text` 转成字符串，再用字节偏移调用 `match_at`"
//! 能正确工作，尤其针对 CJK 全角字符（1 char = 2 display cols）的场景。

use weft_core::grid::CellWidth;
use weft_core::smart_select::{match_at, SmartTargetKind};
use weft_core::vt::Terminal;

/// 取一行文本的字节偏移（与 Grid::row_text 返回的 String 索引一致）。
fn row_byte_idx(terminal: &Terminal, row: usize, needle: &str) -> usize {
    terminal
        .grid()
        .row_text(row)
        .find(needle)
        .unwrap_or_else(|| {
            panic!(
                "needle {needle:?} not found in row {row}: {:?}",
                terminal.grid().row_text(row)
            )
        })
}

#[test]
fn smart_select_url_from_grid_with_trailing_cjk() {
    // 模拟终端输出：CJK 注释 + URL + CJK 注释
    let mut t = Terminal::new(2, 60);
    t.process("访问 https://example.com/path 查看文档".as_bytes());
    assert_no_orphaned_wide(&t);

    let text = t.grid().row_text(0);
    let idx = text.find("example").unwrap();
    let target = match_at(&text, idx).expect("should detect URL");
    assert_eq!(target.kind, SmartTargetKind::Url);
    let matched = &text[target.start..target.end];
    assert_eq!(matched, "https://example.com/path");
    // 关键：不包含 CJK
    assert!(!matched.contains('访'));
    assert!(!matched.contains('查'));
}

#[test]
fn smart_select_path_line_column_from_grid() {
    let mut t = Terminal::new(2, 60);
    t.process(b"error at src/lib.rs:42:8 see");
    let text = t.grid().row_text(0);
    let idx = row_byte_idx(&t, 0, "lib");
    let target = match_at(&text, idx).expect("should detect path:line:col");
    assert_eq!(target.kind, SmartTargetKind::PathLineColumn);
    assert_eq!(&text[target.start..target.end], "src/lib.rs:42:8");
}

#[test]
fn smart_select_git_hash_from_grid() {
    let mut t = Terminal::new(2, 60);
    t.process(b"fixed in abc1234 yesterday");
    let text = t.grid().row_text(0);
    let idx = text.find("abc1").unwrap();
    let target = match_at(&text, idx).expect("should detect git hash");
    assert_eq!(target.kind, SmartTargetKind::GitHash);
    assert_eq!(&text[target.start..target.end], "abc1234");
}

#[test]
fn smart_select_cjk_word_returns_identifier_no_panic() {
    let mut t = Terminal::new(2, 40);
    t.process("纯中文无URL".as_bytes());
    assert_no_orphaned_wide(&t);
    let text = t.grid().row_text(0);
    let idx = text.find("中").unwrap();
    let target = match_at(&text, idx).expect("should fall back to identifier");
    assert_eq!(target.kind, SmartTargetKind::Identifier);
}

#[test]
fn smart_select_emoji_does_not_break_adjacent_url() {
    let mut t = Terminal::new(2, 60);
    // emoji (wide) + URL
    t.process("😀 https://x.com test".as_bytes());
    assert_no_orphaned_wide(&t);
    let text = t.grid().row_text(0);
    let idx = text.find("x.com").unwrap();
    let target = match_at(&text, idx).expect("URL after emoji");
    assert_eq!(target.kind, SmartTargetKind::Url);
    assert_eq!(&text[target.start..target.end], "https://x.com");
}

/// 镜像 replay_fixtures.rs 的 wide-cell 不变性检查（独立定义避免跨文件依赖）。
fn assert_no_orphaned_wide(t: &Terminal) {
    use weft_core::grid::CellFlags;
    let g = t.grid();
    for row in 0..g.num_rows {
        for col in 0..g.num_cols {
            let c = g.cell(row, col);
            if c.flags.contains(CellFlags::WIDE_SPACER) {
                assert!(col > 0, "wide spacer at left edge {row}:{col}");
                assert_eq!(
                    g.cell(row, col - 1).width,
                    CellWidth::Full,
                    "orphaned spacer {row}:{col}"
                );
            }
            if c.width == CellWidth::Full {
                assert!(col + 1 < g.num_cols, "wide lead at right edge {row}:{col}");
                assert!(
                    g.cell(row, col + 1).flags.contains(CellFlags::WIDE_SPACER),
                    "orphaned wide lead {row}:{col}"
                );
            }
        }
    }
}
