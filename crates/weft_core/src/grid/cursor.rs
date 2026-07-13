//! Cursor position and style.

/// Cursor position and state.
#[derive(Clone, Debug)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
    pub visible: bool,
    pub wrap_pending: bool,
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            row: 0,
            col: 0,
            visible: true,
            wrap_pending: false,
        }
    }
}

/// Cursor style (DECSCUSR).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorStyle {
    /// Steady block █ (default)
    #[default]
    Block,
    /// Blinking block █
    BlinkingBlock,
    /// Blinking underline _
    BlinkingUnderline,
    /// Steady underline _
    Underline,
    /// Blinking bar |
    BlinkingBar,
    /// Steady bar |
    Bar,
}

impl CursorStyle {
    pub fn is_blinking(self) -> bool {
        matches!(
            self,
            Self::BlinkingBlock | Self::BlinkingUnderline | Self::BlinkingBar
        )
    }

    pub fn is_block(self) -> bool {
        matches!(self, Self::Block | Self::BlinkingBlock)
    }

    pub fn is_bar(self) -> bool {
        matches!(self, Self::Bar | Self::BlinkingBar)
    }

    pub fn is_underline(self) -> bool {
        matches!(self, Self::Underline | Self::BlinkingUnderline)
    }
}
