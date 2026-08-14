use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

fn wrap_line_chunk_ranges(text: &str, cols: usize) -> Vec<Range<usize>> {
    if cols == 0 {
        return std::iter::once(0..text.len()).collect();
    }
    let mut ranges = Vec::new();
    let mut start = 0usize;
    let mut col = 0usize;
    for (byte, grapheme) in text.grapheme_indices(true) {
        let width = weft_core::grid::terminal_text_width(grapheme);
        if width == 0 {
            continue;
        }
        if col + width > cols && grapheme.chars().all(char::is_whitespace) {
            col += width;
            continue;
        }
        if col + width > cols && byte > start {
            ranges.push(start..byte);
            start = byte;
            col = 0;
        }
        col += width;
    }
    ranges.push(start..text.len());
    ranges
}

/// Wrap prose by terminal display width while keeping graphemes atomic.
#[cfg(test)]
pub(crate) fn wrap_line_chunks(text: &str, cols: usize) -> impl Iterator<Item = String> {
    wrap_line_chunk_ranges(text, cols)
        .into_iter()
        .map(|range| text[range].to_string())
        .collect::<Vec<_>>()
        .into_iter()
}

/// Return source-relative byte ranges for cached block output. Keeping ranges
/// instead of owned wrapped strings avoids retaining a second copy of every
/// completed block while preserving the exact same wrapping behavior.
pub(crate) fn block_line_chunk_ranges(text: &str, cols: usize) -> Vec<Range<usize>> {
    let kind = classify_structure_line(text);
    if cols == 0 || kind == StructureKind::None {
        return wrap_line_chunk_ranges(text, cols);
    }
    // Progress gauges (ollama pull, brew upgrade, …) carry ETA / speed / size
    // info that users need to see. A *small* overflow (≤ 3 cols) is the common
    // off-by-few case where a program emits cols+N chars due to ambiguous-width
    // accounting; clipping avoids a dangling 1-2 char tail ("9s") on the next
    // row. A *large* overflow means the window was narrowed (or the program
    // hasn't caught up to the new SIGWINCH); wrapping preserves the trailing
    // ETA/speed so the user can still read it instead of seeing it truncated.
    if kind == StructureKind::ProgressGauge {
        let total_width = weft_core::grid::terminal_text_width(text);
        if total_width > cols.saturating_add(PROGRESS_GAUGE_CLIP_TOLERANCE) {
            return wrap_line_chunk_ranges(text, cols);
        }
    }
    let mut end = 0usize;
    let mut col = 0usize;
    for (byte, grapheme) in text.grapheme_indices(true) {
        let width = weft_core::grid::terminal_text_width(grapheme);
        if width > 0 && col + width > cols {
            break;
        }
        col += width;
        end = byte + grapheme.len();
    }
    std::iter::once(0..end).collect()
}

/// Maximum overflow (in columns) for a progress gauge to be clipped rather
/// than wrapped. Covers the common off-by-few case (ambiguous-width chars,
/// SIGWINCH race) without truncating large overflows caused by window
/// narrowing.
const PROGRESS_GAUGE_CLIP_TOLERANCE: usize = 3;

/// Keep terminal-drawn structure atomic across history resizes; prose reflows.
pub(crate) fn block_line_chunks(text: &str, cols: usize) -> impl Iterator<Item = String> {
    block_line_chunk_ranges(text, cols)
        .into_iter()
        .map(|range| text[range].to_string())
        .collect::<Vec<_>>()
        .into_iter()
}

/// Take the longest prefix of `s` whose terminal display width fits within
/// `cap` columns, keeping graphemes atomic (CJK counts 2 columns). Trailing
/// whitespace that would overflow is left for the next chunk so continuation
/// lines don't start with blank space. Returns `("", s)` when the first
/// grapheme alone exceeds `cap` (caller force-takes one grapheme).
fn take_width_prefix(s: &str, cap: usize) -> (&str, &str) {
    let mut col = 0usize;
    let mut end = 0usize;
    for (byte, grapheme) in s.grapheme_indices(true) {
        let width = weft_core::grid::terminal_text_width(grapheme);
        if width == 0 {
            continue;
        }
        if col + width > cap {
            if grapheme.chars().all(char::is_whitespace) {
                col += width; // overflow whitespace lands on the next chunk
                continue;
            }
            break;
        }
        col += width;
        end = byte + grapheme.len();
    }
    (&s[..end], &s[end..])
}

