//! WP2 Phase A (PLAN_v1181_WP2 §4-A2): the zoom rig's giant-block precharge
//! state machine.
//!
//! The giant rig variants (gate env WEFT_ZOOM_RIG_GIANT=1 shaped /
//! WEFT_ZOOM_RIG_GIANT_INBAND=1 inband, see scripts/performance_gate.sh)
//! need a ~32MB single block in the session BEFORE the zoom rig fires, plus
//! — shaped variant only — five `seq 1 50` blocks that push the giant out
//! of the newest-4 budget exemption (shaping invariant, plan §4-A2 ②:
//! Σ new `base_row_count` ≥ 2× band high AND giant not newest-4 — without
//! the seq rounds the giant would sit inside the newest-4 exempt set
//! (grid_cache.rs BUDGET_EXEMPT_NEWEST_BLOCKS) and never degrade, killing
//! the T1 signature).
//!
//! Why in-app: the rig session runs a fresh config and the harness cannot
//! inject shell input (weft self-manages ZDOTDIR — shell.rs), so the
//! injection runs as an event-loop-driven state machine pumped from
//! `step_self_zoom`. Commands go through the REGULAR user submit path
//! (`Terminal::run_command` → `build_submit_bytes` → `Pane::write_user_input`
//! — no new PTY API, plan §4-A2 item 4), and completion is observed as the
//! `ShellPhase` returning to `AtPrompt` with one more finished block than at
//! submit time (both 133;B and 133;D processed).
//!
//! Test-hook discipline (plan §4-A2 item 6): the sequence/timeout/transition
//! logic is pure ([`Precharge::advance`] + the two expiry predicates +
//! [`command_for`], truth-tabled in the sibling `rig_precharge_tests.rs`);
//! the PTY/clock side effects live in [`pump`] only.
//!
//! Timeout contract (plan R2 P2): the WHOLE sequence shares one 60s total
//! bound — a per-step bound would stack to 360s, past the gate's 90s sleep,
//! and a mid-sequence kill would leave neither an error line nor zoom output
//! (an unattributable red). Expiry releases the zoom rig anyway: the gate
//! criteria go red on the missing giant signature while the error line
//! carries the attribution.

use std::ffi::OsStr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use weft_core::blocks::ShellPhase;

use crate::App;

/// Env key arming the precharge rig (gate giant variants only).
pub(crate) const PRECHARGE_ENV: &str = "WEFT_ZOOM_RIG_PRECHARGE";
/// Env key selecting the inband variant (giant only, no seq shaping). The
/// gate sets it when launching this process; the state machine reads it
/// once at state creation.
const INBAND_ENV: &str = "WEFT_ZOOM_RIG_GIANT_INBAND";

/// Bounded wait for the first `ShellPhase::AtPrompt` (plan R2/R3 P2 guard):
/// `NotIntegrated` is the tracker's `#[default]` initial state (blocks.rs),
/// so an immediate check would always trip; a fresh shell emits its first
/// `133;A` within ~0.5-2s. Expiry logs `zoom rig precharge: shell not
/// integrated` and releases the zoom rig.
const SHELL_WAIT: Duration = Duration::from_secs(10);

/// Shared total bound for the ENTIRE injection sequence (plan R2 P2).
const TOTAL_BUDGET: Duration = Duration::from_secs(60);

/// The 32MiB single block: ~907k lines of 37 bytes ≈ 32MB captured output →
/// an L1 table ≈ 268MB (8B/grapheme, grid_cache.rs) — one block alone
/// exceeds the 256MiB cache budget (the plan §1 arithmetic).
const GIANT_COMMAND: &str = "yes '0123456789abcdefghijklmnopqrstuvwxyz' | head -c 33554432";
/// Shaping command: 50 rows each; 5 rounds = 250 rows ≈ 2× the default
/// window's band high (plan R3 P3 caliber), clearing the band after the
/// giant so the giant leaves the newest-4 exemption.
const SEQ_COMMAND: &str = "seq 1 50";
/// Number of `seq 1 50` rounds in the shaped variant (inband runs 0).
const SEQ_ROUNDS: usize = 5;
/// Giant identity check for the Await(0) completion: the finished block must
/// be at least this big to count as THE giant. WHY (2026-11-01 rig round 1
/// evidence): the first-run welcome banner (`session_pump` first_run_welcome)
/// is a shell command too, and on a rig's fresh config dir it races our
/// submission through the shared `command_from_editor` slot — the banner's
/// `133;B` stole the giant's command identity and its 374-byte block
/// satisfied a bare "+1 block" test, desynchronizing the whole sequence by
/// one (the 32MB output then landed in the first seq block's capture
/// window). A bare count test cannot tell the two apart; the size check can
/// (banner 374B / seq ~140B / giant 32MB).
const GIANT_MIN_OUTPUT_BYTES: usize = 1024 * 1024;

