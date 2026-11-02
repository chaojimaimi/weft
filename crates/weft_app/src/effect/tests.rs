//! Inline tests for `effect`, extracted verbatim from `effect.rs` (F6-2,
//! PLAN_v1183_F6_CASCADE_TAB_SCOPE §2-F6-2) to keep the production file
//! under the 800-line architecture gate. See `effect.rs` (where
//! `#[cfg(test)] mod tests;` declares this module — Rust 2018 sibling-file
//! resolution, same shape as `tab.rs` + `tab/tests.rs`).
//!
//! The `pending_resize_effects` rows are the F6 truth table: the ready gate
//! compares session ids (active session → always ready; cross-tab session
//! → only past the 100ms settle debounce), the per-pane interval gate and
//! its alt-screen bypass stay as FIX-α pinned them. NOTE: the third
//! parameter changed meaning in F6 — it is the bare `elapsed_debounce` leg
//! now (the zoom force bit is retired); every row below passes `true` as
//! "debounce elapsed", never as "animation force".

use super::{
    close_tab_effects, context_clipboard_effects, copy_clipboard_effects, ime_commit_effects,
    passthrough_key_effects, pending_resize_effects, process_message_effects, Effect,
    PendingPaneResize, RESIZE_COMMIT_MIN_INTERVAL,
};
use std::time::{Duration, Instant};

#[test]
fn slash_and_question_mark_each_emit_exactly_one_raw_pty_write() {
    for byte in *b"/?" {
        assert_eq!(
            passthrough_key_effects(2, vec![byte]),
            [Effect::WritePty {
                session_id: 2,
                bytes: vec![byte],
            }]
        );
    }
}

#[test]
fn ime_commit_is_raw_utf8_write_followed_by_redraw() {
    assert_eq!(
        ime_commit_effects(1, "搜索"),
        [
            Effect::WritePty {
                session_id: 1,
                bytes: "搜索".as_bytes().to_vec(),
            },
            Effect::RequestRedraw,
        ]
    );
}

#[test]
fn ctrl_c_uses_one_atomic_interrupt_before_redraw() {
    assert_eq!(
        passthrough_key_effects(0, vec![0x03]),
        [
            Effect::InterruptPty { session_id: 0 },
            Effect::RequestRedraw,
        ]
    );
}

#[test]
fn active_resize_flushes_immediately_while_background_tabs_wait() {
    use weft_core::pane_layout::PaneId;
    let pending = [
        (
            1,
            vec![PendingPaneResize::new(
                PaneId(1),
                (30, 100),
                false,
                false,
                None,
            )],
        ),
        (
            2,
            vec![PendingPaneResize::new(
                PaneId(2),
                (40, 120),
                false,
                false,
                None,
            )],
        ),
        (3, vec![]),
    ];
    // F6 semantic note: `false` = the debounce has NOT elapsed, so the
    // cross-tab session 1 waits while the active session 2 flushes. Same
    // reading as the pre-F6 `cascade_settled=false` row.
    assert_eq!(
        pending_resize_effects(&pending, 2, false, Instant::now()),
        [Effect::ResizePty {
            session_id: 2,
            pane_id: PaneId(2),
            rows: 40,
            cols: 120,
        }]
    );
}

#[test]
fn settled_resize_emits_only_latest_pending_dimensions_per_tab() {
    use weft_core::pane_layout::PaneId;
    let pending = [
        (
            0,
            vec![PendingPaneResize::new(
                PaneId(1),
                (44, 132),
                false,
                false,
                None,
            )],
        ),
        (
            1,
            vec![PendingPaneResize::new(
                PaneId(2),
                (36, 90),
                false,
                false,
                None,
            )],
        ),
    ];
    // F6 semantic note: `true` = the settle debounce HAS elapsed (this is
    // the cross-tab settle-once commit row — never a force row; the force
    // leg retired with F6).
    assert_eq!(
        pending_resize_effects(&pending, 0, true, Instant::now()),
        [
            Effect::ResizePty {
                session_id: 0,
                pane_id: PaneId(1),
                rows: 44,
                cols: 132,
            },
            Effect::ResizePty {
                session_id: 1,
                pane_id: PaneId(2),
                rows: 36,
                cols: 90,
            },
        ]
    );
}