/// Wrap a command line: the first line gets `first_cols` (prompt/chevron
/// indent), continuation lines get full `cols`. Mirrors prompt.rs dual-
/// capacity wrapping with pure prose rules (commands never hit PureBox /
/// ProgressGauge structure). Always returns at least one chunk.
pub(crate) fn command_line_chunks(cmd: &str, first_cols: usize, cols: usize) -> Vec<String> {
    if cmd.is_empty() {
        return vec![String::new()];
    }
    let cols_eff = cols.max(1);
    let mut chunks = Vec::new();
    let mut rest = cmd;
    let mut cap = first_cols.max(1);
    while !rest.is_empty() {
        let (taken, next) = take_width_prefix(rest, cap);
        if taken.is_empty() {
            // A single grapheme wider than `cap`: force-take one grapheme
            // (keeps CJK atoms intact) to guarantee forward progress.
            let g = rest.graphemes(true).next().unwrap_or("");
            if g.is_empty() {
                break;
            }
            chunks.push(g.to_string());
            rest = &rest[g.len()..];
        } else {
            chunks.push(taken.to_string());
            rest = next;
        }
        cap = cols_eff; // continuation lines use the full width
    }
    chunks
}

/// Classification of a terminal line for wrap/clip decisions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StructureKind {
    /// Prose / ordinary output — wraps freely.
    None,
    /// Pure Box Drawing / Block Elements line (table rules, separator bars).
    /// Decorative; clipping is safe.
    PureBox,
    /// Progress bar / gauge (ollama pull, brew upgrade, …). Carries ETA /
    /// speed / size info; small overflow clips, large overflow wraps.
    ProgressGauge,
    /// Table row with explicit vertical borders. Decorative borders; clipping
    /// is safe.
    TableRow,
}

fn classify_structure_line(text: &str) -> StructureKind {
    let mut visible = 0usize;
    let mut box_drawing = 0usize;
    // U+2500..=U+257F — Box Drawing (lines, corners, branches):
    //   ─ │ ┌ ┐ └ ┘ ├ ┤ ┬ ┴ ┼ ...
    // U+2580..=U+259F — Block Elements:
    //   █ ▀ ▄ ▌ ▐ ░ ▒ ▓ ▕ ▏ ... (progress bars, separators, UI fill blocks)
    let mut block_chars = 0usize;
    let mut vertical_separators = 0usize;
    for ch in text.chars().filter(|ch| !ch.is_whitespace()) {
        visible += 1;
        let is_box = matches!(ch, '\u{2500}'..='\u{259f}');
        box_drawing += usize::from(is_box);
        if matches!(ch, '\u{2580}'..='\u{259f}') {
            block_chars += 1;
        }
        vertical_separators += usize::from(matches!(ch, '│' | '┃' | '║' | '┆' | '┇' | '┊' | '┋'));
    }
    // 1. Pure Box Drawing / Block Elements (table rules, separator bars):
    //    every visible character belongs to the combined range.
    if visible >= 8 && box_drawing == visible {
        return StructureKind::PureBox;
    }
    // 2. Progress bar / gauge heuristic: a line with 4+ Block Elements
    //    (█▕▏░▒▓ etc.) is almost certainly a progress/status gauge drawn
    //    with block chars (ollama pull, brew upgrade, wget -q --show-progress,
    //    pv, docker pull, cargo -Z build-std timings, …). Gauges live in a
    //    single row and are rewritten in-place with \r; wrapping their tail
    //    (e.g. `9s` of a `1m59s` ETA) produces a confusing dangling chunk.
    //    Small overflows clip; large overflows wrap (see block_line_chunk_ranges).
    if block_chars >= 4 {
        return StructureKind::ProgressGauge;
    }
    // 3. Explicit vertical borders (║, │ heavy, double, dashed variants) at
    //    least 2x — a table row drawn with multi-byte border glyphs even if
    //    the content between them contains ASCII (mixed tables).
    if vertical_separators >= 2 {
        return StructureKind::TableRow;
    }
    StructureKind::None
}

