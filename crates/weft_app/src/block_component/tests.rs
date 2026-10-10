// ── Tests ───────────────────────────────────────────────────────────────

use super::*;
use std::time::{Duration, SystemTime};
use weft_core::blocks::{Block, BlockId};

#[test]
fn line_end_col_measures_display_cells_one_past_last_glyph() {
    let output = "cmd\nPassword: \n";
    assert_eq!(block_view_line_end_col(output, 0), 3);
    assert_eq!(block_view_line_end_col(output, 1), 10);
    // CJK glyphs occupy two cells — matches the painted-glyph convention.
    assert_eq!(block_view_line_end_col("密码\n", 0), 4);
    // Out-of-range line saturates to column 0.
    assert_eq!(block_view_line_end_col(output, 7), 0);
}

#[test]
fn tui_caret_row_matches_live_rows_only() {
    // v1.10.5 (reviewer HIGH): 已完成块的 0 基输出索引与 live 快照行索引数字冲突,
    // caret/preedit 只能画在 live 行(block_id == None)。
    let cursor_line = 33;
    assert!(
        tui_caret_row_matches(None, cursor_line, cursor_line),
        "live row at the cursor line matches"
    );
    assert!(
        !tui_caret_row_matches(None, cursor_line + 1, cursor_line),
        "live row off the cursor line does not match"
    );
    assert!(
        !tui_caret_row_matches(Some(7), cursor_line, cursor_line),
        "finished-block row with a colliding index must NOT match"
    );
    assert!(
        !tui_caret_row_matches(Some(7), 0, cursor_line),
        "finished-block row off the cursor line does not match"
    );
}

fn block(exit_code: Option<i32>, collapsed: bool) -> Block {
    let started_at = SystemTime::UNIX_EPOCH;
    Block {
        id: BlockId(1),
        command: "cargo test".into(),
        cwd: Some("/tmp/weft".into()),
        output: "one\ntwo\nthree\n".into(),
        styled_output: None,
        exit_code,
        started_at,
        finished_at: Some(started_at + Duration::from_millis(1200)),
        collapsed,
        screen_origin: false,
    }
}

#[test]
fn collapsed_block_uses_one_line_summary() {
    let presentation = block_presentation(&block(Some(0), true), 3);
    assert_eq!(presentation.cwd, "3 lines");
    assert_eq!(presentation.duration, "1.2s");
    assert_eq!(presentation.status, "", "成功块 status 必须为空");
    assert_eq!(presentation.label(), "3 lines · 1.2s");
    assert_eq!(presentation.tone, BlockTone::Success);
}

#[test]
fn expanded_block_keeps_context_and_surfaces_failure() {
    let presentation = block_presentation(&block(Some(7), false), 3);
    assert_eq!(presentation.cwd, "/tmp/weft");
    assert_eq!(presentation.duration, "1.2s");
    assert_eq!(presentation.status, "exit 7");
    assert_eq!(presentation.label(), "/tmp/weft · 1.2s · exit 7");
    assert_eq!(presentation.tone, BlockTone::Error);
}

#[test]
fn interrupted_block_has_warning_tone() {
    let presentation = block_presentation(&block(None, true), 1);
    assert_eq!(presentation.cwd, "1 line");
    assert_eq!(presentation.duration, "1.2s");
    assert_eq!(presentation.status, "interrupted");
    assert_eq!(presentation.label(), "1 line · 1.2s · interrupted");
    assert_eq!(presentation.tone, BlockTone::Warning);
}

#[test]
fn interrupted_opencode_block_surfaces_stable_resume_commands() {
    let mut interrupted = block(None, false);
    interrupted.command = "/Users/me/.opencode/bin/opencode".into();
    assert_eq!(
        command_resume_hints(&interrupted),
        [
            "Continue last session: opencode -c",
            "Choose a session: opencode session list; opencode -s <session-id>",
        ]
    );

    interrupted.exit_code = Some(130);
    assert!(!command_resume_hints(&interrupted).is_empty());
    interrupted.exit_code = Some(0);
    assert!(command_resume_hints(&interrupted).is_empty());
}

#[test]
fn running_block_metrics_include_context_as_a_live_header() {
    fn running_terminal(cwd: bool) -> Terminal {
        let mut terminal = Terminal::new(24, 80);
        if cwd {
            terminal.process(b"\x1b]7;file://localhost/Users/me/.hermes\x07");
        }
        terminal.process(b"\x1b]133;A\x07hermes update\x1b]133;B\x07\x1b]133;C\x07");
        terminal
    }

    let mut late_cwd = running_terminal(false);
    assert_eq!(block_content_metrics(&late_cwd, 80, 1).0, 2);
    late_cwd.process(b"\x1b]7;file://localhost/Users/me/.hermes\x07");
    assert_eq!(block_content_metrics(&late_cwd, 80, 1).0, 3);
    assert_eq!(block_content_metrics(&running_terminal(true), 80, 1).0, 3);
    // live_context_label 的形状断言迁往 paint/ui_helpers.rs(与
    // block_duration_str 同域;P1 加 elapsed 参数后本文件超行数预算)。
}

