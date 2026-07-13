/// Mouse button for terminal events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

/// Mouse action type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Release,
    Move,
}

/// SGR mouse protocol mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseProtocol {
    /// No mouse protocol — mouse events not forwarded to PTY.
    Off,
    /// X10 mode: report on button press only (CSI M).
    X10,
    /// Normal tracking: report button press/release (CSI M).
    Normal,
    /// Button-event tracking: report press/release + motion with button held.
    ButtonEvent,
    /// Any-event tracking: report all mouse events.
    AnyEvent,
}