/// Backward-compatible boolean predicate used by tests.
#[cfg(test)]
fn is_terminal_structure_line(text: &str) -> bool {
    classify_structure_line(text) != StructureKind::None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping_keeps_emoji_grapheme_clusters_atomic() {
        assert_eq!(
            wrap_line_chunks("A👩‍🔬B", 3).collect::<Vec<_>>(),
            ["A👩‍🔬", "B"]
        );
    }

    #[test]
    fn zero_width_graphemes_preserve_source_offsets_across_chunks() {
        let line = "ab\u{200b}cdef";
        let chunks = wrap_line_chunks(line, 2).collect::<Vec<_>>();
        assert_eq!(chunks.concat(), line);
        assert_eq!(chunks[0].chars().count(), 3);
        assert_eq!(chunks[1], "cd");
    }

    #[test]
    fn terminal_rules_and_table_rows_are_clipped() {
        assert_eq!(
            block_line_chunks("────────────────────", 8).collect::<Vec<_>>(),
            ["────────"]
        );
        let table = block_line_chunks("│ 磁盘 │ Data 426G / 926G │ 充裕 │", 16).collect::<Vec<_>>();
        assert_eq!(table.len(), 1);
        assert!(weft_core::grid::terminal_text_width(&table[0]) <= 16);
    }

    #[test]
    fn column_aligned_ls_output_wraps_without_loss_or_continuation_indent() {
        let line = "CLAUDE.md          projects          safari-proxy-research.md";
        let chunks = block_line_chunks(line, 24).collect::<Vec<_>>();
        assert!(chunks.len() > 1);
        assert_eq!(chunks.concat(), line);
        assert!(chunks
            .iter()
            .skip(1)
            .all(|chunk| chunk.chars().next().map_or(true, |ch| !ch.is_whitespace())));
    }

    #[test]
    fn prose_and_double_sentence_spaces_still_reflow() {
        assert_eq!(block_line_chunks("ordinary terminal prose", 8).count(), 3);
        let prose = "First sentence.  Second sentence.  Third sentence continues";
        assert!(block_line_chunks(prose, 24).count() > 1);
    }

    #[test]
    fn command_wraps_first_line_indent_then_full_width() {
        // cols=10, first_cols=8: 首行 8 列,续行 10 列
        let chunks = command_line_chunks("abcdefghijklmnop", 8, 10);
        assert_eq!(chunks, vec!["abcdefgh", "ijklmnop"]);
        // 拼接必须还原原文(无字符丢失)
        assert_eq!(chunks.concat(), "abcdefghijklmnop");
    }

    #[test]
    fn command_wrap_counts_cjk_width() {
        let chunks = command_line_chunks("中文命令测试数据", 4, 4); // 每行 2 个 CJK
        assert_eq!(chunks.len(), 4);
        assert_eq!(chunks.concat(), "中文命令测试数据");
    }

    #[test]
    fn command_wrap_handles_empty_and_narrow_first_cols() {
        // 空命令 → 单个空 chunk(调用方 .max(1) 防御)
        assert_eq!(command_line_chunks("", 8, 10), vec![String::new()]);
        // 单个 CJK 宽于 first_cols:强制取一个 grapheme,不死循环
        let chunks = command_line_chunks("中文", 1, 4);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks.concat(), "中文");
    }

    #[test]
    fn progress_bar_small_overflow_clips_tail() {
        // v1.7.5 regression: ollama pull 的进度条用 ▕█▏ (Block Elements,
        // U+2580..=U+259F) 原地刷新。当行宽比 PTY cols 多 2 字符时（off-by-few，
        // 常见于 ambiguous-width 字符计算差异），"9s" 被错误地换到下一个 chunk
        // （= 下一行），破坏单行进度条语义。小溢出（≤ 3 cols）应走 clip 分支，
        // 尾部溢出直接截断，不产生 dangling chunk。
        let prefix = "pulling d4b8b4f4c350:  22% ▕";
        let bar = "█".repeat(15);
        let suffix = "▏ 3.9 GB/ 17 GB  113 MB/s   1m59s";
        let fixed_width = weft_core::grid::terminal_text_width(prefix)
            + weft_core::grid::terminal_text_width(&bar)
            + weft_core::grid::terminal_text_width(suffix);
        // 目标：总宽 = cols + 2（小溢出，在 clip 容差 3 内）
        let cols = 80usize;
        let overflow = 2usize;
        assert!(
            fixed_width <= cols + overflow,
            "test setup: fixed_width {fixed_width} must be ≤ cols+overflow to avoid usize underflow"
        );
        let spaces_needed = cols + overflow - fixed_width;
        let mut line = String::from(prefix);
        line.push_str(&bar);
        line.push_str(&" ".repeat(spaces_needed));
        line.push_str(suffix);
        let total_width = weft_core::grid::terminal_text_width(&line);
        assert_eq!(
            total_width,
            cols + overflow,
            "test setup: line width must be cols+{} for small-overflow case",
            overflow
        );
        assert!(total_width <= cols + PROGRESS_GAUGE_CLIP_TOLERANCE);

        let chunks = block_line_chunks(&line, cols).collect::<Vec<_>>();
        assert_eq!(
            chunks.len(),
            1,
            "progress bar with small overflow must NOT wrap; got {} chunks (tail would become a dangling row)",
            chunks.len()
        );
        let chunk_width = weft_core::grid::terminal_text_width(&chunks[0]);
        assert!(
            chunk_width <= cols,
            "clipped chunk width {chunk_width} must be ≤ cols {cols}"
        );
        assert!(
            !chunks[0].ends_with("9s"),
            "tail '9s' should be clipped, not wrapped to a dangling chunk"
        );
    }

    #[test]
    fn progress_bar_large_overflow_wraps_to_preserve_eta() {
        // v1.7.5 follow-up: 窗口缩窄时（或程序未及时响应 SIGWINCH），进度条
        // 行宽远超当前 cols。此时 clip 会截断 ETA/速度等关键信息，用户看不到
        // "113 MB/s 1m59s"。大溢出（> 3 cols）应走 wrap 分支，让用户能看到
        // 完整信息（即使换行也比截断好）。
        let mut line = String::from("pulling d4b8b4f4c350:  22% ▕");
        line.push_str(&"█".repeat(15));
        line.push_str(&" ".repeat(40));
        line.push_str("▏ 3.9 GB/ 17 GB  113 MB/s   1m59s");
        let total_width = weft_core::grid::terminal_text_width(&line);
        // cols 远小于行宽，模拟窗口缩窄到 60 列
        let cols = 60usize;
        assert!(
            total_width > cols + PROGRESS_GAUGE_CLIP_TOLERANCE,
            "test setup: line width {total_width} must exceed cols+tolerance ({}) to trigger wrap",
            cols + PROGRESS_GAUGE_CLIP_TOLERANCE
        );

        let chunks = block_line_chunks(&line, cols).collect::<Vec<_>>();
        assert!(
            chunks.len() > 1,
            "progress bar with large overflow must WRAP to preserve ETA/speed info; got 1 chunk (clipped)"
        );
        // wrap 后所有 chunk 拼接应保留完整原文（信息无丢失）
        assert_eq!(
            chunks.concat(),
            line,
            "wrapped chunks must preserve full content"
        );
        // 最后一个 chunk 应包含 ETA 信息（证明尾部没被截断）
        assert!(
            chunks.last().unwrap().contains("1m59s") || chunks.concat().ends_with("1m59s"),
            "ETA '1m59s' must be visible in wrapped output, not truncated"
        );
    }

    #[test]
    fn progress_bar_with_minimal_block_chars_still_recognized() {
        // 阈值 4：3 个 Block Elements 不足以识别为进度条（避免误判），
        // 4 个起识别。这里用 4 个 █ 构造最小进度条场景。
        let line = "▕████▏ downloading 50%";
        assert!(is_terminal_structure_line(line));

        // 3 个 Block Elements 不识别（保守阈值）。
        // ▕ + █ + ▏ = 3 个 Block Elements 字符。
        let line3 = "▕█▏ downloading 50%";
        assert!(
            !is_terminal_structure_line(line3),
            "3 block chars should NOT be flagged as progress gauge (false-positive risk)"
        );
    }

    #[test]
    fn pure_ascii_progress_bar_still_wraps() {
        // ASCII 进度条（# = = 等）不包含 Block Elements，走 wrap 分支
        // （这是已知限制：纯 ASCII 进度条不在本次修复范围内）。
        let line = "[####] downloading 50%   1m59s";
        let chunks = block_line_chunks(line, 10).collect::<Vec<_>>();
        assert!(
            chunks.len() > 1,
            "pure-ASCII progress bars still wrap (only Block Elements trigger clip)"
        );
    }

    #[test]
    fn progress_gauge_clip_tolerance_boundary() {
        // 精确边界：PROGRESS_GAUGE_CLIP_TOLERANCE=3，条件是 total_width > cols+3
        // cols+3 → clip（3 > 3 = false），cols+4 → wrap（4 > 3 = true）
        let prefix = "pulling d4b8b4f4c350:  22% ▕";
        let suffix = "▏ 3.9 GB/ 17 GB  113 MB/s   1m59s";
        let bar = "█".repeat(15);
        let fixed_width = weft_core::grid::terminal_text_width(prefix)
            + weft_core::grid::terminal_text_width(&bar)
            + weft_core::grid::terminal_text_width(suffix);
        let cols = 80usize;

        // 边界 1: total_width = cols + 3 → 仍然 clip（边界值，3 不大于 3）
        let overflow_clip = PROGRESS_GAUGE_CLIP_TOLERANCE;
        assert!(fixed_width <= cols + overflow_clip);
        let spaces_clip = cols + overflow_clip - fixed_width;
        let mut line_clip = String::from(prefix);
        line_clip.push_str(&bar);
        line_clip.push_str(&" ".repeat(spaces_clip));
        line_clip.push_str(suffix);
        assert_eq!(
            weft_core::grid::terminal_text_width(&line_clip),
            cols + overflow_clip
        );
        let chunks = block_line_chunks(&line_clip, cols).collect::<Vec<_>>();
        assert_eq!(
            chunks.len(),
            1,
            "total_width = cols + {} (boundary) must still CLIP, not wrap",
            overflow_clip
        );

        // 边界 2: total_width = cols + 4 → wrap（4 > 3 = true）
        let overflow_wrap = PROGRESS_GAUGE_CLIP_TOLERANCE + 1;
        assert!(fixed_width <= cols + overflow_wrap);
        let spaces_wrap = cols + overflow_wrap - fixed_width;
        let mut line_wrap = String::from(prefix);
        line_wrap.push_str(&bar);
        line_wrap.push_str(&" ".repeat(spaces_wrap));
        line_wrap.push_str(suffix);
        assert_eq!(
            weft_core::grid::terminal_text_width(&line_wrap),
            cols + overflow_wrap
        );
        let chunks = block_line_chunks(&line_wrap, cols).collect::<Vec<_>>();
        assert_eq!(
            chunks.len(),
            2,
            "total_width = cols + {} (just past tolerance) must WRAP",
            overflow_wrap
        );
    }

    #[test]
    fn pure_box_line_always_clips_even_with_large_overflow() {
        // 纯 Box Drawing / Block Elements 行（表格框、分隔线）始终 clip，
        // 不受 PROGRESS_GAUGE_CLIP_TOLERANCE 影响——这些是装饰性行，截断无害。
        let line = "██████████████████████████████████████████";
        let cols = 10;
        assert_eq!(classify_structure_line(line), StructureKind::PureBox);
        let chunks = block_line_chunks(line, cols).collect::<Vec<_>>();
        assert_eq!(
            chunks.len(),
            1,
            "PureBox line must always clip (never wrap), even with large overflow"
        );
        assert!(weft_core::grid::terminal_text_width(&chunks[0]) <= cols);
    }
}