/// The state machine's step. `WaitShell` is the plan's front guard; `Await`
/// carries the sequence index of the command in flight; `Done`/`Failed` are
/// terminal (the zoom rig is released either way — see [`precharge_done`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    WaitShell,
    Await(usize),
    Done,
    Failed,
}

/// The shell layer's verdict for one tick (pure; effects live in [`pump`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// Submit the sequence command at this index now.
    Submit(usize),
    /// Nothing to do; keep waiting.
    Wait,
    /// Sequence complete — release the zoom rig.
    Done,
}

/// Shell state observed through the active tab (injected in tests).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ShellObs {
    pub(crate) phase: ShellPhase,
    pub(crate) finished_blocks: usize,
    /// Byte length of the NEWEST finished block's output (0 when none) —
    /// the Await(0) giant identity check reads this (see
    /// [`GIANT_MIN_OUTPUT_BYTES`]).
    pub(crate) newest_output_len: usize,
}

/// The precharge state machine. Construction is the plan's "state machine
/// start"; `started` anchors both timeout bounds.
pub(crate) struct Precharge {
    step: Step,
    /// Finished-block count at the moment the in-flight command was
    /// submitted — the "+1 block" completion test compares against THIS.
    baseline_blocks: usize,
    started: Instant,
    /// Sequence length: 1 (inband) or 1 + [`SEQ_ROUNDS`] (shaped).
    seq_len: usize,
}

impl Precharge {
    fn new(inband: bool) -> Self {
        Precharge {
            step: Step::WaitShell,
            baseline_blocks: 0,
            started: Instant::now(),
            seq_len: if inband { 1 } else { 1 + SEQ_ROUNDS },
        }
    }

    /// Pure transition for one tick. Completion requires the phase back at
    /// `AtPrompt` AND one more finished block than at submit time: a fast
    /// command whose 133;B and 133;D both land inside one tick gap satisfies
    /// both on the same observation, while a still-running command holds
    /// `CommandExecuting` (or `AtPrompt` with the count unchanged before
    /// 133;B arrives) — either way `Wait`. The GIANT step additionally
    /// requires the finished block to pass the size identity check
    /// ([`GIANT_MIN_OUTPUT_BYTES`]) so a foreign startup block (the
    /// first-run banner) can't satisfy it — see that constant's WHY note.
    fn advance(&self, obs: ShellObs) -> Action {
        match self.step {
            Step::WaitShell => {
                if obs.phase == ShellPhase::AtPrompt {
                    Action::Submit(0)
                } else {
                    Action::Wait
                }
            }
            Step::Await(index) => {
                let finished =
                    obs.phase == ShellPhase::AtPrompt && obs.finished_blocks > self.baseline_blocks;
                if !finished {
                    return Action::Wait;
                }
                if index == 0 && obs.newest_output_len < GIANT_MIN_OUTPUT_BYTES {
                    // A block finished, but it isn't the giant — a foreign
                    // startup block won the race (see GIANT_MIN_OUTPUT_BYTES).
                    // Keep waiting for the real 32MB capture.
                    return Action::Wait;
                }
                let next = index + 1;
                if next < self.seq_len {
                    Action::Submit(next)
                } else {
                    Action::Done
                }
            }
            // Pump stops visiting terminal states (checked before advance).
            Step::Done | Step::Failed => Action::Wait,
        }
    }
}

/// Shell-wait expiry (pure; the clock enters as an elapsed Duration).
fn shell_wait_expired(elapsed: Duration) -> bool {
    elapsed >= SHELL_WAIT
}

/// Total-budget expiry (pure; the clock enters as an elapsed Duration).
fn total_budget_expired(elapsed: Duration) -> bool {
    elapsed >= TOTAL_BUDGET
}

/// The command for sequence index `index` (0 = giant, 1.. = seq shaping).
fn command_for(index: usize) -> &'static str {
    if index == 0 {
        GIANT_COMMAND
    } else {
        SEQ_COMMAND
    }
}

// ── Shell layer (the only side-effectful code) ──────────────────────────

/// Process-wide state machine slot (main-thread only; the mutex matches the
/// macos_zoom statics house style and guards any future second caller).
static PRECHARGE: Mutex<Option<Precharge>> = Mutex::new(None);
/// Completion stamp — also the zoom rig's timing anchor (see [`rig_anchor`]).
static DONE_AT: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
/// Armed flag cached once: the env is fixed at process start, and the
/// OnceLock fast path is one atomic load per event-loop pass.
static ARMED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

fn armed() -> bool {
    *ARMED.get_or_init(|| std::env::var_os(PRECHARGE_ENV).as_deref() == Some(OsStr::new("1")))
}

fn inband() -> bool {
    std::env::var_os(INBAND_ENV).as_deref() == Some(OsStr::new("1"))
}

