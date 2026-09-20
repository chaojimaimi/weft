//! Cell-level types: flags, colors, and the [`Cell`] struct.

use bitflags::bitflags;

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
    pub struct CellFlags: u16 {
        const BOLD          = 0x0001;
        const ITALIC        = 0x0002;
        const UNDERLINE     = 0x0004;
        const DOUBLE_UNDER  = 0x0008;
        const STRIKETHROUGH = 0x0010;
        const REVERSE       = 0x0020;
        const DIM           = 0x0040;
        const HIDDEN        = 0x0080;
        const DIRTY         = 0x0200;
        const WIDE_SPACER   = 0x0400;
        const CURSOR        = 0x0800;
        const SELECTION     = 0x1000;
        const HYPERLINK     = 0x2000;
        /// v1.6.0: this cell has multi-scalar grapheme data in `RowExtras`.
        /// Consumers (selection, copy, block capture, renderer) must consult
        /// `RowExtras::grapheme_at(col)` when this bit is set; otherwise the
        /// cell's `character` field is the whole cluster.
        const EXTRA         = 0x4000;
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    pub const DEFAULT_FG: Color = Color::rgb(204, 204, 204);
    pub const DEFAULT_BG: Color = Color::rgb(26, 26, 46);

    /// The xterm 256-color palette: 16 ANSI + 6×6×6 cube (16-231) + grayscale
    /// (232-255). Shared by the VT palette seed and theme defaults so there is
    /// a single source of truth.
    pub fn standard_palette() -> [Color; 256] {
        let mut palette = [Color::DEFAULT_FG; 256];

        // Standard 16 colors
        let standard = [
            (0, 0, 0),       // 0 Black
            (205, 0, 0),     // 1 Red
            (0, 205, 0),     // 2 Green
            (205, 205, 0),   // 3 Yellow
            (0, 0, 238),     // 4 Blue
            (205, 0, 205),   // 5 Magenta
            (0, 205, 205),   // 6 Cyan
            (229, 229, 229), // 7 White
            (127, 127, 127), // 8 Bright Black
            (255, 0, 0),     // 9 Bright Red
            (0, 255, 0),     // 10 Bright Green
            (255, 255, 0),   // 11 Bright Yellow
            (92, 92, 255),   // 12 Bright Blue
            (255, 0, 255),   // 13 Bright Magenta
            (0, 255, 255),   // 14 Bright Cyan
            (255, 255, 255), // 15 Bright White
        ];
        for (i, (r, g, b)) in standard.iter().enumerate() {
            palette[i] = Color::rgb(*r, *g, *b);
        }

        // 16-231: 6x6x6 color cube
        let cube_values = [0, 95, 135, 175, 215, 255];
        let mut idx = 16;
        for r in &cube_values {
            for g in &cube_values {
                for b in &cube_values {
                    palette[idx] = Color::rgb(*r, *g, *b);
                    idx += 1;
                }
            }
        }

        // 232-255: grayscale ramp
        for i in 0u8..24 {
            let v = 8 + i * 10;
            palette[232 + i as usize] = Color::rgb(v, v, v);
        }

        palette
    }
}

/// Underline shape stored per cell (v1.11.3, PLAN_v1113 §1.1).
///
/// `#[repr(u8)]`: the enum niche (the unused discriminant patterns) is what
/// makes `Option<CellColor>` 5 bytes instead of 6 (PLAN_v1113 §1.1) — that
/// niche is a compiler optimization contract, so both enums pin their
/// representation explicitly instead of relying on layout inference.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub enum UnderlineStyle {
    #[default]
    Single,
    Double,
    Wavy,
    Dotted,
    Dashed,
}

/// Where a cell's color comes from. Stored on the cell so a theme/palette
/// change can recolor the whole screen instantly: cells remember their origin
/// (default / palette index / explicit RGB) rather than a pre-resolved color.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub enum CellColor {
    /// Use the theme default (foreground or background depending on slot).
    #[default]
    Default,
    /// Index into the 256-color palette (ANSI 0-15 + 6×6×6 cube + grayscale).
    Palette(u8),
    /// Explicit truecolor (SGR 38;2;r;g;b).
    Rgb(Color),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CellWidth {
    Half = 1,
    Full = 2,
}

/// Column width used by the terminal protocol. East Asian ambiguous symbols
/// (box/block drawing, middle dot, ellipsis) stay one cell, matching xterm and
/// the cursor math used by modern TUIs; CJK ideographs remain two cells.
///
/// v1.6.0: Emoji modifiers (U+1F3FB..U+1F3FF, Fitzpatrick skin tones) are
/// treated as width 0 because they combine with the preceding emoji and
/// must never consume a terminal column. `unicode_width` returns 2 for
/// them (they're emoji-presentation by default), which is correct for
/// isolated rendering but wrong for terminal grid layout.
///
/// v1.6.0: Regional indicator symbols (U+1F1E6..U+1F1FF) are treated as
/// width 2 — they form flag emoji when paired (🇨🇳, 🇺🇸) and every major
/// terminal renders them as double-width. `unicode_width` 0.2 returns 1
/// (Neutral), which is the Unicode East Asian Width property but not the
/// terminal convention.
pub fn terminal_char_width(ch: char) -> usize {
    // v1.6.0: Emoji modifiers combine with the preceding base emoji — they
    // must not advance the cursor or occupy a cell.
    if matches!(ch, '\u{1f3fb}'..='\u{1f3ff}') {
        return 0;
    }
    // v1.6.0: Regional indicators are double-width in terminals (flags).
    if matches!(ch, '\u{1f1e6}'..='\u{1f1ff}') {
        return 2;
    }
    unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0)
}

