//! Pure presentation model for BlockView metadata.

use crate::paint::grid_cache::{block_line_chunks, command_line_chunks};
use crate::paint::ui_helpers::{abbreviate_path, block_duration_str, strip_prompt_prefix};
use weft_core::blocks::Block;
use weft_core::vt::Terminal;

mod hover;
pub(crate) use hover::{block_header_action_at, hovered_block_for_row, BlockHeaderAction};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BlockTone {
    Success,
    Error,
    Warning,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BlockPresentation {
    /// 首段:CWD 或 "{n} lines" 折叠摘要
    pub(crate) cwd: String,
    /// 计时 < 1ms 时为空串,不画
    pub(crate) duration: String,
    /// 仅失败/中断有文案;成功为空串
    pub(crate) status: String,
    pub(crate) tone: BlockTone,
}

impl BlockPresentation {
    /// 拼接各段(" · ")供 fallback 文本;bv_rows 已内联相同逻辑,仅单测调用,保留为 canonical join 防漂移。
    #[allow(dead_code)]
    pub(crate) fn label(&self) -> String {
        let mut parts = vec![self.cwd.as_str()];
        if !self.duration.is_empty() {
            parts.push(self.duration.as_str());
        }
        if !self.status.is_empty() {
            parts.push(self.status.as_str());
        }
        parts.join(" · ")
    }
}

pub(crate) fn block_presentation(block: &Block, output_lines: usize) -> BlockPresentation {
    let duration = block_duration_str(block);
    let tone = match block.exit_code {
        Some(0) => BlockTone::Success,
        Some(_) => BlockTone::Error,
        None => BlockTone::Warning,
    };

    let cwd = if block.collapsed {
        let unit = if output_lines == 1 { "line" } else { "lines" };
        format!("{output_lines} {unit}")
    } else {
        block
            .cwd
            .as_deref()
            .map(abbreviate_path)
            .unwrap_or_else(|| "~".to_string())
    };
    // 成功已由 tone 传达;仅失败/中断保留可操作的显式 status 文案。
    let status = if block.exit_code != Some(0) {
        match block.exit_code {
            Some(code) => format!("exit {code}"),
            None => "interrupted".to_string(),
        }
    } else {
        String::new()
    };

    BlockPresentation {
        cwd,
        duration,
        status,
        tone,
    }
}

/// OpenCode 1.18.x 退出时清屏但不发 session card(已从原始 PTY 流验证);给出稳定的 CLI 恢复命令,不臆造 session id。
pub(crate) fn command_resume_hints(block: &Block) -> &'static [&'static str] {
    const OPENCODE_HINTS: &[&str] = &[
        "Continue last session: opencode -c",
        "Choose a session: opencode session list; opencode -s <session-id>",
    ];
    let executable = block
        .command
        .split_whitespace()
        .next()
        .and_then(|part| part.rsplit('/').next());
    if block.exit_code != Some(0) && executable == Some("opencode") {
        OPENCODE_HINTS
    } else {
        &[]
    }
}

/// Prompt 高度只属于 shell 编辑器;运行中的全屏应用把 BlockView 画到视口底部。
pub(crate) fn block_prompt_lines(terminal: &Terminal) -> Option<usize> {
    (terminal.effective_input_mode() == weft_core::input::InputMode::Editor)
        .then_some(terminal.editor().buffer.lines.len())
}

/// Total/visible BlockView rows used by scrollbar and input geometry.
/// R2-2 (stage 2): 命令可折行,`command_wrap_rows` 计多行,再 +1 separator。
pub(crate) fn completed_block_row_count(
    output_lines: usize,
    header_rows: usize,
    command_wrap_rows: usize,
) -> usize {
    output_lines
        + command_output_gap_rows(output_lines)
        + header_rows.max(1)
        + command_wrap_rows
        + 1
}

/// Warp 风格:命令与其首行输出之间的留白;空/折叠命令无输出,保持紧凑。
pub(crate) fn command_output_gap_rows(output_rows: usize) -> usize {
    usize::from(output_rows > 0)
}

