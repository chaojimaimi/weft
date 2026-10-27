//! Cohesive UI state groups extracted from the application shell.
//!
//! These types own their reset invariants so tab/panel lifecycle code no
//! longer edits a collection of unrelated `App` fields individually.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use weft_core::blocks::annotations::AnnotationStore;
use weft_core::blocks::BlockId;
use weft_core::config::{Config, ConfigSectionMask};
use weft_core::find::{BlockMatch, FindMatch};
use weft_core::persistence::BlockStore;

use crate::find_worker::FindWorker;
use crate::overlay::SettingsTab;
use crate::tab::Tab;

mod session;

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
    /// v1.7.3-C: Sidecar annotation store (bookmark/note/tags) sharing the
    /// same `blocks.db` file. Opened alongside `BlockStore` so both see the
    /// same schema state.
    annotation_store: Option<AnnotationStore>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            tabs: Vec::new(),
            active_tab: 0,
            prev_drawn_tab: 0,
            block_store: None,
            annotation_store: None,
        }
    }

    // ── Read accessors ────────────────────────────────────────────────

    /// v1.12.25 (audit 3-B, P1-01): `Option`-ized — `None` on the empty-tabs
    /// transient (after the last tab exits) instead of a bare index panic.
    /// Empty-tabs semantics = "ignore the event"; callers handle `None` by
    /// returning early, never by panicking.
    pub fn active(&self) -> Option<&Tab> {
        self.tabs.get(self.active_tab)
    }

    /// v1.12.25 (audit 3-B, P1-01): `Option`-ized twin of [`Self::active`].
    pub fn active_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active_tab)
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

    /// v1.7.3-C: Annotation store (bookmark/note/tags) — `None` when the DB
    /// failed to open (persistence disabled). Safe to call every frame; the
    /// controller treats `None` as a no-op for annotation actions.
    pub fn annotation_store(&self) -> Option<&AnnotationStore> {
        self.annotation_store.as_ref()
    }

    pub fn set_annotation_store(&mut self, store: Option<AnnotationStore>) {
        self.annotation_store = store;
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
}

