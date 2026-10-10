//! Deterministic normalization for native keyboard and IME events.
//!
//! The winit callback and the replay tests consume the same pure routing
//! functions. Tests can therefore exercise multi-event ownership sequences
//! without constructing an AppKit window, Metal renderer or live PTY.

use crate::input_router::OverlayInputOwner;
use crate::tab::Tab;
use weft_core::input::{InputMode, KeyCode};
use winit::keyboard::KeyCode as WinitKeyCode;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ImeInput {
    Enabled,
    Preedit {
        text: String,
        cursor: Option<(usize, usize)>,
    },
    Commit(String),
    Disabled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ImeRouteContext {
    pub(crate) owner: Option<OverlayInputOwner>,
    pub(crate) input_mode: InputMode,
    pub(crate) active_tab: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImeCommitTarget {
    // v1.12.24 (N-1): conservative sink (router-reachable only by invariant breach).
    NoteEditorConsumed,
    Palette,
    SettingsConsumed,
    Find,
    ContextMenuConsumed,
    PanelSearch,
    Editor { tab: usize },
    Pty { tab: usize },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ImeRoutingAction {
    ClearActivePreedit,
    SetActivePreedit {
        text: String,
        cursor: Option<(usize, usize)>,
    },
    /// v1.12.26 (P1-02): target-carrying variants for the find bar and the
    /// panel search box — both grew their own preedit state (palette/note
    /// precedent). Deliberately NOT one generalized SetActivePreedit: the
    /// controller arm reading the action must know which state owner to
    /// write, and a target-less variant would force a second guess off
    /// `context.owner` (the exact drift these pure routing actions exist
    /// to prevent).
    SetActiveFindPreedit {
        text: String,
        cursor: Option<(usize, usize)>,
    },
    SetActivePanelPreedit {
        text: String,
        cursor: Option<(usize, usize)>,
    },
    ClearAllPreedit,
    Commit {
        target: ImeCommitTarget,
        text: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImeContextResetAction {
    ClearAllPreedit,
    DiscardNativeMarkedText,
}

pub(crate) fn reset_ime_context_actions() -> [ImeContextResetAction; 2] {
    [
        ImeContextResetAction::ClearAllPreedit,
        ImeContextResetAction::DiscardNativeMarkedText,
    ]
}

pub(crate) fn route_ime_input(input: ImeInput, context: ImeRouteContext) -> Vec<ImeRoutingAction> {
    match input {
        ImeInput::Enabled => Vec::new(),
        ImeInput::Preedit { text, cursor } => {
            // v1.8.4: Palette (Search + AiCommand) supports inline preedit
            // so CJK IME works. v1.12.26 (P1-02/P1-03): Find/PanelSearch
            // grew their own preedit state, so they route to target-carrying
            // Set actions — the old blanket ClearActivePreedit here dropped
            // the composition display entirely and left the macOS candidate
            // window anchored at the terminal caret (v1.8.4 self-confessed
            // gap). Settings/ContextMenu consume (clear) — they're not
            // text-entry surfaces.
            match context.owner {
                Some(OverlayInputOwner::Palette) => {
                    vec![ImeRoutingAction::SetActivePreedit { text, cursor }]
                }
                Some(OverlayInputOwner::Find) => {
                    vec![ImeRoutingAction::SetActiveFindPreedit { text, cursor }]
                }
                Some(OverlayInputOwner::PanelSearch) => {
                    vec![ImeRoutingAction::SetActivePanelPreedit { text, cursor }]
                }
                Some(_) => vec![ImeRoutingAction::ClearActivePreedit],
                None => vec![ImeRoutingAction::SetActivePreedit { text, cursor }],
            }
        }
        ImeInput::Commit(text) => {
            let mut actions = vec![ImeRoutingAction::ClearActivePreedit];
            if text.is_empty() {
                return actions;
            }
            let target = match context.owner {
                // v1.12.24 (N-1): conservative arm (intercepted pre-router).
                Some(OverlayInputOwner::NoteEditor) => ImeCommitTarget::NoteEditorConsumed,
                Some(OverlayInputOwner::Palette) => ImeCommitTarget::Palette,
                Some(OverlayInputOwner::Settings) => ImeCommitTarget::SettingsConsumed,
                Some(OverlayInputOwner::Find) => ImeCommitTarget::Find,
                Some(OverlayInputOwner::ContextMenu) => ImeCommitTarget::ContextMenuConsumed,
                Some(OverlayInputOwner::PanelSearch) => ImeCommitTarget::PanelSearch,
                None if context.input_mode == InputMode::Editor => ImeCommitTarget::Editor {
                    tab: context.active_tab,
                },
                None => ImeCommitTarget::Pty {
                    tab: context.active_tab,
                },
            };
            actions.push(ImeRoutingAction::Commit { target, text });
            actions
        }
        ImeInput::Disabled => vec![ImeRoutingAction::ClearAllPreedit],
    }
}

pub(crate) fn clear_all_preedit(tabs: &mut [Tab]) {
    for tab in tabs {
        tab.ime_preedit.clear();
        tab.ime_preedit_cursor = None;
    }
}

pub(crate) fn map_winit_key(key: WinitKeyCode) -> Option<KeyCode> {
    Some(match key {
        WinitKeyCode::Enter => KeyCode::Enter,
        WinitKeyCode::Backspace => KeyCode::Backspace,
        WinitKeyCode::Tab => KeyCode::Tab,
        WinitKeyCode::Escape => KeyCode::Escape,
        WinitKeyCode::ArrowUp => KeyCode::Up,
        WinitKeyCode::ArrowDown => KeyCode::Down,
        WinitKeyCode::ArrowLeft => KeyCode::Left,
        WinitKeyCode::ArrowRight => KeyCode::Right,
        WinitKeyCode::Home => KeyCode::Home,
        WinitKeyCode::End => KeyCode::End,
        WinitKeyCode::PageUp => KeyCode::PageUp,
        WinitKeyCode::PageDown => KeyCode::PageDown,
        WinitKeyCode::Delete => KeyCode::Delete,
        WinitKeyCode::Insert => KeyCode::Insert,
        WinitKeyCode::F1 => KeyCode::F(1),
        WinitKeyCode::F2 => KeyCode::F(2),
        WinitKeyCode::F3 => KeyCode::F(3),
        WinitKeyCode::F4 => KeyCode::F(4),
        WinitKeyCode::F5 => KeyCode::F(5),
        WinitKeyCode::F6 => KeyCode::F(6),
        WinitKeyCode::F7 => KeyCode::F(7),
        WinitKeyCode::F8 => KeyCode::F(8),
        WinitKeyCode::F9 => KeyCode::F(9),
        WinitKeyCode::F10 => KeyCode::F(10),
        WinitKeyCode::F11 => KeyCode::F(11),
        WinitKeyCode::F12 => KeyCode::F(12),
        WinitKeyCode::Space => KeyCode::Char(' '),
        WinitKeyCode::KeyA => KeyCode::Char('a'),
        WinitKeyCode::KeyB => KeyCode::Char('b'),
        WinitKeyCode::KeyC => KeyCode::Char('c'),
        WinitKeyCode::KeyD => KeyCode::Char('d'),
        WinitKeyCode::KeyE => KeyCode::Char('e'),
        WinitKeyCode::KeyF => KeyCode::Char('f'),
        WinitKeyCode::KeyG => KeyCode::Char('g'),
        WinitKeyCode::KeyH => KeyCode::Char('h'),
        WinitKeyCode::KeyI => KeyCode::Char('i'),
        WinitKeyCode::KeyJ => KeyCode::Char('j'),
        WinitKeyCode::KeyK => KeyCode::Char('k'),
        WinitKeyCode::KeyL => KeyCode::Char('l'),
        WinitKeyCode::KeyM => KeyCode::Char('m'),
        WinitKeyCode::KeyN => KeyCode::Char('n'),
        WinitKeyCode::KeyO => KeyCode::Char('o'),
        WinitKeyCode::KeyP => KeyCode::Char('p'),
        WinitKeyCode::KeyQ => KeyCode::Char('q'),
        WinitKeyCode::KeyR => KeyCode::Char('r'),
        WinitKeyCode::KeyS => KeyCode::Char('s'),
        WinitKeyCode::KeyT => KeyCode::Char('t'),
        WinitKeyCode::KeyU => KeyCode::Char('u'),
        WinitKeyCode::KeyV => KeyCode::Char('v'),
        WinitKeyCode::KeyW => KeyCode::Char('w'),
        WinitKeyCode::KeyX => KeyCode::Char('x'),
        WinitKeyCode::KeyY => KeyCode::Char('y'),
        WinitKeyCode::KeyZ => KeyCode::Char('z'),
        WinitKeyCode::Digit0 => KeyCode::Char('0'),
        WinitKeyCode::Digit1 => KeyCode::Char('1'),
        WinitKeyCode::Digit2 => KeyCode::Char('2'),
        WinitKeyCode::Digit3 => KeyCode::Char('3'),
        WinitKeyCode::Digit4 => KeyCode::Char('4'),
        WinitKeyCode::Digit5 => KeyCode::Char('5'),
        WinitKeyCode::Digit6 => KeyCode::Char('6'),
        WinitKeyCode::Digit7 => KeyCode::Char('7'),
        WinitKeyCode::Digit8 => KeyCode::Char('8'),
        WinitKeyCode::Digit9 => KeyCode::Char('9'),
        WinitKeyCode::Minus => KeyCode::Char('-'),
        WinitKeyCode::Equal => KeyCode::Char('='),
        WinitKeyCode::BracketLeft => KeyCode::Char('['),
        WinitKeyCode::BracketRight => KeyCode::Char(']'),
        WinitKeyCode::Backslash => KeyCode::Char('\\'),
        WinitKeyCode::Semicolon => KeyCode::Char(';'),
        WinitKeyCode::Quote => KeyCode::Char('\''),
        WinitKeyCode::Backquote => KeyCode::Char('`'),
        WinitKeyCode::Comma => KeyCode::Char(','),
        WinitKeyCode::Period => KeyCode::Char('.'),
        WinitKeyCode::Slash => KeyCode::Char('/'),
        _ => return None,
    })
}

// Tests live in the gate-exempt sibling module (repo test-module
// convention, pty/tests.rs precedent) so inline test lines stay out of
// the production-file budget.
#[cfg(test)]
#[path = "event_replay/tests.rs"]
mod tests;
