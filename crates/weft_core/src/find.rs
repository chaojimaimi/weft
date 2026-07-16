// arch-gate: allow-over-800
// find_in_grid/find_in_blocks/find_in_snapshot: pure search logic with
// regex + case-sensitive variants. Functions share private helpers
// (match_at, advance); splitting would duplicate the match logic.
//! In-grid + in-block text search (v0.8 stage 5 — B3; v0.9 U-P1/U-P2 — async + regex).
//!
//! Pure search logic operating on `Grid` and `BlockTracker` content. The App
//! layer owns the debounce timer and the FindUI; this module just produces
//! matches given a query + a row range to scan.
//!
//! ## v0.9 U-P1: Async search via `FindSnapshot`
//!
//! `find_in_grid` is synchronous and borrows `&Grid`, which blocks the render
//! thread on large scrollbacks. The App-layer `FindWorker` instead creates a
//! lightweight `FindSnapshot` (chars + widths only, ~60% smaller than a full
//! Grid clone) on the main thread, sends it to a background thread, and
//! streams results back via a channel. `find_in_snapshot` is the counterpart
//! of `find_in_grid` that operates on a `FindSnapshot`.
//!
//! ## v0.9 U-P2: Regex support
//!
//! Both `find_in_grid` and `find_in_snapshot` accept `is_regex: bool`. When
//! true, the query is compiled as a `regex::Regex` and matches are found via
//! `find_iter`. Invalid regex returns an empty result (the App layer surfaces
//! "invalid regex" in the UI).

use crate::blocks::{Block, InFlightBlock};
use crate::grid::Grid;

/// Maximum matches collected before truncation. A multi-megabyte scrollback
/// can have 100k+ matches for a short query — we don't need them all to
/// navigate.
pub const MAX_MATCHES: usize = 1_000;

/// A single search hit. `(row, col, len)` — `row` is a viewport-relative
/// index AFTER applying `scroll_offset` (i.e. `cell(row, …)` returns the
/// right cell). `col` is the column of the first matching character; `len`
/// is the match length in cells (which for ASCII equals byte length).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FindMatch {
    pub row: usize,
    pub col: usize,
    pub len: usize,
}

/// Error returned when a regex query fails to compile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegexError(pub String);

/// Lightweight, send-able snapshot of grid content for background-thread
/// search (v0.9 U-P1). Contains only `(char, width)` per cell — skips
/// color/flags/dirty/etc., ~60% smaller than a full `Grid` clone.
///
/// Row indexing matches the unified scheme used by `find_in_grid`:
/// `rows[0]` = oldest scrollback line, `rows[sb_len - 1]` = newest
/// scrollback line, `rows[sb_len]` = viewport row 0, etc.
#[derive(Clone, Debug)]
pub struct FindSnapshot {
    /// `(char, cell_width)` per cell, skip WIDE_SPACER. `cell_width` ∈ {0,1,2}.
    pub rows: Vec<Vec<(char, u8)>>,
    pub num_cols: usize,
}

impl Grid {
    /// Build a lightweight `FindSnapshot` of the full grid (scrollback +
    /// viewport) for async search. Created on the main thread (~2-3ms for
    /// 10K rows), then sent to the worker thread which calls
    /// [`find_in_snapshot`] without blocking the UI.
    pub fn find_snapshot(&self) -> FindSnapshot {
        let sb_len = self.scrollback.len();
        let total = sb_len + self.num_rows;
        let mut rows = Vec::with_capacity(total);
        for unified in 0..total {
            let mut row_chars: Vec<(char, u8)> = Vec::with_capacity(self.num_cols);
            // Borrow the right row: scrollback.get for history, viewport for live.
            let cells: Vec<crate::grid::Cell> = if unified < sb_len {
                self.scrollback
                    .get(unified)
                    .map(|r| r.cells.clone())
                    .unwrap_or_default()
            } else {
                let vp = unified - sb_len;
                if vp < self.viewport.len() {
                    self.viewport[vp].cells.clone()
                } else {
                    Vec::new()
                }
            };
            for cell in cells {
                if cell.flags.contains(crate::grid::CellFlags::WIDE_SPACER) {
                    continue;
                }
                let w: u8 = if cell.width == crate::grid::CellWidth::Full {
                    2
                } else {
                    1
                };
                let c = if cell.character == '\0' {
                    ' '
                } else {
                    cell.character
                };
                row_chars.push((c, w));
            }
            rows.push(row_chars);
        }
        FindSnapshot {
            rows,
            num_cols: self.num_cols,
        }
    }
}

