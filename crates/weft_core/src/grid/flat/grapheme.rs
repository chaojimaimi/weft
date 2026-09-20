//! Grapheme width/byte utilities over the flat content stream (ported from
//! Warp `grapheme.rs` + `char_or_str.rs`).
//!
//! Width authority (PLAN_S3 §二 D2, 评审 P2): a grapheme's terminal width is
//! whatever the print path stored in `Cell.width` (the `terminal_char_width`
//! semantics — Regional Indicators are 2, emoji modifiers are 0).
//! `unicode-width` is only ever a cross-check and is deliberately NOT
//! consulted at runtime; the deliberate divergences are pinned by tests.

use std::num::NonZeroU16;

use super::content::ByteOffset;
use super::index::GraphemeInfo;
use crate::grid::cell::{Cell, CellWidth};

#[cfg(test)]
use crate::grid::cell::terminal_char_width;

/// Either a single char or a borrowed string — lets single-scalar cells be
/// pushed into [`super::content::Content`] without materializing a `String`
/// (port of Warp's `char_or_str.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CharOrStr<'a> {
    Char(char),
    Str(&'a str),
}

impl CharOrStr<'_> {
    pub(crate) fn len_utf8(&self) -> usize {
        match self {
            CharOrStr::Char(c) => c.len_utf8(),
            CharOrStr::Str(s) => s.len(),
        }
    }
}

/// One user-perceived character plus its flat-storage sizing metadata.
#[derive(Debug)]
pub(crate) struct Grapheme<'a> {
    info: GraphemeInfo,
    content: CharOrStr<'a>,
}

impl<'a> Grapheme<'a> {
    /// The blank placeholder used to backfill skipped default cells so byte
    /// offsets stay column-aligned. Port note: Warp uses associated consts
    /// here, which need const-stable `NonZeroU16::new` (post-1.75); weft's
    /// MSRV is 1.75, so these are plain functions — no observable cost.
    pub(crate) fn empty_cell() -> Grapheme<'static> {
        Grapheme {
            info: GraphemeInfo {
                cell_width: 1,
                utf8_bytes: nonzero_u16(1),
            },
            content: CharOrStr::Char(' '),
        }
    }

    /// The row terminator grapheme (width 0).
    pub(crate) fn newline() -> Grapheme<'static> {
        Grapheme {
            info: GraphemeInfo {
                cell_width: 0,
                utf8_bytes: nonzero_u16(1),
            },
            content: CharOrStr::Char('\n'),
        }
    }

    /// Builds a grapheme from a viewport cell. `cluster` carries the full
    /// multi-scalar string from `RowExtras` when present (whole-cluster
    /// bytes replace the base scalar's bytes, per D2).
    ///
    /// Width comes from `cell.width` — never recomputed here.
    pub(crate) fn new_from_cell<'c>(cell: &'c Cell, cluster: Option<&'c str>) -> Grapheme<'c> {
        let cell_width = match cell.width {
            CellWidth::Full => 2,
            CellWidth::Half => 1,
        };
        let content = match cluster {
            Some(cluster) => CharOrStr::Str(cluster),
            None => CharOrStr::Char(cell.character),
        };
        let utf8_bytes = u16::try_from(content.len_utf8())
            .ok()
            .and_then(NonZeroU16::new)
            .expect("grapheme length must be 1..=u16::MAX bytes");
        Grapheme {
            info: GraphemeInfo {
                cell_width,
                utf8_bytes,
            },
            content,
        }
    }

    /// Reassembles a grapheme from its content slice plus stored sizing
    /// info — the materialization (`get`) path.
    pub(crate) fn new_from_str_and_info<'s>(grapheme: &'s str, info: GraphemeInfo) -> Grapheme<'s> {
        Grapheme {
            info,
            content: CharOrStr::Str(grapheme),
        }
    }

    /// Builds a grapheme from a string, computing the width with weft's
    /// authority rule. Test-only (rows in production come from cells).
    #[cfg(test)]
    pub(crate) fn new_from_str(grapheme: &'a str) -> Self {
        let cell_width = str_to_cell_width(grapheme);
        let utf8_bytes = u16::try_from(grapheme.len())
            .ok()
            .and_then(NonZeroU16::new)
            .expect("grapheme length must be 1..=u16::MAX bytes");
        Grapheme {
            info: GraphemeInfo {
                cell_width,
                utf8_bytes,
            },
            content: CharOrStr::Str(grapheme),
        }
    }

    /// Width and byte-length metadata.
    pub(crate) fn sizing_info(&self) -> GraphemeInfo {
        self.info
    }

    /// Terminal columns occupied (0 for the newline terminator).
    pub(crate) fn cell_width(&self) -> u8 {
        self.info.cell_width
    }

    /// UTF-8 byte length.
    pub(crate) fn len(&self) -> ByteOffset {
        ByteOffset::from_usize(self.info.utf8_bytes.get() as usize)
    }

    pub(crate) fn content(&self) -> CharOrStr<'_> {
        self.content
    }

    /// True when this grapheme terminates a row.
    pub(crate) fn starts_new_row(&self) -> bool {
        match self.content {
            CharOrStr::Char(c) => c == '\n',
            CharOrStr::Str(s) => s == "\n",
        }
    }

    /// Scalars of this grapheme, zero-alloc (std-only — no itertools
    /// `Either` at this MSRV).
    pub(crate) fn chars(&self) -> GraphemeChars<'a> {
        match self.content {
            CharOrStr::Char(c) => GraphemeChars::Char(std::iter::once(c)),
            CharOrStr::Str(s) => GraphemeChars::Str(s.chars()),
        }
    }
}