#[test]
fn multi_pane_resize_emits_one_effect_per_pane() {
    use weft_core::pane_layout::PaneId;
    // Tab 0 has two panes pending resize; tab 1 has one.
    let pending = [
        (
            0,
            vec![
                PendingPaneResize::new(PaneId(1), (30, 80), false, false, None),
                PendingPaneResize::new(PaneId(2), (30, 40), false, false, None),
            ],
        ),
        (
            1,
            vec![PendingPaneResize::new(
                PaneId(3),
                (40, 100),
                false,
                false,
                None,
            )],
        ),
    ];
    // F6 semantic note: `true` = debounce elapsed (the cross-tab session 1
    // rides the settle path; the per-pane fan-out is what this row pins).
    let effects = pending_resize_effects(&pending, 0, true, Instant::now());
    assert_eq!(effects.len(), 3);
    assert!(effects.iter().any(|e| matches!(
        e,
        Effect::ResizePty {
            pane_id: PaneId(2),
            rows: 30,
            cols: 40,
            ..
        }
    )));
}

#[test]
fn synchronized_panes_defer_resize_without_blocking_ready_siblings() {
    use weft_core::pane_layout::PaneId;
    let pending = [(
        0,
        vec![
            PendingPaneResize::new(PaneId(1), (30, 80), true, false, None),
            PendingPaneResize::new(PaneId(2), (30, 40), false, false, None),
        ],
    )];

    assert_eq!(
        pending_resize_effects(&pending, 0, false, Instant::now()),
        [Effect::ResizePty {
            session_id: 0,
            pane_id: PaneId(2),
            rows: 30,
            cols: 40,
        }]
    );

    let committed_after_frame = [(
        0,
        vec![PendingPaneResize::new(
            PaneId(1),
            (30, 80),
            false,
            false,
            None,
        )],
    )];
    assert_eq!(
        pending_resize_effects(&committed_after_frame, 0, false, Instant::now()),
        [Effect::ResizePty {
            session_id: 0,
            pane_id: PaneId(1),
            rows: 30,
            cols: 80,
        }]
    );
}

// ── FIX-α (FIX_DRAG_RESIZE_STUTTER): per-pane commit-interval throttle ──
// All four states use synthetic `Instant` construction (same style as
// tab/resize.rs) — no real sleeps.

/// State 1 — a commit inside the interval is filtered: during a live
/// drag the pending slot is refreshed every frame, but at most one
/// reflow per pane per `RESIZE_COMMIT_MIN_INTERVAL` may reach the apply
/// point. 5ms sits inside the B1 8ms interval.
#[test]
fn resize_inside_commit_interval_is_filtered() {
    use weft_core::pane_layout::PaneId;
    let now = Instant::now();
    let pending = [(
        1,
        vec![PendingPaneResize::new(
            PaneId(1),
            (30, 100),
            false,
            false,
            Some(now - Duration::from_millis(5)),
        )],
    )];
    assert!(
        pending_resize_effects(&pending, 1, false, now).is_empty(),
        "a commit 5ms after the last one must wait out the 8ms interval"
    );
}

