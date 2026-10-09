//! Smart Select app wiring: shared Grid/BlockView matching, selection and
//! explicit safe opening. All matching remains pure in `weft_core`.

use super::*;
use std::path::{Component, Path, PathBuf};
use weft_core::grid::terminal_char_width;

/// 与 matcher (`byte_index_at_display_col`/`display_col_at_byte`) 共享同一宽度模型：
/// 逐字符用 `terminal_char_width` 求和。`terminal_text_width` 走 `UnicodeWidthStr::width`，
/// 对 regional indicator（旗帜 emoji 单字符）返回 1，而终端约定为 2；两者混用会导致
/// 跨行点击在旗帜所在 chunk 之后定位偏移。
fn line_display_width(text: &str) -> usize {
    text.chars().map(terminal_char_width).sum()
}
use weft_core::smart_select::{byte_index_at_char, match_at, match_display_at, SmartTargetKind};

enum SmartSelection {
    Grid {
        start: GridPos,
        end: GridPos,
    },
    Block {
        start: BlockSelAnchor,
        end: BlockSelAnchor,
    },
}

struct ResolvedSmartTarget {
    kind: SmartTargetKind,
    text: String,
    selection: SmartSelection,
}

type BlockSegment = (usize, usize, usize);

impl App {
    pub(super) fn handle_smart_select_click(&mut self, x: f64, y: f64, open: bool) {
        let Some(resolved) = self.smart_target_at_pixel(x, y) else {
            self.surface_config_error("Smart Select: no semantic target at pointer");
            return;
        };
        self.apply_smart_selection(resolved.selection);
        if open {
            if let Err(error) = self.open_smart_target(resolved.kind, &resolved.text) {
                self.surface_config_error(&format!("Smart Select: {error}"));
            }
        }
        self.request_redraw();
    }

    fn smart_target_at_pixel(&self, x: f64, y: f64) -> Option<ResolvedSmartTarget> {
        if self.block_view_active() {
            // v1.10.26: hit-test the row band directly (the anchor API no
            // longer carries a row index; the target needs the row's
            // (block_id, line) key to build anchors).
            let (rows, _, _) = self.compute_block_view_rows()?;
            let row_index = rows.iter().position(|r| r.contains_y(y as f32))?;
            let row = &rows[row_index];
            let renderer = self.renderer.as_ref()?;
            let ctx = renderer.layout_ctx?;
            let (content_left, _) = crate::layout::block_content_x_bounds(&ctx);
            let click_char = weft_core::selection::pixel_x_to_char_index(
                &row.text,
                x,
                content_left as f64,
                renderer.cell_width() as f64,
                row.indent_cols,
            );
            let (text, click_char, segments) = block_logical_line(&rows, row_index, click_char)?;
            let byte = byte_index_at_char(&text, click_char)?;
            let target = match_at(&text, byte)?;
            let selected_text = target.text(&text).to_string();
            let start_char = text[..target.start].chars().count();
            let end_char = text[..target.end].chars().count();
            let start = block_pos_at_char(&rows, &segments, start_char, false)?;
            let end = block_pos_at_char(&rows, &segments, end_char, true)?;
            return Some(ResolvedSmartTarget {
                kind: target.kind,
                text: selected_text,
                selection: SmartSelection::Block { start, end },
            });
        }

        if !self.terminal_content_contains(x, y) {
            return None;
        }
        let pos = self.pixel_to_grid(x, y);
        let terminal = self.sessions.active().and_then(|tab| tab.lock_terminal())?;
        let (text, click_col, segments) = grid_logical_line(terminal.grid(), pos);
        let target = match_display_at(&text, click_col)?;
        let start = grid_pos_at_display_col(&segments, target.start_display_col)?;
        let end = grid_pos_at_display_col(&segments, target.end_display_col.saturating_sub(1))?;
        Some(ResolvedSmartTarget {
            kind: target.target.kind,
            text: target.target.text(&text).to_string(),
            selection: SmartSelection::Grid { start, end },
        })
    }

