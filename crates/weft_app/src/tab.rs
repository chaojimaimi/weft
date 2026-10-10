//! Per-tab session state (v0.9 H1, v1.3 multi-pane).
//!
//! A `Tab` owns one or more [`Pane`]s arranged by a [`SplitTree`]. Each pane
//! carries its own PTY, VT `Terminal`, input/selection handlers, IME preedit,
//! and block-scroll state. The tab-level concerns (split tree, active pane
//! id, stable tab `session_id` for tab-bar / context-menu ownership) live
//! here.
//!
//! `Tab` implements `Deref<Target=Pane>` / `DerefMut` so existing call sites
//! that read `tab.pty` / `tab.input_handler` / … keep compiling unchanged:
//! the field access auto-dereferences to the active pane. The terminal is
//! reached through the `Pane` accessors (`with_terminal` / `lock_terminal` —
//! T10 P1). New code that needs to operate on a non-active pane should go
//! through `tab.pane(id)` / `tab.pane_mut(id)`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use weft_core::pane_layout::{PaneId, SplitTree};
use weft_core::pty::PtyError;

use crate::pane::Pane;
use crate::AppEvent;
#[cfg(test)]
use crate::AppMsg;

/// v1.3.4: Geometry inputs for `Tab::split_active_pane`. Bundles the
/// renderer's current content rect + cell size into a single value so the
/// split method stays under clippy's `too_many_arguments` threshold and
/// the call site reads naturally. Built by the dispatch layer from
/// `App::terminal_layout()`; zeroed values mean "renderer not ready" and
/// the split method falls back to the active pane's current size.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PaneSplitGeometry {
    /// Content-area rect in physical pixels `[x0, y0, x1, y1]`.
    pub content_rect: weft_core::pane_layout::Rect,
    /// Cell width in physical pixels.
    pub cell_w: f32,
    /// Cell height in physical pixels.
    pub cell_h: f32,
}

mod lifecycle;
mod pane_lifecycle;
mod pane_pump;
mod primary_history;
mod resize;
mod scroll;
mod snapshot;
mod tui_scroll;

pub(crate) use primary_history::PrimaryHistoryRefresh;
pub(crate) use scroll::BlockScrollAnchor;
pub(crate) use tui_scroll::PendingTuiScroll;
pub use tui_scroll::TuiScrollResolution;

/// v1.10.26 (D-1) + v1.10.27 (FIX_RESIZE_DOUBLE_REDRAW): the two most recent
/// alt-screen flips from one source pane — the burst-storm signature for
/// `burst_locked_cols`. FIX_background_pane_pump §2.5: stored per source
/// pane (`Tab::alt_flip_history` map), so one pane's record can never be
/// reset by another pane's flip.
///
/// A real alt TUI (vim/less) enters with ONE flip and goes quiet; an omp
/// repaint feedback loop toggles DEC 1049 every ~130ms. The freeze needs the
/// *two-flip* signature: two flips from the same pane inside the debounce
/// window, the most recent still fresh — so an isolated single flip is never
/// frozen and a storm is pinned at the current grid size until quiet, then
/// the live kind converges once. `count` records how many real flips are
/// stored (0..=2); a value of 1 (a lone flip) never forms a record no matter
/// how fresh it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AltFlipHistory {
    /// Pane that produced these flips. The lock is scoped to this pane:
    /// pane A's storm cannot widen pane B's real TUI target (v1.10.19
    /// "A 面板风暴误伤 B 面板").
    pub(crate) src_pane: PaneId,
    /// The older of the two most recent flips (`count == 1` → not meaningful).
    pub(crate) older: std::time::Instant,
    /// The most recent flip.
    pub(crate) newer: std::time::Instant,
    /// Real flips recorded, capped at 2. `< 2` → no storm signature.
    pub(crate) count: u8,
}

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

