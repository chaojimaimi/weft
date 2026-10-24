//! Drag / gesture value types + the context-menu selection state, moved out
//! of `app_state.rs` (v1.14.0 PLAN_v1140 file-budget split; verbatim code
//! motion — re-exported from the parent so consumers are unchanged).

use weft_core::blocks::BlockId;

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
