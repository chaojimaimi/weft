use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

/// Wrap `text` into contiguous byte ranges whose terminal display width fits
/// `cols`, keeping graphemes atomic (CJK counts 2 columns).
///
/// v1.11.10 (PLAN_v11110 M-A/D-a): word-aware breaks. A trailing word (a
/// run of non-whitespace after the last whitespace in the chunk) that fits
/// alone on the next line moves there whole instead of being split mid-word
/// (`skills` → `s|kills` was the character-count artifact). Lines without
/// whitespace (CJK) and words wider than `cols` (git hashes, URLs) keep the
/// historical character cut via the `byte > start` guard.
///
/// P2-1: a chunk's DISPLAYED width may exceed `cols` — trailing overflow
/// whitespace absorbs into the chunk before the break (continuation lines
/// never start with blank space). Consumers must handle the clip at render
/// time and must NOT assume chunk width ≤ cols.
fn wrap_line_chunk_ranges(text: &str, cols: usize) -> Vec<Range<usize>> {
    if cols == 0 {
        return std::iter::once(0..text.len()).collect();
    }
    let mut ranges = Vec::new();
    let mut start = 0usize;
    let mut col = 0usize;
    // v1.11.10 (M-A): trailing-word state — byte offset where the current
    // word started, and the chunk-relative column it started at.
    let mut word_start: Option<usize> = None;
    let mut col_at_word_start: Option<usize> = None;
    for (byte, grapheme) in text.grapheme_indices(true) {
        let width = weft_core::grid::terminal_text_width(grapheme);
        if width == 0 {
            continue;
        }
        if grapheme.chars().all(char::is_whitespace) {
            // Overflowing or not, whitespace always terminates the trailing
            // word (P1-2): a word ending exactly at the column boundary
            // followed by absorbed overflow spaces must not keep its word
            // state, or the next short word would be cut at the wrong
            // point (`zz aaaaaaa␣␣XY` @ cols=10 → break must land at "XY").
            col += width;
            word_start = None;
            col_at_word_start = None;
            continue;
        }
        if word_start.is_none() {
            word_start = Some(byte);
            col_at_word_start = Some(col);
        }
        if col + width > cols {
            // Overflow. Word-aware fallback decision:
            if let (Some(ws), Some(c0)) = (word_start, col_at_word_start) {
                // The whole word moved to the next line must fit there alone.
                let word_fits_alone = (col - c0) + width <= cols;
                if ws > start && word_fits_alone {
                    // chunk1 keeps everything through the trailing padding
                    // whitespace; the word (including the part already past
                    // `cols` and the current grapheme) opens the next chunk.
                    ranges.push(start..ws);
                    start = ws;
                    // New chunk's current column = the width the word has
                    // already consumed. The word is now the chunk head, so
                    // col_at_word_start becomes 0 — a stale pre-fallback
                    // column must not leak into later overflow decisions.
                    col -= c0;
                    col_at_word_start = Some(0);
                    // No continue: this grapheme counts into the new chunk
                    // via `col += width` below.
                } else {
                    // Character hard cut — the `byte > start` guard stays
                    // (P1-1): a single grapheme wider than cols (CJK w=2 @
                    // cols=1) must not push an empty range; advance col only,
                    // the next grapheme breaks.
                    if byte > start {
                        ranges.push(start..byte);
                        start = byte;
                        col = 0;
                    }
                }
            } else if byte > start {
                ranges.push(start..byte);
                start = byte;
                col = 0;
            }
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
    std::iter::once(0..grapheme_prefix_end(text, cols)).collect()
}

/// Maximum overflow (in columns) for a progress gauge to be clipped rather
/// than wrapped. Covers the common off-by-few case (ambiguous-width chars,
/// SIGWINCH race) without truncating large overflows caused by window
/// narrowing.
const PROGRESS_GAUGE_CLIP_TOLERANCE: usize = 3;

/// Longest grapheme-atomic prefix of `text` whose terminal display width fits
/// within `cols` columns; returns the prefix's end byte index. Shared by the
/// structural clip (`block_line_chunk_ranges`) and the screen-origin clip —
/// identical prefix math, no per-site byte-loop duplication. `cols == 0` is
/// handled by the call sites (whole line, see `wrap_line_chunk_ranges`).
fn grapheme_prefix_end(text: &str, cols: usize) -> usize {
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
    end
}

/// Keep terminal-drawn structure atomic across history resizes; prose reflows.
pub(crate) fn block_line_chunks(text: &str, cols: usize) -> impl Iterator<Item = String> {
    block_line_chunk_ranges(text, cols)
        .into_iter()
        .map(|range| text[range].to_string())
        .collect::<Vec<_>>()
        .into_iter()
}

/// Screen-origin (TUI frame) line clip, v1.10.26
/// (FIX_WRAP_EPOCH_AND_VIEWPORT_KEEP B-1): a primary-screen document row is a
/// hard terminal row, so a narrower window CLIPS its right edge into a single
/// chunk instead of soft-wrapping it — a TUI `|]` border must NEVER fold onto
/// the next line at indent 0. Same grapheme-atomic prefix logic as the
/// structural clip above, but unconditional (applies to plain ASCII frames
/// like `[| … |]` that `classify_structure_line` treats as prose). Always
/// returns exactly one source range; shell-output blocks keep soft-wrap via
/// [`block_line_chunk_ranges`].
pub(crate) fn screen_origin_line_chunk_ranges(text: &str, cols: usize) -> Vec<Range<usize>> {
    // v1.10.26 Batch B review nit: cols == 0 must return the WHOLE line as a
    // single chunk, aligned with `wrap_line_chunk_ranges` — a zero-width
    // (degenerate/unit-test) layout must not produce an empty clipped row.
    if cols == 0 {
        return std::iter::once(0..text.len()).collect();
    }
    std::iter::once(0..grapheme_prefix_end(text, cols)).collect()
}

/// Chunked view over [`screen_origin_line_chunk_ranges`] — always a single,
/// right-clipped chunk per line.
pub(crate) fn screen_origin_line_chunks(text: &str, cols: usize) -> impl Iterator<Item = String> {
    screen_origin_line_chunk_ranges(text, cols)
        .into_iter()
        .map(|range| {
            if range.is_empty() {
                String::new()
            } else {
                text[range].to_string()
            }
        })
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

    /// v1.10.25 Batch 2 (FIX_TUI_INPUT_WIDTH_ALIGNMENT): omp input-line
    /// border regression. omp draws its UI at exactly the PTY cols it
    /// receives; the v1.10.19 full-width scheme gave it 203 cols while the
    /// BlockView wraps at the content width 200 — the `|]` border folded to a
    /// continuation chunk at indent 0 ("both border chars on the left",
    /// diagnostic exp2). With the PTY target unified to content width the
    /// source line is AT MOST the wrap width and the border must stay on one
    /// line (chunks == 1).
    #[test]
    fn border_box_within_content_width_stays_on_one_line() {
        // Border boxes up to the content width never fold their `|]` tail.
        let cols = 200usize;
        for body in [0usize, 100, 196] {
            let line = format!("[|{}|]", "x".repeat(body));
            let width = weft_core::grid::terminal_text_width(&line);
            assert_eq!(width, body + 4);
            assert!(
                width <= cols,
                "setup: the TUI source width must fit the block wrap width"
            );
            let chunks = block_line_chunks(&line, cols).collect::<Vec<_>>();
            assert_eq!(
                chunks.len(),
                1,
                "source width {width} <= {cols}: the border must not fold"
            );
            assert!(
                chunks[0].ends_with("|]"),
                "the right border stays at the end of its line"
            );
        }
        // The pre-fix full-width grid (203 > 200) is the counter-example that
        // caused the fold — kept as documentation of the bug this fixes.
        let old_full_width = format!("[|{}|]", "x".repeat(199));
        assert_eq!(
            weft_core::grid::terminal_text_width(&old_full_width),
            203,
            "the v1.10.19 full-width grid exceeded the wrap width by 3"
        );
        assert!(
            block_line_chunks(&old_full_width, cols).count() > 1,
            "a 203-col full-width source still wraps at 200 (removed by Batch 2)"
        );
    }

    /// v1.10.26 Batch B (FIX_WRAP_EPOCH_AND_VIEWPORT_KEEP B-1): a 200-col TUI
    /// frame row laid out at 91 cols must clip to a single chunk — no
    /// continuation row, and `|]` must NEVER appear at the head of a folded
    /// next line. This is the exact problem-2 regression: maximize a TUI to
    /// 200, then narrow to 91; the old soft-wrap folded the right border onto
    /// the next line at indent 0.
    #[test]
    fn screen_origin_superwide_line_clips_not_wraps() {
        let line = format!("[|{}|]", "x".repeat(196));
        assert_eq!(
            weft_core::grid::terminal_text_width(&line),
            200,
            "setup: a full-width TUI source line at 200 cols"
        );
        let cols = 91usize;

        // Screen-origin (TUI frame) row: clip — single chunk, right edge cut.
        let chunks = screen_origin_line_chunks(&line, cols).collect::<Vec<_>>();
        assert_eq!(
            chunks.len(),
            1,
            "screen-origin line must not produce a continuation chunk"
        );
        assert!(
            weft_core::grid::terminal_text_width(&chunks[0]) <= cols,
            "clipped width must fit the layout width {cols}"
        );
        assert!(
            !chunks[0].ends_with("|]"),
            "`|]` falls in the clipped region and must not appear at the next line head"
        );

        // The same text as an ordinary shell-output line keeps soft-wrap:
        // logical lines may overflow and fold across rows (content preserved).
        let wrapped = block_line_chunks(&line, cols).collect::<Vec<_>>();
        assert!(
            wrapped.len() > 1,
            "shell-output lines still soft-wrap at a narrower layout"
        );
        assert_eq!(
            wrapped.concat(),
            line,
            "wrapped shell chunks preserve the full text"
        );
    }

    #[test]
    fn screen_origin_line_that_fits_stays_one_chunk() {
        // A screen-origin line narrower than the layout must remain a single
        // chunk ending in its border (nothing clipped).
        let line = "[| hello |]";
        let chunks = screen_origin_line_chunks(line, 40).collect::<Vec<_>>();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], line);
    }

    /// v1.10.26 Batch B review nit: the screen-origin clip's cols == 0 branch
    /// aligns with `wrap_line_chunk_ranges` — the whole line stays one chunk
    /// (a degenerate width must not collapse to an empty clipped row).
    #[test]
    fn screen_origin_clip_cols_zero_keeps_whole_line() {
        let line = "[| hello |]";
        assert_eq!(
            screen_origin_line_chunk_ranges(line, 0),
            vec![0..line.len()],
            "cols == 0 must be the whole line, matching wrap semantics"
        );
        assert_eq!(
            screen_origin_line_chunks(line, 0).collect::<Vec<_>>(),
            vec![line.to_string()]
        );
    }

    // ── v1.11.10 (PLAN_v11110 M-A/D-a): word-aware breaks ──────────────
    // Every test below corresponds to one state-machine cell of the plan
    // pseudocode (word_start / col_at_word_start tracking + fallback rules).

    #[test]
    fn word_aware_ls_columns_keep_words_whole() {
        // cols=10: the pure character cut splits "skills" (`s|kills`) and
        // "USER.md"; the word-aware rule moves each word whole into the
        // next chunk and keeps the padding whitespace in the chunk before.
        let chunks = wrap_line_chunks("A  B  C  skills  USER.md", 10).collect::<Vec<_>>();
        assert_eq!(chunks, ["A  B  C  ", "skills  ", "USER.md"]);
        assert_eq!(chunks.concat(), "A  B  C  skills  USER.md");
    }

    #[test]
    fn long_no_whitespace_run_keeps_character_cuts_and_preserves_concat() {
        // An 80-char hash has no whitespace: word_start == chunk start, so
        // `ws > start` never holds → the historical character cut applies
        // and concat still reconstructs the original.
        let hash = "0123456789abcdef".repeat(5);
        let chunks = wrap_line_chunks(&hash, 10).collect::<Vec<_>>();
        assert!(chunks.len() > 1);
        assert_eq!(chunks.concat(), hash);
        assert!(chunks.iter().all(|c| c.len() <= 10));
    }

    #[test]
    fn cjk_no_whitespace_lines_keep_chunk_boundaries_byte_identical() {
        // CJK lines (no whitespace anywhere) must keep the per-grapheme
        // character cut — chunk boundaries byte-identical to the pre-change
        // implementation (a 4-col line fits exactly two 2-col graphemes).
        let chunks = wrap_line_chunks("中文命令测试数据", 4).collect::<Vec<_>>();
        assert_eq!(chunks, ["中文", "命令", "测试", "数据"]);
        assert_eq!(chunks.concat(), "中文命令测试数据");
    }

    #[test]
    fn word_exactly_cols_wide_moves_whole_and_does_not_break_again() {
        // The trailing word is exactly `cols` wide: `word_fits_alone` is
        // inclusive, so it fits on the next line and no further break
        // happens inside it.
        let chunks = wrap_line_chunks("ab cdefgh", 6).collect::<Vec<_>>();
        assert_eq!(chunks, ["ab ", "cdefgh"]);
    }

    #[test]
    fn trailing_overflow_whitespace_absorption_is_unchanged() {
        // Line-end overflow whitespace still absorbs into the previous chunk
        // (continuation lines never start with blank space).
        let chunks = wrap_line_chunks("abcdefghi   ", 5).collect::<Vec<_>>();
        assert_eq!(chunks, ["abcde", "fghi   "]);
        assert_eq!(chunks.concat(), "abcdefghi   ");
    }

    #[test]
    fn single_grapheme_wider_than_cols_pushes_no_empty_range() {
        // P1-1: a CJK grapheme (w=2) at cols=1 cannot fit; without the
        // `byte > start` guard the first chunk would be an empty range.
        // Chunk count must match the pre-change implementation.
        let chunks = wrap_line_chunks("中文", 1).collect::<Vec<_>>();
        assert_eq!(chunks, ["中", "文"]);
        assert!(chunks.iter().all(|c| !c.is_empty()));
    }

    #[test]
    fn absorbed_overflow_whitespace_ends_the_trailing_word() {
        // P1-2 anchor: "zz aaaaaaa␣␣XY" @ cols=10 — the word "aaaaaaa" ends
        // exactly at the column boundary, then two overflow spaces absorb.
        // They must TERMINATE the trailing word: "XY" then breaks at its own
        // start. A stale word start would misplace the breakpoint into
        // ["zz ", "aaaaaaa␣␣XY"].
        let chunks = wrap_line_chunks("zz aaaaaaa  XY", 10).collect::<Vec<_>>();
        assert_eq!(chunks, ["zz aaaaaaa  ", "XY"]);
        assert_eq!(chunks.concat(), "zz aaaaaaa  XY");
    }

    #[test]
    fn three_breaks_in_one_line_restart_word_state_between() {
        // P1-3: one line, breaks in sequence — word fallback, then character
        // hard cut, then word fallback again; ws/c0 must restart each time.
        let chunks = wrap_line_chunks("aa bbbbbbbbbb cccccc dd ee ff", 8).collect::<Vec<_>>();
        // 1) the "bbbbbbbbbb" word overflows: it fits alone → fallback to its
        //    start ("aa " keeps the padding whitespace).
        // 2) the same word overflows again with ws == start → hard cut
        //    (chunk2 chars the word, chunk3 continues mid-word "bb ").
        // 3) whitespace reset, word "cccccc" overflows → word fallback again
        //    (chunk4 starts with the whole word).
        assert_eq!(chunks, ["aa ", "bbbbbbbb", "bb ", "cccccc ", "dd ee ff"]);
        assert_eq!(chunks.concat(), "aa bbbbbbbbbb cccccc dd ee ff");
    }
}