    fn apply_smart_selection(&mut self, selection: SmartSelection) {
        // v1.12.25 (audit 3-B, P1-01): the empty-tabs transient has nothing
        // to select — both arms ignore the event.
        match selection {
            SmartSelection::Grid { start, end } => {
                let Some(handler) = self
                    .sessions
                    .active_mut()
                    .map(|tab| &mut tab.selection_handler)
                else {
                    return;
                };
                handler.start(start, SelectionMode::Simple);
                handler.extend(end);
                handler.end();
            }
            SmartSelection::Block { start, end } => {
                let Some(pane) = self.sessions.active_mut() else {
                    return;
                };
                let fingerprint = pane
                    .with_terminal(|t| {
                        crate::selection::block_selection_fingerprint(
                            t.block_tracker().session_blocks(),
                            t.screen_head_lines(),
                        )
                    })
                    .unwrap_or_default();
                crate::selection::start_block_selection(
                    &mut pane.selection_handler,
                    start,
                    fingerprint,
                );
                pane.selection_handler.extend_block_view(end);
                pane.selection_handler.end();
            }
        }
    }

    fn open_smart_target(&self, kind: SmartTargetKind, text: &str) -> Result<(), String> {
        match kind {
            SmartTargetKind::Url => open_url(text).map_err(|error| error.to_string()),
            SmartTargetKind::Path | SmartTargetKind::PathLineColumn => {
                let cwd = self
                    .sessions
                    .active()
                    .and_then(|tab| tab.with_terminal(|t| t.cwd().map(str::to_owned)))
                    .flatten()
                    .map(PathBuf::from);
                let cwd = cwd.as_deref();
                let home = std::env::var_os("HOME").map(PathBuf::from);
                let path = resolve_local_target(text, cwd, home.as_deref())?;
                reveal_path_in_finder(&path).map_err(|error| error.to_string())
            }
            _ => Err("target was selected, but this target type has no open action".into()),
        }
    }
}

fn grid_logical_line(
    grid: &weft_core::grid::Grid,
    clicked: GridPos,
) -> (String, usize, Vec<(usize, String)>) {
    let mut first = clicked.row;
    while first > 0 && grid.displayed_row_wrapped(first - 1) {
        first -= 1;
    }
    let mut last = clicked.row;
    while last + 1 < grid.num_rows && grid.displayed_row_wrapped(last) {
        last += 1;
    }
    let segments: Vec<_> = (first..=last)
        .map(|row| (row, grid.displayed_row_text(row)))
        .collect();
    let click_col = segments
        .iter()
        .take_while(|(row, _)| *row != clicked.row)
        .map(|(_, text)| line_display_width(text))
        .sum::<usize>()
        + clicked.col;
    let text = segments.iter().map(|(_, text)| text.as_str()).collect();
    (text, click_col, segments)
}

fn grid_pos_at_display_col(segments: &[(usize, String)], target: usize) -> Option<GridPos> {
    let mut offset = 0usize;
    for (row, text) in segments {
        let width = line_display_width(text);
        if target < offset + width {
            return Some(GridPos::new(*row, target - offset));
        }
        offset += width;
    }
    None
}

