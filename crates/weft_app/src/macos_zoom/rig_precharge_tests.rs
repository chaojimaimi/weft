//! WP2 A2 truth tables for the precharge state machine (plan §4-A2 item 6:
//! pure logic with injected phase/clock/block counts — no PTY, no
//! wall-clock). Sibling `#[path]` module keeps rig_precharge.rs clear of
//! the module-size gate (grid_cache budget_tests.rs precedent).

use super::{
    command_for, shell_wait_expired, total_budget_expired, Action, Precharge, ShellObs,
    GIANT_MIN_OUTPUT_BYTES,
};
use weft_core::blocks::ShellPhase;

fn obs(phase: ShellPhase, finished_blocks: usize) -> ShellObs {
    obs_sized(phase, finished_blocks, 0)
}

/// `obs` with the newest block's output length (the Await(0) identity check).
fn obs_sized(phase: ShellPhase, finished_blocks: usize, newest_output_len: usize) -> ShellObs {
    ShellObs {
        phase,
        finished_blocks,
        newest_output_len,
    }
}

/// The Await(0) state with `baseline` as the submit-time block count. The
/// `started` instant is construction noise — `advance` never reads it (time
/// enters only through the two expiry predicates).
fn awaiting_giant(seq_len: usize, baseline: usize) -> Precharge {
    awaiting(0, seq_len, baseline)
}

fn awaiting(index: usize, seq_len: usize, baseline: usize) -> Precharge {
    let mut state = Precharge::new(seq_len == 1, false);
    state.step = super::Step::Await(index);
    state.baseline_blocks = baseline;
    state
}

// ── WaitShell (the front guard) ─────────────────────────────────────────

#[test]
fn waitshell_holds_until_first_prompt() {
    // NotIntegrated is the tracker's default initial state; an immediate
    // check must NOT submit (plan R2 P3 guard).
    let state = Precharge::new(false, false);
    assert_eq!(
        state.advance(obs(ShellPhase::NotIntegrated, 0)),
        Action::Wait
    );
    // Mid-command output before the first prompt: still no submit.
    assert_eq!(
        state.advance(obs(ShellPhase::CommandExecuting, 0)),
        Action::Wait
    );
    assert_eq!(
        state.advance(obs(ShellPhase::AtPrompt, 0)),
        Action::Submit(0)
    );
}

// ── Await completion predicate (prompt AND +1 block AND giant size) ─────

#[test]
fn await_giant_waits_while_command_running() {
    let state = awaiting_giant(6, 2);
    // 133;B processed, command streaming: phase left the prompt.
    assert_eq!(
        state.advance(obs_sized(
            ShellPhase::CommandExecuting,
            3,
            GIANT_MIN_OUTPUT_BYTES
        )),
        Action::Wait
    );
}

#[test]
fn await_giant_waits_while_block_count_unchanged() {
    let state = awaiting_giant(6, 2);
    // Prompt is back but no new finished block yet (pre-133;B tick, or a
    // command that produced no block): not complete.
    assert_eq!(
        state.advance(obs_sized(ShellPhase::AtPrompt, 2, GIANT_MIN_OUTPUT_BYTES)),
        Action::Wait
    );
}

#[test]
fn await_giant_ignores_small_foreign_block() {
    // 2026-11-01 rig evidence: the first-run welcome banner (374B) races the
    // giant submission through the shared command_from_editor slot, so a
    // prompt + "+1 block" observation can precede the giant by seconds. The
    // size identity check must hold the sequence until the real capture.
    let state = awaiting_giant(6, 0);
    assert_eq!(
        state.advance(obs_sized(ShellPhase::AtPrompt, 1, 374)),
        Action::Wait
    );
    // Boundary: exactly the threshold passes the check (`<` holds the wait,
    // so >= completes) — one byte below it does not.
    assert_eq!(
        state.advance(obs_sized(
            ShellPhase::AtPrompt,
            1,
            GIANT_MIN_OUTPUT_BYTES - 1
        )),
        Action::Wait
    );
    assert_eq!(
        state.advance(obs_sized(ShellPhase::AtPrompt, 1, GIANT_MIN_OUTPUT_BYTES)),
        Action::Submit(1)
    );
    // Inband (seq_len 1): the giant's completion is the sequence's completion.
    let inband = awaiting_giant(1, 0);
    assert_eq!(
        inband.advance(obs_sized(ShellPhase::AtPrompt, 1, GIANT_MIN_OUTPUT_BYTES)),
        Action::Done
    );
}

