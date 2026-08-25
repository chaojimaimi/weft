//! FIX_LIVE_STYLED_OUTPUT regressions.
//!
//! Root cause: `BlockTracker::styled_output` was set to `None` at
//! `on_command_start` and ONLY ever rebuilt at finalize (`take_styled` in
//! continuation.rs), so the live block rendered in-flight output without
//! program-emitted SGR colors — colors appeared only after `133;D`. The
//! OutputCapture style RLE records every run faithfully throughout, so the
//! fix publishes throttled non-consuming snapshots (`peek_styled`) while the
//! command streams.
//!
//! These tests pin the live-publish contract:
//! - styled output is exposed via `in_flight().styled_output` while
//!   streaming, BEFORE the closing `133;D`;
//! - finalize still produces the correctly-styled block (regression guard);
//! - default-styled streaming never builds a snapshot (no spurious Arc);
//! - the publish throttle only fires after the min interval;
//! - a style-run overflow freezes the LAST good snapshot instead of
//!   flickering the live block back to unstyled.

use crate::blocks::{BlockTracker, CapturedStyle, ShellPhase, MAX_STYLE_RUNS_PER_BLOCK};
use crate::grid::{CellColor, CellFlags};
use crate::vt::Terminal;

/// Untagged `133;A` bootstraps shell integration (no tagged marker seen yet),
/// leaving the tracker AtPrompt — the state an integrated prompt idles in.
fn boot_prompt(terminal: &mut Terminal) {
    terminal.process(b"\x1b]133;A\x07");
    assert_eq!(terminal.block_tracker().phase(), ShellPhase::AtPrompt);
}

#[test]
fn live_block_exposes_styled_output_while_streaming() {
    let mut t = Terminal::new(24, 80);
    boot_prompt(&mut t);
    t.process(b"\x1b]133;B\x07\x1b]133;C\x07");
    t.process(b"\x1b[31mred\x1b[0m plain\r\n");

    // No `133;D` yet — the command is still streaming. The live block must
    // expose a styled snapshot while streaming (Warp parity: colors visible
    // during the run, not only after finalize).
    let live = t.block_tracker().in_flight().expect("command is in flight");
    let styled = live
        .styled_output
        .expect("live styled output while streaming");
    let line = styled.line(0).expect("line 0 of the live snapshot");
    assert_eq!(
        line.foreground_at(0),
        Some(CellColor::Palette(1)),
        "the 'r' of 'red' must carry the SGR 31 foreground"
    );

    // Regression guard: finalizing must still produce the correctly-styled
    // block through take_styled (this assertion is green on the old code).
    t.process(b"\x1b]133;D;0\x07");
    let block = t.block_tracker().blocks().last().expect("one block");
    let styled = block
        .styled_output
        .as_deref()
        .expect("finalized block keeps styled output");
    let line = styled.line(0).expect("line 0 of the finalized block");
    assert_eq!(
        line.foreground_at(0),
        Some(CellColor::Palette(1)),
        "finalized block preserves SGR 31 foreground"
    );
}

#[test]
fn plain_streaming_keeps_styled_output_none() {
    let mut t = Terminal::new(24, 80);
    boot_prompt(&mut t);
    t.process(b"\x1b]133;B\x07\x1b]133;C\x07");
    t.process(b"plain text\r\n");

    // No SGR attributes were emitted — the capture has zero non-default
    // style runs, so publishing must NOT build an Arc<StyledOutput>.
    let live = t.block_tracker().in_flight().expect("command is in flight");
    assert!(
        live.styled_output.is_none(),
        "default-styled streaming must not synthesize a styled snapshot"
    );

    t.process(b"\x1b]133;D;0\x07");
    let block = t.block_tracker().blocks().last().expect("one block");
    assert!(
        block.styled_output.is_none(),
        "default-styled block finalizes without styled output"
    );
}

#[test]
fn live_styled_publish_throttles_within_interval() {
    let now = std::time::Instant::now();
    // Never published before → due immediately.
    assert!(BlockTracker::live_styled_publish_due(None, now));
    // Same instant → inside the min interval → not due.
    assert!(!BlockTracker::live_styled_publish_due(Some(now), now));
    // 50ms after the last publish → still inside the 100ms window → not due.
    let within = now
        .checked_add(std::time::Duration::from_millis(50))
        .expect("instant arithmetic");
    assert!(!BlockTracker::live_styled_publish_due(Some(now), within));
    // Past the 100ms window → due again.
    let past = now
        .checked_add(std::time::Duration::from_millis(101))
        .expect("instant arithmetic");
    assert!(BlockTracker::live_styled_publish_due(Some(now), past));
}

