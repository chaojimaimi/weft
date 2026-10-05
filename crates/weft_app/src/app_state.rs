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
    ///
    /// v1.11.16 (Fix B3): removing the ACTIVE tab must match `close_active`
    /// semantics (focus falls to the previous tab) instead of
    /// `close_background`'s keep-index behavior (focus lands on the next).
    pub fn remove_dead(&mut self, idx: usize) -> bool {
        if idx == self.active_tab {
            return self.close_active();
        }
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

    /// v1.11: Move a tab from `from` to `to` (drag-to-reorder). No-op when
    /// indices are equal or out of bounds. `active_tab` is adjusted so it
    /// continues to point at the same tab (by identity, not position).
    pub fn move_tab(&mut self, from: usize, to: usize) {
        if from == to || from >= self.tabs.len() || to >= self.tabs.len() {
            return;
        }
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.active_tab = adjust_index_after_move(self.active_tab, from, to);
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

/// v1.11: Tab drag-to-reorder state. Set when the user presses on a tab label
/// (after switching to it); cleared on mouse release. While `moved` is false,
/// the gesture is indistinguishable from a plain click (tab already switched
/// on press, or a close request pending when `close_on_release` is set).
/// Once the pointer exceeds the drag threshold, `moved` flips to true and
/// subsequent moves update `insert_index` (ghost gap slot) without touching
/// the `SessionManager` order — the reorder is committed once on release.
#[derive(Clone, Copy)]
pub struct TabBarDragState {
    /// Physical X where the press started.
    pub start_x: f64,
    /// Physical Y where the press started.
    pub start_y: f64,
    /// Index of the dragged tab in the `tabs` Vec. Stable for the whole
    /// gesture — the Vec is only reordered on release.
    pub drag_index: usize,
    /// Pointer X offset within the dragged tab's rect at lift (physical px).
    /// The ghost pill keeps this grip so it never jumps on the first move.
    pub grab_offset: f32,
    /// Current gap slot (0..=n-1) the pointer would drop the dragged tab at.
    /// Updated on every move past the threshold; rendered as a one-slot gap.
    pub insert_index: usize,
    /// Whether the drag threshold has been exceeded. Until this flips, no
    /// reorder happens — the press was treated as a normal click.
    pub moved: bool,
    /// Press landed on the tab's close "×" button: a click (no movement)
    /// performs the close instead of being a no-op tab switch.
    pub close_on_release: bool,
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
    pub block_selected: Option<BlockId>,
    pub block_action_hovered: Option<crate::block_component::BlockHeaderAction>,
    /// F3-3: Active sidebar resize drag. Set on press at the sidebar's right
    /// edge; cleared on release (which persists the width to config).
    pub sidebar_drag: Option<SidebarDragState>,
    /// v1.3.2: Active pane divider resize drag. Set on press at a pane
    /// divider; cleared on release. Not persisted (ratio is in-memory only).
    pub pane_divider_drag: Option<PaneDividerDragState>,
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
            block_selected: None,
            block_action_hovered: None,
            sidebar_drag: None,
            pane_divider_drag: None,
            tab_drag: None,
            selection_drag_pos: None,
            selection_autoscroll_carry: 0.0,
            prev_focus: None,
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

/// v1.7.3-C: Inline note editor state. Opened from the block context menu
/// ("Add Note"). When `open`, keyboard input is captured at the top of
/// `handle_key_event` (before overlay routing) so the user can type a
/// freeform note. Enter saves to `AnnotationStore::set_note`; Esc cancels.
#[derive(Debug, Default)]
pub struct NoteEditorState {
    pub open: bool,
    pub target_block_id: Option<BlockId>,
    pub buffer: String,
    /// Byte offset of the caret in `buffer`.
    pub cursor: usize,
    /// v1.12.24 (N-1): IME composition state, mirrors palette.ime_preedit.
    pub ime_preedit: String,
    /// v1.12.24 (N-1): raw winit (cursor, selection) tuple — the palette
    /// stores it unconverted, so this field mirrors that exactly.
    pub ime_preedit_cursor: Option<(usize, usize)>,
}

impl NoteEditorState {
    pub fn open_for(&mut self, block_id: BlockId, existing: Option<&str>) {
        self.open = true;
        self.target_block_id = Some(block_id);
        self.buffer = existing.unwrap_or("").to_string();
        self.cursor = self.buffer.len();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.target_block_id = None;
        self.buffer.clear();
        self.cursor = 0;
        // v1.12.24 (N-1): stale composition must never outlive the card.
        self.ime_preedit.clear();
        self.ime_preedit_cursor = None;
    }
}

/// v1.8.2: Per-block AI diagnose state. Stored in a `HashMap<BlockId, DiagnoseState>`
/// on `App`. When `pending_id` is `Some`, the block's header shows a "thinking…"
/// indicator; when `result` is `Some`, an inline panel is rendered below the
/// block's output showing the model's explanation.
#[derive(Debug, Clone)]
pub struct BlockDiagnoseState {
    /// The `AiState` request id currently in flight for this block, if any.
    /// Used to correlate with `AiResultEvent::Diagnose { id, .. }`.
    pub pending_id: Option<u64>,
    /// The last completed diagnose result (plain-text explanation). Cleared
    /// when the user closes the panel or triggers a new diagnose.
    pub result: Option<Result<String, String>>,
}

impl BlockDiagnoseState {
    pub fn is_thinking(&self) -> bool {
        self.pending_id.is_some()
    }

    #[allow(dead_code)]
    pub fn has_panel(&self) -> bool {
        self.result.is_some() || self.is_thinking()
    }
}

/// v1.8.3: Settings LocalAi tab connection status. Updated by
/// `poll_ai_results` when a `ModelsRefreshed` event arrives, and rendered
/// as a status line below the "Test Connection" button.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum AiConnectionStatus {
    /// No test has been run yet.
    #[default]
    Idle,
    /// A `/api/tags` request is in flight.
    Testing,
    /// Connection succeeded — `n` models were discovered.
    Ok(usize),
    /// Connection failed — the string is a user-facing error message
    /// (e.g. "connection refused", "non-loopback endpoint", "timeout").
    Failed(String),
}

impl AiConnectionStatus {
    /// Human-readable summary for the Settings status line.
    pub fn label(&self) -> String {
        match self {
            Self::Idle => "Not tested".into(),
            Self::Testing => "Testing…".into(),
            Self::Ok(n) => format!("Connected ({} models)", n),
            Self::Failed(msg) => format!("Failed: {}", msg),
        }
    }

    /// True when a refresh is in flight (used to disable the button).
    pub fn is_testing(&self) -> bool {
        matches!(self, Self::Testing)
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
        adjust_index_after_move, ContextMenu, InteractionState, PanelState, SessionManager,
        SettingsState, TabBarState, WindowRuntimeState,
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

    #[test]
    fn move_tab_rightward_adjusts_active() {
        let mut sm = SessionManager::new();
        for _ in 0..5 {
            sm.push_tab(Tab::empty());
        }
        // [A, B, C, D, E], active = B (index 1)
        sm.set_active(1);
        sm.move_tab(1, 3); // → [A, C, D, B, E]
        assert_eq!(sm.active_idx(), 3, "active should follow the moved tab");
        assert_eq!(sm.len(), 5);
    }

    #[test]
    fn move_tab_leftward_adjusts_active() {
        let mut sm = SessionManager::new();
        for _ in 0..5 {
            sm.push_tab(Tab::empty());
        }
        // [A, B, C, D, E], active = D (index 3)
        sm.set_active(3);
        sm.move_tab(3, 1); // → [A, D, B, C, E]
        assert_eq!(sm.active_idx(), 1, "active should follow the moved tab");
    }

    #[test]
    fn move_tab_preserves_non_active_indices() {
        let mut sm = SessionManager::new();
        for _ in 0..5 {
            sm.push_tab(Tab::empty());
        }
        // [A, B, C, D, E], active = A (index 0)
        sm.set_active(0);
        sm.move_tab(1, 3); // → [A, C, D, B, E]
        assert_eq!(
            sm.active_idx(),
            0,
            "active was not the moved tab — should stay put"
        );
    }

    #[test]
    fn move_tab_noop_on_equal_or_oob() {
        let mut sm = SessionManager::new();
        for _ in 0..3 {
            sm.push_tab(Tab::empty());
        }
        sm.set_active(1);
        sm.move_tab(1, 1);
        assert_eq!(sm.active_idx(), 1);
        sm.move_tab(0, 99);
        assert_eq!(sm.len(), 3, "out-of-bounds move should be a no-op");
        sm.move_tab(99, 0);
        assert_eq!(sm.len(), 3);
    }

    #[test]
    fn move_tab_session_id_survives_reorder() {
        let mut sm = SessionManager::new();
        for _ in 0..4 {
            sm.push_tab(Tab::empty());
        }
        let moved_id = sm.tab(1).unwrap().session_id;
        let other_id = sm.tab(3).unwrap().session_id;
        sm.move_tab(1, 3);
        assert_eq!(
            sm.tab_index_by_session_id(moved_id),
            Some(3),
            "moved tab should now be at index 3"
        );
        assert_eq!(
            sm.tab_index_by_session_id(other_id),
            Some(2),
            "displaced tab should shift left"
        );
    }

    #[test]
    fn adjust_index_after_move_rightward() {
        // Move from 1 to 3: [A,B,C,D,E] → [A,C,D,B,E]
        assert_eq!(adjust_index_after_move(0, 1, 3), 0); // A stays
        assert_eq!(adjust_index_after_move(1, 1, 3), 3); // B moved
        assert_eq!(adjust_index_after_move(2, 1, 3), 1); // C shifts left
        assert_eq!(adjust_index_after_move(3, 1, 3), 2); // D shifts left
        assert_eq!(adjust_index_after_move(4, 1, 3), 4); // E stays
    }

    #[test]
    fn adjust_index_after_move_leftward() {
        // Move from 3 to 1: [A,B,C,D,E] → [A,D,B,C,E]
        assert_eq!(adjust_index_after_move(0, 3, 1), 0); // A stays
        assert_eq!(adjust_index_after_move(1, 3, 1), 2); // B shifts right
        assert_eq!(adjust_index_after_move(2, 3, 1), 3); // C shifts right
        assert_eq!(adjust_index_after_move(3, 3, 1), 1); // D moved
        assert_eq!(adjust_index_after_move(4, 3, 1), 4); // E stays
    }

    #[test]
    fn remove_dead_active_tab_focuses_previous() {
        // v1.11.16 (Fix B3): removing the active tab must focus the
        // previous tab, matching close_active semantics.
        let mut sm = SessionManager::new();
        for _ in 0..3 {
            sm.push_tab(Tab::empty());
        }
        sm.set_active(1); // [A,B,C] active = B
        assert!(!sm.remove_dead(1)); // remove active B
        assert_eq!(sm.len(), 2);
        assert_eq!(sm.active_idx(), 0); // focus falls to A (index 0)
    }

    #[test]
    fn remove_dead_active_last_tab_focuses_new_last() {
        // v1.11.16 (Fix B3): removing the last active tab focuses the
        // new last tab (index len-1), matching close_active semantics.
        let mut sm = SessionManager::new();
        for _ in 0..3 {
            sm.push_tab(Tab::empty());
        }
        sm.set_active(2); // [A,B,C] active = C
        assert!(!sm.remove_dead(2)); // remove active C
        assert_eq!(sm.len(), 2);
        assert_eq!(sm.active_idx(), 1); // focus falls to B (the new last)
    }

    #[test]
    fn remove_dead_background_tab_keeps_index_behavior() {
        // v1.11.16 (Fix B3): removing a BACKGROUND (non-active) tab must
        // keep close_background's reindex behavior, not close_active's.
        let mut sm = SessionManager::new();
        for _ in 0..3 {
            sm.push_tab(Tab::empty());
        }
        sm.set_active(1); // [A,B,C] active = B
                          // Remove tab 0 (before active): active shifts down to 0.
        assert!(!sm.remove_dead(0));
        assert_eq!(sm.len(), 2);
        assert_eq!(sm.active_idx(), 0);

        // Reset and remove the background tab AFTER active.
        let mut sm = SessionManager::new();
        for _ in 0..3 {
            sm.push_tab(Tab::empty());
        }
        sm.set_active(1); // [A,B,C] active = B
                          // Remove tab 2 (after active): active index stays at 1.
        assert!(!sm.remove_dead(2));
        assert_eq!(sm.len(), 2);
        assert_eq!(sm.active_idx(), 1);
    }

    #[test]
    fn remove_dead_last_remaining_tab_reports_is_last() {
        // v1.11.16 (Fix B3): removing the only remaining tab reports
        // is_last = true (session should exit).
        let mut sm = SessionManager::new();
        sm.push_tab(Tab::empty());
        assert!(sm.remove_dead(0));
        assert!(sm.is_empty());
    }
}
