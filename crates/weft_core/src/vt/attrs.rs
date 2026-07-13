use crate::grid::{CellColor, CellFlags};

/// Current text attributes applied to newly printed characters.
/// Updated by SGR (CSI m) sequences, consumed by `print()`.
#[derive(Clone, Debug)]
pub struct Attrs {
    pub fg: CellColor,
    pub bg: CellColor,
    pub flags: CellFlags,
}

impl Default for Attrs {
    fn default() -> Self {
        Self {
            fg: CellColor::Default,
            bg: CellColor::Default,
            flags: CellFlags::empty(),
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