pub fn terminal_text_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

/// Atlas/grid fallback for a grapheme that cannot fit in the current
/// one-scalar [`Cell`] representation. Never masquerade a partial base glyph
/// as the whole cluster: unsupported narrow clusters use U+FFFD and wide
/// clusters use a full-width question mark, preserving both visibility and
/// terminal column width.
pub fn terminal_grapheme_glyph(grapheme: &str) -> char {
    let mut chars = grapheme.chars();
    let Some(first) = chars.next() else {
        return ' ';
    };
    if chars.next().is_none() {
        first
    } else if terminal_text_width(grapheme) > 1 {
        '\u{ff1f}'
    } else {
        '\u{fffd}'
    }
}

/// Terminal cell (exactly 24 bytes; see the size contract test in
/// grid/tests.rs `cell_struct_stays_at_24_bytes`).
/// Design reference: Warp 24-byte Cell + Alacritty sparse extra.
///
/// `derive(Copy)` (PLAN_S3 D1 评审 P1-1): every field is Copy, and the
/// materialized-history window rebuilds rows by field-copying cells — the
/// compiler now proves that is a pure memcpy (no hidden deep-clone cost).
///
/// Field budget (PLAN_v1113 §1.1): char4 + fg5 + bg5 + flags2 + width1 +
/// style1 + color5 = 23B → align 4 → 24B. One padding byte remains; adding
/// another >1B field must trigger an explicit budget re-evaluation
/// (v0.8_PLAN §5). `Option<CellColor>` is 5B (not 6) via the `#[repr(u8)]`
/// niche on [`CellColor`] — pinned by a size test.
#[derive(Clone, Copy, Debug)]
pub struct Cell {
    pub character: char,
    pub fg: CellColor,
    pub bg: CellColor,
    pub flags: CellFlags,
    pub width: CellWidth,
    /// v1.11.3: underline shape (PLAN_v1113 §1.1). The DOUBLE_UNDER flag
    /// remains alongside during the dual-track transition: the renderer
    /// reads the flag first when both are set (mirror-sync note, v1.12
    /// retirement backlog).
    pub underline_style: UnderlineStyle,
    /// v1.11.3: explicit underline color (SGR 58), application-owned and
    /// never touched by REVERSE swapping (PLAN_v1113 §2.1 S5).
    pub underline_color: Option<CellColor>,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            character: ' ',
            fg: CellColor::Default,
            bg: CellColor::Default,
            flags: CellFlags::empty(),
            width: CellWidth::Half,
            underline_style: UnderlineStyle::Single,
            underline_color: None,
        }
    }
}

/// v1.10.23 (FIX_OMP_CONTENT_LOSS): canonical blank cell returned by
/// [`super::Grid::cell`] for out-of-range columns — a defensive fallback so
/// the column-indexed read side never panics on a history row narrower than
/// the request (the resize invariant normally keeps history rows at least
/// `num_cols` wide; the guard makes that invariant non-load-bearing).
pub(crate) static BLANK_CELL: Cell = Cell {
    character: ' ',
    fg: CellColor::Default,
    bg: CellColor::Default,
    flags: CellFlags::empty(),
    width: CellWidth::Half,
    underline_style: UnderlineStyle::Single,
    underline_color: None,
};

impl Cell {
    pub fn with_char(ch: char) -> Self {
        let width = if terminal_char_width(ch) > 1 {
            CellWidth::Full
        } else {
            CellWidth::Half
        };
        Self {
            character: ch,
            width,
            ..Self::default()
        }
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::{terminal_char_width, terminal_grapheme_glyph, terminal_text_width};

    #[test]
    fn terminal_width_keeps_tui_drawing_symbols_single_cell() {
        for ch in ['▀', '█', '▄', '┃', '·', '…'] {
            assert_eq!(terminal_char_width(ch), 1, "{ch} must occupy one PTY cell");
        }
        assert_eq!(terminal_char_width('中'), 2);
        assert_eq!(terminal_text_width("▀Big·Pickle中"), 13);
        assert_eq!(terminal_text_width("👩‍🔬"), 2);
        assert_eq!(terminal_text_width("*\u{fe0f}"), 2);
        assert_eq!(terminal_grapheme_glyph("e\u{0301}"), '\u{fffd}');
        assert_eq!(terminal_grapheme_glyph("👩‍🔬"), '\u{ff1f}');
        assert_eq!(terminal_grapheme_glyph("*\u{fe0f}"), '\u{ff1f}');
    }
}