/// A tab owns one or more panes. The tab-level `session_id` is the externally
/// visible identifier (tab bar, context menu, autosave); each pane also has
/// its own `pane_session_id` for PTY / trace correlation.
///
/// The active pane is the one that receives keyboard input and renders the
/// cursor. `Deref` / `DerefMut` forward to it so the existing single-pane
/// call sites (`tab.pty`, `tab.input_handler`, …) keep working without
/// change; the terminal goes through the `Pane` lock accessors (T10 P1).
pub struct Tab {
    /// Stable tab-level identifier. Stays constant for the tab's lifetime
    /// regardless of how many panes are open or which is active.
    pub session_id: u64,
    /// Layout tree of pane splits. Owns the structure (direction + ratio at
    /// each internal node, `PaneId` at each leaf) but not the pane state —
    /// the state lives in `panes`.
    #[allow(dead_code)] // v1.3 Batch 5+: read by split_tree() for multi-pane layout
    split_tree: SplitTree,
    /// Per-pane state keyed by `PaneId`. Every leaf in `split_tree` has an
    /// entry here; closing a pane removes both the leaf and the entry.
    panes: HashMap<PaneId, Pane>,
    /// Currently focused pane. Keyboard input, IME, and Deref forward here.
    /// Always points at a leaf present in `split_tree` / `panes`.
    active_pane: PaneId,
    /// v1.10.4: set when any pane enters/exits alt-screen (DEC 1049).
    /// The redraw loop checks this to trigger a geometry recompute so PTY
    /// cols switch between full-width (TUI) and gutter-subtracted (BlockView
    /// shell output). Consumed only when the tab is active; a background
    /// tab's flag self-heals on activation because
    /// `resize_all_panes_for_rect` re-reads the screen mode live.
    ///
    /// v1.10.19: consumption is debounced via the flip history /
    /// `alt_rescale_last_taken` (see `tab/resize.rs`) so a burst of toggles
    /// coalesces into one recompute instead of one per flip. v1.10.26 (D-2):
    /// armed off the u64 flip-counter diff, so even-count batches (h→l
    /// net-zero) still refresh the window.
    ///
    /// FIX_background_pane_pump §2.4 (2): "any pane" is literal now —
    /// background panes are pumped and consumed too, so their flips arm
    /// this flag. The single merged value is deliberate: one main thread
    /// consumes all panes in order (no lost-write race), and the resize
    /// commit itself walks every pane (`resize_all_panes_for_rect`), so one
    /// armed recompute serves all sources.
    pending_alt_rescale: bool,
    /// FIX_background_pane_pump §2.5 (was a single `Option<AltFlipHistory>`
    /// slot): per-pane flip history of the last alt-screen toggles that
    /// armed `pending_alt_rescale` — the last two flip instants of THAT
    /// pane. Keyed by `PaneId`; absent until the pane's first flip. Drives
    /// both the `take_pending_alt_rescale` debounce freshness and the
    /// `burst_locked_cols` storm freeze (see `tab/resize.rs`). Per-pane
    /// storage is the point: a single slot let pane B's isolated flip
    /// discard pane A's accumulated record, un-freezing A's storm early
    /// (the v1.10.19 "A 面板风暴误伤 B 面板" mirror regression).
    alt_flip_history: HashMap<PaneId, AltFlipHistory>,
    /// v1.10.19: time the pending rescale was last consumed by
    /// `take_pending_alt_rescale`. A fresh toggle inside the debounce window
    /// after a consumed recompute marks a burst (the SIGWINCH feedback loop
    /// toggles every ~130ms); the recompute then holds until the toggles go
    /// quiet so the burst coalesces into one.
    alt_rescale_last_taken: Option<std::time::Instant>,
    /// v1.10.25 Batch 3 (FIX_SELECTION_AND_RESIZE_REMAINING): armed after a
    /// committed PTY resize; the first following PTY output logs the
    /// post-resize repaint latency (stage 3/4 of the RESIZE_PROBE chain) and
    /// disarms itself, so it fires once per resize instead of per byte.
    resize_output_probe: Option<std::time::Instant>,
}

