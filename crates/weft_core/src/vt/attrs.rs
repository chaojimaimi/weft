use crate::grid::{CellColor, CellFlags, UnderlineStyle};

/// Current text attributes applied to newly printed characters.
/// Updated by SGR (CSI m) sequences, consumed by `print()`.
#[derive(Clone, Debug)]
pub struct Attrs {
    pub fg: CellColor,
    pub bg: CellColor,
    pub flags: CellFlags,
    /// v1.11.3 (PLAN_v1113 §2.1): underline shape for `4:x` colon subparams.
    /// Wavy/Dotted/Dashed have no flag bit — this field is their carrier.
    pub underline_style: UnderlineStyle,
    /// v1.11.3 (PLAN_v1113 §2.1): underline color from SGR 58. Kept out of
    /// REVERSE swapping (application-owned, see handle_sgr overlay order).
    pub underline_color: Option<CellColor>,
}

impl Default for Attrs {
    fn default() -> Self {
        Self {
            fg: CellColor::Default,
            bg: CellColor::Default,
            flags: CellFlags::empty(),
            underline_style: UnderlineStyle::Single,
            underline_color: None,
        }
    }
}

/// Shell integration markers (OSC 133).
/// Stored during v0.1 for future use in v0.5 block parsing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellMarker {
    PromptStart,
    CommandStart,
    CommandOutputStart,
    CommandEnd { exit_code: i32 },
}