#[test]
fn primary_tui_document_exposes_positive_local_scroll_range() {
    let mut terminal = Terminal::new(4, 32);
    terminal.process(b"\x1b]133;A\x07claude\x1b]133;B\x07\x1b]133;C\x07");
    terminal.process(b"\x1b[H\x1b[2;1H\x1b[2J\x1b[Hone\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix");
    terminal.set_primary_history_view(true);

    let total = block_content_metrics(&terminal, 32, 2).0;
    assert!(total > terminal.grid().num_rows);
    assert!(total.saturating_sub(terminal.grid().num_rows) > 0);
}

#[test]
fn primary_history_repaint_does_not_force_follow_live_tail() {
    use weft_core::blocks::ShellPhase;

    assert!(should_follow_running_output(
        ShellPhase::CommandExecuting,
        false
    ));
    assert!(!should_follow_running_output(
        ShellPhase::CommandExecuting,
        true
    ));
    assert!(!should_follow_running_output(ShellPhase::AtPrompt, false));
}

#[test]
fn block_scroll_is_reconciled_when_a_larger_viewport_shrinks_the_range() {
    let compact = crate::layout::LayoutCtx::new((800.0, 400.0), 10.0, 20.0, 10.0, 10.0);
    let expanded = crate::layout::LayoutCtx::new((1200.0, 900.0), 10.0, 20.0, 10.0, 10.0);
    let compact_visible = crate::layout::block_visible_rows(&compact, None, false);
    let expanded_visible = crate::layout::block_visible_rows(&expanded, None, false);
    assert!(expanded_visible > compact_visible);

    let total = 100;
    let compact_top = total - compact_visible;
    let expanded_top = total - expanded_visible;
    assert_eq!(
        reconciled_block_scroll(compact_top, total, expanded_visible),
        expanded_top
    );
    assert_eq!(reconciled_block_scroll(12, 20, 30), 0);
}

#[test]
fn terminal_scroll_reconciliation_uses_shared_layout_geometry() {
    let mut terminal = Terminal::new(8, 32);
    terminal.process(
        b"\x1b]133;A\x07long-command\x1b]133;B\x07\x1b]133;C\x07\x1b[H\x1b[2;1H\x1b[2J\x1b[Hone\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix",
    );
    terminal.set_primary_history_view(true);
    let layout = crate::layout::LayoutCtx::new((1200.0, 900.0), 10.0, 20.0, 10.0, 10.0);

    let (scroll, total, visible) =
        reconciled_terminal_block_scroll(&terminal, &layout, 2, usize::MAX).unwrap();
    assert_eq!(scroll, total.saturating_sub(visible));
}

#[test]
fn block_metrics_include_wrapped_resume_hint_rows() {
    let mut terminal = Terminal::new(24, 80);
    terminal.process(b"\x1b]133;A\x07opencode\x1b]133;B\x07\x1b]133;C\x07\x1b]133;D;130\x07");

    assert_eq!(block_content_metrics(&terminal, 80, 1).0, 6);
    assert!(block_content_metrics(&terminal, 20, 1).0 > 6);
    assert_eq!(
        block_content_metrics(&terminal, 80, 2).0,
        block_content_metrics(&terminal, 80, 1).0 + 1
    );
}

#[test]
fn command_output_gap_only_exists_when_a_result_is_visible() {
    assert_eq!(command_output_gap_rows(0), 0);
    assert_eq!(command_output_gap_rows(1), 1);
    assert_eq!(command_output_gap_rows(2000), 1);
}

#[test]
fn clear_block_reserves_a_reachable_terminal_sized_history_band() {
    let mut terminal = Terminal::new(6, 80);
    terminal.process(
        b"\x1b]133;A\x07echo old\x1b]133;B\x07\x1b]133;C\x07old output\r\n\x1b]133;D;0\x07",
    );
    terminal.process(b"\x1b]133;A\x07clear\x1b]133;B\x07\x1b]133;C\x07\x1b]133;D;0\x07");

    assert_eq!(clear_block_spacer_rows("clear", 6), 6);
    assert_eq!(clear_block_spacer_rows("clear --keep-scrollback", 6), 6);
    assert_eq!(clear_block_spacer_rows("printf clear", 6), 0);

    let total = block_content_metrics(&terminal, 80, 1).0;
    assert_eq!(total, 14);
    assert!(
        total.saturating_sub(terminal.grid().num_rows) >= terminal.grid().num_rows,
        "the block above clear must remain reachable: total={total}"
    );
}