/// v1.11 audit P1-1 (PLAN_audit_fix_batch3 C2): immutable draw inputs for one
/// non-active pane, derived under the [`Tab::active_pane_views`] invariants.
/// The rect is NOT carried here — layout is the caller's concern; pair
/// `pane_id` against `split_tree().layout(content_rect)` at the call site.
pub(crate) struct BackgroundPaneView {
    /// Pane the view came from — the pairing key for the caller's rects.
    pub pane_id: PaneId,
    /// Owned terminal lock guard (T10 P1). Background panes' terminals are
    /// only read during draw; the guard is `lock_arc`-owned, so the view
    /// carries no borrow of the `Tab`.
    pub terminal: crate::pane::TerminalGuard,
    /// Block-scroll snapshot (`offset_value + fraction`) taken at derivation
    /// time, so the draw path needs no second borrow of the pane.
    pub block_scroll: f32,
    /// v1.4.1: pane-scoped namespace for the styled-line vertex cache.
    pub pane_session_id: u64,
}

/// v1.11 audit P1-1 (PLAN_audit_fix_batch3 C2): the split-borrow view of a
/// tab's panes for one draw frame — mutable access to the active pane plus
/// shared reads of every background pane, replacing the raw `*const
/// Terminal` pointer bridge in redraw_controller.
pub(crate) struct PaneViewSet<'a> {
    pub active: &'a mut Pane,
    pub backgrounds: Vec<BackgroundPaneView>,
}

impl std::ops::Deref for Tab {
    type Target = Pane;
    fn deref(&self) -> &Self::Target {
        self.panes
            .get(&self.active_pane)
            .expect("active_pane always points at a present pane")
    }
}

impl std::ops::DerefMut for Tab {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.panes
            .get_mut(&self.active_pane)
            .expect("active_pane always points at a present pane")
    }
}

impl Tab {
    /// Borrow the active pane.
    pub(crate) fn active(&self) -> &Pane {
        self
    }

    /// Mutably borrow the active pane.
    pub(crate) fn active_mut(&mut self) -> &mut Pane {
        self
    }