/// 命令是否有非空输出(决定首行缩进与可折叠态);与 `compute_block_layout` 的 foldable 判定同源。
pub(crate) fn block_foldable(block: &Block) -> bool {
    block
        .output
        .lines()
        .rev()
        .take(500)
        .any(|line| !line.trim().is_empty())
}

/// 命令首行可用列数:foldable 缩 3 列,否则 2 列;与 grid_cache / layout_pass 同参数。
pub(crate) fn command_first_cols(cols: usize, foldable: bool) -> usize {
    cols.saturating_sub(if foldable { 3 } else { 2 }).max(1)
}

/// 命令折行行数:折叠=1(单行),展开=`command_line_chunks` 计数。
/// 三处几何路径(grid_cache、layout_pass::Command、completed_block_* 行数)共用单一来源,量纲不漂移。
pub(crate) fn command_wrap_rows_for(block: &Block, cols: usize, foldable: bool) -> usize {
    if block.collapsed {
        1
    } else {
        let cleaned = strip_prompt_prefix(&block.command);
        command_line_chunks(&cleaned, command_first_cols(cols, foldable), cols)
            .len()
            .max(1)
    }
}

/// `clear` 在两侧块之间留下一整屏空行;统一用行数表达,避免 paint/滚动/查找/历史跳转漂移。
pub(crate) fn clear_block_spacer_rows(command: &str, viewport_rows: usize) -> usize {
    usize::from(command.split_whitespace().next() == Some("clear")) * viewport_rows.max(1)
}

/// M5-b (PLAN_M5 §三): the row count reads the L1/L2 tables via the shared
/// machine (`hint_rows + rows.len()`); the legacy per-line collect +
/// re-wrap (`completed_block_visible_lines`) is deleted.
pub(crate) fn completed_block_output_rows(block: &Block, cols: usize) -> usize {
    crate::paint::grid_cache::completed_output_rows(block, cols, None)
}

pub(crate) fn completed_block_layout_rows(
    block: &Block,
    cols: usize,
    header_rows: usize,
    viewport_rows: usize,
) -> usize {
    completed_block_row_count(
        completed_block_output_rows(block, cols),
        header_rows,
        command_wrap_rows_for(block, cols, block_foldable(block)),
    ) + clear_block_spacer_rows(&block.command, viewport_rows)
}

/// 块底到 Find 命中视觉行的距离,镜像渲染器自底向上的顺序(含折行输出与恢复提示)。
pub(crate) fn completed_block_match_row_from_bottom(
    block: &Block,
    hit: &weft_core::find::BlockMatch,
    cols: usize,
) -> usize {
    if block.collapsed {
        return 1;
    }
    let hints = command_resume_hints(block)
        .iter()
        .map(|hint| block_line_chunks(hint, cols).count())
        .sum::<usize>();
    // Per-hit find-jump geometry stays on the text path (shared wrap
    // machine keeps it consistent with the L2 tables; not a per-frame cost).
    // Same trailing trim the L1 builder applies.
    let mut lines: Vec<&str> = block.output.lines().collect();
    let visible_len = crate::paint::grid_cache::trimmed_output_line_count(&lines);
    lines.truncate(visible_len);
    if hit.is_command {
        let output_rows = hints
            + lines
                .iter()
                .map(|line| block_line_chunks(line, cols).count())
                .sum::<usize>();
        return output_rows + command_output_gap_rows(output_rows) + 1;
    }

    let line_index = hit.line.min(lines.len().saturating_sub(1));
    let rows_after = lines
        .iter()
        .skip(line_index.saturating_add(1))
        .map(|line| block_line_chunks(line, cols).count())
        .sum::<usize>();
    let chunks = lines
        .get(line_index)
        .map(|line| block_line_chunks(line, cols).collect::<Vec<_>>())
        .unwrap_or_default();
    let mut chars_before = 0usize;
    let chunk_index = chunks
        .iter()
        .position(|chunk| {
            let end = chars_before + chunk.chars().count();
            let owns = hit.col < end || end == chars_before;
            chars_before = end;
            owns
        })
        .unwrap_or_else(|| chunks.len().saturating_sub(1));
    hints + rows_after + chunks.len().saturating_sub(chunk_index + 1) + 1
}

