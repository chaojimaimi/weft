//! Pure presentation model for BlockView metadata.

use crate::paint::grid_cache::{block_line_chunks, command_line_chunks};
use crate::paint::ui_helpers::{
    abbreviate_path, block_duration_str, live_context_label, strip_prompt_prefix,
};
use weft_core::blocks::continuation::{classify_exit, ExitClass};
use weft_core::blocks::{Block, BlockId};
use weft_core::vt::Terminal;

mod hover;
pub(crate) use hover::{
    block_header_action_at, hovered_block_for_row, hovered_block_in_chrome_bands, BlockChromeBand,
    BlockHeaderAction,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BlockTone {
    Success,
    /// PLAN_v1.13.16: Ctrl-C(130)/SIGPIPE(141)—— 用户/管道常规中止,
    /// 非 Error(无红条红底),头部仍显示 muted "exit 130"。
    Cancelled,
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
    // PLAN_v1.13.16: 分类收口到 classify_exit —— 130/141 归 Cancelled(中性),
    // 不再落入 Error 红色系。
    let tone = match classify_exit(block.exit_code) {
        ExitClass::Success => BlockTone::Success,
        ExitClass::Cancelled => BlockTone::Cancelled,
        ExitClass::Failed => BlockTone::Error,
        ExitClass::Orphan => BlockTone::Warning,
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
    // PLAN_v1.13.16: 有意不迁移到 ExitClass::Failed —— 这里的语义是
    // "opencode 非干净退出"(含 Ctrl-C 130/141 与孤儿 None),此时恢复
    // 提示对用户仍有用,与失败判定(仅 Failed)是两个独立的决策点。
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

/// v1.20.0 (PLAN_v1200 §4 D2/D2f): flag-routed twin of
/// [`completed_block_row_count`] — the unified chokepoint every completed
/// row-count consumer (scrollbar metrics, find jump, panel jump) goes
/// through.
/// v1.20.6 (FIX_V1206): overlay detours the per-block chrome (gap, header
/// band, separator) out of the VERTEX stream only — the scroll-distance
/// domain keeps advancing over them (completed_pass.rs cursor_dist: gap
/// :357, header :392, separator :412), so the metrics total must count the
/// same chrome rows or max_scroll under-clamps and pins scrolling to 0.
/// OFF stays byte-identical to the legacy formula (its defensive .max(1)
/// predates the guarantee that compact_control_row_span >= 1,
/// ui_tokens.rs:156; the assembly adds header_height unconditionally).
pub(crate) fn completed_block_row_count_for(
    output_lines: usize,
    header_rows: usize,
    command_wrap_rows: usize,
    complete_block_overlay: bool,
) -> usize {
    if complete_block_overlay {
        output_lines + command_output_gap_rows(output_lines) + header_rows + command_wrap_rows + 1
    // separator
    } else {
        completed_block_row_count(output_lines, header_rows, command_wrap_rows)
    }
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

/// v1.20.0 (PLAN_v1200 §4 D2a): diagnose-panel chunk texts — the SINGLE
/// source shared by the layout pass (`completed_pass.rs` lays these rows)
/// and the overlay-flag metrics sister domain (counts `chunks.len()`).
/// Mirrors the v1.8.2 panel content contract: thinking → one placeholder
/// row, result → one "AI: …"/"AI error: …" logical line wrapped at
/// `cols - 2` (panel padding). Collapsed blocks show no panel (callers
/// gate on `collapsed` themselves, same as the layout pass).
pub(crate) fn diagnose_panel_chunks(
    state: &crate::app_state::BlockDiagnoseState,
    cols: usize,
) -> Vec<String> {
    if state.is_thinking() {
        return vec!["Diagnosing…".to_string()];
    }
    match &state.result {
        Some(result) => {
            let text = match result {
                Ok(explanation) => format!("AI: {}", explanation),
                Err(err) => format!("AI error: {}", err),
            };
            text.lines()
                .flat_map(|line| {
                    block_line_chunks(line, cols.saturating_sub(2)).collect::<Vec<_>>()
                })
                .collect()
        }
        None => Vec::new(),
    }
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
    complete_block_overlay: bool,
) -> usize {
    completed_block_row_count_for(
        completed_block_output_rows(block, cols),
        header_rows,
        command_wrap_rows_for(block, cols, block_foldable(block)),
        complete_block_overlay,
    ) + clear_block_spacer_rows(&block.command, viewport_rows)
}

/// 块底到 Find 命中视觉行的距离,镜像渲染器自底向上的顺序(含折行输出与恢复提示)。
/// v1.20.6 (FIX_V1206): flag-agnostic. The assembly advances cursor_dist over
/// the per-block chrome under BOTH flags (overlay only changes the carrier),
/// so one formula serves both domains; the v1.20.0 ON special case (dropping
/// gap/separator) measured the wrong domain and is deleted.
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
    let row_from_chunk = hints + rows_after + chunks.len().saturating_sub(chunk_index + 1);
    row_from_chunk + 1
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

/// WP1.5 F1: the production callers (reconciled_terminal_block_scroll /
/// reconciled_with_memo) now go through the cache-backed variant; this
/// None-wrapper remains as the tests' uncached baseline (same shape as the
/// `block_view_line_end_col` precedent below).
#[cfg(test)]
pub(crate) fn block_content_metrics(
    terminal: &Terminal,
    cols: usize,
    header_rows: usize,
) -> (usize, usize) {
    block_content_metrics_with_cache(terminal, cols, header_rows, None, None, None)
}

/// R2-2: 缓存变体。缓存命中时 O(1) 读 `output_rows`,否则回退直接计算
/// (如最后一帧后才 finalized 的块,或无渲染器访问权的调用方)。
/// v1.10.23: `live_cache`(调用方已 sync)让 live 分支 O(1) 读累计行数;`None` 回退旧路径。
/// v1.19.0 (PLAN_v1190 §4 P1-8): `region_live_rows` 是 region 轨（flag ON）
/// 的 live 显示行总数（P1-6 域，组装体同源）——优先于捕获轨行数，使
/// block_scroll_metrics 的 total 与 laid rows 一致；`None`（flag OFF 或无
/// 快照）保持捕获轨行为 byte-identical。
pub(crate) fn block_content_metrics_with_cache(
    terminal: &Terminal,
    cols: usize,
    header_rows: usize,
    cache: Option<&crate::paint::grid_cache::BlockLayoutCache>,
    live_cache: Option<&crate::paint::live_cache::LiveLayoutCache>,
    region_live_rows: Option<usize>,
) -> (usize, usize) {
    block_content_metrics_with_overlay(
        terminal,
        cols,
        header_rows,
        cache,
        live_cache,
        region_live_rows,
        false,
        None,
    )
}

/// v1.20.0 (PLAN_v1200 §4 D2a): flag-routed twin of
/// [`block_content_metrics_with_cache`]. `complete_block_overlay = true`
/// reads the sister domain (per completed block: hint + output + wrap +
/// diagnose + clear; NO gap / header band / separator rows) so the
/// scrollbar total matches what the layout pass actually lays out under
/// the overlay track. `false` (legacy callers: reconciled_terminal_block_scroll,
/// reconcile_scroll memo, every existing test) is byte-identical to the
/// pre-v1.20.0 behavior. `block_diagnose_state` feeds the diagnose rows of
/// the sister domain (mirror of `completed_pass.rs`' panel rows via
/// [`diagnose_panel_chunks`]).
/// v1.20.0 (D2f): the overlay flag + diagnose map join the parameter list
/// of the legacy twin; the allow matches the build_row_instances precedent.
#[allow(clippy::too_many_arguments)]
pub(crate) fn block_content_metrics_with_overlay(
    terminal: &Terminal,
    cols: usize,
    header_rows: usize,
    cache: Option<&crate::paint::grid_cache::BlockLayoutCache>,
    live_cache: Option<&crate::paint::live_cache::LiveLayoutCache>,
    region_live_rows: Option<usize>,
    complete_block_overlay: bool,
    block_diagnose_state: Option<
        &std::collections::HashMap<BlockId, crate::app_state::BlockDiagnoseState>,
    >,
) -> (usize, usize) {
    use weft_core::blocks::ShellPhase;

    // WP1.5 acceptance-2 probe (PLAN_v1180_WP15_G4_RECONCILE_CACHE §3-2):
    // the O(document) fallback is the regression this whole plan removes —
    // a >10 ms scan here means the cache supply broke (uncached caller,
    // try_borrow degradation). Grep tag RECONCILE_PROBE; steady-state cost
    // is two Instant reads + one compare per call.
    let metrics_started = std::time::Instant::now();
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
        // F7-1b: tombstone lookup only pays O(1) when the entry is absent
        // (the only branch that consumes it).
        let tombstone = if cached.is_none() {
            cache.and_then(|c| c.get_metrics_tombstone(block.id.0))
        } else {
            None
        };
        let output_rows = match cached {
            None => match tombstone {
                Some(t) if !block.collapsed => t.output_rows,
                Some(_) => 0, // collapsed：与 PRESENT 分支 `Some(_) if block.collapsed => 0` 同构
                None => {
                    if let Some(c) = cache {
                        c.note_metrics_fallback();
                    }
                    crate::paint::grid_cache::completed_output_rows(block, cols, None)
                    // v1.18.4 兜底
                }
            },
            Some(_) if block.collapsed => 0,
            Some(c) => c.stale_output_rows,
        };
        let command_wrap_rows = cached
            .map(|c| c.command_wrap_rows)
            .or_else(|| tombstone.map(|t| t.command_wrap_rows))
            .unwrap_or_else(|| command_wrap_rows_for(block, cols, block_foldable(block)));
        // v1.20.0 (D2a): unified chokepoint — the overlay flag routes to the
        // sister domain (output + wrap; the gap / header band / separator
        // rows no longer exist in the laid stream) and the diagnose panel
        // rows join the total (same chunks the layout pass lays; collapsed
        // blocks show no panel, same gate as `completed_pass.rs`).
        total += completed_block_row_count_for(
            output_rows,
            header_rows,
            command_wrap_rows,
            complete_block_overlay,
        ) + clear_block_spacer_rows(&block.command, viewport_rows);
        if complete_block_overlay && !block.collapsed {
            if let Some(state) = block_diagnose_state.and_then(|m| m.get(&block.id)) {
                total += diagnose_panel_chunks(state, cols).len();
            }
        }
    }
    if terminal.block_tracker().phase() == ShellPhase::CommandExecuting {
        if let Some(live) = terminal.block_tracker().in_flight() {
            // v1.19.0 (PLAN_v1190 §4 P1-8): region track total wins when
            // supplied (same snapshot the layout pass lays out); the capture
            // branches below are the flag-OFF path, untouched.
            let output_rows = region_live_rows
                .or_else(|| live_cache.map(|c| c.total_display_rows()))
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
    } else if terminal.is_live_block_grid_render()
        && region_live_rows.is_some()
        // v1.20.7 review (medium-2): Editor frames never count pseudo rows —
        // the layout's editor branch passes region=None (draw_phases.rs), so
        // the scrollbar total must match. This gates the residual-anchor
        // window (133;A after a PS2 Ctrl+C leaves the primed anchor behind
        // while the phase is already AtPrompt/Editor).
        && terminal.effective_input_mode() != weft_core::input::InputMode::Editor
    {
        // v1.20.7 (PLAN_v1207 §3 D4): pseudo-block frame — in_flight None
        // (between submit and the shell's 133;B) but a region snapshot is
        // live. Row budget matches the pseudo segment's assembly domain
        // (region_rows.rs D3): region rows + Blank (only when any) +
        // Separator + cwd header (v1.20.9 D3: the pseudo-block lays a
        // LiveHeader when a cwd exists — same predicate as the assembly).
        // Sits OUTSIDE the CommandExecuting gate above: non-integrated shells
        // never enter CommandExecuting at all (Passthrough throughout).
        let pseudo_rows = region_live_rows.unwrap_or(0);
        // v1209-review N4: this predicate must stay source-identical to the
        // assembly side — LayoutPassInput.cwd flows from the same accessors
        // (draw_phases.rs terminal.cwd()/git_branch() → PaintModel →
        // LayoutPassInput → region_rows.rs). Any change of source on either
        // side desyncs the scrollbar by one row.
        let pseudo_header =
            live_context_label(terminal.cwd(), terminal.git_branch(), None).is_some();
        total += pseudo_rows + usize::from(pseudo_rows > 0) + 1 + usize::from(pseudo_header);
    }
    let metrics_ms = metrics_started.elapsed().as_millis();
    if metrics_ms > 10 {
        tracing::warn!(
            ms = metrics_ms,
            fallbacks = cache.map_or(0, |c| c.metrics_fallback_rebuilds()),
            "RECONCILE_PROBE block metrics slow"
        );
    }
    (total, terminal.grid().num_rows.max(1))
}

/// v1.11.14: display column (cells) one past the last glyph of live text
/// line `line`. T16b removed the production caller — the renderer's formula
/// fallback now anchors the capture tail via `tui_cursor_display_col` (same
/// per-char convention); kept as the convention's named reference and test
/// fixture. Per-char widths follow the `block_match_visual_ranges`
/// convention so the column aligns with the painted glyphs (CJK = 2).
#[cfg(test)]
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
/// WP1.5 F1 (PLAN_v1180_WP15_G4_RECONCILE_CACHE): 对账接 `BlockLayoutCache` ——
/// PRESENT 条目读 stale 标量(M6-b B-3 语义,cols-stale 容忍),zoom 步进/拖拽每 tick
/// 的对账从 O(文档) 直算降为 O(块数) 标量读(sample 实证 45% 主线程卡在本站点)。
/// `live_cache` 维持 None:对账读 session_blocks + in_flight,in_flight 有 live.version
/// 键控的独立语义,不引入。`None` 回退旧直算(无缓存访问权的调用方/测试)。
pub(crate) fn reconciled_terminal_block_scroll(
    terminal: &Terminal,
    layout_ctx: &crate::layout::LayoutCtx,
    header_rows: usize,
    current: usize,
    cache: Option<&crate::paint::grid_cache::BlockLayoutCache>,
) -> Option<(usize, usize, usize)> {
    if !terminal.show_block_view() {
        return None;
    }
    let (total, _) = block_content_metrics_with_cache(
        terminal,
        terminal.grid().num_cols,
        header_rows,
        cache,
        None,
        // Reconcile path has no renderer region snapshot access — flag ON
        // falls back to the capture total here (memo + scrollbar stay
        // authoritative; see PLAN_v1190 §4 P1-8 备案).
        None,
    );
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

// Tests live in the gate-exempt sibling module (repo test-module
// convention, pty/tests.rs precedent) so inline test lines stay out of
// the production-file budget.
#[cfg(test)]
#[path = "block_component/tests.rs"]
mod tests;