#[test]
fn wrapped_rows_keep_emoji_graphemes_atomic() {
    assert_eq!(block_line_chunks("A👩‍🔬B", 4).count(), 1);
    assert_eq!(block_line_chunks("A👩‍🔬B", 3).count(), 2);
}

#[test]
fn block_metrics_match_renderer_for_structural_rows() {
    let mut terminal = Terminal::new(24, 80);
    terminal.process(
        b"\x1b]133;A\x07report\x1b]133;B\x07\x1b]133;C\x07\
          \xe2\x94\x82 column one \xe2\x94\x82 column two \xe2\x94\x82\r\n\
          \xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\r\n\
          \x1b]133;D;0\x07",
    );

    // Both structural lines are clipped to one rendered row at 8 cols.
    // Command "report" (6 cols) exceeds first_cols=5 (foldable → -3),
    // so it wraps to 2 rows: block = 2 output + 1 gap + 1 header + 2 command + 1 separator.
    assert_eq!(block_content_metrics(&terminal, 8, 1).0, 7);
}

#[test]
fn find_ranges_use_display_columns_for_cjk() {
    let ranges = block_match_visual_ranges("中文测试：你好世界", 5, 2, 80);
    assert_eq!(ranges, vec![(0, 10, 4)]);
}

#[test]
fn find_ranges_follow_wrapped_visual_chunk() {
    let ranges = block_match_visual_ranges("abcd你好ef", 4, 2, 4);
    assert_eq!(ranges, vec![(1, 0, 4)]);
}

#[test]
fn block_metrics_trim_the_same_finished_prompt_rows_as_renderer() {
    let mut terminal = Terminal::new(24, 80);
    terminal.process(
        b"\x1b]133;A\x07report\x1b]133;B\x07\x1b]133;C\x07output\r\n%\r\n$\r\n#\r\n\x1b]133;D;0\x07",
    );

    assert_eq!(block_content_metrics(&terminal, 80, 1).0, 5);
}

#[test]
fn block_metrics_cap_live_rows_to_the_renderer_window() {
    let mut terminal = Terminal::new(24, 80);
    terminal.process(b"\x1b]133;A\x07stream\x1b]133;B\x07\x1b]133;C\x07");
    let output = (0..2005)
        .map(|index| format!("line {index}\r\n"))
        .collect::<String>();
    terminal.process(output.as_bytes());

    assert_eq!(block_content_metrics(&terminal, 80, 1).0, 2003);
}

#[test]
fn spinner_char_returns_static_dot_for_reduce_motion() {
    assert_eq!(spinner_char_for_phase(0.0, true), '●');
    assert_eq!(spinner_char_for_phase(0.5, true), '●');
}

#[test]
fn spinner_char_returns_static_dot_for_negative_phase() {
    assert_eq!(spinner_char_for_phase(-1.0, false), '●');
}

#[test]
fn spinner_char_cycles_through_all_glyphs() {
    let n = SPINNER_CHARS.len();
    for (i, expected) in SPINNER_CHARS.iter().enumerate() {
        let phase = i as f32 / n as f32;
        assert_eq!(spinner_char_for_phase(phase, false), *expected);
    }
}

#[test]
fn spinner_char_wraps_around_at_one() {
    // Phase 1.0 折回 index 0。
    assert_eq!(spinner_char_for_phase(1.0, false), SPINNER_CHARS[0]);
    // 略小于 1.0 时为最后一个字形。
    let last_idx = SPINNER_CHARS.len() - 1;
    let last = last_idx as f32 / SPINNER_CHARS.len() as f32;
    assert_eq!(
        spinner_char_for_phase(last + 0.001, false),
        SPINNER_CHARS[last_idx]
    );
}

// ---- R2-2 假设证伪测试:钉住现状行为,供前缀和优化验证 ----

fn r22_block(command: &str, output: &str) -> Block {
    Block {
        id: BlockId(1),
        command: command.into(),
        cwd: Some("/tmp/weft".into()),
        output: output.into(),
        styled_output: None,
        exit_code: Some(0),
        started_at: SystemTime::UNIX_EPOCH,
        finished_at: Some(SystemTime::UNIX_EPOCH + Duration::from_millis(100)),
        collapsed: false,
        screen_origin: false,
    }
}

