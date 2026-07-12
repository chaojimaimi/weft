//! Cohesive UI state groups extracted from the application shell.
//!
//! These types own their reset invariants so tab/panel lifecycle code no
//! longer edits a collection of unrelated `App` fields individually.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use weft_core::blocks::BlockId;
use weft_core::config::Config;
use weft_core::find::{BlockMatch, FindMatch};
use weft_core::persistence::BlockStore;

use crate::find_worker::FindWorker;
use crate::overlay::SettingsTab;
use crate::tab::Tab;

pub struct SessionState {
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
    pub prev_drawn_tab: usize,
    pub block_store: Option<BlockStore>,
}

impl SessionState {
    pub fn new() -> Self {
        Self {
            tabs: Vec::new(),
            active_tab: 0,
            prev_drawn_tab: 0,
            block_store: None,
        }
    }
}

pub struct ConfigState {
    pub config: Config,
    pub keybindings: weft_core::config::KeyBindings,
    pub path_bins: Vec<String>,
    pub theme_is_dark: bool,
    pub preferred_dark_theme: String,
    pub font_scale: f32,
}

impl ConfigState {
    pub fn new(config: Config, path_bins: Vec<String>) -> Self {
        let preferred_dark_theme =
            if !config.theme.name.contains("light") && !config.theme.name.is_empty() {
                config.theme.name.clone()
            } else {
                config
                    .theme
                    .dark_name
                    .clone()
                    .unwrap_or_else(|| "weft-warm".into())
            };
        let keybindings = config.keybindings();
        Self {
            config,
            keybindings,
            path_bins,
            theme_is_dark: true,
            preferred_dark_theme,
            font_scale: 1.0,
        }
    }
}

pub struct WindowRuntimeState {
    pub cursor_blink_on: bool,
    pub cursor_blink_phase: f32,
    pub cursor_blink_time: Instant,
    pub cursor_anim_active: Arc<AtomicBool>,
    pub last_resize_instant: Instant,
    pub last_system_appearance_dark: Option<bool>,
    pub last_appearance_check: Instant,
    pub current_logo_variant: weft_core::config::LogoVariant,
}

