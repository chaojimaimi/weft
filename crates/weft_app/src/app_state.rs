//! Cohesive UI state groups extracted from the application shell.
//!
//! These types own their reset invariants so tab/panel lifecycle code no
//! longer edits a collection of unrelated `App` fields individually.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use weft_core::blocks::BlockId;
use weft_core::config::{Config, ConfigSectionMask};
use weft_core::find::{BlockMatch, FindMatch};
use weft_core::persistence::BlockStore;

use crate::find_worker::FindWorker;
use crate::overlay::SettingsTab;
use crate::tab::Tab;

/// M3: Encapsulated tab lifecycle owner. Controllers go through accessor
/// methods (`active()`, `active_mut()`, `tab(idx)`, …) and lifecycle methods
/// (`open_tab`, `switch_to`, `next`, `prev`, `close_active`, …) instead of
/// reaching into `tabs` / `active_tab` directly. The collection is the
/// encapsulation boundary; individual `Tab` fields stay public.
pub struct SessionManager {
    tabs: Vec<Tab>,
    active_tab: usize,
    prev_drawn_tab: usize,
    block_store: Option<BlockStore>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            tabs: Vec::new(),
            active_tab: 0,
            prev_drawn_tab: 0,
            block_store: None,
        }
    }

    // ── Read accessors ────────────────────────────────────────────────

    pub fn active(&self) -> &Tab {
        &self.tabs[self.active_tab]
    }

    pub fn active_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active_tab]
    }

    pub fn active_idx(&self) -> usize {
        self.active_tab
    }

    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    pub fn tabs_mut(&mut self) -> &mut [Tab] {
        &mut self.tabs
    }

    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    pub fn block_store(&self) -> Option<&BlockStore> {
        self.block_store.as_ref()
    }

    pub fn set_block_store(&mut self, store: Option<BlockStore>) {
        self.block_store = store;
    }

    pub fn prev_drawn_tab(&self) -> usize {
        self.prev_drawn_tab
    }

    pub fn set_prev_drawn_tab(&mut self, idx: usize) {
        self.prev_drawn_tab = idx;
    }

    pub fn tab(&self, idx: usize) -> Option<&Tab> {
        self.tabs.get(idx)
    }

    pub fn tab_mut(&mut self, idx: usize) -> Option<&mut Tab> {
        self.tabs.get_mut(idx)
    }

    pub fn tab_index_by_session_id(&self, session_id: u64) -> Option<usize> {
        self.tabs
            .iter()
            .position(|tab| tab.session_id == session_id)
    }

    // ── Tab lifecycle ─────────────────────────────────────────────────

    /// Open a new tab and make it active. Returns the new tab index.
    pub fn open_tab(
        &mut self,
        rows: usize,
        cols: usize,
        scrollback: usize,
        proxy: &winit::event_loop::EventLoopProxy<crate::AppEvent>,
        cwd: Option<&str>,
    ) -> usize {
        let tab = Tab::new(rows, cols, scrollback, proxy, cwd);
        self.tabs.push(tab);
        let idx = self.tabs.len() - 1;
        self.active_tab = idx;
        idx
    }

    /// Switch to tab at `idx` (clamped). Returns `(new_idx, prev_idx)` so
    /// callers can run post-switch hooks (IME reset, find refresh).
    pub fn switch_to(&mut self, idx: usize) -> (usize, usize) {
        let prev = self.active_tab;
        let new = idx.min(self.tabs.len().saturating_sub(1));
        self.active_tab = new;
        (new, prev)
    }

    /// Switch to next tab (wraps around). Returns `(new_idx, prev_idx)`.
    pub fn next(&mut self) -> (usize, usize) {
        let n = self.tabs.len();
        if n <= 1 {
            return (self.active_tab, self.active_tab);
        }
        let prev = self.active_tab;
        self.active_tab = (self.active_tab + 1) % n;
        (self.active_tab, prev)
    }

    /// Switch to previous tab (wraps around). Returns `(new_idx, prev_idx)`.
    pub fn prev(&mut self) -> (usize, usize) {
        let n = self.tabs.len();
        if n <= 1 {
            return (self.active_tab, self.active_tab);
        }
        let prev = self.active_tab;
        self.active_tab = (self.active_tab + n - 1) % n;
        (self.active_tab, prev)
    }

    /// Close the active tab. Returns `is_last` so the caller can emit Exit.
    /// After close, `active_tab` moves to the previous tab (wrapping to the
    /// last tab when the first is closed), matching the original close_tab UX.
    pub fn close_active(&mut self) -> bool {
        if self.tabs.is_empty() {
            return true;
        }
        self.tabs.remove(self.active_tab);
        if self.tabs.is_empty() {
            self.active_tab = 0;
            return true;
        }
        if self.active_tab > 0 {
            self.active_tab -= 1;
        } else {
            self.active_tab = self.tabs.len() - 1;
        }
        false
    }

    /// Close a background tab at `idx`. Returns `is_last`. If the closed
    /// tab was before `active_tab`, adjust `active_tab` down.
    pub fn close_background(&mut self, idx: usize) -> bool {
        if idx >= self.tabs.len() {
            return self.tabs.is_empty();
        }
        self.tabs.remove(idx);
        if self.tabs.is_empty() {
            self.active_tab = 0;
            return true;
        }
        if idx < self.active_tab {
            self.active_tab -= 1;
        } else if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        }
        false
    }

    /// Remove a dead tab (shell exited) at `idx`. Same as `close_background`
    /// but semantically distinct for future cleanup hooks.
    pub fn remove_dead(&mut self, idx: usize) -> bool {
        self.close_background(idx)
    }

    // ── Restore path ──────────────────────────────────────────────────

    /// Push a pre-built tab (restore path). Does NOT change `active_tab`.
    pub fn push_tab(&mut self, tab: Tab) {
        self.tabs.push(tab);
    }

    /// Replace the tab at `idx` (restore first-tab rebuild). No-op when the
    /// index is out of bounds.
    pub fn replace_tab(&mut self, idx: usize, tab: Tab) {
        if idx < self.tabs.len() {
            self.tabs[idx] = tab;
        }
    }

    /// Set active tab (restore completion). Clamped to the last valid index.
    pub fn set_active(&mut self, idx: usize) {
        self.active_tab = idx.min(self.tabs.len().saturating_sub(1));
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

// v1.5.0: ConfigState moved to `config_state.rs` so the new profile fields
// (source_config, config_fingerprint) don't push this file past its
// architecture-gate ceiling. Re-exported here so the 48 existing
// `config_state.config.<field>` read sites keep compiling unchanged.
pub use crate::config_state::ConfigState;

pub struct WindowRuntimeState {
    pub cursor_blink_on: bool,
    pub cursor_blink_phase: f32,
    pub cursor_blink_time: Instant,
    pub cursor_anim_active: Arc<AtomicBool>,
    pub synchronized_output_watchdog_pending: Arc<AtomicBool>,
    pub last_resize_instant: Instant,
    pub last_system_appearance_dark: Option<bool>,
    pub last_appearance_check: Instant,
    pub current_logo_variant: weft_core::config::LogoVariant,
    /// F3-2: Spinner animation phase in [0, 1). Advances ~every 80ms while a
    /// command is running (CommandExecuting). The renderer maps it to a braille
    /// spinner glyph. Driven by a dedicated wake timer (see `spinner_anim_active`).
    pub spinner_phase: f32,
    /// F3-2: When true, a dedicated timer wakes the loop ~every 80ms so the
    /// running-command spinner animates even when no PTY output is streaming.
    pub spinner_anim_active: Arc<AtomicBool>,
    /// F3-2: Last time the spinner phase was advanced. Anchors the phase
    /// computation to real elapsed time (frame-count independent).
    pub spinner_time: Instant,
    /// F3-2: macOS Reduce Motion setting. When true, the spinner is replaced
    /// by a static `●` indicator. Polled alongside system appearance (1Hz).
    pub reduce_motion: bool,
    /// F6: macOS Increase Contrast accessibility setting. When true, the
    /// renderer strengthens borders, selection highlights and focus rings so
    /// state is perceivable without relying on subtle color differences.
    /// Polled alongside system appearance (1Hz).
    pub increase_contrast: bool,
    pub tab_snapshots: crate::snapshot_persistence::SnapshotPersistenceState,
}

impl WindowRuntimeState {
    pub fn new() -> Self {
        Self {
            cursor_blink_on: true,
            cursor_blink_phase: 0.0,
            cursor_blink_time: Instant::now(),
            cursor_anim_active: Arc::new(AtomicBool::new(true)),
            synchronized_output_watchdog_pending: Arc::new(AtomicBool::new(false)),
            last_resize_instant: Instant::now(),
            last_system_appearance_dark: None,
            last_appearance_check: Instant::now(),
            current_logo_variant: weft_core::config::LogoVariant::Cool,
            spinner_phase: 0.0,
            spinner_anim_active: Arc::new(AtomicBool::new(false)),
            spinner_time: Instant::now(),
            reduce_motion: false,
            increase_contrast: false,
            tab_snapshots: crate::snapshot_persistence::SnapshotPersistenceState::default(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DragTarget {
    Right,
    Top,
}

/// F3-3: Sidebar resize drag state. Captured at drag start so the new width
/// can be computed from the pointer delta without accumulating rounding error.
#[derive(Clone, Copy)]
pub struct SidebarDragState {
    /// Physical X where the drag started.
    pub start_x: f64,
    /// Logical sidebar width (points) at drag start.
    pub start_width: f32,
}

/// v1.3.2: Pane divider resize drag state. Stashed at drag start so each
/// mouse-move can compute the new ratio from the pointer position and the
/// split's bounds (stored here) without re-deriving which divider was grabbed.
#[derive(Clone, Copy)]
pub struct PaneDividerDragState {
    /// The divider axis (Vertical → left/right panes; Horizontal → top/bottom).
    pub axis: crate::paint::pane_dividers::DividerAxis,
    /// The first child (top/left) of the split being resized.
    pub first: weft_core::pane_layout::PaneId,
    /// The second child (bottom/right) of the split being resized.
    pub second: weft_core::pane_layout::PaneId,
    /// The union rect of the two panes the divider separates. Used to compute
    /// `new_ratio = (pointer - bounds[0]) / (bounds[2] - bounds[0])`.
    pub bounds: crate::layout::Rect,
}

#[derive(Clone)]
pub struct DragState {
    pub target: DragTarget,
    pub start_x: f64,
    pub start_y: f64,
    pub start_scale: f32,
    pub start_rows: usize,
    pub cell_h: f32,
}

pub struct ContextMenu {
    pub session_id: u64,
    pub block_id: Option<BlockId>,
    pub x: f32,
    pub y: f32,
    pub selection: usize,
}

impl ContextMenu {
    pub fn belongs_to_session(&self, session_id: u64) -> bool {
        self.session_id == session_id
    }
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
    pub panel_scrollbar_drag: Option<crate::panel_scrollbar::PanelScrollbarDragState>,
    pub scrollbar_hovered: bool,
    pub precise_scroll: crate::scroll_input::PreciseScrollAccumulator,
    pub modal_mouse_capture: crate::input_router::ModalMouseCapture,
    pub context_menu: Option<ContextMenu>,
    /// F3-1: Block currently hovered by the mouse in the block view, if any.
    /// Drives the inline copy/fold action buttons rendered on the header row.
    pub block_hovered: Option<BlockId>,
    /// F3-3: Active sidebar resize drag. Set on press at the sidebar's right
    /// edge; cleared on release (which persists the width to config).
    pub sidebar_drag: Option<SidebarDragState>,
    /// v1.3.2: Active pane divider resize drag. Set on press at a pane
    /// divider; cleared on release. Not persisted (ratio is in-memory only).
    pub pane_divider_drag: Option<PaneDividerDragState>,
    /// F4: FocusId of the element that had keyboard focus before a modal
    /// surface (Palette/Find/Settings/ContextMenu) opened. Used to restore
    /// focus (visually / for accessibility) when the modal closes. `None`
    /// when no modal is open or the focus was already on the modal target.
    pub prev_focus: Option<crate::scene::FocusId>,
    /// F6: Scope stack tracking which [`FocusScope`](crate::scene::FocusScope)
    /// the keyboard focus currently belongs to. Pushed when a modal opens,
    /// popped when it closes, so Tab/Shift+Tab cycles only within the active
    /// scope. The last element is the current scope; an empty stack means
    /// Terminal (the default scope).
    #[allow(dead_code)] // F6: scaffolding; wired into the renderer in a follow-up
    pub focus_stack: Vec<crate::scene::FocusScope>,
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
            panel_scrollbar_drag: None,
            scrollbar_hovered: false,
            precise_scroll: crate::scroll_input::PreciseScrollAccumulator::default(),
            modal_mouse_capture: crate::input_router::ModalMouseCapture::default(),
            context_menu: None,
            block_hovered: None,
            sidebar_drag: None,
            pane_divider_drag: None,
            prev_focus: None,
            focus_stack: Vec::new(),
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
    /// F3-4: Block-level scroll offset for the sidebar history list (number
    /// of filtered blocks skipped from the newest end). 0 = newest visible.
    /// Clamped to `[0, total_filtered - visible_blocks]`.
    pub scroll_offset: usize,
}

impl PanelState {
    pub fn close(&mut self) {
        self.open = false;
        self.search_focused = false;
        self.highlight = None;
        self.highlight_until = None;
        self.last_click = None;
        self.scroll_offset = 0;
    }

    pub fn clear_transient_selection(&mut self) {
        self.selection = 0;
        self.expanded = None;
        self.highlight = None;
        self.highlight_until = None;
        self.last_click = None;
        self.scroll_offset = 0;
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
    pub worker_generation: u64,
}

impl FindState {
    pub fn new(proxy: winit::event_loop::EventLoopProxy<crate::AppEvent>) -> Self {
        Self::new_with_waker(Arc::new(move || {
            let _ = proxy.send_event(crate::AppEvent::Wake);
        }))
    }

    fn new_with_waker(waker: Arc<dyn Fn() + Send + Sync + 'static>) -> Self {
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
            worker: FindWorker::spawn_with_waker(waker),
            regex_error: None,
            worker_busy: false,
            worker_generation: 0,
        }
    }

    #[cfg(test)]
    pub fn new_for_test() -> Self {
        Self::new_with_waker(Arc::new(|| {}))
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
        self.worker_generation = self.worker.invalidate();
    }

    pub fn arm_refresh(&mut self, now: Instant) {
        self.worker_generation = self.worker.invalidate();
        self.worker_busy = !self.query.is_empty();
        self.last_key = Some(now);
        self.matches.clear();
        self.index = 0;
        self.truncated = false;
        self.block_matches.clear();
        self.block_index = 0;
        self.block_truncated = false;
        self.regex_error = None;
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
    pub dirty_sections: ConfigSectionMask,
    pub error: Option<String>,
    /// F5: In narrow mode, true = show content (user drilled into a category),
    /// false = show sidebar. Wide mode ignores this (both are visible).
    pub drill_down: bool,
    /// F5: Field-level validation errors (field_label, message). Set by
    /// validation before save; cleared on successful save or panel close.
    pub field_errors: Vec<(String, String)>,
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
            dirty_sections: ConfigSectionMask::empty(),
            error: None,
            drill_down: false,
            field_errors: Vec::new(),
        }
    }

    pub fn open_from(&mut self, config: &Config) {
        self.open = true;
        self.tab = SettingsTab::Appearance;
        self.selection = 0;
        self.scroll_offset = 0;
        self.draft = config.clone();
        self.dirty = false;
        self.dirty_sections = ConfigSectionMask::empty();
        self.error = None;
        self.drill_down = false;
        self.field_errors.clear();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.dirty = false;
        self.dirty_sections = ConfigSectionMask::empty();
        self.error = None;
        self.drill_down = false;
        self.field_errors.clear();
    }

    pub fn mark_dirty(&mut self, section: ConfigSectionMask) {
        self.dirty = true;
        self.dirty_sections.insert(section);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ContextMenu, InteractionState, PanelState, SessionManager, SettingsState, TabBarState,
        WindowRuntimeState,
    };
    use crate::tab::Tab;
    use std::time::Instant;
    use weft_core::config::Config;

    // v1.5.0: `config_state_preserves_preferred_dark_theme` moved to
    // `config_state.rs` alongside the ConfigState struct.

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
        assert!(state.block_hovered.is_none());
    }

    #[test]
    fn context_menu_ownership_uses_stable_session_identity() {
        let menu = ContextMenu {
            session_id: 42,
            block_id: None,
            x: 0.0,
            y: 0.0,
            selection: 0,
        };
        assert!(menu.belongs_to_session(42));
        assert!(!menu.belongs_to_session(7));
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
    fn session_manager_starts_without_an_invalid_active_tab() {
        let state = SessionManager::new();
        assert!(state.is_empty());
        assert_eq!(state.active_idx(), 0);
        assert_eq!(state.prev_drawn_tab(), 0);
    }

    #[test]
    fn open_tab_increments_active() {
        let mut sm = SessionManager::new();
        sm.push_tab(Tab::empty());
        sm.set_active(0);
        assert_eq!(sm.len(), 1);
        assert_eq!(sm.active_idx(), 0);
        sm.push_tab(Tab::empty());
        sm.set_active(1);
        assert_eq!(sm.active_idx(), 1);
    }

    #[test]
    fn close_last_tab_signals_exit() {
        let mut sm = SessionManager::new();
        sm.push_tab(Tab::empty());
        // Closing the only tab → is_last = true
        assert!(sm.close_active());
        assert!(sm.is_empty());
    }

    #[test]
    fn close_non_last_tab_keeps_session() {
        let mut sm = SessionManager::new();
        sm.push_tab(Tab::empty());
        sm.push_tab(Tab::empty());
        sm.set_active(1);
        // Closing active (tab 1, not the last remaining) → is_last = false
        assert!(!sm.close_active());
        assert_eq!(sm.len(), 1);
    }

    #[test]
    fn close_background_reindexes_active() {
        let mut sm = SessionManager::new();
        for _ in 0..3 {
            sm.push_tab(Tab::empty());
        }
        sm.set_active(2);
        // Close tab 0 (before active) → active should shift to 1
        assert!(!sm.close_background(0));
        assert_eq!(sm.active_idx(), 1);
        assert_eq!(sm.len(), 2);
    }

    #[test]
    fn stable_session_id_survives_reindex_and_removed_owner_disappears() {
        let mut sm = SessionManager::new();
        for _ in 0..3 {
            sm.push_tab(Tab::empty());
        }
        let removed = sm.tab(0).unwrap().session_id;
        let survivor = sm.tab(2).unwrap().session_id;

        assert!(!sm.close_background(0));
        assert_eq!(sm.tab_index_by_session_id(removed), None);
        assert_eq!(sm.tab_index_by_session_id(survivor), Some(1));
    }

    #[test]
    fn switch_wraps_around() {
        let mut sm = SessionManager::new();
        for _ in 0..3 {
            sm.push_tab(Tab::empty());
        }
        sm.set_active(2);
        let (new, prev) = sm.next();
        assert_eq!(prev, 2);
        assert_eq!(new, 0); // wraps
        let (new, prev) = sm.prev();
        assert_eq!(prev, 0);
        assert_eq!(new, 2); // wraps back
    }

    #[test]
    fn switch_to_clamps_index() {
        let mut sm = SessionManager::new();
        sm.push_tab(Tab::empty());
        let (new, prev) = sm.switch_to(99);
        assert_eq!(new, 0);
        assert_eq!(prev, 0);
    }
}