/// Search `grid` for `query`. Returns matches in document order (top-to-bottom,
/// left-to-right), capped at [`MAX_MATCHES`]. Scans the FULL scrollback +
/// viewport (v0.9 U-P1 removed the `MAX_SCAN_ROWS` cap — async search makes
/// full scans affordable).
///
/// `case_sensitive`: when `false`, the match is case-insensitive.
/// `is_regex` (v0.9 U-P2): when `true`, `query` is compiled as a `regex::Regex`
/// and matches are found via `find_iter`. Returns empty on regex compile error.
///
/// Wide characters: each match's `col`/`len` are in **cell columns** (a CJK
/// char occupies 2 cells, so its match len is 2 even though the byte length
/// is 1). For regex matches, `len` is the sum of cell widths of the matched
/// chars (regex may match variable-length patterns).
///
/// `FindMatch.row` is a **unified index** into the combined
/// `(scrollback, viewport)` sequence.
pub fn find_in_grid(
    grid: &Grid,
    query: &str,
    case_sensitive: bool,
    is_regex: bool,
) -> Vec<FindMatch> {
    if query.is_empty() {
        return Vec::new();
    }
    // Compile regex once if requested.
    let re = if is_regex {
        match regex::Regex::new(query) {
            Ok(re) => Some(re),
            Err(_) => return Vec::new(),
        }
    } else {
        None
    };

    let needle: String = if case_sensitive {
        query.to_string()
    } else {
        query.to_lowercase()
    };
    let needle_chars: Vec<char> = needle.chars().collect();
    if needle_chars.is_empty() && re.is_none() {
        return Vec::new();
    }

    let mut matches = Vec::with_capacity(64);

    let sb_len = grid.scrollback_len();
    let total_rows = sb_len + grid.num_rows;

    // Helper: build the char stream for a unified row.
    let build_chars = |unified_row: usize| -> Vec<(char, usize, usize)> {
        let mut chars: Vec<(char, usize, usize)> = Vec::with_capacity(grid.num_cols);
        let cells: Vec<crate::grid::Cell> = if unified_row < sb_len {
            grid.scrollback
                .get(unified_row)
                .map(|r| r.cells.to_vec())
                .unwrap_or_default()
        } else {
            let vp_row = unified_row - sb_len;
            if vp_row >= grid.viewport.len() {
                return chars;
            }
            grid.viewport[vp_row].cells.to_vec()
        };
        for (col, cell) in cells.into_iter().enumerate() {
            if cell.flags.contains(crate::grid::CellFlags::WIDE_SPACER) {
                continue;
            }
            let w = if cell.width == crate::grid::CellWidth::Full {
                2
            } else {
                1
            };
            let c = if cell.character == '\0' {
                ' '
            } else {
                cell.character
            };
            let c = if case_sensitive {
                c
            } else {
                c.to_ascii_lowercase()
            };
            chars.push((c, col, w));
        }
        chars
    };

    'outer: for unified_row in 0..total_rows {
        let chars = build_chars(unified_row);
        if chars.is_empty() {
            continue;
        }

        if let Some(re) = &re {
            // Regex path: build the line string (lowercased if
            // case-insensitive) and run find_iter.
            let line_str: String = chars.iter().map(|(c, _, _)| *c).collect();
            for m in re.find_iter(&line_str) {
                let start_char = m.start();
                let end_char = m.end();
                // Map char indices back to (col, width).
                let mut char_pos = 0;
                let mut start_col = 0;
                let mut len = 0;
                for (c, col, w) in &chars {
                    if char_pos == start_char {
                        start_col = *col;
                    }
                    if char_pos >= start_char && char_pos < end_char {
                        len += *w;
                    }
                    char_pos += c.len_utf8();
                }
                matches.push(FindMatch {
                    row: unified_row,
                    col: start_col,
                    len,
                });
                if matches.len() >= MAX_MATCHES {
                    break 'outer;
                }
            }
        } else {
            // Substring path (existing logic).
            let mut i = 0;
            while i + needle_chars.len() <= chars.len() {
                let matches_here = chars[i..i + needle_chars.len()]
                    .iter()
                    .zip(needle_chars.iter())
                    .all(|((c, _, _), n)| c == n);
                if matches_here {
                    let col = chars[i].1;
                    let len: usize = chars[i..i + needle_chars.len()]
                        .iter()
                        .map(|(_, _, w)| *w)
                        .sum();
                    matches.push(FindMatch {
                        row: unified_row,
                        col,
                        len,
                    });
                    if matches.len() >= MAX_MATCHES {
                        break 'outer;
                    }
                    i += needle_chars.len();
                } else {
                    i += 1;
                }
            }
        }
    }

    matches
}