/// State 1 flip anchor (v1.12.2, PLAN_S2_render B1): the original state-1
/// fixture used a 10ms-old stamp and asserted suppression under the old
/// 30ms interval. At 8ms the same fixture flips to ADMITTED — 10ms is
/// past the interval. This test pins the downgrade: if someone restores
/// a larger interval (≥10ms) without revisiting the docs, this fails.
#[test]
fn resize_commit_10ms_after_last_is_admitted_at_8ms_interval() {
    use weft_core::pane_layout::PaneId;
    let now = Instant::now();
    let pending = [(
        1,
        vec![PendingPaneResize::new(
            PaneId(1),
            (30, 100),
            false,
            false,
            Some(now - Duration::from_millis(10)),
        )],
    )];
    assert_eq!(
        pending_resize_effects(&pending, 1, false, now),
        [Effect::ResizePty {
            session_id: 1,
            pane_id: PaneId(1),
            rows: 30,
            cols: 100,
        }],
        "10ms after the last commit is past the 8ms interval — must emit"
    );
}

/// B1 invariant anchor: the FIX-α margin argument (constant doc, and the
/// original rationale in FIX_DRAG_RESIZE_STUTTER.md) requires interval +
/// one 60Hz frame budget to stay far inside the 100ms active-redraw
/// window (`App::about_to_wait`'s post-Resized redraw proxy — winit 0.30
/// has no drag-end event). Anchored against the real constant so a
/// future bump that breaks the margin fails here instead of silently
/// delaying the final drag commit past the window.
#[test]
fn commit_interval_margin_invariant_holds_at_8ms() {
    let frame_budget = Duration::from_millis(16);
    let active_window = Duration::from_millis(100);
    assert_eq!(
        RESIZE_COMMIT_MIN_INTERVAL,
        Duration::from_millis(8),
        "B1 pinned the interval at 8ms; revisit the margin + doc if this changes"
    );
    assert!(
        RESIZE_COMMIT_MIN_INTERVAL + frame_budget < active_window,
        "interval {:?} + frame budget {:?} must stay inside the {:?} \
         active-redraw window",
        RESIZE_COMMIT_MIN_INTERVAL,
        frame_budget,
        active_window
    );
}

/// State 2 — once the interval expires the resize is admitted again
/// (`>=` boundary: exactly `RESIZE_COMMIT_MIN_INTERVAL` old is due).
#[test]
fn resize_admitted_once_commit_interval_expires() {
    use weft_core::pane_layout::PaneId;
    let now = Instant::now();
    let pending = [(
        1,
        vec![PendingPaneResize::new(
            PaneId(1),
            (30, 100),
            false,
            false,
            Some(now - RESIZE_COMMIT_MIN_INTERVAL),
        )],
    )];
    assert_eq!(
        pending_resize_effects(&pending, 1, false, now),
        [Effect::ResizePty {
            session_id: 1,
            pane_id: PaneId(1),
            rows: 30,
            cols: 100,
        }]
    );
}

/// State 3 (review P3) — the ioctl-failure retry must never get stuck.
/// The stamp is taken only at APPLY SUCCESS, so a pending whose stamp is
/// stale (or absent) re-emits on every drain, however frequent. The
/// function is side-effect free: repeated calls keep emitting, which is
/// exactly what keeps a failed ioctl's retained request alive.
#[test]
fn failed_ioctl_retry_keeps_re_emitting_every_drain() {
    use weft_core::pane_layout::PaneId;
    let start = Instant::now();
    let pending = [(
        1,
        vec![PendingPaneResize::new(
            PaneId(1),
            (30, 100),
            false,
            false,
            Some(start - Duration::from_secs(3600)),
        )],
    )];
    for drain in 0..5u32 {
        // Drains 1ms apart — rapid enough that a stamp-on-emission design
        // would suppress everything after the first.
        let now = start + Duration::from_millis(u64::from(drain));
        assert_eq!(
            pending_resize_effects(&pending, 1, false, now).len(),
            1,
            "drain {drain}: a stale-stamped pending must keep flowing — retries are never throttled"
        );
    }
}

