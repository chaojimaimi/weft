use bitflags::bitflags;

bitflags! {
    /// Keyboard modifier flags.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct Modifiers: u8 {
        const SHIFT   = 0x01;
        const ALT     = 0x02;
        const CONTROL = 0x04;
        const SUPER   = 0x08;
    }
}

/// Physical key code, independent of platform key event types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyCode {
    /// Printable character key.
    Char(char),
    /// Enter / Return.
    Enter,
    /// Backspace.
    Backspace,
    /// Tab.
    Tab,
    /// Escape.
    Escape,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Home key.
    Home,
    /// End key.
    End,
    /// Page Up.
    PageUp,
    /// Page Down.
    PageDown,
    /// Delete (forward).
    Delete,
    /// Insert.
    Insert,
    /// Function key F1–F12.
    F(u8),
    /// Numpad key (0–9).
    Numpad(char),
}