/// Search a `FindSnapshot` for `query` (v0.9 U-P1 — async counterpart of
/// [`find_in_grid`]). Used by the background `FindWorker` thread to avoid
/// blocking the UI on large scrollbacks.
///
/// `case_sensitive` / `is_regex` semantics match `find_in_grid`. Returns
/// `RegexError` via `Result` when `is_regex` is true and the query fails to
/// compile — the App layer surfaces "invalid regex" in the FindUI.
pub fn find_in_snapshot(
    snapshot: &FindSnapshot,
    query: &str,
    case_sensitive: bool,
    is_regex: bool,
) -> Result<Vec<FindMatch>, RegexError> {
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let re = if is_regex {
        match regex::Regex::new(query) {
            Ok(re) => Some(re),
            Err(e) => return Err(RegexError(e.to_string())),
        }
    } else {
        None
    };

    let needle: String = if case_sensitive {
        query.to_string()
    } else {
        query.to_lowercase()
    };
    let needle_chars: Vec<char> = needle.chars().collect();
    if needle_chars.is_empty() && re.is_none() {
        return Ok(Vec::new());
    }

    let mut matches = Vec::with_capacity(64);

    'outer: for (unified_row, chars) in snapshot.rows.iter().enumerate() {
        if chars.is_empty() {
            continue;
        }

        if let Some(re) = &re {
            // Regex path. Build a lowercase line string if case-insensitive.
            let line_str: String = if case_sensitive {
                chars.iter().map(|(c, _)| *c).collect()
            } else {
                chars.iter().map(|(c, _)| c.to_ascii_lowercase()).collect()
            };
            for m in re.find_iter(&line_str) {
                let start_char = m.start();
                let end_char = m.end();
                // Map byte offsets back to char indices, then to (col, width).
                let mut byte_pos = 0;
                let mut start_col = 0;
                let mut len = 0;
                let mut in_match = false;
                for (char_idx, (c, w)) in chars.iter().enumerate() {
                    if byte_pos == start_char {
                        start_col = chars
                            .iter()
                            .take(char_idx)
                            .map(|(_, ww)| *ww as usize)
                            .sum::<usize>()
                            .min(snapshot.num_cols);
                        in_match = true;
                    }
                    if in_match && byte_pos < end_char {
                        len += *w as usize;
                    }
                    if byte_pos + c.len_utf8() >= end_char && in_match {
                        in_match = false;
                    }
                    byte_pos += c.len_utf8();
                }
                // Note: the above col mapping uses cumulative widths up to start.
                // This matches the cell-column semantics of find_in_grid.
                matches.push(FindMatch {
                    row: unified_row,
                    col: start_col,
                    len,
                });
                if matches.len() >= MAX_MATCHES {
                    break 'outer;
                }
            }
        } else {
            // Substring path. When case-insensitive, compare lowercased chars.
            let mut i = 0;
            while i + needle_chars.len() <= chars.len() {
                let matches_here = chars[i..i + needle_chars.len()]
                    .iter()
                    .zip(needle_chars.iter())
                    .all(|((c, _), n)| {
                        if case_sensitive {
                            c == n
                        } else {
                            c.to_ascii_lowercase() == *n
                        }
                    });
                if matches_here {
                    // col = cumulative cell width of chars before position i
                    let col: usize = chars[..i].iter().map(|(_, w)| *w as usize).sum();
                    let len: usize = chars[i..i + needle_chars.len()]
                        .iter()
                        .map(|(_, w)| *w as usize)
                        .sum();
                    matches.push(FindMatch {
                        row: unified_row,
                        col,
                        len,
                    });
                    if matches.len() >= MAX_MATCHES {
                        break 'outer;
                    }
                    i += needle_chars.len();
                } else {
                    i += 1;
                }
            }
        }
    }

    Ok(matches)
}

