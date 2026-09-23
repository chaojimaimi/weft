//! Headless unit tests for the zoom frame cache (pure decision logic:
//! watermark dedup, reentry guard, stash rules, lean scissor clamping).
//! Metal-object paths stay compile+field verified (no device in CI).

use super::*;

fn content_cache() -> FrameCache {
    let mut cache = FrameCache::empty();
    let replaced = cache.apply_stash(
        vec![0.0; 24],
        vec![0.0; 16],
        Vec::new(),
        (0.1, 0.2, 0.3, 1.0),
        (800.0, 600.0),
        Vec::new(),
        Vec::new(),
        (800.0, 600.0),
    );
    assert!(replaced, "a full stash must report replacement");
    cache
}

#[test]
fn watermark_dedup_truth_table() {
    // First frame (never presented): pull.
    assert!(watermark_requests_pull(None, (800.0, 600.0)));
    // Same size already on screen: skip.
    assert!(!watermark_requests_pull(
        Some((800.0, 600.0)),
        (800.0, 600.0)
    ));
    // Sub-pixel jitter within the 0.5 px tolerance (LOW-1, vp_mismatch
    // convention): skip -- the on-screen frame already covers this size.
    assert!(!watermark_requests_pull(
        Some((800.0, 600.0)),
        (800.4, 600.3)
    ));
    // Beyond tolerance: pull.
    assert!(watermark_requests_pull(
        Some((800.0, 600.0)),
        (801.0, 600.0)
    ));
    // Different size: pull.
    assert!(watermark_requests_pull(
        Some((800.0, 600.0)),
        (900.0, 700.0)
    ));
}

#[test]
fn reentry_guard_blocks_nested_entry_and_releases_on_drop() {
    let flag = AtomicBool::new(false);
    let guard = ReentryGuard::try_enter(&flag).expect("first entry succeeds");
    assert!(
        ReentryGuard::try_enter(&flag).is_none(),
        "nested entry while held must fail"
    );
    drop(guard);
    assert!(
        ReentryGuard::try_enter(&flag).is_some(),
        "dropping the guard must release the slot"
    );
}

/// The E-3 nesting rule, end to end: an idle frame advances ONLY the
/// watermark, so the displayLayer nested in the following flush sees
/// requested == watermark and skips (no overwrite of the fresh frame).
#[test]
fn idle_stash_keeps_content_but_advances_watermark_blocks_nested_pull() {
    let mut cache = content_cache();
    let content_before = cache.vertices.clone();
    // Idle path: all streams empty, drawable grew to 900x700.
    let replaced = cache.apply_stash(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        (0.9, 0.9, 0.9, 1.0),
        (900.0, 700.0),
        Vec::new(),
        Vec::new(),
        (900.0, 700.0),
    );
    assert!(!replaced, "an idle stash must NOT report replacement");
    assert_eq!(cache.vertices, content_before, "content must be kept");
    assert_eq!(cache.viewport, (800.0, 600.0), "viewport must be kept");
    assert_eq!(
        cache.clear_color,
        (0.1, 0.2, 0.3, 1.0),
        "clear color must be kept"
    );
    assert_eq!(cache.last_presented, Some((900.0, 700.0)));
    // The nested displayLayer request for the new size must SKIP.
    assert!(!watermark_requests_pull(
        cache.last_presented,
        (900.0, 700.0)
    ));
}

#[test]
fn main_path_stash_replaces_all_streams_and_watermark() {
    let mut cache = content_cache();
    let replaced = cache.apply_stash(
        vec![1.0; 12],
        Vec::new(),
        vec![2.0; 32],
        (0.0, 0.5, 1.0, 1.0),
        (1000.0, 800.0),
        Vec::new(),
        Vec::new(),
        (1000.0, 800.0),
    );
    assert!(replaced);
    assert_eq!(cache.vertices, vec![1.0; 12]);
    assert!(cache.bg_stream.is_empty(), "empty stream replaces old");
    assert_eq!(cache.glyph_stream, vec![2.0; 32]);
    assert_eq!(cache.clear_color, (0.0, 0.5, 1.0, 1.0));
    assert_eq!(cache.viewport, (1000.0, 800.0));
    assert_eq!(cache.last_presented, Some((1000.0, 800.0)));
    assert!(cache.pullable());
}