    /// v1.11 audit P1-1 (PLAN_audit_fix_batch3 C2): split-borrow the pane map
    /// for one draw frame — `&mut` on the active pane plus shared reads of
    /// every background pane, so callers never need a raw-pointer bridge.
    ///
    /// CALLER ORDER CONTRACT: `split_tree().layout(content_rect)` (and any
    /// other read of the tab) must complete BEFORE this call — the returned
    /// views hold `&mut Tab` for their whole lifetime, so the read-only
    /// layout walk has to finish first. See redraw_controller::run_redraw.
    ///
    /// WHY SAFE CODE, not the plan-reviewed raw-pointer sketch: the locked
    /// form (`map_ptr → get_mut(active)` then `(*map_ptr).iter()`) is UB
    /// under Stacked Borrows in BOTH orders — the second reborrow through
    /// the raw pointer pops the first one's tag, and Miri (A9, nightly
    /// 2026-09-18) rejects it: "trying to retag ... Unique permission ...
    /// tag does not exist in the borrow stack" at the return of this fn.
    /// `iter_mut` derives the same shape soundly: std guarantees the yielded
    /// `&mut Pane`s are pairwise disjoint, so keeping the active entry's
    /// `&mut` and shrinking every other element to a shared `&Terminal`
    /// needs no unsafe — the borrow checker now enforces what the five
    /// invariants could only document.
    pub(crate) fn active_pane_views(&mut self) -> PaneViewSet<'_> {
        let active_id = self.active_pane_id();
        let mut active: Option<&mut Pane> = None;
        let mut backgrounds: Vec<BackgroundPaneView> = Vec::new();
        for (id, pane) in self.panes.iter_mut() {
            if *id == active_id {
                active = Some(pane);
            } else if let Some(terminal) = pane.lock_terminal() {
                // Same snapshot the raw-pointer bridge took: offset + the
                // transient trackpad fraction, read once per frame.
                backgrounds.push(BackgroundPaneView {
                    pane_id: *id,
                    terminal,
                    block_scroll: pane.block_scroll_anchor.offset_value() as f32
                        + pane.block_scroll_fraction,
                    pane_session_id: pane.pane_session_id,
                });
            }
        }
        PaneViewSet {
            active: active.expect("active_pane always points at a present pane"),
            backgrounds,
        }
    }

    /// Borrow a specific pane by id. Returns `None` if the pane is not in
    /// this tab (closed or never created).
    pub(crate) fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes.get(&id)
    }

    /// Mutably borrow a specific pane by id.
    pub(crate) fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.panes.get_mut(&id)
    }

    /// v1.12.2 B3-2 (PLAN_S2_render): clear the dirty-row flags of every
    /// non-active pane after the frame consumed them. Mirrors the active
    /// pane's post-draw `clear_all_dirty` (redraw_controller): background
    /// grids are never cleared otherwise, so their dirty set only grows and
    /// the per-pane incremental row cache would keep rebuilding long-idle
    /// rows.
    pub(crate) fn clear_background_grid_dirty(&mut self, active_id: PaneId) {
        for (id, pane) in self.panes.iter_mut() {
            if *id == active_id {
                continue;
            }
            pane.with_terminal(|t| t.grid_mut().clear_all_dirty());
        }
    }

    /// v1.5.0: Mutable iterator over all panes. Used by `apply_config` to
    /// reseed palette / scrollback on every pane in every tab when a
    /// profile switch or config reload fires.
    pub(crate) fn panes_mut(&mut self) -> impl Iterator<Item = &mut Pane> {
        self.panes.values_mut()
    }

    pub(crate) fn panes(&self) -> impl Iterator<Item = (PaneId, &Pane)> {
        self.panes.iter().map(|(id, pane)| (*id, pane))
    }

    /// Id of the currently focused pane.
    pub(crate) fn active_pane_id(&self) -> PaneId {
        self.active_pane
    }

    /// Read-only view of the split tree (for layout / hit-testing).
    pub(crate) fn split_tree(&self) -> &SplitTree {
        &self.split_tree
    }

    /// Number of panes in this tab. O(n) — walks the tree.
    ///
    /// v1.3: the tab-bar and a future pane-close affordance will read this.
    /// Currently only exercised by tests; the allow keeps the gate clean
    /// until the UI consumer lands.
    #[allow(dead_code)]
    pub(crate) fn pane_count(&self) -> usize {
        self.split_tree.pane_count()
    }

    /// Create a new tab with a PTY + Terminal pair at the given size.
    ///
    /// `cwd` — if `Some(path)`, the shell starts in that directory (via
    /// `chdir` in the child process before exec). Pass `None` to inherit
    /// the weft process's cwd. Using `chdir` instead of sending a `cd`
    /// command keeps the initial tab clean — no `cd` appears in the
    /// terminal, shell history, or block tracker.
    pub fn new(
        rows: usize,
        cols: usize,
        scrollback_lines: usize,
        proxy: &winit::event_loop::EventLoopProxy<AppEvent>,
        cwd: Option<&str>,
    ) -> Self {
        let pane = Pane::spawn(rows, cols, scrollback_lines, proxy, cwd);
        Self::from_single_pane(pane)
    }

    /// PTY-less test constructor — terminal/pty stay None. Production code
    /// goes through `Tab::new` (a real PTY spawn); v1.12.23 audit batch 2
    /// narrowed this to `#[cfg(test)]` since tests are its only callers
    /// (the only way to build a Tab without spawning a PTY).
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self::from_single_pane(Pane::empty())
    }

    /// Build a tab from a single starting pane. The pane becomes the root
    /// of the split tree and the active pane. Used by `new` / `empty` and
    /// by tests that construct a pane manually.
    fn from_single_pane(pane: Pane) -> Self {
        let pane_id = PaneId(pane.pane_session_id);
        let mut panes = HashMap::new();
        panes.insert(pane_id, pane);
        Self {
            session_id: NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed),
            split_tree: SplitTree::new(pane_id),
            panes,
            active_pane: pane_id,
            pending_alt_rescale: false,
            alt_flip_history: HashMap::new(),
            alt_rescale_last_taken: None,
            resize_output_probe: None,
        }
    }

    /// Test-only constructor: build a tab wrapping a single pre-built pane.
    /// Used by snapshot / scroll tests that need a Terminal without spawning
    /// a PTY.
    #[cfg(test)]
    pub(crate) fn with_single_pane(pane: Pane) -> Self {
        Self::from_single_pane(pane)
    }

    /// Bump and return the next per-tab input-event sequence number.
    ///
    /// Currently called at the keyboard-encode chokepoint only. IME commit,
    /// mouse gestures, and paste are not yet wired — extending coverage to
    /// those sources is a follow-up if cross-source trace correlation is
    /// needed. The counter is per-process monotonic (the static allocator
    /// never resets), so two tabs never share a sequence number — useful for
    /// cross-tab log correlation during free testing.
    pub(crate) fn next_input_seq(&mut self) -> u64 {
        self.active_mut().next_input_seq()
    }

    /// Current input-event sequence (last value returned by `next_input_seq`).
    /// Read from trace points that observe an Effect but did not themselves
    /// bump the counter (e.g. `drain_effects`).
    pub(crate) fn input_seq(&self) -> u64 {
        self.active().input_seq
    }

    /// Grid dimensions of the active pane for the current split layout.
    /// Unlike the full content-area dimensions, this remains equal to the
    /// active Terminal grid after a split and is safe for redraw convergence.
    pub(crate) fn active_pane_dimensions_for_rect(
        &self,
        content_rect: weft_core::pane_layout::Rect,
        cell_w: f32,
        cell_h: f32,
    ) -> Option<(usize, usize)> {
        if cell_w <= 0.0 || cell_h <= 0.0 {
            return None;
        }
        let rect = self
            .split_tree
            .layout(content_rect)
            .into_iter()
            .find_map(|(id, rect)| (id == self.active_pane).then_some(rect))?;
        let [x0, y0, x1, y1] = rect;
        let pane_width = (x1 - x0).max(0.0);
        let pane_height = (y1 - y0).max(0.0);
        // v1.10.19/25 Batch 2: cols track the pane's terminal cols *kind* and
        // must not swing with transient DEC 1049 toggles (see
        // `Terminal::tui_cols_kind`; Content for primary TUIs, Full on alt).
        // v1.10.27 (FIX_RESIZE_DOUBLE_REDRAW): `burst_locked_cols` freezes
        // rows+cols at the pane's current grid size while a toggle storm is
        // fresh — desired == current → the shared drift check queues nothing
        // mid-storm, so the app gets a single post-quiet redraw (tab/resize.rs).
        let (rows, cols) =
            self.burst_locked_cols(self.active_pane, pane_width, pane_height, cell_w, cell_h);
        (rows > 0 && cols > 0).then_some((rows, cols))
    }

    /// v1.3 Batch 6: Resize every pane's terminal according to its split-tree-
    /// computed rect. `content_rect` is the full content area (chrome already
    /// subtracted); `cell_w` / `cell_h` are physical-pixel cell dimensions.
    /// Each pane's (rows, cols) is derived from its rect size ÷ cell size.
    /// Returns `true` if the active pane was resized.
    ///
    /// For single-pane tabs this is equivalent to the pane-level
    /// `resize_terminal_and_queue` — the split tree returns one rect equal to
    /// `content_rect`.
    pub(crate) fn resize_all_panes_for_rect(
        &mut self,
        content_rect: weft_core::pane_layout::Rect,
        cell_w: f32,
        cell_h: f32,
    ) -> bool {
        if cell_w <= 0.0 || cell_h <= 0.0 {
            return false;
        }
        let layouts = self.split_tree.layout(content_rect);
        let active = self.active_pane;
        let mut active_resized = false;
        for (pane_id, rect) in layouts {
            let [x0, y0, x1, y1] = rect;
            let pane_width = (x1 - x0).max(0.0);
            let pane_height = (y1 - y0).max(0.0);
            // v1.10.19/25 Batch 2: cols track the pane's terminal cols kind and
            // must not swing with transient DEC 1049 toggles — the 99↔102 flip
            // re-queues a PTY resize each cycle, feeding the SIGWINCH loop.
            // v1.10.27 (FIX_RESIZE_DOUBLE_REDRAW): `burst_locked_cols` freezes
            // rows+cols at the current grid size during a toggle storm
            // (identically to `active_pane_dimensions_for_rect`), so the
            // queued PTY resize is a no-op mid-storm.
            let (rows, cols) =
                self.burst_locked_cols(pane_id, pane_width, pane_height, cell_w, cell_h);
            if rows == 0 || cols == 0 {
                continue;
            }
            if let Some(pane) = self.panes.get_mut(&pane_id) {
                if pane.resize_terminal_and_queue(rows, cols) && pane_id == active {
                    active_resized = true;
                }
            }
        }
        active_resized
    }

    /// v1.3 Batch 6: Find the pane whose split-tree-computed rect contains
    /// the physical-pixel point `(x, y)`. Used by mouse hit-testing to route
    /// clicks / scrolls to the correct pane. Returns `None` if the point is
    /// outside all panes (e.g. in the padding or chrome).
    pub(crate) fn pane_hit_test(
        &self,
        x: f32,
        y: f32,
        content_rect: weft_core::pane_layout::Rect,
    ) -> Option<PaneId> {
        let layouts = self.split_tree.layout(content_rect);
        layouts.into_iter().find_map(|(id, rect)| {
            let [x0, y0, x1, y1] = rect;
            (x >= x0 && x < x1 && y >= y0 && y < y1).then_some(id)
        })
    }

    /// v1.11.15 (FIX A, PLAN_v11115 §1.2): whether the active pane's PTY
    /// reader observed this session's mouse-disable sequence (or its exit).
    /// While true, no hover/wheel bytes may be written to this pane — the
    /// shell behind the in-flight disable may already be in cooked mode.
    /// Per-pane by design: only THIS session's sends are suppressed.
    pub fn mouse_suppressed(&self) -> bool {
        weft_core::input::is_suppressed(&self.mouse_suppress)
    }

    /// v1.11.15 (FIX D, PLAN_v11115 §4): copy the active pane's negotiated
    /// input modes into `input_handler` before mouse-event encoding. One
    /// method closes three sync gaps (Release path, TerminalOwner move
    /// shortcut, non-active owner tabs) — it reads the TARGET tab.
    /// Borrow-split precedent: mouse_controller.rs's sync.
    pub fn sync_mouse_modes(&mut self) {
        // T10: mode flags read under ONE short guard ending at the `drop`
        // below; the `input_handler` writes afterwards are lock-free pane
        // fields (reads extracted first). Re-entrancy (D9 rule 5): this
        // method locks — never call it inside another guard scope.
        let Some(terminal) = self.lock_terminal() else {
            return;
        };
        let mouse_protocol = terminal.mouse_protocol();
        let sgr_mouse = terminal.sgr_mouse();
        let app_cursor_keys = terminal.app_cursor_keys();
        drop(terminal);
        self.input_handler.app_cursor_keys = app_cursor_keys;
        self.input_handler.mouse_protocol = mouse_protocol;
        self.input_handler.sgr_mouse = sgr_mouse;
    }

    /// Deliver one PTY-native interrupt without discarding command output.
    ///
    /// Ctrl+C is always one ETX byte, regardless of whether the foreground app
    /// uses the alternate screen, primary-screen cursor addressing, raw mode,
    /// SSH, or a conventional shell command. Output is intentionally retained:
    /// interactive tools write confirmation/resume tails after the first or
    /// second Ctrl+C, and a post-write `tcflush` can race away the ETX itself.
    pub fn interrupt_pty(&mut self) -> bool {
        self.active_mut().interrupt_pty()
    }

    /// Write input initiated by the user to this tab's PTY.
    ///
    /// A first Ctrl+C may freeze the current primary-screen transcript while
    /// an interactive program decides whether to exit. Any later user input
    /// means the program continued, so that frozen transcript is stale and
    /// must not be reused by a future interrupt. Keeping this cancellation at
    /// the PTY boundary covers keyboard, paste, mouse, wheel, workflow, and
    /// delayed TUI-scroll input uniformly.
    ///
    /// v1.11.15 (FIX E): returns the bytes the PTY actually accepted
    /// (`Pty::write_sync_reported`) so a partial write is observable;
    /// existing `is_ok()` / `let _ =` callers compile unchanged, and the
    /// paste path consumes the count for a truncation toast.
    pub fn write_user_input(&mut self, data: &[u8]) -> weft_core::pty::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        // Genuine PTY input is the semantic boundary for returning to the
        // live view — EXCEPT while a command is executing. During
        // CommandExecuting, keystrokes are input TO the running program (a
        // primary-screen TUI like openclaw, or an interactive prompt), not a
        // request to leave history browsing. Snapping here would clear
        // `primary_history_view`, and when the program's redraw then crosses
        // the TUI detection threshold, `show_block_view()` flips to the live
        // grid mid-interaction — the openclaw "jump to top" symptom. The
        // redraw_controller's follow logic already snaps on fresh output when
        // appropriate, so this snap is redundant during execution anyway.
        // [P2 TOCTOU 登记·接受一帧滞后] executing=probe 短锁快照，与分支动
        // 作之间 worker 可翻转 phase：最坏单键入走错分支、下一键自纠；PTY
        // 写两分支均无条件执行，无输入丢失/数据损坏。
        let executing = self
            .with_terminal(|t| {
                t.block_tracker().phase() == weft_core::blocks::ShellPhase::CommandExecuting
            })
            .unwrap_or(false);
        if !executing {
            self.snap_to_bottom();
        } else if self
            .with_terminal(|t| t.is_alt_screen_active())
            .unwrap_or(false)
        {
            // Round 4 review (LOW-1): inside an alternate-screen child
            // (vim/less) the BlockView is not rendered — a snapshot bypass
            // window has no consumer. Keep the keystroke from arming a
            // window that would only rescan the frozen primary document.
            //
            // v1.10.12: any keystroke also exits the alt-screen history peek
            // (the input is meant for the TUI, so return to its live grid and
            // reset the browse position to the tail for the next peek).
            self.snap_to_bottom();
        } else {
            // v1.10.4: EVERY keystroke during execution drives the running
            // program's repaint. A screen-owned primary-screen TUI (openclaw)
            // has suspended print capture, so its live block updates only via
            // snapshot refresh — arm the rate-limit bypass window on every
            // keystroke, whether or not the user is browsing history. Without
            // this, a keypress right after the user scrolled back to the tail
            // (history browsing off) is gated by the 50ms snapshot rate limit
            // and the selection change lags a blink (the persistent flicker).
            // The redraw's split pty batches (cursor-move header, then
            // content) each get an unthrottled snapshot inside the window.
            self.primary_history_refresh.arm_force();
            // v1.10.6: also refresh the snapshot NOW (synchronously) so the
            // precise cursor line is available for the next paint frame —
            // even if no PTY output arrives (e.g. IME preedit before commit,
            // or a TUI that redraws on the next vsync). Without this the
            // cursor_snapshot_line stays None until the force window is
            // consumed, and the caret/preedit fall back to the imprecise
            // formula (which lands on the wrong row).
            self.with_terminal(|terminal| {
                if terminal.show_block_view() {
                    terminal.snapshot_primary_screen_output_for_caret();
                }
            });
        }
        let pane = self.active_mut();
        pane.with_terminal(|t| t.cancel_primary_screen_interrupt_capture());
        // T10 P2 (D4): through the shared write half — this lock serializes
        // against the parse worker's VT replies at the fd boundary.
        let writer = pane.writer.as_ref().ok_or_else(|| {
            PtyError::Write(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "tab has no PTY",
            ))
        })?;
        writer.write_sync_reported(data)
    }

    /// Directory inherited by a newly-created sibling tab. Prefer live OSC 7
    /// state, retaining a restored cwd until the shell reports one.
    pub fn launch_cwd(&self) -> Option<String> {
        self.with_terminal(|t| t.cwd().map(str::to_owned))
            .flatten()
            .or_else(|| self.restored_cwd_fallback().map(str::to_owned))
    }
}

#[cfg(test)]
#[path = "tab/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "tab/snapshot_tests.rs"]
mod snapshot_tests;

#[cfg(test)]
#[path = "tab/tui_render_mode_tests.rs"]
mod tui_render_mode_tests;
