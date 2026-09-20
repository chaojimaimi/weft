//! Block-content search (v0.8 B3): command + output text of finished
//! blocks and the in-flight capture buffer. Split out of `find.rs` at T3 —
//! plain-text search over `Block` output has no grid/flat dependency; only
//! the grid-side search stays in [`super`].

use super::{RegexError, MAX_MATCHES};
use crate::blocks::{Block, InFlightBlock};

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