/// Reconstruct the logical line containing the clicked row (all wrapped
/// chunks of one source line), returning the joined text, the click's char
/// offset, and the chunk segments. v1.10.26 (rust-reviewer B1): the guard is
/// now ONLY the content line — `block_id` may be `None` (a LIVE output row)
/// or `Some(id)` (a finished block's output), both addressable as anchors
/// (the anchor model's `block: Option<u64>`, `None` = live segment). Rows
/// WITHOUT a content line (command/header/separator/LiveCommand) are not
/// addressable as anchors, so smart select degrades to `None` there.
fn block_logical_line(
    rows: &[weft_core::selection::BlockViewRow],
    clicked_row_index: usize,
    click_char: usize,
) -> Option<(String, usize, Vec<BlockSegment>)> {
    let clicked_row = rows.get(clicked_row_index)?;
    let line = clicked_row.line?;
    let block = clicked_row.block_id;
    let mut segments: Vec<_> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.block_id == block && row.line == Some(line))
        .map(|(index, row)| (index, row.chunk_char_offset, row.text.chars().count()))
        .collect();
    segments.sort_unstable_by_key(|(_, offset, _)| *offset);
    let text: String = segments
        .iter()
        .filter_map(|(index, _, _)| rows.get(*index))
        .map(|row| row.text.as_str())
        .collect();
    Some((text, clicked_row.chunk_char_offset + click_char, segments))
}

/// Map a char offset in the logical line back to a content anchor. The
/// anchor's `char_offset` is the LOGICAL-line offset (the source line is the
/// unit the selection copies); the chunk only locates which rendered row the
/// click belongs to.
fn block_pos_at_char(
    rows: &[weft_core::selection::BlockViewRow],
    segments: &[BlockSegment],
    target: usize,
    exclusive_end: bool,
) -> Option<BlockSelAnchor> {
    for (index, offset, len) in segments {
        if target < offset + len || (exclusive_end && target == offset + len) {
            let row = rows.get(*index)?;
            let line = row.line?;
            return Some(BlockSelAnchor {
                block: row.block_id.map(|b| b.0),
                line,
                char_offset: target,
            });
        }
    }
    None
}

fn resolve_local_target(
    target: &str,
    cwd: Option<&Path>,
    home: Option<&Path>,
) -> Result<PathBuf, String> {
    if target.chars().any(char::is_control)
        || target
            .chars()
            .any(|ch| matches!(ch, ';' | '|' | '&' | '`' | '$'))
    {
        return Err("unsafe characters in local path".into());
    }
    let decoded_target = decode_terminal_path(target);
    let mut path_text = decoded_target.as_str();
    for _ in 0..2 {
        let Some((head, suffix)) = path_text.rsplit_once(':') else {
            break;
        };
        if suffix.is_empty() || !suffix.chars().all(|ch| ch.is_ascii_digit()) {
            break;
        }
        path_text = head;
    }

    let path = if path_text == "~" {
        home.ok_or_else(|| "home directory is unavailable".to_string())?
            .to_path_buf()
    } else if let Some(rest) = path_text.strip_prefix("~/") {
        home.ok_or_else(|| "home directory is unavailable".to_string())?
            .join(rest)
    } else {
        let candidate = Path::new(path_text);
        if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            cwd.ok_or_else(|| "terminal working directory is unavailable".to_string())?
                .join(candidate)
        }
    };
    if path.components().any(|part| part == Component::ParentDir) {
        return Err("parent-directory traversal is not allowed".into());
    }
    if !path.exists() {
        return Err(format!("path does not exist: {}", path.display()));
    }
    Ok(path)
}

