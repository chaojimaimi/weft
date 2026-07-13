//! Cell-level types: flags, colors, and the [`Cell`] struct.

use bitflags::bitflags;

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
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
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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

/// Where a cell's color comes from. Stored on the cell so a theme/palette
/// change can recolor the whole screen instantly: cells remember their origin
/// (default / palette index / explicit RGB) rather than a pre-resolved color.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CellColor {
    /// Use the theme default (foreground or background depending on slot).
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

/// Terminal cell (~24 bytes).
/// Design reference: Warp 24-byte Cell + Alacritty sparse extra.
#[derive(Clone, Debug)]
pub struct Cell {
    pub character: char,
    pub fg: CellColor,
    pub bg: CellColor,
    pub flags: CellFlags,
    pub width: CellWidth,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            character: ' ',
            fg: CellColor::Default,
            bg: CellColor::Default,
            flags: CellFlags::empty(),
            width: CellWidth::Half,
        }
    }
}

impl Cell {
    pub fn with_char(ch: char) -> Self {
        let width = if unicode_width::UnicodeWidthChar::width_cjk(ch).unwrap_or(0) > 1 {
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