/// 把按字符索引的命中转成折行后的视觉行范围;列/长度均为显示列,保证 CJK 与绘制的字形对齐。
pub(crate) fn block_match_visual_ranges(
    line: &str,
    hit_col: usize,
    hit_len: usize,
    cols: usize,
) -> Vec<(usize, usize, usize)> {
    let hit_end = hit_col.saturating_add(hit_len);
    let mut source_col = 0usize;
    let mut ranges = Vec::new();
    for (chunk_index, chunk) in block_line_chunks(line, cols).enumerate() {
        let chunk_len = chunk.chars().count();
        let chunk_end = source_col + chunk_len;
        let start = hit_col.max(source_col);
        let end = hit_end.min(chunk_end);
        if start < end {
            let display_col = chunk
                .chars()
                .take(start - source_col)
                .map(|ch| unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0))
                .sum();
            let display_len = chunk
                .chars()
                .skip(start - source_col)
                .take(end - start)
                .map(|ch| unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0))
                .sum();
            if display_len > 0 {
                ranges.push((chunk_index, display_col, display_len));
            }
        }
        source_col = chunk_end;
    }
    ranges
}

pub(crate) fn block_content_metrics(
    terminal: &Terminal,
    cols: usize,
    header_rows: usize,
) -> (usize, usize) {
    block_content_metrics_with_cache(terminal, cols, header_rows, None, None)
}

/// R2-2: 缓存变体。缓存命中时 O(1) 读 `output_rows`,否则回退直接计算
/// (如最后一帧后才 finalized 的块,或无渲染器访问权的调用方)。
/// v1.10.23: `live_cache`(调用方已 sync)让 live 分支 O(1) 读累计行数;`None` 回退旧路径。
pub(crate) fn block_content_metrics_with_cache(
    terminal: &Terminal,
    cols: usize,
    header_rows: usize,
    cache: Option<&crate::paint::grid_cache::BlockLayoutCache>,
    live_cache: Option<&crate::paint::live_cache::LiveLayoutCache>,
) -> (usize, usize) {
    use weft_core::blocks::ShellPhase;

    let mut total = 0;
    let viewport_rows = terminal.grid().num_rows.max(1);
    for block in terminal.block_tracker().session_blocks() {
        // M6-b B-3 (PLAN_M6 §三, 评审 P0-2): a PRESENT entry is read as
        // scalars only — even when its cols are stale (band-deferred; M6-c
        // degraded). `stale_output_rows` / `command_wrap_rows` are the same
        // snapshots the prefix sum's stale `base_row_count` was composed
        // from, so the scrollbar total can't disagree with laid-out geometry,
        // and the old expired branch's per-frame L2 rebuild (~11.5ms per
        // 1MiB block) is gone. Uncached blocks still fall back to a direct
        // computation (memo 兜底), counted by `metrics_fallback_rebuilds`.
        let cached = cache.and_then(|c| c.get_if_cached(block.id.0));
        let output_rows = match cached {
            None => {
                if let Some(c) = cache {
                    c.note_metrics_fallback();
                }
                crate::paint::grid_cache::completed_output_rows(block, cols, None)
            }
            Some(_) if block.collapsed => 0,
            Some(c) => c.stale_output_rows,
        };
        let command_wrap_rows = cached
            .map(|c| c.command_wrap_rows)
            .unwrap_or_else(|| command_wrap_rows_for(block, cols, block_foldable(block)));
        total += completed_block_row_count(output_rows, header_rows, command_wrap_rows)
            + clear_block_spacer_rows(&block.command, viewport_rows);
    }
    if terminal.block_tracker().phase() == ShellPhase::CommandExecuting {
        if let Some(live) = terminal.block_tracker().in_flight() {
            let output_rows = live_cache
                .map(|c| c.total_display_rows())
                .unwrap_or_else(|| {
                    let lines: Vec<&str> = live.output.lines().collect();
                    let start = lines
                        .len()
                        .saturating_sub(crate::paint::grid_cache::MAX_LAYOUT_LINES_LIVE);
                    lines[start..]
                        .iter()
                        .map(|line| block_line_chunks(line, cols).count())
                        .sum::<usize>()
                });
            total += output_rows + command_output_gap_rows(output_rows);
            total += 2; // command + separator
            total += usize::from(live.cwd.or(terminal.cwd()).is_some());
        }
    }
    (total, terminal.grid().num_rows.max(1))
}