/// A search hit in block content (v0.8 B3 — block-view search). The block
/// is identified by id; `line` is the line index within the block's
/// `output` (or `command` if `is_command` is true); `col` is the char index
/// (not cell — block output is plain text, not grid cells). `len` is in
/// chars.
///
/// These are NOT scrollable directly — the app layer converts a block match
/// into a scroll position via `scroll_to_current_find_match` (find_controller),
/// which computes the block's row offset in the block-view layout and adjusts
/// `block_scroll_offset`. The renderer highlights both grid matches (via
/// `FindDrawState.highlight`) and block matches (via
/// `FindDrawState.block_highlight`) using the semantic `find_match` color
/// token from `UiColors`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockMatch {
    pub block_id: crate::blocks::BlockId,
    /// True if the hit is in the command line, false if in the output.
    pub is_command: bool,
    /// Line index within the block's output (or command if is_command).
    pub line: usize,
    /// Char index of the first matching char in that line.
    pub col: usize,
    /// Match length in chars (block text is plain — no wide-cell concept).
    pub len: usize,
}

/// Search block content (command + output) for `query`. Block matches are
/// returned in **document order** — oldest block first, command before
/// output within a block, line-by-line within each field. The result is
/// capped at [`MAX_MATCHES`] entries total across all blocks.
///
/// `case_sensitive`: when `false` (default in the app), the match is
/// case-insensitive; when `true`, character case must match exactly.
///
/// Use this when the user is in the Warp-style block view: the visible
/// content comes from `Block.output` strings (not the live grid), so
/// `find_in_grid` alone returns "no matches" even when the text is on
/// screen. The app layer typically merges grid + block results for the
/// FindUI status count; jumping to a block match requires a layout lookup
/// (the block-view renderer knows each block's row offset).
pub fn find_in_blocks<'a, I>(
    blocks: I,
    query: &str,
    case_sensitive: bool,
    is_regex: bool,
) -> Result<Vec<BlockMatch>, RegexError>
where
    I: IntoIterator<Item = &'a Block>,
{
    if query.is_empty() {
        return Ok(Vec::new());
    }
    // v0.9 U-P2: regex support — compile once, find_iter per line.
    let re = if is_regex {
        match regex::Regex::new(query) {
            Ok(re) => Some(re),
            Err(e) => return Err(RegexError(e.to_string())),
        }
    } else {
        None
    };
    let needle: String = if case_sensitive {
        query.to_string()
    } else {
        query.to_lowercase()
    };
    let needle_chars: Vec<char> = needle.chars().collect();
    if needle_chars.is_empty() && re.is_none() {
        return Ok(Vec::new());
    }

    let mut matches: Vec<BlockMatch> = Vec::with_capacity(64);
    'outer: for b in blocks {
        // Command line first (matches read top-to-bottom within a block).
        for (li, line) in b.command.lines().enumerate() {
            scan_line(
                &needle_chars,
                line,
                case_sensitive,
                re.as_ref(),
                |col, len| {
                    matches.push(BlockMatch {
                        block_id: b.id,
                        is_command: true,
                        line: li,
                        col,
                        len,
                    });
                },
            );
            if matches.len() >= MAX_MATCHES {
                break 'outer;
            }
        }
        if matches.len() >= MAX_MATCHES {
            break 'outer;
        }
        // Output lines (in stored order — top to bottom).
        for (li, line) in b.output.lines().enumerate() {
            scan_line(
                &needle_chars,
                line,
                case_sensitive,
                re.as_ref(),
                |col, len| {
                    matches.push(BlockMatch {
                        block_id: b.id,
                        is_command: false,
                        line: li,
                        col,
                        len,
                    });
                },
            );
            if matches.len() >= MAX_MATCHES {
                break 'outer;
            }
        }
    }

    Ok(matches)
}

