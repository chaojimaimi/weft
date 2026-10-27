//! Panel / overlay state structs, moved verbatim out of `app_state.rs`
//! (v1.15.2 governance cart, zero-rewrite). Re-exported from `app_state`
//! so every existing `app_state::X` path keeps compiling unchanged.

use super::*;

#[derive(Debug, Default)]
pub struct PanelState {
    pub open: bool,
    pub query: String,
    /// v1.12.26 (P1-03): active IME composition for the panel search box —
    /// mirrors `find.ime_preedit` (palette/note precedent). Rendered inline
    /// after the query; cleared on commit/query clear/close.
    pub ime_preedit: String,
    /// v1.12.26 (P1-03): raw winit (cursor, selection) tuple, stored
    /// unconverted exactly like `palette.ime_preedit_cursor`.
    pub ime_preedit_cursor: Option<(usize, usize)>,
    pub selection: usize,
    /// v1.13.17 S2: mouse-hovered history row (topmost visible = 0, the
    /// `PanelTarget::Row` index space — the same space the paint loop's `i`
    /// iterates). Transient interaction state; cleared on close. NOTE: the
    /// expanded-block output push-down in the paint path is not mirrored by
    /// the hit geometry (known drift, review P3) — hover matches clicks.
    pub hovered_row: Option<usize>,
    pub expanded: Option<BlockId>,
    pub search_focused: bool,
    pub highlight: Option<BlockId>,
    pub highlight_until: Option<Instant>,
    pub last_click: Option<(Instant, usize)>,
    /// F3-4: Block-level scroll offset for the sidebar history list (number
    /// of filtered blocks skipped from the newest end). 0 = newest visible.
    /// Clamped to `[0, total_filtered - visible_blocks]`.
    pub scroll_offset: usize,
    /// v1.14.0 (PLAN_v1140 T2): sidebar open/close width tween (180ms open,
    /// 150ms close). While in flight, each frame writes the tween value
    /// STRAIGHT into `renderer.sidebar_width_override` (bypassing the
    /// `[240, 360]` clamp so an opening 0→target sweep is not pinned at 240).
    pub sidebar_width_anim: Option<crate::app::motion::Tween>,
    /// v1.14.0 (T2): the pre-animation `sidebar_width_override` restored
    /// when the tween lands — `None` (responsive-default width) must not be
    /// frozen into a fixed width by the animation channel.
    pub sidebar_width_restore: Option<Option<f32>>,
    /// v1.14.0 (T2): true while an OPEN tween runs; false while closing
    /// (the real `panel.close()` runs when the tween lands — 先动画后状态).
    pub sidebar_width_opening: bool,
    /// v1.14.1 (F3): last instant the in-flight tween applied its override
    /// write + reflow (`app::motion::should_apply` 8ms throttle). `None` on
    /// a fresh tween = the next maintenance frame applies immediately.
    pub sidebar_width_anim_last_apply: Option<Instant>,
    /// v1.14.2 (A2/PLAN §2): last instant the sidebar DRAG applied its width
    /// write + reflow (same `app::motion::should_apply` 8ms gate as the
    /// tween path). Cleared on release.
    pub sidebar_drag_last_apply: Option<Instant>,
    /// v1.14.2 (A2): pointer width seen inside the 8ms throttle window —
    /// parked here instead of applied, then written by the next due frame
    /// or by the release (终值必写).
    pub pending_drag_width: Option<f32>,
}

impl PanelState {
    pub fn close(&mut self) {
        self.open = false;
        self.search_focused = false;
        // v1.13.17 S2: hover is transient — never outlives the surface.
        self.hovered_row = None;
        // v1.12.26 (P1-03): a stale composition must never outlive the bar.
        self.ime_preedit.clear();
        self.ime_preedit_cursor = None;
        self.highlight = None;
        self.highlight_until = None;
        self.last_click = None;
        self.scroll_offset = 0;
    }

    /// v1.14.0 (T2): toggle-side predicate — true when the sidebar is
    /// visually present AND not shrinking. A closing width animation keeps
    /// `open` true (visible) until it lands, but the next toggle must reopen
    /// (reverse the tween) instead of closing twice.
    pub fn sidebar_stable_open(&self) -> bool {
        self.open && !(self.sidebar_width_anim.is_some() && !self.sidebar_width_opening)
    }

    pub fn clear_transient_selection(&mut self) {
        self.selection = 0;
        self.expanded = None;
        // v1.13.17 S2: stale hover must not survive a fresh open either.
        self.hovered_row = None;
        // v1.12.26 (P1-03): the panel-open path clears the query right
        // before this call — the composition goes with it.
        self.ime_preedit.clear();
        self.ime_preedit_cursor = None;
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
    /// v1.14.0 (PLAN_v1140 T1): open/close reveal tween (palette/settings
    /// twin). `open` stays true through the Closing phase.
    pub reveal: crate::app::motion::Reveal,
}

impl NoteEditorState {
    pub fn open_for(&mut self, block_id: BlockId, existing: Option<&str>) {
        self.open = true;
        self.target_block_id = Some(block_id);
        self.buffer = existing.unwrap_or("").to_string();
        self.cursor = self.buffer.len();
    }

    /// v1.14.0 (T1): the input-routing predicate — true only while the card
    /// is open AND not fading out (palette_state twin).
    pub fn interactive(&self) -> bool {
        self.open && !self.reveal.is_closing()
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
    /// v1.12.26 (P1-02): active IME composition for the find bar — mirrors
    /// `palette.ime_preedit` (palette/note precedent). Rendered inline after
    /// the query tail; cleared on commit/query reset/close.
    pub ime_preedit: String,
    /// v1.12.26 (P1-02): raw winit (cursor, selection) tuple, stored
    /// unconverted exactly like `palette.ime_preedit_cursor`.
    pub ime_preedit_cursor: Option<(usize, usize)>,
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
    /// v1.14.0 (PLAN_v1140 T1): open/close reveal tween (palette/settings
    /// twin). `open` stays true through the Closing phase.
    pub reveal: crate::app::motion::Reveal,
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
            ime_preedit: String::new(),
            ime_preedit_cursor: None,
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
            reveal: crate::app::motion::Reveal::default(),
        }
    }

    /// v1.14.0 (T1): the input-routing predicate — true only while the bar
    /// is open AND not fading out (palette_state twin).
    pub fn interactive(&self) -> bool {
        self.open && !self.reveal.is_closing()
    }

    #[cfg(test)]
    pub fn new_for_test() -> Self {
        Self::new_with_waker(Arc::new(|| {}))
    }

    pub fn reset_query(&mut self) {
        self.query.clear();
        // v1.12.26 (P1-02): a stale composition must never outlive the bar
        // (reset_query runs on both the Cmd+F open and the close paths).
        self.ime_preedit.clear();
        self.ime_preedit_cursor = None;
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