/// v1.11.14 (password caret): the BlockView caret anchor plus whether the
/// grid cursor row maps to a MATERIALIZED live text line. When the second
/// element is false, the cursor sits on a trailing empty row the live
/// document does not carry — password readers (privilege helpers, ssh,
/// su) disable echo and emit their own newline, parking the cursor on an
/// empty row below the last output line. The caller must then anchor the
/// caret at the END of that last line (`block_view_line_end_col`) instead
/// of the raw grid column, which is 0 after CRLF and historically jumped
/// the caret to the FRONT of the prompt text.
/// v1.10.5 lineage: `line_count - grid_rows + cursor_row` maps the grid
/// cursor row into the live block snapshot (`document_start .. grid
/// end`); BlockView paint (caret/IME preedit) and the native IME anchor
/// share this document line. v1.10.6: a snapshot shorter than the
/// viewport clamps to the last live text line so the caret/preedit never
/// disappears.
pub(crate) fn block_view_tui_cursor_anchor(
    live_line_count: usize,
    grid_rows: usize,
    cursor_row: usize,
) -> (usize, bool) {
    let unclamped = live_line_count
        .saturating_sub(grid_rows)
        .saturating_add(cursor_row);
    let line = unclamped.min(live_line_count.saturating_sub(1));
    (line, unclamped < live_line_count)
}

/// v1.11.14: display column (cells) one past the last glyph of live text
/// line `line` — the caret anchor when the grid cursor row is not
/// materialized. Per-char widths follow the `block_match_visual_ranges`
/// convention so the column aligns with the painted glyphs (CJK = 2).
pub(crate) fn block_view_line_end_col(output: &str, line: usize) -> usize {
    output
        .lines()
        .nth(line)
        .map(|l| {
            l.chars()
                .map(|ch| unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0))
                .sum()
        })
        .unwrap_or(0)
}

/// v1.10.5: 仅 LIVE 行(block_id=None)可挂 TUI caret/IME preedit;已完成块用
/// 0 基输出索引,数字上与快照行索引冲突,否则会在其第 N 行画出 caret/preedit。
pub(crate) fn tui_caret_row_matches(
    block_id: Option<u64>,
    line_idx: usize,
    cursor_line: usize,
) -> bool {
    block_id.is_none() && line_idx == cursor_line
}

/// 新 PTY 输出是否让 BlockView 保持钉在 live 尾部。主屏应用 SIGWINCH 重绘也会输出;
/// 用户已进入 detached 历史视图后,该重绘不得抢回视口切回 live Grid。
pub(crate) fn should_follow_running_output(
    phase: weft_core::blocks::ShellPhase,
    primary_history_view: bool,
) -> bool {
    phase == weft_core::blocks::ShellPhase::CommandExecuting && !primary_history_view
}

/// 把自底偏移钳制到当前视口/折行宽度产生的范围内;宽高/字体/侧栏/prompt 变化都会
/// 缩小范围,沿用旧偏移会使最新行不可达或画空白。
pub(crate) fn reconciled_block_scroll(
    scroll: usize,
    total_rows: usize,
    visible_rows: usize,
) -> usize {
    scroll.min(total_rows.saturating_sub(visible_rows))
}