/// State 4 — alt-screen panes bypass the interval entirely: their resize
/// takes the dimension-only `resize_dims` path (no reflow), so even a
/// brand-new stamp must not delay them.
#[test]
fn alt_screen_resize_bypasses_commit_interval() {
    use weft_core::pane_layout::PaneId;
    let now = Instant::now();
    let pending = [(
        1,
        vec![PendingPaneResize::new(
            PaneId(1),
            (30, 100),
            false,
            true,
            Some(now),
        )],
    )];
    assert_eq!(
        pending_resize_effects(&pending, 1, false, now),
        [Effect::ResizePty {
            session_id: 1,
            pane_id: PaneId(1),
            rows: 30,
            cols: 100,
        }]
    );
}

// ── F6 truth table (PLAN_v1183_F6_CASCADE_TAB_SCOPE §2-F6-2) ────────────
// The five ready-gate states, re-anchored onto same-session constructs
// (review P1-2). `pending_resize_effects` is pure — animation state never
// enters it (the inactive branch always reads the debounce), so no
// animation stubs are needed.

/// F6 state 1 — regression lock: a cross-tab (non-active) session with the
/// debounce NOT elapsed emits ZERO effects. The retired Phase C force bit
/// passed this row (per-step commit of every background tab — the v1.18.3
/// field root cause); this fails if anyone reintroduces a force leg into
/// the ready gate.
#[test]
fn inactive_session_before_debounce_emits_zero_effects() {
    use weft_core::pane_layout::PaneId;
    let now = Instant::now();
    let pending = [(
        1,
        vec![PendingPaneResize::new(
            PaneId(1),
            (30, 100),
            false,
            false,
            None,
        )],
    )];
    assert!(
        pending_resize_effects(&pending, 2, false, now).is_empty(),
        "cross-tab session before the settle debounce must emit nothing \
         (F6 anti-regression lock)"
    );
}

/// F6 state 2 — the settle-once commit: a cross-tab session past the
/// debounce emits EXACTLY ONE effect carrying the final coalesced size
/// (every Resized overwrote the single pending slot; one ioctl = one
/// shell repaint, not one per animation step).
#[test]
fn inactive_session_after_debounce_emits_one_final_size() {
    use weft_core::pane_layout::PaneId;
    let now = Instant::now();
    let pending = [(
        1,
        vec![PendingPaneResize::new(
            PaneId(1),
            (40, 120),
            false,
            false,
            None,
        )],
    )];
    assert_eq!(
        pending_resize_effects(&pending, 2, true, now),
        [Effect::ResizePty {
            session_id: 1,
            pane_id: PaneId(1),
            rows: 40,
            cols: 120,
        }],
        "cross-tab session past the debounce commits exactly once, at the \
         final size"
    );
}

/// F6 state 3 — Phase C split-pane regression lock: the ACTIVE session is
/// always ready (independent of the debounce), so its panes — including an
/// alt pane — emit in ONE drain even with the debounce unelapsed. The
/// per-step commit that keeps split+top panes from tearing rides THIS row
/// after the F6 cross-tab narrowing.
#[test]
fn active_session_commits_all_panes_regardless_of_debounce() {
    use weft_core::pane_layout::PaneId;
    let now = Instant::now();
    let pending = [(
        7,
        vec![
            PendingPaneResize::new(PaneId(1), (30, 80), false, false, None),
            PendingPaneResize::new(PaneId(2), (30, 40), false, true, None),
        ],
    )];
    assert_eq!(
        pending_resize_effects(&pending, 7, false, now).len(),
        2,
        "the active session's panes commit every drain (ready=true carries \
         the Phase C per-step commit for same-session panes)"
    );
}

