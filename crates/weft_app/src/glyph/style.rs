//! SGR glyph style variants (v1.10.12 bold/italic atlas faces).
//!
//! Split from `glyph/mod.rs` so the main atlas file stays under the
//! 800-line limit. Only the primary (Latin) font gets bold/italic faces —
//! CJK / emoji / symbol fallbacks render regular (they have no usable
//! style faces), matching terminal conventions.

/// Glyph font-style variant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct GlyphStyle {
    pub bold: bool,
    pub italic: bool,
}

impl GlyphStyle {
    pub const REGULAR: Self = Self {
        bold: false,
        italic: false,
    };
    pub const BOLD: Self = Self {
        bold: true,
        italic: false,
    };
    pub const ITALIC: Self = Self {
        bold: false,
        italic: true,
    };
    pub const BOLD_ITALIC: Self = Self {
        bold: true,
        italic: true,
    };

    /// Derive the style from a cell's SGR attribute flags.
    pub fn from_flags(flags: weft_core::grid::CellFlags) -> Self {
        Self {
            bold: flags.contains(weft_core::grid::CellFlags::BOLD),
            italic: flags.contains(weft_core::grid::CellFlags::ITALIC),
        }
    }

    /// Compact 0..=3 key (bit 0 = bold, bit 1 = italic) for atlas indexing.
    #[inline]
    pub fn key(self) -> u8 {
        (self.bold as u8) | ((self.italic as u8) << 1)
    }
}

#[cfg(test)]
mod tests {
    use super::GlyphStyle;
    use weft_core::grid::CellFlags;

    #[test]
    fn glyph_style_keys_cover_all_variants() {
        assert_eq!(GlyphStyle::REGULAR.key(), 0);
        assert_eq!(GlyphStyle::BOLD.key(), 1);
        assert_eq!(GlyphStyle::ITALIC.key(), 2);
        assert_eq!(GlyphStyle::BOLD_ITALIC.key(), 3);
    }

    #[test]
    fn glyph_style_derives_from_cell_flags() {
        assert_eq!(
            GlyphStyle::from_flags(CellFlags::empty()),
            GlyphStyle::REGULAR
        );
        assert_eq!(GlyphStyle::from_flags(CellFlags::BOLD), GlyphStyle::BOLD);
        assert_eq!(
            GlyphStyle::from_flags(CellFlags::ITALIC),
            GlyphStyle::ITALIC
        );
        assert_eq!(
            GlyphStyle::from_flags(CellFlags::BOLD | CellFlags::ITALIC),
            GlyphStyle::BOLD_ITALIC
        );
        assert_eq!(
            GlyphStyle::from_flags(CellFlags::UNDERLINE),
            GlyphStyle::REGULAR
        );
    }
}
