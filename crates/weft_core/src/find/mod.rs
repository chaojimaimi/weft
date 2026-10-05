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

mod blocks;

pub use blocks::{find_in_blocks, find_in_flight, BlockMatch};

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
/// search (v0.9 U-P1).
///
/// T3 (PLAN_S3 D5-1): the scrollback region is a one-shot BYTE snapshot of
/// the flat content plus per-row RLE grapheme runs — no per-cell structs for
/// history (the old shape cost ~32B/cell; this is ~1B/byte of text). The
/// live viewport stays per-cell tokens (it is a Cell grid, not flat).
///
/// Row indexing matches the unified scheme used by `find_in_grid`:
/// scrollback rows `[0, scrollback_rows)` first (oldest → newest), then
/// viewport rows.
#[derive(Clone, Debug)]
pub struct FindSnapshot {
    /// Flat scrollback region: content bytes + per-row search data.
    pub scrollback: FindSnapshotScrollback,
    /// `(char, cell_width, Option<cluster>)` per cell, skip WIDE_SPACER.
    /// `cell_width` ∈ {1,2}. `cluster` is `Some` when the cell has
    /// `CellFlags::EXTRA` and a `RowExtras` grapheme entry.
    pub viewport_rows: Vec<Vec<FindSnapshotCell>>,
    pub num_cols: usize,
}

/// The flat scrollback half of a [`FindSnapshot`]: one owned byte buffer for
/// the whole retained history plus a per-row view into it.
#[derive(Clone, Debug)]
pub struct FindSnapshotScrollback {
    /// Concatenated cell bytes of every retained row (row terminators —
    /// the `\n` bytes — excluded; the walk only needs cells).
    pub content: Vec<u8>,
    /// One entry per retained scrollback row, in document order.
    pub rows: Vec<FindSnapshotRow>,
}

impl FindSnapshotScrollback {
    /// Number of retained scrollback rows.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// True when no scrollback rows are retained.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// Per-row search data for the flat scrollback region: the row's cell bytes
/// inside [`FindSnapshotScrollback::content`] plus the RLE grapheme sizing
/// needed to walk cells (mirrors `flat::index::GraphemeRun` in a pub form —
/// the snapshot crosses the crate boundary to weft_app's find worker).
#[derive(Clone, Debug)]
pub struct FindSnapshotRow {
    /// Start of this row's cell bytes within the region's `content`.
    pub start: usize,
    /// Cell byte count (excludes the row's trailing `\n`, if any).
    pub cell_bytes: usize,
    /// RLE runs: `(grapheme count, cell width, utf8 byte length)`.
    /// Walking them yields exactly the row's cells in column order — the
    /// same data `Index::content_offset_to_point` maps offsets with, kept
    /// row-local so the worker thread needs no `Index`.
    pub runs: Vec<(u16, u8, u16)>,
}

impl FindSnapshot {
    /// Total unified row count (scrollback + viewport).
    pub fn total_rows(&self) -> usize {
        self.scrollback.len() + self.viewport_rows.len()
    }