impl WindowRuntimeState {
    pub fn new() -> Self {
        Self {
            cursor_blink_on: true,
            cursor_blink_phase: 0.0,
            cursor_blink_time: Instant::now(),
            cursor_anim_active: Arc::new(AtomicBool::new(true)),
            last_resize_instant: Instant::now(),
            last_system_appearance_dark: None,
            last_appearance_check: Instant::now(),
            current_logo_variant: weft_core::config::LogoVariant::Cool,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DragTarget {
    Right,
    Top,
}

#[derive(Clone)]
pub struct DragState {
    pub target: DragTarget,
    pub start_x: f64,
    pub start_y: f64,
    pub start_scale: f32,
    pub start_rows: usize,
    #[allow(dead_code)]
    pub cell_w: f32,
    pub cell_h: f32,
}

#[allow(dead_code)]
pub struct ContextMenu {
    pub block_id: Option<BlockId>,
    pub x: f32,
    pub y: f32,
    pub selection: usize,
}

pub struct InteractionState {
    pub mods: winit::event::Modifiers,
    pub last_mouse_x: f64,
    pub last_mouse_y: f64,
    pub prompt_dragging: bool,
    pub popup_width_scale: f32,
    pub popup_max_rows: usize,
    pub drag_state: Option<DragState>,
    pub scrollbar_drag: Option<crate::scrollbar_component::ScrollbarDragState>,
    pub scrollbar_hovered: bool,
    pub context_menu: Option<ContextMenu>,
}

impl InteractionState {
    pub fn new() -> Self {
        Self {
            mods: winit::event::Modifiers::default(),
            last_mouse_x: 0.0,
            last_mouse_y: 0.0,
            prompt_dragging: false,
            popup_width_scale: 0.6,
            popup_max_rows: 8,
            drag_state: None,
            scrollbar_drag: None,
            scrollbar_hovered: false,
            context_menu: None,
        }
    }
}

#[derive(Debug, Default)]
pub struct TabBarState {
    pub hovered_tab: Option<usize>,
    pub scroll_offset: f32,
    pub plus_hovered: bool,
    pub arrow_left_hovered: bool,
    pub arrow_right_hovered: bool,
    pub last_titlebar_click: Option<Instant>,
}

impl TabBarState {
    pub fn clear_hover(&mut self) {
        self.hovered_tab = None;
        self.plus_hovered = false;
        self.arrow_left_hovered = false;
        self.arrow_right_hovered = false;
    }
}

#[derive(Debug, Default)]
pub struct PanelState {
    pub open: bool,
    pub query: String,
    pub selection: usize,
    pub expanded: Option<BlockId>,
    pub search_focused: bool,
    pub highlight: Option<BlockId>,
    pub highlight_until: Option<Instant>,
    pub last_click: Option<(Instant, usize)>,
}

impl PanelState {
    pub fn close(&mut self) {
        self.open = false;
        self.search_focused = false;
        self.highlight = None;
        self.highlight_until = None;
        self.last_click = None;
    }

    pub fn clear_transient_selection(&mut self) {
        self.selection = 0;
        self.expanded = None;
        self.highlight = None;
        self.highlight_until = None;
        self.last_click = None;
    }
}

pub struct FindState {
    pub open: bool,
    pub query: String,
    pub last_key: Option<Instant>,
    pub matches: Vec<FindMatch>,
    pub index: usize,
    pub truncated: bool,
    pub block_matches: Vec<BlockMatch>,
    pub block_index: usize,
    pub block_truncated: bool,
    pub regex_mode: bool,
    pub case_sensitive: bool,
    pub worker: FindWorker,
    pub regex_error: Option<String>,
    pub worker_busy: bool,
}

impl FindState {
    pub fn new() -> Self {
        Self {
            open: false,
            query: String::new(),
            last_key: None,
            matches: Vec::new(),
            index: 0,
            truncated: false,
            block_matches: Vec::new(),
            block_index: 0,
            block_truncated: false,
            regex_mode: false,
            case_sensitive: false,
            worker: FindWorker::spawn(),
            regex_error: None,
            worker_busy: false,
        }
    }

    pub fn reset_query(&mut self) {
        self.query.clear();
        self.last_key = None;
        self.matches.clear();
        self.index = 0;
        self.truncated = false;
        self.block_matches.clear();
        self.block_index = 0;
        self.block_truncated = false;
        self.regex_mode = false;
        self.case_sensitive = false;
        self.regex_error = None;
        self.worker_busy = false;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.reset_query();
    }
}

pub struct SettingsState {
    pub open: bool,
    pub tab: SettingsTab,
    pub selection: usize,
    pub scroll_offset: usize,
    pub draft: Config,
    pub dirty: bool,
    pub error: Option<String>,
}

impl SettingsState {
    pub fn new() -> Self {
        Self {
            open: false,
            tab: SettingsTab::Appearance,
            selection: 0,
            scroll_offset: 0,
            draft: Config::default(),
            dirty: false,
            error: None,
        }
    }

    pub fn open_from(&mut self, config: &Config) {
        self.open = true;
        self.tab = SettingsTab::Appearance;
        self.selection = 0;
        self.scroll_offset = 0;
        self.draft = config.clone();
        self.dirty = false;
        self.error = None;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.dirty = false;
        self.error = None;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConfigState, FindState, InteractionState, PanelState, SessionState, SettingsState,
        TabBarState, WindowRuntimeState,
    };
    use std::time::Instant;
    use weft_core::config::Config;

    #[test]
    fn tab_bar_clear_hover_preserves_scroll_and_click_history() {
        let now = Instant::now();
        let mut state = TabBarState {
            hovered_tab: Some(2),
            scroll_offset: 42.0,
            plus_hovered: true,
            arrow_left_hovered: true,
            arrow_right_hovered: true,
            last_titlebar_click: Some(now),
        };
        state.clear_hover();
        assert_eq!(state.hovered_tab, None);
        assert!(!state.plus_hovered);
        assert!(!state.arrow_left_hovered);
        assert!(!state.arrow_right_hovered);
        assert_eq!(state.scroll_offset, 42.0);
        assert_eq!(state.last_titlebar_click, Some(now));
    }

    #[test]
    fn closing_panel_clears_only_transient_ownership() {
        let mut state = PanelState {
            open: true,
            query: "git".into(),
            selection: 3,
            search_focused: true,
            last_click: Some((Instant::now(), 3)),
            ..PanelState::default()
        };
        state.close();
        assert!(!state.open);
        assert!(!state.search_focused);
        assert!(state.last_click.is_none());
        assert_eq!(state.query, "git");
        assert_eq!(state.selection, 3);
    }

    #[test]
    fn find_close_clears_all_query_ownership() {
        let mut state = FindState::new();
        state.open = true;
        state.query = "needle".into();
        state.regex_mode = true;
        state.case_sensitive = true;
        state.worker_busy = true;
        state.regex_error = Some("bad regex".into());
        state.close();
        assert!(!state.open);
        assert!(state.query.is_empty());
        assert!(!state.regex_mode);
        assert!(!state.case_sensitive);
        assert!(!state.worker_busy);
        assert!(state.regex_error.is_none());
    }

    #[test]
    fn settings_open_uses_fresh_config_and_close_discards_transient_flags() {
        let mut config = Config::default();
        config.font.size = 19.0;
        let mut state = SettingsState::new();
        state.open_from(&config);
        assert!(state.open);
        assert_eq!(state.draft.font.size, 19.0);
        state.dirty = true;
        state.error = Some("save failed".into());
        state.close();
        assert!(!state.open);
        assert!(!state.dirty);
        assert!(state.error.is_none());
    }

    #[test]
    fn interaction_defaults_keep_popup_within_supported_bounds() {
        let state = InteractionState::new();
        assert!((0.3..=0.95).contains(&state.popup_width_scale));
        assert!((3..=20).contains(&state.popup_max_rows));
        assert!(state.drag_state.is_none());
        assert!(state.context_menu.is_none());
    }

    #[test]
    fn window_runtime_starts_with_visible_cursor_animation() {
        let state = WindowRuntimeState::new();
        assert!(state.cursor_blink_on);
        assert_eq!(state.cursor_blink_phase, 0.0);
        assert!(state
            .cursor_anim_active
            .load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn session_state_starts_without_an_invalid_active_tab() {
        let state = SessionState::new();
        assert!(state.tabs.is_empty());
        assert_eq!(state.active_tab, 0);
        assert_eq!(state.prev_drawn_tab, 0);
    }

    #[test]
    fn config_state_preserves_preferred_dark_theme() {
        let mut config = Config::default();
        config.theme.name = "solarized-dark".into();
        let state = ConfigState::new(config, vec!["cargo".into()]);
        assert_eq!(state.preferred_dark_theme, "solarized-dark");
        assert_eq!(state.path_bins, ["cargo"]);
        assert_eq!(state.font_scale, 1.0);
    }
}
