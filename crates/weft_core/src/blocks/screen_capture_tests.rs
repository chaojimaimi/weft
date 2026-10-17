use super::*;

#[test]
fn screen_owned_command_uses_final_snapshot_not_repaint_stream() {
    let mut tracker = BlockTracker::new();
    tracker.on_prompt_start();
    tracker.on_command_start("screen-app".to_string());
    tracker.on_print_ascii_run(b"partial repaint", CapturedStyle::default());
    tracker.begin_screen_owned_output(7);
    tracker.on_print_ascii_run(b"ignored cursor frame", CapturedStyle::default());
    tracker.replace_screen_output("final screen\nresume command");
    tracker.on_command_end(130);

    assert_eq!(tracker.blocks().len(), 1);
    assert_eq!(
        tracker.blocks()[0].output.as_ref(),
        "final screen\nresume command"
    );
    assert_eq!(tracker.blocks()[0].exit_code, Some(130));
}

/// v1.13.13 FIX-D ②: `split_screen_history` (the >1MiB long-session rewrite
/// branch, reached via snapshot.rs:343 + freeze.rs:479/516) replaces the
/// in-flight capture's tail wholesale through `output.replace` — it must
/// bump the screen-rewrite epoch so the renderer's in-flight warm watermark
/// identity changes and the new tail is rescanned from line 0.
#[test]
fn split_screen_history_bumps_the_screen_rewrite_epoch() {
    let mut tracker = BlockTracker::new();
    tracker.on_prompt_start();
    tracker.on_command_start("long tui".to_string());
    tracker.begin_screen_owned_output(0);
    assert_eq!(tracker.screen_rewrite_epoch(), 0);

    tracker.split_screen_history(
        vec![("head chunk\n".to_string(), None)],
        "tail\n".to_string(),
        None,
        0,
    );

    assert_eq!(
        tracker.screen_rewrite_epoch(),
        1,
        "the split rewrite must invalidate the live warm watermark identity"
    );
}

/// v1.13.13 FIX-D P2-1 (rust-reviewer): `replace_screen_output` is the third
/// `output.replace` wholesale rewrite point (same contract + same
/// `screen_document_start` gate as `replace_screen_snapshot` /
/// `split_screen_history`) — it must bump the screen-rewrite epoch too.
#[test]
fn replace_screen_output_bumps_the_screen_rewrite_epoch() {
    let mut tracker = BlockTracker::new();
    tracker.on_prompt_start();
    tracker.on_command_start("screen-app".to_string());
    tracker.begin_screen_owned_output(7);
    assert_eq!(tracker.screen_rewrite_epoch(), 0);

    tracker.replace_screen_output("final screen\nresume command");

    assert_eq!(
        tracker.screen_rewrite_epoch(),
        1,
        "the third rewrite site must invalidate the live warm watermark identity"
    );
}
