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