/// An empty cache must never pull: presenting empty streams is the
/// 1.12.9 whitewash symptom (bare background flash).
#[test]
fn empty_cache_never_pulls() {
    let cache = FrameCache::empty();
    assert!(!cache.pullable());
    assert!(!cache.populated);
    // After the first real stash the cache becomes pullable.
    assert!(content_cache().pullable());
}

/// Z-f Resized-branch truth tables: drag step -> forced synchronous draw;
/// zoom animation step and non-gesture nudge -> displayLayer pull (no
/// forced draw); zoom arming only outside a live-resize gesture.
#[test]
fn resized_branch_truth_tables() {
    // Drag step: same-tick synchronous draw stays.
    assert!(forced_sync_draw_for_resize(true));
    // Zoom animation step / non-gesture nudge: pull serves the frame.
    assert!(!forced_sync_draw_for_resize(false));
    // Arming: any size change outside a gesture arms; drag steps never do.
    assert!(zoom_jump_should_arm(true, false));
    assert!(!zoom_jump_should_arm(true, true));
    assert!(!zoom_jump_should_arm(false, false));
}

/// H-2: the lean path renders into the CURRENT drawable, so a pane scissor
/// must clamp to the target as well as the cached viewport -- zoom-out makes
/// the target SMALLER and an unclamped edge rect would run past the render
/// attachment (Metal validation UB). Zoom-in keeps the viewport clamp (the
/// main-path semantics).
#[test]
fn lean_pane_scissor_clamps_to_drawable_truth_table() {
    let viewport = (800.0, 600.0);
    // A pane rect hugging the cached viewport's right/bottom edge.
    let edge_rect = [600.0, 400.0, 800.0, 600.0];

    // Equal size: identical in both spaces -- full rect.
    assert_eq!(
        lean_pane_scissor(edge_rect, viewport, (800.0, 600.0)),
        (600, 400, 200, 200)
    );
    // Zoom-in (target larger): the viewport is still the bound.
    assert_eq!(
        lean_pane_scissor(edge_rect, viewport, (1600.0, 1200.0)),
        (600, 400, 200, 200)
    );
    // Zoom-out (target smaller): clamped to the drawable -- never past it.
    assert_eq!(
        lean_pane_scissor(edge_rect, viewport, (400.0, 300.0)),
        (400, 300, 0, 0)
    );
    // Zoom-out mid-rect: the visible part is clipped to the target edge.
    assert_eq!(
        lean_pane_scissor([200.0, 100.0, 500.0, 400.0], viewport, (400.0, 300.0)),
        (200, 100, 200, 200)
    );
    // Degenerate rect never yields a negative-width scissor.
    assert_eq!(
        lean_pane_scissor([500.0, 500.0, 100.0, 100.0], viewport, (400.0, 300.0)),
        (400, 300, 0, 0)
    );
}

/// M-2: a presentsWithTransaction bind left over past the 300 ms deadline
/// (IMP panicked between bind and unbind) is swept by the per-frame stash
/// path; a fresh bind is left alone.
#[test]
fn stale_transaction_bind_expires_but_fresh_bind_survives() {
    let mut cache = content_cache();
    // No layer handle in the headless cache: set_layer_transaction no-ops,
    // but the bind state itself must still be cleared.
    cache.tx_bound_since = Some(Instant::now() - TX_BIND_TIMEOUT - Duration::from_millis(50));
    expire_stale_tx_bind(&mut cache);
    assert!(
        cache.tx_bound_since.is_none(),
        "stale bind must be swept by the stash path"
    );

    cache.tx_bound_since = Some(Instant::now());
    expire_stale_tx_bind(&mut cache);
    assert!(
        cache.tx_bound_since.is_some(),
        "a bind inside the deadline must survive the sweep"
    );
}