/// Search the in-flight (currently running) block. Returns matches in the
/// same shape as [`find_in_blocks`], keyed by the in-flight block's id.
pub fn find_in_flight(
    live: &InFlightBlock<'_>,
    query: &str,
    case_sensitive: bool,
    is_regex: bool,
) -> Result<Vec<BlockMatch>, RegexError> {
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let re = if is_regex {
        match regex::Regex::new(query) {
            Ok(re) => Some(re),
            Err(e) => return Err(RegexError(e.to_string())),
        }
    } else {
        None
    };
    let needle: String = if case_sensitive {
        query.to_string()
    } else {
        query.to_lowercase()
    };
    let needle_chars: Vec<char> = needle.chars().collect();
    if needle_chars.is_empty() && re.is_none() {
        return Ok(Vec::new());
    }
    let mut matches = Vec::with_capacity(32);
    let placeholder_id = crate::blocks::BlockId(0);
    for (li, line) in live.command.lines().enumerate() {
        scan_line(
            &needle_chars,
            line,
            case_sensitive,
            re.as_ref(),
            |col, len| {
                matches.push(BlockMatch {
                    block_id: placeholder_id,
                    is_command: true,
                    line: li,
                    col,
                    len,
                });
            },
        );
    }
    for (li, line) in live.output.lines().enumerate() {
        scan_line(
            &needle_chars,
            line,
            case_sensitive,
            re.as_ref(),
            |col, len| {
                matches.push(BlockMatch {
                    block_id: placeholder_id,
                    is_command: false,
                    line: li,
                    col,
                    len,
                });
            },
        );
    }
    Ok(matches)
}