    /// Expands `unified_row` into a flat `char` stream plus per-char
    /// `(col, width)` back-pointers. `chars` is the row's searchable text (a
    /// grapheme contributes its whole multi-scalar cluster text, or the lead
    /// char alone; trailing never-written cells emit `' '` up to `num_cols`
    /// — the old per-cell walk covered them, so whitespace queries keep
    /// matching there), and `cell_of[i]` is the `(col, cell width)` the char
    /// at index `i` belongs to. Rows come straight from the byte
    /// snapshot (the first cut built a `String` per grapheme ≈ 800k small
    /// allocs on a 10k×80 scrollback). Case folding happens in the match
    /// loops, per char, ASCII-only.
    ///
    /// The `(col, width)` walk is the row-local arithmetic of
    /// `Index::content_offset_to_point` (cumulative cell widths over the RLE
    /// runs), so hit → column mapping needs no `Index` on the worker thread.
    pub fn row_char_stream(&self, unified_row: usize) -> (Vec<char>, Vec<(usize, usize)>) {
        let sb_rows = self.scrollback.len();
        let mut chars: Vec<char> = Vec::new();
        let mut cell_of: Vec<(usize, usize)> = Vec::new();

        if unified_row < sb_rows {
            let row = &self.scrollback.rows[unified_row];
            let mut byte_off = 0usize;
            let mut col = 0usize;
            for &(count, width, utf8_len) in &row.runs {
                let (width, utf8_len) = (width as usize, utf8_len as usize);
                for _ in 0..count {
                    let bytes = &self.scrollback.content
                        [row.start + byte_off..row.start + byte_off + utf8_len];
                    // Runs are constructed from whole graphemes, so the slice
                    // is always char-aligned; the fallback only keeps a
                    // hostile-corruption read safe.
                    let text = std::str::from_utf8(bytes).unwrap_or(" ");
                    debug_assert!(
                        std::str::from_utf8(bytes).is_ok(),
                        "grapheme run bytes must be char-aligned"
                    );
                    for ch in text.chars() {
                        chars.push(ch);
                        cell_of.push((col, width));
                    }
                    byte_off += utf8_len;
                    col += width;
                }
            }
            while col < self.num_cols {
                chars.push(' ');
                cell_of.push((col, 1));
                col += 1;
            }
            return (chars, cell_of);
        }

        let Some(viewport_cells) = self.viewport_rows.get(unified_row - sb_rows) else {
            return (chars, cell_of);
        };
        let mut cumulative_col: usize = 0;
        for cell in viewport_cells {
            let col = cumulative_col.min(self.num_cols);
            let cluster = cell.cluster.as_deref().unwrap_or("");
            if cluster.is_empty() {
                chars.push(cell.ch);
                cell_of.push((col, cell.width as usize));
            } else {
                for ch in cluster.chars() {
                    chars.push(ch);
                    cell_of.push((col, cell.width as usize));
                }
            }
            cumulative_col = cumulative_col.saturating_add(cell.width as usize);
        }
        (chars, cell_of)
    }
}

/// v1.6.0: A single cell in a [`FindSnapshot`]. Carries the lead `char`,
/// its terminal width (1 or 2), and an optional multi-scalar cluster string.
/// `cluster` is `Some` only when `CellFlags::EXTRA` is set on the source cell.
#[derive(Clone, Debug)]
pub struct FindSnapshotCell {
    pub ch: char,
    pub width: u8,
    pub cluster: Option<std::sync::Arc<str>>,
}

impl Grid {
    /// Build a lightweight `FindSnapshot` of the full grid (scrollback +
    /// viewport) for async search. Created on the main thread, then sent to
    /// the worker thread which calls [`find_in_snapshot`] without blocking
    /// the UI.
    ///
    /// T3 (D5-1): history is captured as ONE byte snapshot of the flat
    /// content plus RLE run tables — the per-cell `FindSnapshotCell` clones
    /// (≈32B/cell, ~25MB at 10k×80) are gone. The live viewport keeps the
    /// per-cell token shape (it is a Cell grid).
    pub fn find_snapshot(&self) -> FindSnapshot {
        let (content, rows) = self.scrollback.search_rows_snapshot();
        let viewport_rows = self
            .viewport
            .iter()
            .take(self.num_rows)
            .map(|row| {
                row.cells
                    .iter()
                    .enumerate()
                    .filter_map(|(col, cell)| {
                        if cell.flags.contains(crate::grid::CellFlags::WIDE_SPACER) {
                            return None;
                        }
                        let width = if cell.width == crate::grid::CellWidth::Full {
                            2
                        } else {
                            1
                        };
                        let ch = if cell.character == '\0' {
                            ' '
                        } else {
                            cell.character
                        };
                        let cluster = if cell.flags.contains(crate::grid::CellFlags::EXTRA) {
                            row.extras.grapheme_at(col).map(std::sync::Arc::<str>::from)
                        } else {
                            None
                        };
                        Some(FindSnapshotCell { ch, width, cluster })
                    })
                    .collect()
            })
            .collect();
        FindSnapshot {
            scrollback: FindSnapshotScrollback { content, rows },
            viewport_rows,
            num_cols: self.num_cols,
        }
    }
}

/// v1.12.25 (audit core P1-2): one searchable cell. `Char` holds the lead
/// scalar of an ordinary cell (zero-alloc); `Str` borrows the multi-scalar
/// grapheme cluster text from the row's `RowExtras`. Replaces the old
/// per-cell `String` tokens (`c.to_string()` / `grapheme_at().to_string()`
/// plus a second `to_lowercase()` alloc when folding).
enum Token<'a> {
    Char(char),
    Str(&'a str),
}

/// Build the per-cell token stream for a row from borrowed `(cells, extras)`.
/// Each token carries either the multi-scalar cluster (when EXTRA is set) or
/// the lead `char` alone; `(col, width)` back-pointers ride along. Split out
/// of `find_in_grid`'s old closure so scrollback rows (flat storage — no
/// `&Row` exists; `FlatStorage::get` materializes an owned row) and viewport
/// rows (borrowed in place) share one token builder.
fn tokens_from_row<'a>(
    cells: &'a [crate::grid::Cell],
    extras: &'a crate::grid::RowExtras,
    out: &mut Vec<(Token<'a>, usize, usize)>,
) {
    for (col, cell) in cells.iter().enumerate() {
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
        // v1.6.0: prefer the multi-scalar cluster when present.
        let token = if cell.flags.contains(crate::grid::CellFlags::EXTRA) {
            extras.grapheme_at(col).map_or(Token::Char(c), Token::Str)
        } else {
            Token::Char(c)
        };
        out.push((token, col, w));
    }
}

/// Expand tokens into a flat char stream plus per-char `(col, width)`
/// back-pointers, applying the query's case mode. Folding per token: ASCII
/// takes the allocation-free `to_ascii_lowercase` path; non-ASCII keeps the
/// Unicode `to_lowercase` semantics the old per-token `String::to_lowercase`
/// had (for a single scalar, `char::to_lowercase` == `str::to_lowercase`).
/// A multi-char expansion (e.g. `İ` → "i̇") pushes one back-pointer per
/// produced char, exactly like the old lowercased-String expansion.
fn expand_tokens(
    tokens: &[(Token<'_>, usize, usize)],
    case_sensitive: bool,
) -> (Vec<char>, Vec<(usize, usize)>) {
    let mut line_chars: Vec<char> = Vec::with_capacity(tokens.len());
    let mut char_to_cell: Vec<(usize, usize)> = Vec::with_capacity(tokens.len());
    for (token, col, w) in tokens {
        match *token {
            Token::Char(c) => {
                if case_sensitive || c.is_ascii() {
                    line_chars.push(if case_sensitive {
                        c
                    } else {
                        c.to_ascii_lowercase()
                    });
                    char_to_cell.push((*col, *w));
                } else {
                    for lc in c.to_lowercase() {
                        line_chars.push(lc);
                        char_to_cell.push((*col, *w));
                    }
                }
            }
            Token::Str(s) => {
                if case_sensitive {
                    for ch in s.chars() {
                        line_chars.push(ch);
                        char_to_cell.push((*col, *w));
                    }
                } else if s.is_ascii() {
                    for ch in s.chars().map(|c| c.to_ascii_lowercase()) {
                        line_chars.push(ch);
                        char_to_cell.push((*col, *w));
                    }
                } else {
                    for ch in s.to_lowercase().chars() {
                        line_chars.push(ch);
                        char_to_cell.push((*col, *w));
                    }
                }
            }
        }
    }
    (line_chars, char_to_cell)
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

    // v1.12.25 (audit core P1-2): the old inline closure cloned every row
    // (`cells.to_vec()` + `extras.clone()`, plus 1-2 heap allocs per cell)
    // and folded with a per-token `to_lowercase()` alloc. Rows are now
    // tokenized in place — viewport rows borrow straight off the grid;
    // scrollback rows use the row materialized by the flat storage (the old
    // second clone on top of it is gone) — and case folding happens once,
    // during the char-stream expansion below. Matching semantics are
    // byte-identical (guarded by the existing find tests).
    'outer: for unified_row in 0..total_rows {
        // Flat storage materializes an owned row per access (no `&Row`
        // exists in the byte store); tokens borrow from that local binding —
        // the old second clone (`cells.to_vec()` + `extras.clone()`) is gone.
        // The binding lives at iteration scope so `tokens` can borrow either
        // this row or the viewport row below.
        let materialized_row;
        let mut tokens: Vec<(Token<'_>, usize, usize)> = Vec::with_capacity(grid.num_cols);
        if unified_row < sb_len {
            materialized_row = match grid.scrollback.get(unified_row) {
                Some(row) => row,
                None => continue,
            };
            tokens_from_row(
                &materialized_row.cells,
                &materialized_row.extras,
                &mut tokens,
            );
        } else {
            let vp_row = unified_row - sb_len;
            if vp_row >= grid.viewport.len() {
                continue;
            }
            let row = &grid.viewport[vp_row];
            tokens_from_row(&row.cells, &row.extras, &mut tokens);
        }
        if tokens.is_empty() {
            continue;
        }

        if let Some(re) = &re {
            // Regex path (v1.6.0 review C2 fix): expand tokens into a per-char
            // stream with back-pointers to (col, width), so byte offsets that
            // fall inside a multi-scalar cluster token can be correctly mapped.
            // Previously byte_pos jumped by token length and could skip past
            // start_byte, producing col=0/len=0 bogus matches.
            let (line_chars, char_to_cell) = expand_tokens(&tokens, case_sensitive);
            let line_str: String = line_chars.iter().collect();
            for m in re.find_iter(&line_str) {
                let start_byte = m.start();
                let end_byte = m.end();
                // Map byte offsets back to char indices, then to (col, width).
                let start_char = line_str[..start_byte].chars().count();
                let end_char = line_str[..end_byte].chars().count();
                let mut len = 0;
                let mut prev_col: Option<usize> = None;
                let mut start_col = 0;
                for (i, &(cell_col, cell_w)) in char_to_cell.iter().enumerate() {
                    if i == start_char {
                        start_col = cell_col;
                    }
                    if i >= start_char && i < end_char && prev_col != Some(cell_col) {
                        len += cell_w;
                        prev_col = Some(cell_col);
                    }
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
            // Substring path: build a flat char stream + per-char → (col, w)
            // back-pointers so a match can be mapped back to its originating
            // cells. A multi-scalar token expands to multiple chars that all
            // share the same (col, w) — the match length in cells is the sum
            // of the widths of the *distinct* cells the match spans.
            let (line_chars, char_to_cell) = expand_tokens(&tokens, case_sensitive);

            let mut i = 0;
            while i + needle_chars.len() <= line_chars.len() {
                let matches_here = line_chars[i..i + needle_chars.len()]
                    .iter()
                    .zip(needle_chars.iter())
                    .all(|(c, n)| *c == *n);
                if matches_here {
                    let start_col = char_to_cell[i].0;
                    // Sum widths of distinct cells covered by
                    // [i, i+needle_chars.len()). A multi-scalar cluster
                    // contributes multiple chars but only one cell width.
                    let mut len = 0;
                    let mut prev_col: Option<usize> = None;
                    for &(c_col, c_w) in char_to_cell.iter().skip(i).take(needle_chars.len()) {
                        if prev_col != Some(c_col) {
                            len += c_w;
                            prev_col = Some(c_col);
                        }
                    }
                    matches.push(FindMatch {
                        row: unified_row,
                        col: start_col,
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

/// Search `snapshot` rows `[start, end)` (unified indexing) for `query` —
/// the chunked entry point the background worker calls; row indices in the
/// result are absolute (the caller adds nothing). `find_in_snapshot` is the
/// whole-range convenience wrapper.
///
/// `case_sensitive` / `is_regex` semantics match `find_in_grid` for ASCII;
/// folding is ASCII-only here while `find_in_grid` folds with Unicode
/// `to_lowercase` — non-ASCII caseful queries diverge between the two paths
/// (documented pre-existing split, see the folding note below). Returns
/// `RegexError` via `Result` when `is_regex` is true and the query fails to
/// compile — the App layer surfaces "invalid regex" in the FindUI.
pub fn find_in_snapshot_range(
    snapshot: &FindSnapshot,
    start: usize,
    end: usize,
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

    // Case folding is ASCII-only and applies to BOTH the substring and the
    // regex path — the historical FindSnapshot口径 (the cell-walk path folded
    // per char with `to_ascii_lowercase` too). Note the deliberate divergence
    // from `find_in_grid`, which folds needle and tokens with Unicode
    // `to_lowercase` (T3 review P3-1): non-ASCII caseful queries behave
    // differently between the sync and async paths, exactly as they did
    // before T3. (The T3 rework initially skipped folding for regex mode —
    // caught by the review round as an `HELLO`/`hello` regression and
    // restored here; the differential test carries uppercase fixtures as the
    // tripwire.)
    let needle: String = if case_sensitive {
        query.to_string()
    } else {
        query.to_string().to_ascii_lowercase()
    };
    let needle_chars: Vec<char> = needle.chars().collect();
    if needle_chars.is_empty() && re.is_none() {
        return Ok(Vec::new());
    }

    let end = end.min(snapshot.total_rows());
    let mut matches = Vec::with_capacity(64);

    'outer: for unified_row in start..end {
        // Char stream + per-char → (col, width) back-pointers: a
        // multi-scalar cluster expands to several chars sharing one
        // (col, width), so a match's cell length is the width sum of the
        // DISTINCT cells it spans. Same shape as the `find_in_grid` loops.
        let (mut line_chars, char_to_cell) = snapshot.row_char_stream(unified_row);
        if line_chars.is_empty() {
            continue;
        }
        if !case_sensitive {
            for c in line_chars.iter_mut() {
                *c = c.to_ascii_lowercase();
            }
        }

        if let Some(re) = &re {
            let line_str: String = line_chars.iter().collect();
            for m in re.find_iter(&line_str) {
                // Map byte offsets back to char indices, then to (col, width).
                let start_char = line_str[..m.start()].chars().count();
                let end_char = line_str[..m.end()].chars().count();
                let mut len = 0;
                let mut start_col = 0;
                let mut prev_col: Option<usize> = None;
                for (i, &(cell_col, cell_w)) in char_to_cell.iter().enumerate() {
                    if i == start_char {
                        start_col = cell_col;
                    }
                    if i >= start_char && i < end_char && prev_col != Some(cell_col) {
                        len += cell_w;
                        prev_col = Some(cell_col);
                    }
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
            let mut i = 0;
            while i + needle_chars.len() <= line_chars.len() {
                let matches_here = line_chars[i..i + needle_chars.len()]
                    .iter()
                    .zip(needle_chars.iter())
                    .all(|(c, n)| *c == *n);
                if matches_here {
                    let start_col = char_to_cell[i].0;
                    let mut len = 0;
                    let mut prev_col: Option<usize> = None;
                    for &(c_col, c_w) in char_to_cell.iter().skip(i).take(needle_chars.len()) {
                        if prev_col != Some(c_col) {
                            len += c_w;
                            prev_col = Some(c_col);
                        }
                    }
                    matches.push(FindMatch {
                        row: unified_row,
                        col: start_col,
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

/// Search a `FindSnapshot` for `query` (v0.9 U-P1 — async counterpart of
/// [`find_in_grid`]). Scans the whole snapshot; see
/// [`find_in_snapshot_range`] for the chunked form the worker uses.
pub fn find_in_snapshot(
    snapshot: &FindSnapshot,
    query: &str,
    case_sensitive: bool,
    is_regex: bool,
) -> Result<Vec<FindMatch>, RegexError> {
    find_in_snapshot_range(
        snapshot,
        0,
        snapshot.total_rows(),
        query,
        case_sensitive,
        is_regex,
    )
}

#[cfg(test)]
mod tests;