/// 几何变化(窗口/面板/字体/恢复窗口)后统一重算单 tab 的 detached transcript 滚动,防止各路径漂移。
pub(crate) fn reconciled_terminal_block_scroll(
    terminal: &Terminal,
    layout_ctx: &crate::layout::LayoutCtx,
    header_rows: usize,
    current: usize,
) -> Option<(usize, usize, usize)> {
    if !terminal.show_block_view() {
        return None;
    }
    let (total, _) = block_content_metrics(terminal, terminal.grid().num_cols, header_rows);
    let cwd_header = crate::layout::block_cwd_header_active(
        terminal.effective_input_mode() == weft_core::input::InputMode::Editor,
        terminal.cwd().is_some(),
    );
    let visible =
        crate::layout::block_visible_rows(layout_ctx, block_prompt_lines(terminal), cwd_header);
    Some((
        reconciled_block_scroll(current, total, visible),
        total,
        visible,
    ))
}

/// F3-2: 运行命令活动指示器的盲文 spinner 字形,按 `spinner_phase` 从左到右循环。
pub(crate) const SPINNER_CHARS: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// F3-2: 归一化 phase → 盲文字形;reduce_motion 或负 phase 返回静态 `●`。纯逻辑,已单测。
pub(crate) fn spinner_char_for_phase(phase: f32, reduce_motion: bool) -> char {
    if reduce_motion || phase < 0.0 {
        return '●';
    }
    let idx = ((phase * SPINNER_CHARS.len() as f32) as usize) % SPINNER_CHARS.len();
    SPINNER_CHARS[idx]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};
    use weft_core::blocks::{Block, BlockId};

    #[test]
    fn tui_cursor_anchor_maps_viewport_row_into_snapshot() {
        // v1.10.5: 快照跨 document_start..grid end,光标快照行 = line_count - grid_rows + cursor_row。
        assert_eq!(
            block_view_tui_cursor_anchor(34, 34, 33),
            (33, true),
            "bottom row maps to the last snapshot line"
        );
        assert_eq!(
            block_view_tui_cursor_anchor(34, 34, 0),
            (0, true),
            "top row maps to the first viewport line"
        );
        assert_eq!(
            block_view_tui_cursor_anchor(40, 34, 33),
            (39, true),
            "scrollback above the viewport shifts the mapping"
        );
        // Degenerate inputs saturate instead of underflowing.
        assert_eq!(block_view_tui_cursor_anchor(2, 34, 0), (0, true));
        // v1.10.6: 快照 < grid 时 cursor_row 可能超出 live_line_count,钳制到末行。
        // v1.11.14: 钳制发生即未落实(cursor 行在 live 文本之外)——调用方须改用行尾锚定。
        assert_eq!(
            block_view_tui_cursor_anchor(10, 33, 13),
            (9, false),
            "cursor_row beyond snapshot clamps to the last live line, unmaterialized"
        );
        assert_eq!(
            block_view_tui_cursor_anchor(10, 33, 9),
            (9, true),
            "cursor_row at snapshot boundary maps to the last line"
        );
    }

    #[test]
    fn tui_cursor_anchor_reports_unmaterialized_trailing_row() {
        // v1.11.14: password reader scenario — live text "cmd\nPassword: \n"
        // (2 lines), cursor parked on the empty row below (row 2): the line
        // clamps back to the prompt line but must be flagged unmaterialized.
        assert_eq!(block_view_tui_cursor_anchor(2, 10, 2), (1, false));
        // Typing phase — cursor ON the prompt row: materialized.
        assert_eq!(block_view_tui_cursor_anchor(2, 10, 1), (1, true));
        // Full-viewport document (scrollback present): mapping exact.
        assert_eq!(block_view_tui_cursor_anchor(40, 34, 33), (39, true));
        assert_eq!(block_view_tui_cursor_anchor(40, 34, 0), (6, true));
        // Empty live text: nothing materializes.
        assert_eq!(block_view_tui_cursor_anchor(0, 10, 3), (0, false));
    }

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
}
