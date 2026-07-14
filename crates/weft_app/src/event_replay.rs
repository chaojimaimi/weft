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
            if context.owner.is_some() {
                vec![ImeRoutingAction::ClearActivePreedit]
            } else {
                vec![ImeRoutingAction::SetActivePreedit { text, cursor }]
            }
        }
        ImeInput::Commit(text) => {
            let mut actions = vec![ImeRoutingAction::ClearActivePreedit];
            if text.is_empty() {
                return actions;
            }
            let target = match context.owner {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect::{ime_commit_effects, Effect};
    use crate::input_router::{OverlayInputContext, OverlayInputOwner};
    use crate::paint::command_surface::{
        compute_current_focus, resolve_command_surface_key, save_focus_for_modal,
        CommandSurfaceKeyAction,
    };
    use crate::scene::FocusId;
    use weft_core::input::Modifiers;

    #[derive(Default)]
    struct ReplayState {
        tabs: Vec<Tab>,
        active_tab: usize,
        palette: String,
        find: String,
        panel: String,
        editor_commits: Vec<(usize, String)>,
        effects: Vec<Effect>,
        native_marked_text: bool,
    }

    impl ReplayState {
        fn with_tabs(count: usize) -> Self {
            Self {
                tabs: (0..count).map(|_| Tab::empty()).collect(),
                ..Self::default()
            }
        }

        fn dispatch(&mut self, input: ImeInput, owner: Option<OverlayInputOwner>, mode: InputMode) {
            match &input {
                ImeInput::Preedit { text, .. } => self.native_marked_text = !text.is_empty(),
                ImeInput::Commit(_) | ImeInput::Disabled => self.native_marked_text = false,
                ImeInput::Enabled => {}
            }
            let actions = route_ime_input(
                input,
                ImeRouteContext {
                    owner,
                    input_mode: mode,
                    active_tab: self.active_tab,
                },
            );
            for action in actions {
                match action {
                    ImeRoutingAction::ClearActivePreedit => {
                        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                            tab.ime_preedit.clear();
                            tab.ime_preedit_cursor = None;
                        }
                    }
                    ImeRoutingAction::SetActivePreedit { text, cursor } => {
                        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                            tab.ime_preedit = text;
                            tab.ime_preedit_cursor = cursor;
                        }
                    }
                    ImeRoutingAction::ClearAllPreedit => clear_all_preedit(&mut self.tabs),
                    ImeRoutingAction::Commit { target, text } => match target {
                        ImeCommitTarget::Palette => self.palette.push_str(&text),
                        ImeCommitTarget::SettingsConsumed
                        | ImeCommitTarget::ContextMenuConsumed => {}
                        ImeCommitTarget::Find => self.find.push_str(&text),
                        ImeCommitTarget::PanelSearch => self.panel.push_str(&text),
                        ImeCommitTarget::Editor { tab } => {
                            if self.tabs.get(tab).is_some() {
                                self.editor_commits.push((tab, text));
                            }
                        }
                        ImeCommitTarget::Pty { tab } => {
                            let tab_exists = self.tabs.get(tab).is_some();
                            self.effects
                                .extend(ime_commit_effects(tab, &text).into_iter().filter(
                                    |effect| {
                                        tab_exists || !matches!(effect, Effect::WritePty { .. })
                                    },
                                ));
                        }
                    },
                }
            }
        }

        fn switch_tab(&mut self, tab: usize) {
            self.reset_ime_context();
            self.active_tab = tab;
        }

        fn reset_ime_context(&mut self) {
            for action in reset_ime_context_actions() {
                match action {
                    ImeContextResetAction::ClearAllPreedit => clear_all_preedit(&mut self.tabs),
                    ImeContextResetAction::DiscardNativeMarkedText => {
                        self.native_marked_text = false;
                    }
                }
            }
        }

        fn deliver_pending_native_commit(
            &mut self,
            text: &str,
            owner: Option<OverlayInputOwner>,
            mode: InputMode,
        ) {
            if self.native_marked_text {
                self.dispatch(ImeInput::Commit(text.into()), owner, mode);
            }
        }
    }

    #[test]
    fn ime_preedit_cannot_cross_a_tab_switch_and_fresh_commit_targets_new_tab_once() {
        let mut replay = ReplayState::with_tabs(2);
        replay.dispatch(
            ImeInput::Preedit {
                text: "旧组合".into(),
                cursor: Some((3, 6)),
            },
            None,
            InputMode::Passthrough,
        );
        assert_eq!(replay.tabs[0].ime_preedit, "旧组合");

        replay.switch_tab(1);
        assert!(replay.tabs.iter().all(|tab| tab.ime_preedit.is_empty()));
        replay.dispatch(
            ImeInput::Commit("新输入".into()),
            None,
            InputMode::Passthrough,
        );
        assert_eq!(
            replay.effects,
            [
                Effect::WritePty {
                    tab: 1,
                    bytes: "新输入".as_bytes().to_vec(),
                },
                Effect::RequestRedraw,
            ]
        );
    }

    #[test]
    fn ime_owner_sequence_never_leaks_overlay_commits_to_pty() {
        let mut replay = ReplayState::with_tabs(1);
        for (owner, text) in [
            (OverlayInputOwner::Palette, "命令"),
            (OverlayInputOwner::Settings, "忽略"),
            (OverlayInputOwner::Find, "查找"),
            (OverlayInputOwner::ContextMenu, "菜单忽略"),
            (OverlayInputOwner::PanelSearch, "历史"),
        ] {
            replay.dispatch(
                ImeInput::Preedit {
                    text: text.into(),
                    cursor: None,
                },
                Some(owner),
                InputMode::Editor,
            );
            replay.dispatch(
                ImeInput::Commit(text.into()),
                Some(owner),
                InputMode::Editor,
            );
        }
        assert_eq!(replay.palette, "命令");
        assert_eq!(replay.find, "查找");
        assert_eq!(replay.panel, "历史");
        assert!(replay.effects.is_empty());
        assert!(replay.editor_commits.is_empty());
    }

    #[test]
    fn modal_close_discards_marked_text_before_focus_returns_to_pty() {
        for owner in [OverlayInputOwner::Settings, OverlayInputOwner::ContextMenu] {
            let mut replay = ReplayState::with_tabs(1);
            replay.dispatch(
                ImeInput::Preedit {
                    text: "未完成".into(),
                    cursor: Some((0, 3)),
                },
                Some(owner),
                InputMode::Passthrough,
            );
            assert!(replay.native_marked_text);

            replay.reset_ime_context();
            replay.deliver_pending_native_commit("不得泄漏", None, InputMode::Passthrough);

            assert!(!replay.native_marked_text);
            assert!(replay.effects.is_empty(), "owner={owner:?}");
        }
    }

    #[test]
    fn disabled_event_clears_preedit_from_every_tab() {
        let mut replay = ReplayState::with_tabs(3);
        for (index, tab) in replay.tabs.iter_mut().enumerate() {
            tab.ime_preedit = format!("tab-{index}");
            tab.ime_preedit_cursor = Some((0, 1));
        }
        replay.dispatch(ImeInput::Disabled, None, InputMode::Editor);
        assert!(replay
            .tabs
            .iter()
            .all(|tab| tab.ime_preedit.is_empty() && tab.ime_preedit_cursor.is_none()));
    }

    #[test]
    fn empty_session_replay_accepts_every_ime_phase_without_indexing_a_tab() {
        let mut replay = ReplayState::with_tabs(0);
        replay.dispatch(ImeInput::Enabled, None, InputMode::Passthrough);
        replay.dispatch(
            ImeInput::Preedit {
                text: "组合".into(),
                cursor: Some((0, 3)),
            },
            None,
            InputMode::Passthrough,
        );
        replay.dispatch(
            ImeInput::Commit("输入".into()),
            None,
            InputMode::Passthrough,
        );
        replay.dispatch(ImeInput::Disabled, None, InputMode::Passthrough);
        assert!(replay.tabs.is_empty());
        assert_eq!(replay.effects, [Effect::RequestRedraw]);
    }

    #[test]
    fn empty_commit_only_clears_preedit_and_editor_commit_keeps_tab_identity() {
        let mut replay = ReplayState::with_tabs(2);
        replay.active_tab = 1;
        replay.tabs[1].ime_preedit = "pending".into();
        replay.dispatch(ImeInput::Commit(String::new()), None, InputMode::Editor);
        assert!(replay.tabs[1].ime_preedit.is_empty());
        assert!(replay.editor_commits.is_empty());
        assert!(replay.effects.is_empty());

        replay.dispatch(ImeInput::Commit("编辑器".into()), None, InputMode::Editor);
        assert_eq!(replay.editor_commits, [(1, "编辑器".into())]);
        assert!(replay.effects.is_empty());
    }

    #[test]
    fn modal_focus_and_keyboard_trace_follow_one_priority_contract() {
        let panel_focus = compute_current_focus(false, false, false, false, true, true, false, 2);
        let saved = save_focus_for_modal(panel_focus, None, FocusId::Settings);
        assert_eq!(saved, Some(FocusId::SidebarSearch));

        let owner = OverlayInputOwner::resolve(OverlayInputContext {
            palette_open: true,
            settings_open: true,
            find_open: true,
            context_menu_open: true,
            panel_search_focused: true,
        });
        assert_eq!(owner, Some(OverlayInputOwner::Palette));
        assert_eq!(
            save_focus_for_modal(Some(FocusId::Settings), saved, FocusId::PaletteQuery),
            Some(FocusId::SidebarSearch)
        );

        let trace = [
            (KeyCode::Down, Modifiers::empty()),
            (KeyCode::PageDown, Modifiers::empty()),
            (KeyCode::Tab, Modifiers::empty()),
            (KeyCode::Tab, Modifiers::SHIFT),
            (KeyCode::Enter, Modifiers::empty()),
            (KeyCode::Escape, Modifiers::empty()),
        ]
        .map(|(key, modifiers)| resolve_command_surface_key(key, modifiers));
        assert_eq!(
            trace,
            [
                CommandSurfaceKeyAction::MoveDown,
                CommandSurfaceKeyAction::PageDown,
                CommandSurfaceKeyAction::CycleFocus,
                CommandSurfaceKeyAction::CycleFocus,
                CommandSurfaceKeyAction::Accept,
                CommandSurfaceKeyAction::Cancel,
            ]
        );
    }

    #[test]
    fn native_key_mapping_replays_function_and_search_keys_without_text_guessing() {
        let cases = [
            (WinitKeyCode::Enter, KeyCode::Enter),
            (WinitKeyCode::ArrowUp, KeyCode::Up),
            (WinitKeyCode::PageDown, KeyCode::PageDown),
            (WinitKeyCode::F12, KeyCode::F(12)),
            (WinitKeyCode::Slash, KeyCode::Char('/')),
            (WinitKeyCode::Semicolon, KeyCode::Char(';')),
            (WinitKeyCode::KeyZ, KeyCode::Char('z')),
        ];
        for (native, expected) in cases {
            assert_eq!(map_winit_key(native), Some(expected));
        }
        assert_eq!(map_winit_key(WinitKeyCode::AudioVolumeUp), None);
    }
}
