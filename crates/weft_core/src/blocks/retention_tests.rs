//! §7.1 retention test matrix (v1.11.2 X4) — extracted from `blocks.rs`'s
//! inline test module for the same ceiling reason as `retention.rs`.
//!
//! Included via `#[cfg(test)] #[path]` so it never ships in release builds.

use super::{Block, BlockId, BlockTracker};
use std::collections::HashSet;
use std::time::{Duration, SystemTime};

// ── v1.11.2 X4: blocks retention (PLAN_v1112 §1 / §7.1) ────────────

/// One minimal finished command without run_one's length assertion
/// (retention may legitimately keep the Vec length flat).
fn finish_cmd(tracker: &mut BlockTracker, command: &str) {
    tracker.on_prompt_start();
    tracker.on_command_start(command.to_string());
    tracker.on_command_end(0);
}

/// A synthetic persisted-style block for load_older_to_front tests,
/// mirroring the shape `BlockStore` queries return.
fn make_block(id: u64, command: &str, age_secs: u64) -> Block {
    Block {
        id: BlockId(id),
        command: command.to_string(),
        cwd: None,
        output: String::new().into(),
        styled_output: None,
        exit_code: Some(0),
        started_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 - age_secs),
        finished_at: None,
        collapsed: false,
        screen_origin: false,
    }
}

#[test]
fn retention_exactly_at_limit_touches_nothing() {
    let mut t = BlockTracker::new();
    t.set_retained_limit(3);
    for i in 0..3 {
        finish_cmd(&mut t, &format!("c{i}"));
    }
    assert_eq!(t.blocks().len(), 3);
    assert!(t.evicted_ids().is_empty(), "no eviction at exactly the cap");
}

#[test]
fn retention_exceeding_by_one_pops_only_the_head() {
    let mut t = BlockTracker::new();
    t.set_retained_limit(3);
    for i in 0..4 {
        finish_cmd(&mut t, &format!("c{i}"));
    }
    assert_eq!(t.blocks().len(), 3, "Vec clamped back to the limit");
    assert_eq!(
        t.blocks().first().unwrap().command,
        "c1",
        "the OLDEST block is gone, newest-last order intact"
    );
    assert_eq!(t.blocks().last().unwrap().command, "c3");
    assert_eq!(t.evicted_ids(), &HashSet::from([1u64]));
}

#[test]
fn retention_accumulates_evicted_ids_across_floods() {
    let mut t = BlockTracker::new();
    t.set_retained_limit(2);
    for i in 0..5 {
        finish_cmd(&mut t, &format!("c{i}"));
    }
    assert_eq!(t.blocks().len(), 2);
    let evicted = t.evicted_ids();
    assert_eq!(evicted, &HashSet::from([1u64, 2u64, 3u64]));
    // Newest-last invariant preserved.
    assert_eq!(t.blocks()[0].command, "c3");
    assert_eq!(t.blocks()[1].command, "c4");
}

#[test]
fn retention_limit_zero_disables_eviction() {
    let mut t = BlockTracker::new();
    t.set_retained_limit(0);
    for i in 0..50 {
        finish_cmd(&mut t, &format!("c{i}"));
    }
    assert_eq!(t.blocks().len(), 50, "limit 0 must disable retention");
    assert!(t.evicted_ids().is_empty());
}

#[test]
fn lowering_limit_enforces_immediately() {
    let mut t = BlockTracker::new();
    for i in 0..5 {
        finish_cmd(&mut t, &format!("c{i}"));
    }
    t.set_retained_limit(2);
    assert_eq!(t.blocks().len(), 2);
    assert_eq!(t.blocks()[0].command, "c3");
    assert_eq!(t.evicted_ids(), &HashSet::from([1u64, 2u64, 3u64]));
}

#[test]
fn retention_removes_evicted_ids_from_side_sets() {
    // screen_owned_blocks: a screen-owned block evicted from the Vec
    // must leave the continuation scope set.
    let mut t = BlockTracker::new();
    t.set_retained_limit(1);
    t.on_prompt_start();
    t.on_command_start("tui".to_string());
    t.begin_screen_owned_output(0);
    t.replace_screen_output("banner");
    t.on_command_end(0);
    assert!(t.blocks()[0].screen_origin);
    // Exceeds the limit → pop block 1.
    finish_cmd(&mut t, "next");
    // Observable proof of the removal: id 1 appears ONLY through the
    // evicted-ids union, and a fresh screen continuation cannot pick it.
    assert!(t.evicted_ids().contains(&1), "screen-owned block 1 evicted");
    let produced = t.session_produced_block_ids();
    assert!(produced.contains(&1));
    assert_eq!(t.blocks().len(), 1);
}