/// F6 state 4 — same-session alt pane: the `alt_active` due bypass still
/// emits immediately with a brand-new stamp while the throttled sibling
/// (inside the 8ms interval) waits. ready=true carries Phase C's original
/// motive; alt bypasses only `due`, never `ready` (review P1-2).
#[test]
fn same_session_alt_pane_is_immediate_while_throttled_sibling_waits() {
    use weft_core::pane_layout::PaneId;
    let now = Instant::now();
    let pending = [(
        1,
        vec![
            PendingPaneResize::new(
                PaneId(1),
                (30, 80),
                false,
                false,
                Some(now - Duration::from_millis(1)),
            ),
            PendingPaneResize::new(PaneId(2), (30, 40), false, true, Some(now)),
        ],
    )];
    assert_eq!(
        pending_resize_effects(&pending, 1, false, now),
        [Effect::ResizePty {
            session_id: 1,
            pane_id: PaneId(2),
            rows: 30,
            cols: 40,
        }],
        "the same-session alt pane bypasses the due gate; the throttled \
         sibling waits out the interval"
    );
}

/// F6 state 5 — the `!resize.synchronized` skip survives the ready-gate
/// narrowing: a synchronized pane stays deferred even when the cross-tab
/// debounce makes its session ready (the skip is orthogonal to `ready`).
#[test]
fn synchronized_pane_stays_skipped_after_debounce_ready() {
    use weft_core::pane_layout::PaneId;
    let now = Instant::now();
    let pending = [(
        1,
        vec![
            PendingPaneResize::new(PaneId(1), (30, 80), true, false, None),
            PendingPaneResize::new(PaneId(2), (30, 40), false, false, None),
        ],
    )];
    assert_eq!(
        pending_resize_effects(&pending, 2, true, now),
        [Effect::ResizePty {
            session_id: 1,
            pane_id: PaneId(2),
            rows: 30,
            cols: 40,
        }],
        "synchronized skip holds on the debounce-settled cross-tab row; \
         the plain sibling still flows"
    );
}

#[test]
fn clipboard_effect_rejects_absent_or_empty_selection() {
    assert!(copy_clipboard_effects(None).is_empty());
    assert!(copy_clipboard_effects(Some(String::new())).is_empty());
    assert_eq!(
        copy_clipboard_effects(Some("selected".into())),
        [Effect::CopyClipboard {
            text: "selected".into(),
        }]
    );
}

#[test]
fn context_copy_preserves_empty_text_as_clear_clipboard_action() {
    assert_eq!(
        context_clipboard_effects(Some(String::new())),
        [Effect::CopyClipboard {
            text: String::new(),
        }]
    );
    assert!(context_clipboard_effects(None).is_empty());
}

#[test]
fn close_tab_effects_preserve_exit_persist_and_redraw_order() {
    // Last tab: TabClosed (is_last=true) + Exit + PersistTabs.
    assert_eq!(
        close_tab_effects(0, 0, true),
        [
            Effect::TabClosed {
                removed_idx: 0,
                new_active: 0,
                is_last: true
            },
            Effect::Exit,
            Effect::PersistTabs,
        ]
    );
    // Non-last tab: TabClosed (is_last=false) + RequestRedraw + PersistTabs.
    assert_eq!(
        close_tab_effects(2, 1, false),
        [
            Effect::TabClosed {
                removed_idx: 2,
                new_active: 1,
                is_last: false
            },
            Effect::RequestRedraw,
            Effect::PersistTabs,
        ]
    );
}

#[test]
fn shell_exit_keeps_completed_blocks_before_redraw() {
    use std::time::SystemTime;
    use weft_core::blocks::{Block, BlockId};

    let completed = Block {
        id: BlockId(9),
        command: "exit".into(),
        cwd: None,
        output: "done".into(),
        styled_output: None,
        exit_code: Some(0),
        started_at: SystemTime::UNIX_EPOCH,
        finished_at: Some(SystemTime::UNIX_EPOCH),
        collapsed: false,
        screen_origin: false,
    };
    assert_eq!(
        process_message_effects(true, vec![completed.clone()], true),
        [
            Effect::Exit,
            Effect::PersistBlocks {
                blocks: vec![completed]
            },
            Effect::RequestRedraw,
        ]
    );
}