/// F-3C verdict truth table: degraded = at least two zoom steps and NOT ONE
/// pull frame (the displayLayer dispatch is dead -- the 1.12.11 signature).
/// A single step is never a verdict (the accepted single-jump form), and any
/// pull frame clears it.
#[test]
fn pull_degrade_verdict_truth_table() {
    assert!(!pull_degrade_verdict(1, 0));
    assert!(pull_degrade_verdict(2, 0));
    assert!(!pull_degrade_verdict(2, 1));
    assert!(pull_degrade_verdict(5, 0));
}

/// F-3B expiry truth table: a hot window re-arms the WaitUntil wake (the
/// pending flag is irrelevant), a lapsed window with a pending flag flushes
/// exactly once, otherwise the tick is a no-op.
#[test]
fn zoom_flush_action_truth_table() {
    assert!(matches!(
        zoom_flush_action(true, false),
        ZoomFlushAction::Arm
    ));
    assert!(matches!(
        zoom_flush_action(true, true),
        ZoomFlushAction::Arm
    ));
    assert!(matches!(
        zoom_flush_action(false, true),
        ZoomFlushAction::Flush
    ));
    assert!(matches!(
        zoom_flush_action(false, false),
        ZoomFlushAction::None
    ));
}

/// Review MEDIUM-2: the two controller gate wirings are pure functions, so
/// the wiring gets a truth-table anchor here. Mutation check: deleting the
/// `!zoom_in` term in `defers_main` turns `defers_main(true, true)` red.
#[test]
fn gate_wiring_truth_tables() {
    // defers_main(active, zoom_in) = active && !zoom_in: only an ACTIVE
    // shrink-direction animation defers the main path (zoom-in keeps the
    // main path tracking live; an inactive anim never defers).
    assert!(!defers_main(false, false));
    assert!(!defers_main(false, true));
    assert!(defers_main(true, false));
    assert!(!defers_main(true, true));
    // cascade_force_commit(zoom_in) = zoom_in: the force-commit bit mirrors
    // the animation direction (the elapsed-debounce leg is composed at the
    // call site, so the pure function pins the force bit only).
    assert!(cascade_force_commit(true));
    assert!(!cascade_force_commit(false));
}

/// Serializes the global-state test below (FRAME_CACHE / PULL_ENABLED are
/// process-wide; every other test in this file stays pure-local by design).
static GLOBAL_ZOOM_STATE_LOCK: Mutex<()> = Mutex::new(());

/// F-3B suppression-gate combos through the real global cache: an empty
/// cache never suppresses (start-up draws immediately), a stale watermark
/// suppresses (the pull is the frame's only supplier), a CAUGHT-UP watermark
/// releases the gate (the expiry flush draw runs as a NORMAL draw), the
/// lever off closes it (exact 1.12.10 push-only), and teardown closes it.
#[test]
fn pull_can_freshen_gate_combinations() {
    let _guard = GLOBAL_ZOOM_STATE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    // Start-up form: empty cache -> not populated -> never suppress.
    teardown();
    assert!(!pull_can_freshen(800.0, 600.0));

    // Animation form: populated at 800x600, inner size moved on -> suppress.
    *lock() = content_cache();
    assert!(pull_can_freshen(900.0, 700.0));

    // M-1 gate (review M1): a pull present in flight closes the gate
    // regardless of the stale watermark -- the deadlock-guard line gets its
    // own pin; dropping the guard reopens it.
    let in_flight =
        ReentryGuard::try_enter(&PULL_ACTIVE).expect("guard must be free outside a present");
    assert!(!pull_can_freshen(900.0, 700.0));
    drop(in_flight);
    assert!(pull_can_freshen(900.0, 700.0));

    // Pull caught up: watermark == requested inner size -> release the gate.
    assert!(!pull_can_freshen(800.0, 600.0));

    // Degradation lever: off closes the gate regardless of the watermark
    // (E-5's degrade promise); restored afterwards.
    set_pull_enabled(false);
    assert!(!pull_can_freshen(900.0, 700.0));
    set_pull_enabled(true);

    // Teardown: cleared cache closes the gate for good.
    teardown();
    assert!(!pull_can_freshen(900.0, 700.0));
}