#[test]
fn await_giant_completes_on_prompt_plus_new_giant_block() {
    // Shaped (seq_len 6): the giant's completion hands off to seq round 1.
    let shaped = awaiting_giant(6, 2);
    assert_eq!(
        shaped.advance(obs_sized(
            ShellPhase::AtPrompt,
            3,
            GIANT_MIN_OUTPUT_BYTES + 1
        )),
        Action::Submit(1)
    );
}

#[test]
fn seq_round_completion_advances_and_final_round_completes() {
    // Round i completes → Submit(i+1); the LAST round completes → Done.
    // Seq rounds carry NO size check (any +1 block is the seq's).
    let state = awaiting(3, 6, 10);
    assert_eq!(
        state.advance(obs(ShellPhase::AtPrompt, 11)),
        Action::Submit(4)
    );
    let last = awaiting(5, 6, 10);
    assert_eq!(last.advance(obs(ShellPhase::AtPrompt, 11)), Action::Done);
}

// ── Full shaped sequence walk (Submit 0..5 then Done) ───────────────────

#[test]
fn shaped_sequence_walks_giant_then_five_seq_rounds() {
    let mut state = Precharge::new(false, false);
    let mut blocks = 0usize;
    let mut actions = Vec::new();
    // First observation: shell integrated.
    actions.push(state.advance(obs(ShellPhase::AtPrompt, blocks)));
    for index in 0..6 {
        // Each submit is followed by an in-flight tick and a completion
        // tick (+1 finished block, phase back at the prompt). The giant's
        // completion tick carries its 32MB output length.
        state.step = super::Step::Await(index);
        state.baseline_blocks = blocks;
        actions.push(state.advance(obs(ShellPhase::CommandExecuting, blocks)));
        blocks += 1;
        let newest = if index == 0 { 32 * 1024 * 1024 } else { 140 };
        actions.push(state.advance(obs_sized(ShellPhase::AtPrompt, blocks, newest)));
    }
    assert_eq!(
        actions,
        vec![
            Action::Submit(0),
            Action::Wait,
            Action::Submit(1),
            Action::Wait,
            Action::Submit(2),
            Action::Wait,
            Action::Submit(3),
            Action::Wait,
            Action::Submit(4),
            Action::Wait,
            Action::Submit(5),
            Action::Wait,
            Action::Done,
        ]
    );
}

#[test]
fn inband_sequence_walks_giant_only() {
    let mut state = Precharge::new(true, false);
    assert_eq!(
        state.advance(obs(ShellPhase::AtPrompt, 0)),
        Action::Submit(0)
    );
    state.step = super::Step::Await(0);
    state.baseline_blocks = 0;
    // No seq ticks: the giant's completion is the sequence's completion.
    assert_eq!(
        state.advance(obs_sized(ShellPhase::AtPrompt, 1, 32 * 1024 * 1024)),
        Action::Done
    );
}

// ── Timeout predicates (clock injected as elapsed; inclusive bounds) ────

#[test]
fn shell_wait_expires_at_exactly_ten_seconds() {
    assert!(!shell_wait_expired(std::time::Duration::from_millis(9_999)));
    assert!(shell_wait_expired(std::time::Duration::from_secs(10)));
    assert!(shell_wait_expired(std::time::Duration::from_secs(61)));
}

#[test]
fn total_budget_expires_at_exactly_sixty_seconds() {
    assert!(!total_budget_expired(std::time::Duration::from_millis(
        59_999
    )));
    assert!(total_budget_expired(std::time::Duration::from_secs(60)));
}

// ── F6-3 NEWTAB variant (Done→OpenTab→AwaitNewTab→Done | timeout) ───────

/// Full NEWTAB walk (inband): the giant's completion hands to OpenTab (not
/// Done), the pump's NewTab execution is idempotently re-fireable from the
/// OpenTab step, and the fresh tab's first prompt releases. The timeout
/// leg rides [`expiry_verdict`] below (the pump's thin shell turns
/// NewTabWait into `Step::Failed` + release).
#[test]
fn newtab_inband_walks_open_tab_then_releases_at_first_prompt() {
    let mut state = Precharge::new(true, true);
    assert_eq!(
        state.advance(obs(ShellPhase::AtPrompt, 0)),
        Action::Submit(0)
    );
    state.step = super::Step::Await(0);
    state.baseline_blocks = 0;
    // The giant's completion now hands to the NEWTAB leg, NOT Done.
    assert_eq!(
        state.advance(obs_sized(ShellPhase::AtPrompt, 1, 32 * 1024 * 1024)),
        Action::OpenTab
    );
    // Pump shell: the state flips to AwaitNewTab after executing NewTab;
    // a raced tick re-fires the OpenTab verdict idempotently.
    state.step = super::Step::OpenTab;
    assert_eq!(state.advance(obs(ShellPhase::AtPrompt, 1)), Action::OpenTab);
    // The fresh tab's tracker starts NotIntegrated (pre-133;A) — hold.
    state.step = super::Step::AwaitNewTab;
    assert_eq!(
        state.advance(obs(ShellPhase::NotIntegrated, 0)),
        Action::Wait
    );
    assert_eq!(
        state.advance(obs(ShellPhase::CommandExecuting, 0)),
        Action::Wait
    );
    // First prompt on the fresh tab: release.
    assert_eq!(state.advance(obs(ShellPhase::AtPrompt, 0)), Action::Done);
}