/// Pure index adjustment after a `remove(from) + insert(to)` move. Returns
/// the new position of whatever was previously at `idx`.
///
/// - If `idx == from`, the moved element is now at `to`.
/// - If `from < to` (rightward move), elements in `(from, to]` shift left by 1.
/// - If `from > to` (leftward move), elements in `[to, from)` shift right by 1.
fn adjust_index_after_move(idx: usize, from: usize, to: usize) -> usize {
    if idx == from {
        return to;
    }
    if from < to {
        if idx > from && idx <= to {
            idx - 1
        } else {
            idx
        }
    } else if idx >= to && idx < from {
        idx + 1
    } else {
        idx
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

// v1.14.0 (PLAN_v1140 batch splits): the drag/gesture + context-menu value
// types moved to `app_state/drag.rs` — the reveal/width/px animation fields
// pushed this file past the 800-line gate. Re-exported so every
// `app_state::{DragState, ...}` consumer keeps compiling unchanged.
mod drag;

pub use drag::{
    ContextMenu, DragState, DragTarget, PaneDividerDragState, SidebarDragState, TabBarDragState,
};

pub struct WindowRuntimeState {
    pub cursor_blink_on: bool,
    pub cursor_blink_phase: f32,
    pub cursor_blink_time: Instant,
    pub cursor_anim_active: Arc<AtomicBool>,
    pub synchronized_output_watchdog_pending: Arc<AtomicBool>,
    pub last_resize_instant: Instant,
    /// PLAN_zoom (field run, 1.12.6 diagnostics): until when a PROGRAMMATIC
    /// resize jump (double-click zoom: one-shot size jump with
    /// inLiveResize already false by the time the Resized event is
    /// dispatched) keeps the zoom channel hot. The zoom: animation is a
    /// live-resize internally (the injected windowWillResize callback sees
    /// inLiveResize == true), so the callback stamp is exempted there; the
    /// jump detection on the Resized event is the reliable signal.
    pub zoom_jump_until: Option<Instant>,
    /// Companion to `zoom_jump_until`: the physical size at the previous
    /// Resized event, for the jump test.
    pub last_resized_physical: Option<(u32, u32)>,
    /// Appendix F-3B: post-expiry flush flag pairing `zoom_jump_until`.
    pub zoom_flush_pending: bool,
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
    /// Drag-selection autoscroll timer gate: set while a block-view drag
    /// selection is held past the content edge so a 40ms timer keeps the
    /// viewport scrolling even when the pointer doesn't move. Cleared when
    /// the pointer re-enters the content band or the button is released.
    pub selection_autoscroll_active: Arc<AtomicBool>,
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
            zoom_jump_until: None,
            last_resized_physical: None,
            zoom_flush_pending: false,
            last_system_appearance_dark: None,
            last_appearance_check: Instant::now(),
            current_logo_variant: weft_core::config::LogoVariant::Cool,
            spinner_phase: 0.0,
            spinner_anim_active: Arc::new(AtomicBool::new(false)),
            selection_autoscroll_active: Arc::new(AtomicBool::new(false)),
            spinner_time: Instant::now(),
            reduce_motion: false,
            increase_contrast: false,
            tab_snapshots: crate::snapshot_persistence::SnapshotPersistenceState::default(),
        }
    }
    /// PLAN_zoom: whether the programmatic-zoom channel (armed by a one-shot
    /// size jump) is still hot.
    pub fn zoom_jump_hot(&self) -> bool {
        self.zoom_jump_until
            .is_some_and(|until| std::time::Instant::now() < until)
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
    /// v1.13.17 S2: mouse-hovered completion-popup row (editor
    /// `completion_view` index space = `selected`). Transient interaction
    /// state — never changes the keyboard selection; recomputed (or cleared)
    /// on every free pointer move and never persisted.
    pub completion_hovered_row: Option<usize>,
    pub block_selected: Option<BlockId>,
    pub block_action_hovered: Option<crate::block_component::BlockHeaderAction>,
    /// F3-3: Active sidebar resize drag. Set on press at the sidebar's right
    /// edge; cleared on release (which persists the width to config).
    pub sidebar_drag: Option<SidebarDragState>,
    /// v1.3.2: Active pane divider resize drag. Set on press at a pane
    /// divider; cleared on release. Not persisted (ratio is in-memory only).
    pub pane_divider_drag: Option<PaneDividerDragState>,
    /// v1.14.3 (T2, PLAN_v1143 §2): divider-drag 8ms throttle state — the
    /// InteractionState twin of PanelState's sidebar A2 fields (内聚性: the
    /// gate rides the drag state it throttles). Cleared on release (终值必写
    /// consumed the park) and on the `set_ratio` abort arm.
    pub divider_drag_last_apply: Option<Instant>,
    /// v1.14.3 (T2): pointer ratio seen inside the 8ms throttle window —
    /// parked here instead of applied, then written by the next due event or
    /// by the release (`app::divider_drag::release_ratio`, 终值必写).
    pub pending_divider_ratio: Option<f32>,
    /// v1.11: Active tab drag-to-reorder. Set on press at a tab label;
    /// cleared on release. Live-reorders tabs once the drag threshold is
    /// exceeded.
    pub tab_drag: Option<TabBarDragState>,
    /// Last-known pointer position during a block-view drag selection. The
    /// 40ms autoscroll timer reads it to keep scrolling when the pointer is
    /// held still past the content edge. Cleared on left-button release.
    pub selection_drag_pos: Option<(f64, f64)>,
    /// v1.10.26 (FIX_SELECTION_CONTENT_ANCHORS): fractional rows accumulated
    /// past integer rows during autoscroll ticks (the Warp ramp returns f32;
    /// the scroll API takes whole rows). Reset when the pointer leaves the
    /// edge band.
    pub selection_autoscroll_carry: f32,
    /// F4: FocusId of the element that had keyboard focus before a modal
    /// surface (Palette/Find/Settings/ContextMenu) opened. Used to restore
    /// focus (visually / for accessibility) when the modal closes. `None`
    /// when no modal is open or the focus was already on the modal target.
    pub prev_focus: Option<crate::scene::FocusId>,
    /// v1.14.0 (PLAN_v1140 T4): hover fades for the block action buttons +
    /// scrollbar thumb (Block*/ScrollbarThumb keys) — the plan's 借用面
    /// split: block/scrollbar state lives beside their hover signals.
    pub hover_fades: crate::app::motion::HoverFades,
}

impl InteractionState {
    pub fn new() -> Self {
        Self {
            mods: winit::event::Modifiers::default(),
            last_mouse_x: 0.0,
            last_mouse_y: 0.0,
            prompt_dragging: false,
            popup_width_scale: 0.6,
            // B1: 16 rows (was 8) — short prefixes like "l" match 100+ commands;
            // more visible rows make high-frequency commands findable.
            popup_max_rows: 16,
            drag_state: None,
            scrollbar_drag: None,
            panel_scrollbar_drag: None,
            scrollbar_hovered: false,
            precise_scroll: crate::scroll_input::PreciseScrollAccumulator::default(),
            modal_mouse_capture: crate::input_router::ModalMouseCapture::default(),
            context_menu: None,
            block_hovered: None,
            completion_hovered_row: None,
            block_selected: None,
            block_action_hovered: None,
            sidebar_drag: None,
            pane_divider_drag: None,
            divider_drag_last_apply: None,
            pending_divider_ratio: None,
            tab_drag: None,
            selection_drag_pos: None,
            selection_autoscroll_carry: 0.0,
            prev_focus: None,
            hover_fades: crate::app::motion::HoverFades::default(),
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
    /// v1.14.0 (PLAN_v1140 T3): active-tab indicator slide tween — (x0, x1)
    /// in physical px, 140ms ease-out, from the currently drawn underline
    /// position to the new active tab's rect. `None` = the underline draws
    /// glued to the active tab's slot (pre-T3 behavior). Snapped (cleared)
    /// on scroll-offset change, drag start, and every index-instability
    /// point (close/new/commit) — plan's 防拖影 rule.
    pub indicator: Option<(crate::app::motion::Tween, crate::app::motion::Tween)>,
    /// v1.14.0 (T3): per-index slot-shift tweens (x delta, physical px,
    /// 100ms ease-out) — the eased 让位 of non-dragged tabs during a drag.
    /// Keyed by index (stable within the gesture: the tab Vec only reorders
    /// on release — TabBarDragState contract); fully cleared on
    /// release/commit/cancel where index stability ends. A single tween per
    /// index: the shift is ONE x-delta (tab width is constant, both edges
    /// move together — rust-reviewer P3 de-twin).
    pub tab_slot_shifts: Vec<Option<crate::app::motion::Tween>>,
    /// v1.14.0 (PLAN_v1140 T4): hover color fades for the tab bar's four
    /// elements (row / "+" / ‹› / ×) — Tab* keys only.
    pub hover_fades: crate::app::motion::HoverFades,
    /// v1.14.2 (B1, PLAN_v1142 §3): generation-keyed tab-title snapshots —
    /// `tab_bar_state` and the A4 fingerprint reuse these instead of taking
    /// every tab's FairMutex per frame (tab 风暴债①). `RefCell` because both
    /// consumers run with `&self` on the main thread; the refresh/probe
    /// kernel lives in `app/tab_title_cache.rs`.
    pub(crate) title_cache: std::cell::RefCell<Vec<crate::app::tab_title_cache::TabTitleCached>>,
}

impl TabBarState {
    pub fn clear_hover(&mut self) {
        self.hovered_tab = None;
        self.plus_hovered = false;
        self.arrow_left_hovered = false;
        self.arrow_right_hovered = false;
    }
}

// v1.15.2: panel/overlay state structs, moved verbatim (zero-rewrite);
// re-exported so all existing `app_state::X` paths stay valid.
mod panels;
pub use panels::{AiConnectionStatus, BlockDiagnoseState, FindState, NoteEditorState, PanelState};

pub struct SettingsState {
    pub open: bool,
    pub tab: SettingsTab,
    pub selection: usize,
    pub scroll_offset: usize,
    /// v1.13.17 S2: mouse-hovered settings target, exactly what
    /// `settings_target_at` resolves (SidebarCategory / Theme / ContentRow /
    /// button targets). Transient interaction state — deliberately NOT in
    /// `draft` (the draft is the rollback-able edit state; hover never
    /// survives a close). Only row targets get a hover background.
    pub hovered: Option<crate::settings_component::SettingsTarget>,
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
    /// v1.14.0 (PLAN_v1140 T1): open/close reveal tween. `open` stays true
    /// through the Closing phase so drawing readers are unchanged; input
    /// readers consult [`Self::interactive`].
    pub reveal: crate::app::motion::Reveal,
}

impl SettingsState {
    pub fn new() -> Self {
        Self {
            open: false,
            tab: SettingsTab::Appearance,
            selection: 0,
            scroll_offset: 0,
            hovered: None,
            draft: Config::default(),
            dirty: false,
            dirty_sections: ConfigSectionMask::empty(),
            error: None,
            drill_down: false,
            field_errors: Vec::new(),
            reveal: crate::app::motion::Reveal::default(),
        }
    }

    /// v1.14.0 (T1): the input-routing predicate — true only while the
    /// panel is open AND not fading out (palette_state twin).
    pub fn interactive(&self) -> bool {
        self.open && !self.reveal.is_closing()
    }

    pub fn open_from(&mut self, config: &Config) {
        self.open = true;
        self.tab = SettingsTab::Appearance;
        self.selection = 0;
        self.scroll_offset = 0;
        self.hovered = None;
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
        // v1.13.17 S2: hover is transient — never outlives the panel.
        self.hovered = None;
        self.field_errors.clear();
    }

    pub fn mark_dirty(&mut self, section: ConfigSectionMask) {
        self.dirty = true;
        self.dirty_sections.insert(section);
    }
}

// Tests live in the gate-exempt sibling module (repo test-module
// convention, pty/tests.rs precedent) so inline test lines stay out of
// the production-file budget.
#[cfg(test)]
#[path = "app_state/tests.rs"]
mod tests;