#[test]
fn retention_of_loaded_block_flips_it_to_session_referenced_via_eviction() {
    // A loaded history block sitting at the head gets evicted like any
    // other. Removing it from loaded_ids is observable: after eviction
    // its id reaches session_produced_block_ids via evicted_ids (it WAS
    // persisted this session's store), keeping tab-snapshot semantics.
    let mut t = BlockTracker::new();
    t.load_blocks(vec![make_block(10, "historic", 60)]);
    t.set_retained_limit(1);
    finish_cmd(&mut t, "fresh");
    assert_eq!(t.blocks().len(), 1);
    assert_eq!(t.blocks()[0].command, "fresh");
    assert!(t.evicted_ids().contains(&10));
    let produced = t.session_produced_block_ids();
    assert!(produced.contains(&10), "evicted loaded id stays referenced");
}

#[test]
fn session_produced_block_ids_include_evicted_ids() {
    let mut t = BlockTracker::new();
    t.set_retained_limit(2);
    for i in 0..4 {
        finish_cmd(&mut t, &format!("c{i}"));
    }
    let produced = t.session_produced_block_ids();
    for id in 1u64..=4 {
        assert!(
            produced.contains(&id),
            "evicted id {id} must stay in the per-tab snapshot set"
        );
    }
    assert_eq!(produced.len(), 4, "no duplicates between vec and evicted");
}

/// v1.12.24 (N-3): lineage_block_ids = session-produced UNION loaded, so a
/// per-tab snapshot keeps referencing the previous restore's history instead
/// of dropping one generation on every restart-restore cycle. The narrower
/// session-only accessor keeps its v1.7.6 semantics side by side.
#[test]
fn lineage_block_ids_union_loaded_and_session_produced() {
    let mut t = BlockTracker::new();
    t.load_blocks(vec![
        make_block(1, "one", 30),
        make_block(2, "two", 20),
        make_block(3, "three", 10),
    ]);
    finish_cmd(&mut t, "four");
    // observe() advanced the sequence past the loaded ids → block 4.
    assert_eq!(t.session_produced_block_ids(), vec![4]);
    assert_eq!(t.lineage_block_ids(), vec![1, 2, 3, 4]);
}

/// v1.12.24 (N-3): the lineage union is deterministic — HashSet iteration
/// order must never leak into the snapshot's block_ids.
#[test]
fn lineage_block_ids_are_sorted_and_deduplicated() {
    let mut t = BlockTracker::new();
    // A loaded id that is ALSO referenced via the evicted union (loaded block
    // evicted from memory) must appear exactly once in the lineage.
    t.load_blocks(vec![make_block(10, "historic", 60)]);
    t.set_retained_limit(1);
    finish_cmd(&mut t, "fresh");
    assert!(t.evicted_ids().contains(&10));
    assert_eq!(t.lineage_block_ids(), vec![10, 11]);
}

/// v1.12.24 review P1-2: the panel's "load older" query is GLOBAL — pages
/// brought in this way are OTHER tabs' blocks. They must stay visible in
/// the tracker's block list (panel / block view) but never leak into the
/// per-tab snapshot lineage, or a restart would absorb them into this tab.
#[test]
fn load_older_ids_do_not_enter_lineage() {
    let mut t = BlockTracker::new();
    finish_cmd(&mut t, "mine");
    t.load_older_to_front(vec![
        make_block(90, "other-tab-a", 30),
        make_block(91, "other-tab-b", 90),
    ]);
    // Visible in the block list (pinned, oldest-first prepend)...
    let commands: Vec<&str> = t.blocks().iter().map(|b| b.command.as_str()).collect();
    assert_eq!(commands, vec!["other-tab-b", "other-tab-a", "mine"]);
    // ...but excluded from the snapshot lineage.
    assert_eq!(
        t.lineage_block_ids(),
        vec![1],
        "load-older pages are browsed content, not tab-owned history"
    );
    assert_eq!(t.session_produced_block_ids(), vec![1]);
}

/// Contrast guard for N-3 (P1-2 pair): the RESTORE path (`load_blocks`)
/// must keep feeding `loaded_ids` — the previous generation's ids stay in
/// the tab snapshot lineage. If this regresses, every restart-restore
/// cycle loses one ↑ recall generation again.
#[test]
fn restore_loaded_ids_still_enter_lineage() {
    let mut t = BlockTracker::new();
    t.load_blocks(vec![make_block(1, "prev-gen", 60)]);
    finish_cmd(&mut t, "fresh");
    assert_eq!(t.lineage_block_ids(), vec![1, 2]);
}