/// Shaped NEWTAB: the five seq rounds walk unchanged, only the LAST
/// round's completion hands to OpenTab, and from AwaitNewTab the fresh-tab
/// prompt releases.
#[test]
fn newtab_shaped_holds_open_tab_until_last_round_completes() {
    let mut state = Precharge::new(false, true);
    let mut blocks = 0usize;
    assert_eq!(
        state.advance(obs(ShellPhase::AtPrompt, blocks)),
        Action::Submit(0)
    );
    for index in 0..6 {
        state.step = super::Step::Await(index);
        state.baseline_blocks = blocks;
        blocks += 1;
        let newest = if index == 0 { 32 * 1024 * 1024 } else { 140 };
        let action = state.advance(obs_sized(ShellPhase::AtPrompt, blocks, newest));
        if index < 5 {
            assert_eq!(action, Action::Submit(index + 1));
        } else {
            assert_eq!(action, Action::OpenTab);
        }
    }
    state.step = super::Step::AwaitNewTab;
    assert_eq!(state.advance(obs(ShellPhase::AtPrompt, 0)), Action::Done);
}

/// Unarmed NEWTAB (the v1.18.1-style byte-identical invariant): the
/// sequence completion is `Done` directly, no OpenTab leg.
#[test]
fn newtab_unarmed_completes_directly_without_open_tab() {
    let state = awaiting_giant(1, 0);
    assert_eq!(
        state.advance(obs_sized(ShellPhase::AtPrompt, 1, 32 * 1024 * 1024)),
        Action::Done
    );
}

/// NEWTAB expiry truth table: TOTAL_BUDGET dominates every step; the
/// fresh-tab wait fires at exactly SHELL_WAIT on its OWN clock
/// (`phase_started`, stamped at the NewTab execution); mid-sequence Await
/// steps carry no phase bound (only the total).
#[test]
fn newtab_expiry_verdict_truth_table() {
    use std::time::Duration;
    // Total dominates, including inside the AwaitNewTab leg.
    assert_eq!(
        super::expiry_verdict(
            super::Step::AwaitNewTab,
            Duration::from_secs(60),
            Duration::from_secs(1)
        ),
        super::Expiry::Total
    );
    // Front guard unchanged: WaitShell at its 10s bound.
    assert_eq!(
        super::expiry_verdict(
            super::Step::WaitShell,
            Duration::from_secs(10),
            Duration::from_secs(10)
        ),
        super::Expiry::ShellWait
    );
    // Fresh-tab wait on its own clock: 1ms before the bound holds, the
    // bound itself fires.
    assert_eq!(
        super::expiry_verdict(
            super::Step::AwaitNewTab,
            Duration::from_secs(15),
            Duration::from_millis(9_999)
        ),
        super::Expiry::None
    );
    assert_eq!(
        super::expiry_verdict(
            super::Step::AwaitNewTab,
            Duration::from_secs(15),
            Duration::from_secs(10)
        ),
        super::Expiry::NewTabWait
    );
    // Mid-sequence Await steps: no phase bound, only the total.
    assert_eq!(
        super::expiry_verdict(
            super::Step::Await(0),
            Duration::from_secs(30),
            Duration::from_secs(30)
        ),
        super::Expiry::None
    );
}

// ── Command map ─────────────────────────────────────────────────────────

#[test]
fn command_map_sends_giant_first_then_seq_rounds() {
    assert_eq!(command_for(0), super::GIANT_COMMAND);
    for index in 1..=5 {
        assert_eq!(command_for(index), super::SEQ_COMMAND);
    }
    // The giant is the 32MB capture line; the shaping rounds are seq.
    assert!(command_for(0).contains("yes") && command_for(0).contains("head -c 33554432"));
    assert_eq!(command_for(1), "seq 1 50");
}
