//! Attribute value types for the flat interval maps (ported from Warp
//! `style.rs`, with a review-mandated widening of the bg+style value).
//!
//! The split between foreground color and everything else is a cache-line
//! optimization carried over from the母本: foreground color changes more
//! often than the rest, so a combined map would store the larger value on
//! every fg change. Two maps keep the hot map's payload small.
//!
//! PLAN_S3 §二 D2 (评审 P1-3): Warp's `BgAndStyle` only carries bg + flags —
//! weft history rows must round-trip the v1.11.3 underline slots
//! (`UnderlineStyle` + SGR 58 `underline_color`, cell.rs:204-208), so the
//! value type here is deliberately wider than the母本's. Do not "fix" this
//! back to Warp's shape.

use super::attribute_map::AttributeMap;
use crate::grid::cell::{Cell, CellColor, CellFlags, UnderlineStyle};

/// Foreground color map.
pub(crate) type FgColorMap = AttributeMap<CellColor>;

/// Background + style map: bg color, style flags, and the v1.11.3 underline
/// decoration slots (style + explicit SGR 58 color).
pub(crate) type BgAndStyleMap = AttributeMap<BgAndStyle>;

/// The style bits that survive the Cell → flat encoding.
///
/// Viewport-transient bits (`DIRTY`, `CURSOR`, `SELECTION`) are deliberately
/// dropped (D2: DIRTY 不迁移 — dirty tracking is a viewport concept). The
/// derived bits (`WIDE_SPACER`, `HYPERLINK`, `EXTRA`) are excluded here
/// because materialization rebuilds them from grapheme widths and the
/// hyperlink/cluster maps.
const STYLE_MASK: CellFlags = CellFlags::from_bits_truncate(
    CellFlags::BOLD.bits()
        | CellFlags::ITALIC.bits()
        | CellFlags::UNDERLINE.bits()
        | CellFlags::DOUBLE_UNDER.bits()
        | CellFlags::STRIKETHROUGH.bits()
        | CellFlags::REVERSE.bits()
        | CellFlags::DIM.bits()
        | CellFlags::HIDDEN.bits(),
);

/// The value stored per byte range in the bg+style map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BgAndStyle {
    /// Background color for the range.
    pub bg: CellColor,
    /// Style-related flags (always masked to [`STYLE_MASK`]).
    pub flags: CellFlags,
    /// v1.11.3 underline shape.
    pub underline_style: UnderlineStyle,
    /// v1.11.3 explicit underline color (SGR 58), never touched by REVERSE.
    pub underline_color: Option<CellColor>,
}

impl Default for BgAndStyle {
    fn default() -> Self {
        Self {
            bg: CellColor::Default,
            flags: CellFlags::empty(),
            underline_style: UnderlineStyle::Single,
            underline_color: None,
        }
    }
}

impl BgAndStyle {
    /// Extracts the persisted style slots from a viewport cell.
    pub(crate) fn from_cell(cell: &Cell) -> Self {
        Self {
            bg: cell.bg,
            flags: cell.flags & STYLE_MASK,
            underline_style: cell.underline_style,
            underline_color: cell.underline_color,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_matches_default_cell() {
        assert_eq!(
            BgAndStyle::default(),
            BgAndStyle::from_cell(&Cell::default())
        );
    }

    #[test]
    fn viewport_transient_and_derived_flags_do_not_reach_the_map() {
        let cell = Cell {
            flags: CellFlags::BOLD
                | CellFlags::DIRTY
                | CellFlags::CURSOR
                | CellFlags::SELECTION
                | CellFlags::WIDE_SPACER
                | CellFlags::HYPERLINK
                | CellFlags::EXTRA,
            ..Default::default()
        };
        let style = BgAndStyle::from_cell(&cell);
        assert_eq!(style.flags, CellFlags::BOLD, "only style bits persist");
    }

    #[test]
    fn all_style_bits_are_masked_in() {
        let style_bits = CellFlags::BOLD
            | CellFlags::ITALIC
            | CellFlags::UNDERLINE
            | CellFlags::DOUBLE_UNDER
            | CellFlags::STRIKETHROUGH
            | CellFlags::REVERSE
            | CellFlags::DIM
            | CellFlags::HIDDEN;
        let cell = Cell {
            flags: style_bits,
            ..Default::default()
        };
        assert_eq!(BgAndStyle::from_cell(&cell).flags, style_bits);
    }

    #[test]
    fn underline_slots_round_trip_through_the_value() {
        // 评审 P1-3 专项: wavy underline + SGR 58 palette color must survive
        // the encoding value — this is exactly what Warp's BgAndStyle lacks.
        let cell = Cell {
            underline_style: UnderlineStyle::Wavy,
            underline_color: Some(CellColor::Palette(4)),
            ..Default::default()
        };
        let style = BgAndStyle::from_cell(&cell);
        assert_eq!(style.underline_style, UnderlineStyle::Wavy);
        assert_eq!(style.underline_color, Some(CellColor::Palette(4)));
    }
}