#[test]
fn style_overflow_freezes_last_snapshot() {
    let mut t = BlockTracker::new();
    t.on_prompt_start();
    t.on_command_start("x".to_string());
    // First mutation publishes: a one-char red snapshot.
    t.on_print(
        'r',
        CapturedStyle::from_attrs(
            CellColor::Palette(1),
            CellColor::Default,
            CellFlags::empty(),
        ),
    );
    // Alternating palettes force a new run per char; exhaust the run cap.
    for i in 0..(MAX_STYLE_RUNS_PER_BLOCK + 100) {
        let palette = ((i % 2) as u8) + 1;
        t.on_print(
            'x',
            CapturedStyle::from_attrs(
                CellColor::Palette(palette),
                CellColor::Default,
                CellFlags::empty(),
            ),
        );
    }
    // Overflow is now set. Force a publish attempt (reset the throttle) and
    // append more text: peek_styled returns None on overflow, so the live
    // block must KEEP the last good snapshot instead of flickering unstyled.
    t.last_live_styled_publish = None;
    t.on_print('y', CapturedStyle::default());

    let styled = t
        .in_flight()
        .expect("command is in flight")
        .styled_output
        .expect("overflow must freeze the last good snapshot, not clear it");
    assert_eq!(
        styled.line(0).unwrap().foreground_at(0),
        Some(CellColor::Palette(1)),
        "frozen snapshot keeps the pre-overflow red foreground"
    );
}

/// Review M1 + m1 regression: a colored line that is later rewritten in place
/// with default-styled bytes must lose its color BOTH in the live view and in
/// the finalized block. The pre-refactor code kept the stale snapshot in
/// `styled_output`, and finalize's screen-snapshot fallback (`take()`)
/// persisted it — a plain "ok" rendered red after `133;D`.
#[test]
fn destyle_rewrite_drops_stale_colors_live_and_final() {
    let mut t = BlockTracker::new();
    t.on_prompt_start();
    t.on_command_start("x".to_string());
    let red = CapturedStyle::from_attrs(
        CellColor::Palette(1),
        CellColor::Default,
        CellFlags::empty(),
    );
    // Red "ERROR" publishes a snapshot on the first mutation...
    for ch in "ERROR".chars() {
        t.on_print(ch, red);
    }
    // ...then the program rewrites the whole line from column 0 with
    // default-styled bytes (\r + plain overwrite, spinner/"done" style) and
    // terminates the line. The `\n` is a rewrite boundary that publishes
    // unthrottled, so the fully-cleared run state is what gets published.
    t.on_carriage_return();
    for ch in "ooooo".chars() {
        t.on_print(ch, CapturedStyle::default());
    }
    t.on_newline();

    // m1: every run was cleared by the default rewrite, so the live view must
    // drop the stale snapshot instead of keeping red on unstyled text.
    let live = t.in_flight().expect("command is in flight");
    assert!(
        live.styled_output.is_none(),
        "a full default-color rewrite must clear the live snapshot"
    );

    // M1: finalize's `styled_output.take()` fallback must not resurrect the
    // stale snapshot into the persisted block.
    t.on_command_end(0);
    let block = t.blocks().last().expect("one block");
    assert!(
        block.styled_output.is_none(),
        "finalized block must not resurrect the pre-rewrite styled snapshot"
    );
}

/// Review m2 regression: a styled plain-phase capture that hands over to the
/// screen-owned path must not carry its snapshot across the takeover — the
/// capture buffer is cleared there, so stale line indices (and, without this
/// reset, finalize's fallback) would attach orphan styles to the block.
#[test]
fn screen_takeover_clears_plain_phase_snapshot() {
    let mut t = BlockTracker::new();
    t.on_prompt_start();
    t.on_command_start("omp".to_string());
    t.on_print(
        'r',
        CapturedStyle::from_attrs(
            CellColor::Palette(1),
            CellColor::Default,
            CellFlags::empty(),
        ),
    );
    assert!(
        t.in_flight().expect("in flight").styled_output.is_some(),
        "plain-phase streaming publishes a snapshot"
    );

    t.begin_screen_owned_output(0);
    let live = t.in_flight().expect("still executing");
    assert!(
        live.styled_output.is_none(),
        "screen takeover must clear the plain-phase snapshot"
    );

    // Finalize without any replace_screen_* snapshot: nothing stale persists.
    t.on_command_end(0);
    let block = t.blocks().last().expect("one block");
    assert!(
        block.styled_output.is_none(),
        "no orphan styled output after takeover finalize"
    );
}