#[test]
fn retention_leaves_continuation_flow_intact() {
    // With retention tight (limit 1), the screen-continuation restore
    // path (unproven resume → base pushed back + separate new block)
    // must still behave: the oldest block is evicted, the newest stays,
    // and phase bookkeeping is unchanged.
    const REPLAY: &str = "banner line with enough content\nmodel and account information\nworking directory /tmp/project\nprompt asking for a performance report\nanswer line one with useful detail\nanswer line two with useful detail";
    let mut t = BlockTracker::new();
    t.set_retained_limit(1);
    t.on_prompt_start();
    t.on_command_start("tool".to_string());
    t.begin_screen_owned_output(0);
    t.replace_screen_output(REPLAY);
    t.on_command_end(0);
    t.drain_unpersisted();

    t.on_command_start("tool --resume missing".to_string());
    t.begin_screen_owned_output(10);
    t.replace_screen_output("banner only\nresume failed");
    t.on_command_end(1);

    // Base restored + own block appended → clamp keeps only the newest.
    assert_eq!(t.blocks().len(), 1);
    assert_eq!(t.blocks()[0].command, "tool --resume missing");
    assert!(t.evicted_ids().contains(&1));
    // A follow-up command still works normally.
    finish_cmd(&mut t, "ls");
    assert_eq!(t.blocks().last().unwrap().command, "ls");
}

#[test]
fn load_older_to_front_prepends_in_time_order() {
    let mut t = BlockTracker::new();
    t.set_retained_limit(2);
    for i in 0..3 {
        finish_cmd(&mut t, &format!("c{i}"));
    }

    // DB-shaped input: newest-first (older_than returns DESC).
    let older = vec![make_block(90, "mid-age", 30), make_block(91, "ancient", 90)];
    t.load_older_to_front(older);

    let commands: Vec<&str> = t.blocks().iter().map(|b| b.command.as_str()).collect();
    assert_eq!(
        commands,
        vec!["ancient", "mid-age", "c1", "c2"],
        "prepended ascending, time order preserved overall"
    );
    // They count as LOADED, not session-produced...
    let produced = t.session_produced_block_ids();
    assert!(!produced.contains(&90) && !produced.contains(&91));
    // ...while the previously evicted id survives in the snapshot set.
    assert!(produced.contains(&1));
    // Next fresh allocation must skip past the loaded ids.
    finish_cmd(&mut t, "after");
    assert_eq!(t.blocks().last().unwrap().id, BlockId(92));
}

#[test]
fn load_older_to_front_clears_evicted_marking() {
    let mut t = BlockTracker::new();
    t.set_retained_limit(1);
    finish_cmd(&mut t, "c0");
    finish_cmd(&mut t, "c1");
    assert!(t.evicted_ids().contains(&1));
    // Reload the evicted block from the DB side.
    t.load_older_to_front(vec![make_block(1, "c0", 5)]);
    assert!(
        !t.evicted_ids().contains(&1),
        "a re-loaded id must leave the evicted set"
    );
    assert_eq!(t.blocks().first().unwrap().command, "c0");
}

#[test]
fn load_older_to_front_empty_is_noop() {
    let mut t = BlockTracker::new();
    finish_cmd(&mut t, "only");
    t.load_older_to_front(Vec::new());
    assert_eq!(t.blocks().len(), 1);
}

/// rust-reviewer v1.11.2 Minor-4 regression: blocks the user paged back in
/// via "load older" are PINNED — the next command finalize must evict the
/// oldest NON-pinned blocks instead of dropping the page just loaded.
#[test]
fn retention_never_evicts_user_pinned_load_older_pages() {
    let mut t = BlockTracker::new();
    t.set_retained_limit(2);
    for i in 0..3 {
        finish_cmd(&mut t, &format!("c{i}"));
    }
    // Page two older blocks back in (they prepend and pin).
    let older = vec![make_block(90, "mid-age", 30), make_block(91, "ancient", 90)];
    t.load_older_to_front(older);

    // Next finalize exceeds the limit: the pinned page survives and the
    // oldest session-produced blocks are evicted instead.
    finish_cmd(&mut t, "after");
    let commands: Vec<&str> = t.blocks().iter().map(|b| b.command.as_str()).collect();
    assert_eq!(
        commands,
        vec!["ancient", "mid-age", "c2", "after"],
        "pinned page survives; eviction skips to the oldest non-pinned"
    );
    // Evicted ids remain snapshot-visible via the evicted union.
    let produced = t.session_produced_block_ids();
    assert!(produced.contains(&1) && produced.contains(&2));
    assert!(!produced.contains(&90) && !produced.contains(&91));
}