/// Whether the zoom rig's first fire may proceed: always true when the
/// precharge rig is unarmed (byte-identical legacy rig), otherwise only
/// after the state machine reached a terminal step (shaped, failed, or
/// timed out — all three release the zoom).
pub(crate) fn precharge_done() -> bool {
    if !armed() {
        return true;
    }
    DONE_AT.get().is_some()
}

/// The zoom rig's timing anchor (plan §4-A2 item 1: the anchor moves from
/// "startup" to "shaping complete"). Unarmed sessions keep the startup
/// anchor byte-identical; armed sessions anchor at the precharge completion
/// instant, preserving the 2s/6s fire SPACING relative to the shaping
/// rather than to startup (a long precharge must not collapse the gap
/// between the zoom-in and zoom-out fires).
pub(crate) fn rig_anchor(startup: Instant) -> Instant {
    if !armed() {
        return startup;
    }
    DONE_AT.get().copied().unwrap_or(startup)
}

/// Advance the precharge state machine one tick. Called from
/// `step_self_zoom` before the rig tick; one cached-atomic read per pass
/// when unarmed (inert house shape). All PTY/clock side effects live here.
pub(crate) fn pump(app: &mut App) {
    if !armed() {
        return;
    }
    let mut slot = PRECHARGE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let state = slot.get_or_insert_with(|| Precharge::new(inband()));
    if matches!(state.step, Step::Done | Step::Failed) {
        return;
    }
    let elapsed = state.started.elapsed();
    // Shared total bound FIRST: it also covers a shell that integrated but
    // stalled mid-sequence (e.g. the giant never finalizes).
    if total_budget_expired(elapsed) {
        tracing::error!("zoom rig precharge timeout");
        release(state);
        return;
    }
    if state.step == Step::WaitShell && shell_wait_expired(elapsed) {
        tracing::error!("zoom rig precharge: shell not integrated");
        release(state);
        return;
    }
    let Some(tab) = app.sessions.active_mut() else {
        return;
    };
    let obs = tab.with_terminal(|terminal| ShellObs {
        phase: terminal.block_tracker().phase(),
        finished_blocks: terminal.block_tracker().session_blocks().len(),
        newest_output_len: terminal
            .block_tracker()
            .session_blocks()
            .last()
            .map_or(0, |block| block.output.len()),
    });
    let Some(obs) = obs else {
        return;
    };
    match state.advance(obs) {
        Action::Wait => {}
        Action::Submit(index) => {
            // If the step that just completed was the giant, record its
            // block identity for the baseline attribution (plan §7-2 T1
            // verdict identifies the giant by id or by the precharge log).
            let giant_just_finished = state.step == Step::Await(0);
            let command = command_for(index);
            state.step = Step::Await(index);
            state.baseline_blocks = obs.finished_blocks;
            let bytes = tab.with_terminal(|terminal| terminal.run_command(command));
            match bytes {
                Some(bytes) if !bytes.is_empty() => {
                    if let Err(error) = tab.write_user_input(&bytes) {
                        tracing::error!(%error, "zoom rig precharge: PTY write failed");
                        release(state);
                        return;
                    }
                    tracing::info!(step = index, command, "zoom rig precharge: injected");
                }
                _ => {
                    tracing::error!("zoom rig precharge: submit produced no bytes");
                    release(state);
                    return;
                }
            }
            if giant_just_finished {
                log_giant_identity(tab);
            }
        }
        Action::Done => {
            let giant_just_finished = state.step == Step::Await(0);
            let elapsed_ms = state.started.elapsed().as_millis() as u64;
            state.step = Step::Done;
            tracing::info!(
                elapsed_ms,
                blocks = obs.finished_blocks,
                "zoom rig precharge: complete"
            );
            finish();
            if giant_just_finished {
                log_giant_identity(tab);
            }
        }
    }
}

/// Stamp the completion instant (the zoom rig's anchor + release gate).
/// Idempotent: whichever terminal path reaches it first wins.
fn finish() {
    let _ = DONE_AT.set(Instant::now());
}

/// Terminal failure: mark the state (stops further ticks) and release the
/// zoom rig — the gate goes red on the missing giant signature while the
/// already-logged error line carries the attribution.
fn release(state: &mut Precharge) {
    state.step = Step::Failed;
    finish();
}

/// After the giant command finalizes, the newest finished block IS the
/// giant — log its id + byte length so baseline analysis can attribute the
/// P1/P3 warns (plan §7-2 giant identification).
fn log_giant_identity(tab: &crate::tab::Tab) {
    let identity = tab.with_terminal(|terminal| {
        terminal
            .block_tracker()
            .session_blocks()
            .last()
            .map(|block| (block.id.0, block.output.len()))
    });
    if let Some(Some((id, output_len))) = identity {
        tracing::info!(
            giant_block_id = id,
            output_len,
            "zoom rig precharge: giant block ready"
        );
    }
}

#[cfg(test)]
#[path = "rig_precharge_tests.rs"]
mod tests;