fn decode_terminal_path(target: &str) -> String {
    target.replace("\\ ", " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use weft_core::blocks::BlockId;
    use weft_core::selection::{
        BlockSelAnchor, BlockViewRow, BlockViewRowKind, BlockViewSelection, SelectionContentSource,
    };

    fn block_row(text: &str, offset: usize, line: Option<usize>) -> BlockViewRow {
        BlockViewRow {
            kind: BlockViewRowKind::Output,
            text: text.into(),
            block_id: Some(BlockId(7)),
            y_top: 0.0,
            y_bottom: 20.0,
            line,
            chunk_char_offset: offset,
            indent_cols: 0,
        }
    }

    /// A LIVE segment output row (`block_id` None — the in-flight document).
    fn live_block_row(text: &str, offset: usize, line: usize) -> BlockViewRow {
        BlockViewRow {
            kind: BlockViewRowKind::Output,
            text: text.into(),
            block_id: None,
            y_top: 0.0,
            y_bottom: 20.0,
            line: Some(line),
            chunk_char_offset: offset,
            indent_cols: 0,
        }
    }

    /// In-memory source serving one block (id 7) with the given lines, so
    /// anchor-built selections can be copied the same way the app copies.
    struct SingleBlockSource {
        lines: Vec<String>,
        /// Cached document order (v1.10.26 rust-reviewer S4 — `block_order`
        /// returns a slice, no per-query `Vec` allocation).
        order: Vec<Option<u64>>,
    }

    impl SingleBlockSource {
        fn new(lines: Vec<String>) -> Self {
            Self {
                lines,
                order: vec![Some(7), None],
            }
        }
    }

    impl SelectionContentSource for SingleBlockSource {
        fn line_count(&self, block: Option<u64>) -> usize {
            if block == Some(7) {
                self.lines.len()
            } else {
                0
            }
        }
        fn line_text(&self, block: Option<u64>, line: usize) -> Option<&str> {
            if block == Some(7) {
                self.lines.get(line).map(String::as_str)
            } else {
                None
            }
        }
        fn block_order(&self) -> &[Option<u64>] {
            &self.order
        }
    }

    fn selected_single_row(text: &str, needle: &str) -> String {
        let rows = vec![block_row(text, 0, Some(0))];
        let target = match_at(text, text.find(needle).unwrap()).unwrap();
        let segments = [(0, 0, text.chars().count())];
        let start = block_pos_at_char(
            &rows,
            &segments,
            text[..target.start].chars().count(),
            false,
        )
        .unwrap();
        let end =
            block_pos_at_char(&rows, &segments, text[..target.end].chars().count(), true).unwrap();
        let source = SingleBlockSource::new(vec![text.to_string()]);
        BlockViewSelection::new(start, end).text(&source)
    }

    #[test]
    fn path_line_column_resolves_without_shell_parsing() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let path = resolve_local_target("Cargo.toml:12:3", Some(&root), None).unwrap();
        assert!(path.ends_with("Cargo.toml"));
    }

    #[test]
    fn unsafe_or_traversing_paths_are_rejected() {
        assert!(resolve_local_target("a;open", Some(Path::new("/tmp")), None).is_err());
        assert!(resolve_local_target("../etc/passwd", Some(Path::new("/tmp")), None).is_err());
    }

    #[test]
    fn escaped_space_path_is_decoded_without_a_shell() {
        assert_eq!(decode_terminal_path("/tmp/my\\ file.rs"), "/tmp/my file.rs");
    }

    #[test]
    fn block_view_half_open_end_keeps_ascii_cjk_and_single_char_targets() {
        assert_eq!(selected_single_row("hello", "ell"), "hello");
        assert_eq!(selected_single_row("中文", "文"), "中文");
        assert_eq!(selected_single_row("x", "x"), "x");
    }

    #[test]
    fn block_view_matches_and_copies_a_url_across_wrapped_chunks() {
        let rows = vec![
            block_row("https://ex", 0, Some(3)),
            block_row("ample.com/path", 10, Some(3)),
        ];
        let (text, click, segments) = block_logical_line(&rows, 1, 2).unwrap();
        let target = match_at(&text, byte_index_at_char(&text, click).unwrap()).unwrap();
        assert_eq!(target.text(&text), "https://example.com/path");
        let start = block_pos_at_char(&rows, &segments, 0, false).unwrap();
        let end = block_pos_at_char(&rows, &segments, text.chars().count(), true).unwrap();
        let source = SingleBlockSource::new(vec![
            "unused".into(),
            "unused".into(),
            "unused".into(),
            text.clone(),
        ]);
        assert_eq!(BlockViewSelection::new(start, end).text(&source), text);
        assert_eq!(
            start,
            BlockSelAnchor {
                block: Some(7),
                line: 3,
                char_offset: 0
            },
            "anchor targets the source line, not a chunk row"
        );
    }

    /// Regression (rust-reviewer B1 blocker): LIVE output rows carry
    /// `(block_id = None, line = Some(idx))` — the anchor model supports a
    /// None block (the composed live document), so Cmd+Shift+Click on a live
    /// row must resolve a semantic target instead of degrading to
    /// "no semantic target". The old guard required BOTH `block_id` and
    /// `line` to be `Some`, which dropped every live row.
    #[test]
    fn live_segment_rows_resolve_a_smart_select_target() {
        let rows = vec![
            live_block_row("https://ex", 0, 3),
            live_block_row("ample.com/path", 10, 3),
        ];
        let (text, click, segments) = block_logical_line(&rows, 1, 2).unwrap();
        let target = match_at(&text, byte_index_at_char(&text, click).unwrap()).unwrap();
        assert_eq!(target.text(&text), "https://example.com/path");
        let start = block_pos_at_char(&rows, &segments, 0, false).unwrap();
        let end = block_pos_at_char(&rows, &segments, text.chars().count(), true).unwrap();
        assert_eq!(
            start,
            BlockSelAnchor {
                block: None,
                line: 3,
                char_offset: 0
            },
            "a live row anchors to the live segment (block None)"
        );
        assert_eq!(
            end,
            BlockSelAnchor {
                block: None,
                line: 3,
                char_offset: text.chars().count()
            }
        );
    }

    #[test]
    fn grid_matches_a_url_across_wrapped_rows() {
        let mut grid = weft_core::grid::Grid::new(2, 10);
        for (col, ch) in "https://ex".chars().enumerate() {
            grid.cell_mut(0, col).character = ch;
        }
        grid.viewport[0].wrapped = true;
        for (col, ch) in "ample.com".chars().enumerate() {
            grid.cell_mut(1, col).character = ch;
        }
        let (text, click, segments) = grid_logical_line(&grid, GridPos::new(1, 2));
        let target = match_display_at(&text, click).unwrap();
        assert_eq!(target.target.text(&text), "https://example.com");
        assert_eq!(
            grid_pos_at_display_col(&segments, 0),
            Some(GridPos::new(0, 0))
        );
        assert_eq!(
            grid_pos_at_display_col(&segments, target.end_display_col - 1),
            Some(GridPos::new(1, 8))
        );
    }

    /// 回归（rust-reviewer SUGGESTION）：Grid 路径的显示宽度模型必须与 matcher 内部
    /// 一致。`terminal_text_width`（`UnicodeWidthStr::width`）把 regional indicator pair
    /// 🇨🇳 算成 2（grapheme 层），而终端约定每个 regional indicator 占 2 列（共 4）。
    /// `line_display_width` 逐字符用 `terminal_char_width` 求和，与 matcher 的
    /// `byte_index_at_display_col`/`display_col_at_byte` 共享同一模型。
    /// 若此处退回 `terminal_text_width`，跨行点击在旗帜所在 chunk 之后会偏移 2 列。
    #[test]
    fn line_display_width_matches_regional_indicator_terminal_convention() {
        // 🇨🇳 = U+1F1E8 U+1F1F3，两个 regional indicator
        let flag = "🇨🇳";
        assert_eq!(flag.chars().count(), 2, "flag is two regional indicators");
        // 修复后：与 terminal_char_width 一致，每字符 2 → 共 4
        assert_eq!(line_display_width(flag), 4);
        // 对照：terminal_text_width 给整个 flag 算 2（grapheme 合并），分歧即 bug 源
        assert_eq!(
            weft_core::grid::terminal_text_width(flag),
            2,
            "UnicodeWidthStr treats flag as one grapheme — this is the divergence"
        );
        // 旗帜 + 后续窄字符：累加必须用 per-char 宽度
        assert_eq!(line_display_width("🇨🇳ab"), 6);
        assert_eq!(line_display_width("🇨🇳 xy"), 7);
    }
}