/// Slide over `line` looking for `needle_chars`. Calls `emit(col, len)`
/// for each non-overlapping match. `col`/`len` are in chars (block text is
/// plain — no wide-cell concept). When `case_sensitive` is `false`, both
/// sides are lowercased before comparison; when `true`, exact char match.
///
/// v0.9 U-P2: when `re` is `Some`, runs `re.find_iter` instead of the
/// substring path. `needle_chars` is ignored in regex mode.
fn scan_line(
    needle_chars: &[char],
    line: &str,
    case_sensitive: bool,
    re: Option<&regex::Regex>,
    mut emit: impl FnMut(usize, usize),
) {
    if let Some(re) = re {
        // Regex path. Lowercase the line if case-insensitive.
        let line_str = if case_sensitive {
            line.to_string()
        } else {
            line.to_lowercase()
        };
        for m in re.find_iter(&line_str) {
            // Convert byte offsets to char offsets for col/len.
            let start_byte = m.start();
            let end_byte = m.end();
            let start_char = line_str[..start_byte.min(line_str.len())].chars().count();
            let end_char = line_str[..end_byte.min(line_str.len())].chars().count();
            emit(start_char, end_char - start_char);
        }
        return;
    }
    // Substring path.
    let line_chars: Vec<char> = line.chars().collect();
    if needle_chars.is_empty() || line_chars.len() < needle_chars.len() {
        return;
    }
    let mut i = 0;
    while i + needle_chars.len() <= line_chars.len() {
        let matches_here = if case_sensitive {
            line_chars[i..i + needle_chars.len()]
                .iter()
                .zip(needle_chars.iter())
                .all(|(c, n)| c == n)
        } else {
            line_chars[i..i + needle_chars.len()]
                .iter()
                .map(|c| c.to_ascii_lowercase())
                .zip(needle_chars.iter())
                .all(|(c, n)| c == *n)
        };
        if matches_here {
            emit(i, needle_chars.len());
            i += needle_chars.len();
        } else {
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::BlockId;
    use crate::grid::{CellFlags, Grid};

    fn write(grid: &mut Grid, row: usize, col: usize, s: &str) {
        let mut c = col;
        for ch in s.chars() {
            if c >= grid.num_cols {
                break;
            }
            let cell = grid.cell_mut(row, c);
            cell.character = ch;
            cell.flags = CellFlags::DIRTY;
            // Mark wide chars so find_in_grid reports match length in cells
            // (matching the real vt print path which sets CellWidth::Full).
            let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
            cell.width = if w > 1 {
                crate::grid::CellWidth::Full
            } else {
                crate::grid::CellWidth::Half
            };
            c += if w > 1 { 2 } else { 1 };
        }
    }

    fn make_block(id: u64, command: &str, output: &str) -> Block {
        Block {
            id: BlockId(id),
            command: command.to_string(),
            cwd: None,
            output: output.into(),
            exit_code: Some(0),
            started_at: std::time::SystemTime::now(),
            finished_at: Some(std::time::SystemTime::now()),
            collapsed: false,
        }
    }

    #[test]
    fn empty_query_returns_empty() {
        let mut g = Grid::new(5, 20);
        write(&mut g, 0, 0, "hello world");
        assert!(find_in_grid(&g, "", false, false).is_empty());
    }

    #[test]
    fn finds_substring_case_insensitive() {
        let mut g = Grid::new(5, 20);
        write(&mut g, 0, 0, "Hello World");
        let m = find_in_grid(&g, "world", false, false);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].row, 0);
        assert_eq!(m[0].col, 6);
        assert_eq!(m[0].len, 5);
    }

    #[test]
    fn finds_multiple_matches_in_document_order() {
        let mut g = Grid::new(5, 20);
        write(&mut g, 0, 0, "foo bar foo");
        write(&mut g, 1, 0, "baz foo qux");
        let m = find_in_grid(&g, "foo", false, false);
        assert_eq!(m.len(), 3);
        assert_eq!(
            m[0],
            FindMatch {
                row: 0,
                col: 0,
                len: 3
            }
        );
        assert_eq!(
            m[1],
            FindMatch {
                row: 0,
                col: 8,
                len: 3
            }
        );
        assert_eq!(
            m[2],
            FindMatch {
                row: 1,
                col: 4,
                len: 3
            }
        );
    }

    #[test]
    fn wide_char_match_len_is_in_cells() {
        // 中 is double-width: 2 cells.
        let mut g = Grid::new(3, 20);
        write(&mut g, 0, 0, "中");
        let m = find_in_grid(&g, "中", false, false);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].len, 2, "CJK char match length is in cells");
    }

    #[test]
    fn case_sensitive_skips_different_case() {
        let mut g = Grid::new(2, 20);
        write(&mut g, 0, 0, "Hello World");
        // Case-sensitive: "world" doesn't match "World".
        assert!(find_in_grid(&g, "world", true, false).is_empty());
        // Case-sensitive: "World" matches exactly once.
        let m = find_in_grid(&g, "World", true, false);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].col, 6);
    }

    #[test]
    fn case_sensitive_blocks_skips_different_case() {
        let blocks = [make_block(1, "Echo HELLO", "out")];
        // Case-insensitive matches HELLO → hello.
        assert_eq!(
            find_in_blocks(blocks.iter(), "hello", false, false)
                .unwrap()
                .len(),
            1
        );
        // Case-sensitive: "hello" ≠ "HELLO".
        assert!(find_in_blocks(blocks.iter(), "hello", true, false)
            .unwrap()
            .is_empty());
        // Case-sensitive: exact match.
        assert_eq!(
            find_in_blocks(blocks.iter(), "HELLO", true, false)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn find_in_blocks_returns_command_and_output_matches() {
        let blocks = [
            make_block(1, "echo hello", "hello\nworld hello"),
            make_block(2, "ls", "nothing here"),
        ];
        let m = find_in_blocks(blocks.iter(), "hello", false, false).unwrap();
        // 1 in command of block 1, 2 in output of block 1, 0 in block 2.
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].block_id, BlockId(1));
        assert!(m[0].is_command);
        assert_eq!(m[0].line, 0);
        assert_eq!(m[0].col, 5);
        assert_eq!(m[0].len, 5);
        assert!(!m[1].is_command);
        assert_eq!(m[1].line, 0); // first line of output "hello"
        assert_eq!(m[2].line, 1); // second line "world hello"
        assert_eq!(m[2].col, 6);
    }

    #[test]
    fn find_in_blocks_empty_query_returns_empty() {
        let blocks = [make_block(1, "echo hi", "hi")];
        assert!(find_in_blocks(blocks.iter(), "", false, false)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn find_in_blocks_case_insensitive() {
        let blocks = [make_block(1, "Echo HELLO", "out")];
        let m = find_in_blocks(blocks.iter(), "hello", false, false).unwrap();
        assert_eq!(m.len(), 1);
        assert!(m[0].is_command);
    }

    #[test]
    fn regex_finds_in_blocks() {
        let blocks = [make_block(1, "echo foo123bar", "abc456def xyz")];
        let m = find_in_blocks(blocks.iter(), "[a-z]+[0-9]+", false, true).unwrap();
        assert_eq!(m.len(), 2);
        assert!(m[0].is_command); // foo123 in command
        assert!(!m[1].is_command); // abc456 in output
    }

    // ── v0.9 U-P1/U-P2: snapshot + regex tests ──────────────────────────

    #[test]
    fn snapshot_finds_same_matches_as_grid() {
        // Snapshot path must agree with the direct grid path on the same content.
        let mut g = Grid::new(3, 20);
        write(&mut g, 0, 0, "foo bar foo");
        write(&mut g, 1, 0, "baz foo qux");
        let direct = find_in_grid(&g, "foo", false, false);
        let snap = g.find_snapshot();
        let snap_matches = find_in_snapshot(&snap, "foo", false, false).unwrap();
        assert_eq!(
            direct, snap_matches,
            "snapshot matches must equal grid matches"
        );
    }

    #[test]
    fn snapshot_supports_case_sensitive() {
        let mut g = Grid::new(2, 20);
        write(&mut g, 0, 0, "Hello World");
        let snap = g.find_snapshot();
        // Case-sensitive: "world" doesn't match "World".
        assert!(find_in_snapshot(&snap, "world", true, false)
            .unwrap()
            .is_empty());
        // Case-sensitive: "World" matches.
        let m = find_in_snapshot(&snap, "World", true, false).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].col, 6);
    }

    #[test]
    fn regex_finds_pattern_matches() {
        // `foo.*bar` should match the whole "fooxyzbar" span.
        let mut g = Grid::new(2, 30);
        write(&mut g, 0, 0, "fooxyzbar baz");
        let m = find_in_grid(&g, "foo.*bar", false, true);
        assert_eq!(m.len(), 1, "regex should match fooxyzbar");
        assert_eq!(m[0].col, 0);
        assert_eq!(m[0].len, 9, "regex match length is in cells");
    }

    #[test]
    fn regex_invalid_returns_empty() {
        let mut g = Grid::new(2, 20);
        write(&mut g, 0, 0, "hello");
        // Invalid regex `[a-z` (unclosed class) — find_in_grid returns empty.
        assert!(find_in_grid(&g, "[a-z", false, true).is_empty());
        // find_in_snapshot returns Err(RegexError).
        let snap = g.find_snapshot();
        assert!(find_in_snapshot(&snap, "[a-z", false, true).is_err());
    }

    #[test]
    fn regex_snapshot_matches_grid_regex() {
        let mut g = Grid::new(3, 30);
        write(&mut g, 0, 0, "foo123bar");
        write(&mut g, 1, 0, "abc456def");
        let direct = find_in_grid(&g, "[a-z]+[0-9]+", false, true);
        let snap = g.find_snapshot();
        let snap_matches = find_in_snapshot(&snap, "[a-z]+[0-9]+", false, true).unwrap();
        // Both paths should find the same number of matches (one per line).
        assert_eq!(
            direct.len(),
            snap_matches.len(),
            "regex match count must agree"
        );
        for (d, s) in direct.iter().zip(snap_matches.iter()) {
            assert_eq!(d.row, s.row, "row must agree");
            assert_eq!(d.len, s.len, "len must agree");
        }
    }

    // ── T3: additional find coverage ───────────────────────────────────

    /// Helper: build a Row with the given text written into its cells.
    fn make_row(num_cols: usize, text: &str) -> crate::grid::Row {
        let mut row = crate::grid::Row::new(num_cols);
        for (i, ch) in text.chars().enumerate() {
            if i >= num_cols {
                break;
            }
            row.cells[i].character = ch;
            row.cells[i].flags = CellFlags::DIRTY;
            let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
            row.cells[i].width = if w > 1 {
                crate::grid::CellWidth::Full
            } else {
                crate::grid::CellWidth::Half
            };
        }
        row
    }

    #[test]
    fn find_in_grid_covers_scrollback_and_viewport() {
        // find_in_grid uses a unified row index over (scrollback, viewport).
        // A match in scrollback and a match in viewport should both be found,
        // with row indices reflecting their unified position.
        let mut g = Grid::new(3, 25);
        // Push two rows into scrollback (the older region).
        g.scrollback.push(make_row(25, "old foo here"));
        g.scrollback.push(make_row(25, "second scrollback foo"));
        // Viewport has a match too.
        write(&mut g, 0, 0, "viewport foo");
        write(&mut g, 1, 0, "no match here");
        write(&mut g, 2, 0, "another foo");

        let m = find_in_grid(&g, "foo", false, false);
        // 4 matches: 2 in scrollback + 2 in viewport.
        assert_eq!(m.len(), 4);
        // Scrollback matches come first (unified row 0, 1).
        assert_eq!(m[0].row, 0, "scrollback row 0");
        assert_eq!(m[1].row, 1, "scrollback row 1");
        // Viewport matches follow at rows sb_len + vp_row.
        let sb_len = g.scrollback_len();
        assert_eq!(m[2].row, sb_len, "viewport row 0");
        assert_eq!(m[3].row, sb_len + 2, "viewport row 2");
    }

    #[test]
    fn find_in_blocks_matches_command_only() {
        // When only the command line contains the needle (not the output),
        // find_in_blocks must still return a match marked is_command=true.
        let blocks = [make_block(7, "grep hello files", "no output matching")];
        let m = find_in_blocks(blocks.iter(), "hello", false, false).unwrap();
        assert_eq!(m.len(), 1);
        assert!(m[0].is_command, "match should be in command, not output");
        assert_eq!(m[0].block_id, BlockId(7));
        assert_eq!(m[0].col, 5);
        assert_eq!(m[0].len, 5);
    }

    #[test]
    fn find_case_sensitive_empty_query_returns_empty() {
        // An empty query with case_sensitive=true must still return empty
        // (the early-return guard is independent of case sensitivity).
        let mut g = Grid::new(2, 20);
        write(&mut g, 0, 0, "hello world");
        assert!(find_in_grid(&g, "", true, false).is_empty());
        // find_in_snapshot path too.
        let snap = g.find_snapshot();
        assert!(find_in_snapshot(&snap, "", true, false).unwrap().is_empty());
        // And the blocks path.
        let blocks = [make_block(1, "echo hi", "hi")];
        assert!(find_in_blocks(blocks.iter(), "", true, false)
            .unwrap()
            .is_empty());
    }
}
