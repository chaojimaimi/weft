use super::*;

#[test]
fn reset_to_prompt_finalizes_with_no_exit_code() {
    // v1.0 fix: reset_to_prompt() (called after Ctrl+C flush) finalizes
    // any in-flight block with no exit code, mirroring the 133;A interrupt
    // path. This is a distinct code path from on_prompt_start().
    let mut tracker = BlockTracker::new();
    tracker.on_prompt_start();
    tracker.on_command_start("long-running".to_string());
    tracker.on_print('x');
    tracker.on_print('y');
    // Simulate Ctrl+C flush → reset_to_prompt (no 133;D, no 133;A).
    tracker.reset_to_prompt();

    assert_eq!(tracker.blocks().len(), 1);
    let block = &tracker.blocks()[0];
    assert_eq!(block.command, "long-running");
    assert_eq!(block.output.as_ref(), "xy");
    assert_eq!(
        block.exit_code, None,
        "reset_to_prompt finalizes with no exit code"
    );
    assert_eq!(tracker.phase(), ShellPhase::AtPrompt);
}

#[test]
fn screen_owned_command_uses_final_snapshot_not_repaint_stream() {
    let mut tracker = BlockTracker::new();
    tracker.on_prompt_start();
    tracker.on_command_start("screen-app".to_string());
    tracker.on_print_ascii_run(b"partial repaint");
    tracker.begin_screen_owned_output();
    tracker.on_print_ascii_run(b"ignored cursor frame");
    tracker.replace_screen_output("final screen\nresume command");
    tracker.on_command_end(130);

    assert_eq!(tracker.blocks().len(), 1);
    assert_eq!(
        tracker.blocks()[0].output.as_ref(),
        "final screen\nresume command"
    );
    assert_eq!(tracker.blocks()[0].exit_code, Some(130));
}