/// MSRV-1.75-safe panic-on-zero `NonZeroU16` constructor (const
/// `NonZeroU16::new` is not callable in const context at this MSRV).
fn nonzero_u16(v: u16) -> NonZeroU16 {
    match NonZeroU16::new(v) {
        Some(nz) => nz,
        None => panic!("grapheme sizing must be non-zero"),
    }
}

/// Zero-alloc char iterator over a [`Grapheme`].
pub(crate) enum GraphemeChars<'a> {
    Char(std::iter::Once<char>),
    Str(std::str::Chars<'a>),
}

impl Iterator for GraphemeChars<'_> {
    type Item = char;

    fn next(&mut self) -> Option<char> {
        match self {
            GraphemeChars::Char(it) => it.next(),
            GraphemeChars::Str(it) => it.next(),
        }
    }
}

/// Cell width for a grapheme string, by weft authority: the lead scalar's
/// `terminal_char_width` — exactly what the print path stores in
/// `Cell.width`. Returns 0 for zero-width leads (e.g. bare emoji modifiers)
/// so callers can skip them, mirroring the print path's cluster-append
/// behavior.
#[cfg(test)]
fn str_to_cell_width(grapheme: &str) -> u8 {
    let first = grapheme.chars().next().expect("grapheme is non-empty");
    terminal_char_width(first) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_from_cell_uses_cell_width_as_authority() {
        let wide = Cell::with_char('中');
        let g = Grapheme::new_from_cell(&wide, None);
        assert_eq!(g.cell_width(), 2);
        assert_eq!(g.len().as_usize(), 3);

        let narrow = Cell::with_char('a');
        assert_eq!(Grapheme::new_from_cell(&narrow, None).cell_width(), 1);

        // Multi-scalar cluster: whole-cluster bytes replace the base
        // scalar's bytes; width still comes from the cell.
        let g = Grapheme::new_from_cell(&narrow, Some("e\u{0301}"));
        assert_eq!(g.len().as_usize(), 3);
        assert_eq!(g.cell_width(), 1);
    }

    #[test]
    fn str_width_authority_is_terminal_char_width_not_unicode_width() {
        // Single Regional Indicator: weft grid authority says 2 columns
        // (terminal flag convention) while unicode-width reports 1 — the
        // Cell.width authority wins (v1.6.0 note in cell.rs).
        assert_eq!(terminal_char_width('\u{1f1e6}'), 2);
        assert_eq!(unicode_width::UnicodeWidthStr::width("\u{1f1e6}"), 1);
        assert_eq!(Grapheme::new_from_str("\u{1f1e6}").cell_width(), 2);

        // Emoji modifiers are width 0 for grid layout (they join the
        // previous cluster) though unicode-width reports 2.
        assert_eq!(terminal_char_width('\u{1f3fb}'), 0);
        assert_eq!(unicode_width::UnicodeWidthStr::width("\u{1f3fb}"), 2);
        assert_eq!(Grapheme::new_from_str("\u{1f3fb}").cell_width(), 0);

        // ZWJ sequence takes the lead scalar's terminal width.
        assert_eq!(Grapheme::new_from_str("👩\u{200d}🔬").cell_width(), 2);

        // Combining marks don't add width.
        assert_eq!(Grapheme::new_from_str("e\u{0301}").cell_width(), 1);
    }

    #[test]
    fn newline_terminates_row_and_space_does_not() {
        assert!(Grapheme::newline().starts_new_row());
        assert!(!Grapheme::empty_cell().starts_new_row());
        assert_eq!(Grapheme::newline().cell_width(), 0);
        assert_eq!(Grapheme::empty_cell().cell_width(), 1);
    }

    #[test]
    fn chars_iterates_all_scalars() {
        let g = Grapheme::new_from_str("👩\u{200d}🔬");
        let collected: String = g.chars().collect();
        assert_eq!(collected, "👩\u{200d}🔬");

        let cell = Cell::with_char('x');
        let g = Grapheme::new_from_cell(&cell, None);
        let collected: String = g.chars().collect();
        assert_eq!(collected, "x");
    }
}