/// 假设1: `clear` 的块高度依赖 viewport_rows;R2-2 需将 viewport_rows 纳入缓存键或单独 O(1) 化。
#[test]
fn r22_clear_command_height_depends_on_viewport_rows() {
    let b = r22_block("clear", "");
    let cols = 80;
    let header_rows = 2;

    let h_30 = completed_block_layout_rows(&b, cols, header_rows, 30);
    let h_50 = completed_block_layout_rows(&b, cols, header_rows, 50);

    // clear 产生整屏 spacer,高度必须随 viewport_rows 变化。
    assert_ne!(
        h_30, h_50,
        "clear command height must depend on viewport_rows"
    );
    // spacer = viewport_rows 之差;其余(header + gap)固定。
    assert_eq!(h_50 - h_30, 20, "delta must equal viewport_rows delta");
}

/// 假设1(反例):非 clear 命令不依赖 viewport_rows,依赖隔离在 clear。
#[test]
fn r22_non_clear_command_height_independent_of_viewport_rows() {
    let b = r22_block("ls -la", "file1\nfile2\nfile3\n");
    let cols = 80;
    let header_rows = 2;

    let h_30 = completed_block_layout_rows(&b, cols, header_rows, 30);
    let h_50 = completed_block_layout_rows(&b, cols, header_rows, 50);

    assert_eq!(
        h_30, h_50,
        "non-clear command height must not depend on viewport_rows"
    );
}

/// 假设3: 钉住 `block_content_metrics` 的 (total, visible=grid num_rows) 契约。
#[test]
fn r22_block_content_metrics_returns_total_and_visible() {
    use weft_core::vt::Terminal;
    let mut terminal = Terminal::new(30, 80);
    // 无块 → total = 0,visible = num_rows。
    let (total, visible) = block_content_metrics(&terminal, 80, 2);
    assert_eq!(total, 0);
    assert_eq!(visible, 30);
    // resize 改变 visible(若有 clear 块也会改变 total)。
    terminal.resize(50, 80);
    let (total2, visible2) = block_content_metrics(&terminal, 80, 2);
    assert_eq!(total2, 0);
    assert_eq!(visible2, 50);
}

use crate::paint::grid_cache::completed_output_rows;

/// 假设5: 缓存已存折行块,直接计算为冗余重折行;钉住两者结果一致,验证可安全走缓存。
#[test]
fn r22_cached_chunks_match_completed_block_output_rows() {
    use crate::paint::grid_cache::BlockLayoutCache;
    let b = r22_block("echo hi", "short line\na much longer line that surely wraps past eighty columns when rendered at eighty cols\n");
    let cols = 80;

    // M5-b:缓存路径 = L2 行表行数(无 hints 输出)。
    let mut cache = BlockLayoutCache::default();
    cache.ensure_cached(&b, cols);
    let cached = cache.get(b.id.0);
    let cached_rows: usize = cached.width.rows.len();

    // L2 回退路径(completed_block_output_rows 现为 L2 委托)。
    let direct_rows = completed_block_output_rows(&b, cols);

    assert_eq!(
        cached_rows, direct_rows,
        "L2 row table must match the completed-block row count"
    );
    // 独立文本源 oracle(visual_rows_tests 持有)另测,防同源漂移。
}

/// R2-2 回归:折叠块缓存 output_rows 必须为 0,否则滚动条拇指按可见输出尺寸化。
#[test]
fn r22_collapsed_block_cache_reports_zero_output_rows() {
    use crate::paint::grid_cache::BlockLayoutCache;
    let mut b = r22_block("echo hi", "line one\nline two\nline three\n");
    let cols = 80;

    // 未折叠:output_rows 非零。
    let mut cache = BlockLayoutCache::default();
    cache.ensure_cached(&b, cols);
    let uncollapsed_rows = completed_output_rows(&b, cols, Some(cache.get(b.id.0)));
    assert_eq!(uncollapsed_rows, 3);

    // 折叠后缓存重建,output_rows 必须为 0。
    b.collapsed = true;
    cache.ensure_cached(&b, cols);
    assert_eq!(completed_output_rows(&b, cols, Some(cache.get(b.id.0))), 0);

    // 缓存路径必须与回退路径一致。
    let mut terminal = Terminal::new(24, 80);
    terminal.process(b"\x1b]133;A\x07echo hi\x1b]133;B\x07\x1b]133;C\x07line one\r\nline two\r\nline three\r\n\x1b]133;D;0\x07");
    // 无直接 API 折叠已完成块;以缓存/非缓存两条路径总数一致来验证。
    let (total_none, _) = block_content_metrics(&terminal, 80, 1);
    let cache = terminal.block_tracker(); // borrow to build a cache
    let mut blk_cache = BlockLayoutCache::default();
    for blk in cache.session_blocks() {
        blk_cache.ensure_cached(blk, 80);
    }
    let (total_some, _) =
        block_content_metrics_with_cache(&terminal, 80, 1, Some(&blk_cache), None);
    assert_eq!(
        total_none, total_some,
        "cached and uncached totals must match"
    );
}
