// BlockTracker unit tests (split from blocks.rs to keep it within its
// architecture-gate budget; child-module privacy reaches the private fields
// exactly like the inline module did).

use super::*;

/// Drive a tracker through one command and return the resulting block.
fn run_one<'a>(tracker: &'a mut BlockTracker, command: &str, output: &str, exit: i32) -> &'a Block {
    let before = tracker.blocks().len();
    tracker.on_prompt_start();
    tracker.on_command_start(command.to_string());
    for line in output.split('\n') {
        for ch in line.chars() {
            tracker.on_print(ch, CapturedStyle::default());
        }
        tracker.on_newline();
    }
    tracker.on_command_end(exit);
    assert_eq!(tracker.blocks().len(), before + 1, "expected one new block");
    tracker.blocks().last().unwrap()
}

#[test]
fn starts_not_integrated() {
    let t = BlockTracker::new();
    assert_eq!(t.phase(), ShellPhase::NotIntegrated);
    assert!(!t.bootstrap_ready());
    assert!(t.blocks().is_empty());
    assert!(!t.is_capturing());
}

#[test]
fn format_block_for_copy_full_block() {
    let s = format_block_for_copy(
        Some("/Users/andylee/code"),
        "cargo test",
        "test result: ok. 2317 passed",
    );
    assert_eq!(
        s,
        "/Users/andylee/code\n$ cargo test\ntest result: ok. 2317 passed"
    );
}

#[test]
fn format_block_for_copy_skips_empty_parts() {
    // No cwd → starts at the command; no command → just the output.
    assert_eq!(
        format_block_for_copy(None, "ls", "file.txt"),
        "$ ls\nfile.txt"
    );
    // Command-only block (empty output).
    assert_eq!(format_block_for_copy(Some("~"), "cd ..", ""), "~\n$ cd ..");
    // Empty-string cwd is treated like None (no blank line).
    assert_eq!(format_block_for_copy(Some(""), "ls", "out"), "$ ls\nout");
}

#[test]
fn format_block_for_copy_all_empty_is_empty() {
    assert_eq!(format_block_for_copy(None, "", ""), "");
}

#[test]
fn bootstrap_sets_on_first_prompt_start() {
    let mut t = BlockTracker::new();
    t.on_prompt_start();
    assert!(t.bootstrap_ready());
    assert_eq!(t.phase(), ShellPhase::AtPrompt);
}

#[test]
fn full_lifecycle_produces_one_block() {
    let mut t = BlockTracker::new();
    let block = run_one(&mut t, "ls -la", "file_a\nfile_b", 0);

    assert_eq!(block.command, "ls -la");
    // run_one splits on '\n' and appends a newline after each segment
    // ("file_a\nfile_b\n"); take_styled's PROMPT_SP tail-strip drops the
    // finalized block's trailing '\n'.
    assert_eq!(block.output.as_ref(), "file_a\nfile_b");
    assert_eq!(block.exit_code, Some(0));
    assert!(block.finished_at.is_some());
    assert!(block.finished_at.unwrap() >= block.started_at);
    assert!(!block.collapsed);
    assert_eq!(block.id, BlockId(1));
    assert_eq!(t.phase(), ShellPhase::AtPrompt);
}

#[test]
fn command_end_without_command_start_is_noop() {
    let mut t = BlockTracker::new();
    t.on_prompt_start();
    t.on_command_end(0); // no pending command
    assert!(t.blocks().is_empty());
    assert_eq!(t.phase(), ShellPhase::AtPrompt);
}

#[test]
fn multiple_commands_accumulate_with_rising_ids() {
    let mut t = BlockTracker::new();
    run_one(&mut t, "echo a", "a", 0);
    run_one(&mut t, "echo b", "b", 0);

    let blocks = t.blocks();
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].id, BlockId(1));
    assert_eq!(blocks[1].id, BlockId(2));
    assert_eq!(blocks[0].command, "echo a");
    assert_eq!(blocks[1].command, "echo b");
}

#[test]
fn capture_only_while_executing() {
    let mut t = BlockTracker::new();
    // Printing before any command must not be captured.
    t.on_prompt_start();
    t.on_print('x', CapturedStyle::default());
    t.on_newline();
    assert!(!t.is_capturing());

    t.on_command_start("cmd".to_string());
    assert!(t.is_capturing());
    t.on_print('h', CapturedStyle::default());
    t.on_print('i', CapturedStyle::default());
    t.on_command_end(0);

    let block = t.blocks().last().unwrap();
    assert_eq!(block.output.as_ref(), "hi");
}

#[test]
fn newline_becomes_output_newline() {
    let mut t = BlockTracker::new();
    t.on_prompt_start();
    t.on_command_start("c".to_string());
    t.on_print('a', CapturedStyle::default());
    t.on_newline();
    t.on_print('b', CapturedStyle::default());
    t.on_command_end(0);
    assert_eq!(t.blocks().last().unwrap().output.as_ref(), "a\nb");
}

#[test]
fn failed_command_records_nonzero_exit() {
    let mut t = BlockTracker::new();
    let block = run_one(&mut t, "false", "", 1);
    assert_eq!(block.exit_code, Some(1));
}

/// v1.10.23 review-blocker B2: absolute/relative column moves mutate the
/// captured bytes (padded spaces / wide-char overwrite) so they must bump
/// the live output version — a stale `LiveLayoutCache` would slice the
/// wrong byte range (non-char-boundary panic on multibyte output).
#[test]
fn cursor_column_moves_bump_live_output_version() {
    let mut t = BlockTracker::new();
    t.on_prompt_start();
    t.on_command_start("echo".to_string());
    t.on_print('你', CapturedStyle::default()); // 1 wide char (3 UTF-8 bytes)
    let v0 = t.in_flight().unwrap().version;
    // Column 1 lands inside the wide char → a space is emitted, the byte
    // contents change.
    t.on_set_cursor_column(1);
    assert_eq!(
        t.in_flight().unwrap().version,
        v0 + 1,
        "set_cursor_column mutates output"
    );
    t.on_move_cursor_columns(-1);
    assert_eq!(
        t.in_flight().unwrap().version,
        v0 + 2,
        "move_cursor_columns mutates output"
    );
    // Not capturing: no mutation, no bump.
    t.on_command_end(0);
    t.on_prompt_start();
    assert!(t.in_flight().is_none());
    t.on_set_cursor_column(2);
    t.on_move_cursor_columns(1);
    assert!(t.in_flight().is_none(), "no capture at prompt");
}

#[test]
fn re_prompt_without_end_finalizes_interrupted() {
    let mut t = BlockTracker::new();
    t.on_prompt_start();
    t.on_command_start("sleep 100".to_string());
    t.on_print('z', CapturedStyle::default());
    // User hits Ctrl+C: shell re-prompts with 133;A, no 133;D.
    t.on_prompt_start();

    assert_eq!(t.blocks().len(), 1);
    let block = &t.blocks()[0];
    assert_eq!(block.command, "sleep 100");
    assert_eq!(block.output.as_ref(), "z");
    assert_eq!(block.exit_code, None, "interrupted → no exit code");
    assert_eq!(t.phase(), ShellPhase::AtPrompt);
}

#[test]
fn output_cap_truncates() {
    let mut t = BlockTracker::new();
    t.on_prompt_start();
    t.on_command_start("cat huge".to_string());
    // Push well past the cap.
    for _ in 0..(DEFAULT_OUTPUT_CAP + 1024) {
        t.on_print('a', CapturedStyle::default());
    }
    t.on_command_end(0);

    let block = t.blocks().last().unwrap();
    assert!(
        block
            .output
            .ends_with("(block excerpt truncated at 1 MiB — full output remains in scrollback)"),
        "expected truncation marker, got tail: …{}",
        &block.output[block.output.len().saturating_sub(60)..]
    );
    assert!(
        block.output.len() <= DEFAULT_OUTPUT_CAP + 96,
        "captured output must not exceed the cap by more than the marker"
    );
}

// ── PLAN_v11217 §3.5 (T4): configurable output cap ──────────────────

/// The clamp truth table must hold at the `set_output_cap` layer too
/// (review P2a: the entry point is reachable from live-reload and profile
/// switches, not only the normalized load path). Inputs are MiB-multiple
/// byte counts: 0 → 1, 1 → 1, 64 → 64, 65 → 64.
#[test]
fn set_output_cap_clamps_to_the_legal_mib_range() {
    let clamp = |input: usize| {
        let mut t = BlockTracker::new();
        t.set_output_cap(input);
        t.output_cap()
    };
    assert_eq!(clamp(0), DEFAULT_OUTPUT_CAP, "0 MiB → 1 MiB floor");
    assert_eq!(
        clamp(OUTPUT_CAP_MIN_MIB * MIB),
        DEFAULT_OUTPUT_CAP,
        "1 MiB stays 1 MiB"
    );
    assert_eq!(
        clamp(OUTPUT_CAP_MAX_MIB * MIB),
        OUTPUT_CAP_MAX_MIB * MIB,
        "64 MiB stays 64 MiB"
    );
    assert_eq!(
        clamp((OUTPUT_CAP_MAX_MIB + 1) * MIB),
        OUTPUT_CAP_MAX_MIB * MIB,
        "65 MiB → 64 MiB ceiling"
    );
}

/// cap=2 truncation behavior (§3.5 acceptance): ~2 MiB + ε truncates at
/// the CONFIGURED cap, `truncated` is set, and the marker names "2 MiB"
/// and mentions scrollback (the copy/search truth still holds the rest).
#[test]
fn raised_cap_truncates_at_the_configured_value_with_matching_copy() {
    let mut t = BlockTracker::new();
    t.set_output_cap(2 * MIB);
    t.on_prompt_start();
    t.on_command_start("cat bigger".to_string());
    for _ in 0..(2 * MIB + 1024) {
        t.on_print('a', CapturedStyle::default());
    }
    t.on_command_end(0);

    let block = t.blocks().last().unwrap();
    assert!(
        block.output.contains("block excerpt truncated at 2 MiB"),
        "marker must report the configured cap: …{}",
        &block.output[block.output.len().saturating_sub(80)..]
    );
    assert!(
        block.output.contains("full output remains in scrollback"),
        "marker must clarify the data is not lost"
    );
    assert!(
        block.output.len() > DEFAULT_OUTPUT_CAP,
        "the configured 2 MiB cap must actually admit more than the old 1 MiB"
    );
    assert!(
        block.output.len() <= 2 * MIB + 96,
        "captured output must not exceed the configured cap by more than the marker"
    );
}

/// Default-value legacy twin: a fresh tracker keeps the historical 1 MiB
/// behavior (bytes accepted and marker position byte-identical; only the
/// marker COPY changed — §3.5 改动点 4).
#[test]
fn default_tracker_keeps_the_historical_one_mib_cap() {
    let t = BlockTracker::new();
    assert_eq!(t.output_cap(), DEFAULT_OUTPUT_CAP);
    let mut t = BlockTracker::new();
    t.on_prompt_start();
    t.on_command_start("cat huge".to_string());
    for _ in 0..(DEFAULT_OUTPUT_CAP + 1024) {
        t.on_print('a', CapturedStyle::default());
    }
    t.on_command_end(0);
    let block = t.blocks().last().unwrap();
    // The truncated capture keeps exactly the cap of content bytes; the
    // marker text is excluded from this pin (it is the copy, not the
    // retention, and §3.5 rewrote it deliberately).
    let content_len = block.output.len()
        - block
            .output
            .rfind('\n')
            .map_or(0, |index| block.output.len() - index);
    assert_eq!(
        content_len, DEFAULT_OUTPUT_CAP,
        "legacy twin: exactly DEFAULT_OUTPUT_CAP content bytes survive"
    );
}

/// Applying a cap mid-session updates the capture metadata so the NEXT
/// finalize's marker reports the new value even for a capture that began
/// under the old one (live-reload on an in-flight command).
#[test]
fn set_output_cap_resyncs_capture_marker_metadata() {
    let mut t = BlockTracker::new();
    t.on_prompt_start();
    t.on_command_start("streaming".to_string());
    for _ in 0..(2 * MIB + 8) {
        t.on_print('a', CapturedStyle::default());
    }
    // Live-reload arrives mid-stream: raise the cap to 4 MiB.
    t.set_output_cap(4 * MIB);
    // The capture already marked itself truncated at 2 MiB — the flag is
    // sticky by design (pre-existing bytes are final), but the stored
    // metadata must name the new cap, and a FRESH capture truncates at it.
    t.on_command_end(0);
    let block = t.blocks().last().unwrap();
    assert!(
        block
            .output
            .contains("block excerpt truncated at 4 MiB — full output remains in scrollback"),
        "marker metadata must follow the live cap change: …{}",
        &block.output[block.output.len().saturating_sub(80)..]
    );
}

#[test]
fn drain_unpersisted_then_empty() {
    let mut t = BlockTracker::new();
    run_one(&mut t, "a", "", 0);
    run_one(&mut t, "b", "", 0);

    let drained = t.drain_unpersisted();
    assert_eq!(drained.len(), 2);
    // Blocks remain in history; only the unpersisted queue is drained.
    assert_eq!(t.blocks().len(), 2);
    assert!(t.drain_unpersisted().is_empty());
}

#[test]
fn load_blocks_advances_next_id() {
    let mut t = BlockTracker::new();
    let loaded = vec![
        Block {
            id: BlockId(7),
            command: "old".into(),
            cwd: None,
            output: String::new().into(),
            styled_output: None,
            exit_code: Some(0),
            started_at: SystemTime::UNIX_EPOCH,
            finished_at: Some(SystemTime::UNIX_EPOCH),
            collapsed: false,
            screen_origin: false,
        },
        Block {
            id: BlockId(3),
            command: "older".into(),
            cwd: None,
            output: String::new().into(),
            styled_output: None,
            exit_code: Some(0),
            started_at: SystemTime::UNIX_EPOCH,
            finished_at: Some(SystemTime::UNIX_EPOCH),
            collapsed: false,
            screen_origin: false,
        },
    ];
    t.load_blocks(loaded);
    assert_eq!(t.blocks().len(), 2);

    // Next freshly-detected block must not collide with loaded id 7.
    run_one(&mut t, "new", "", 0);
    assert_eq!(t.blocks().last().unwrap().id, BlockId(8));
}

#[test]
fn session_blocks_include_loaded_history_for_restore() {
    // v1.7.5: 启动/Restore 后主视图应显示上次会话的命令记录（与 Warp 一致）。
    // load_blocks 加载的历史 block 现在通过 session_blocks() 暴露给主视图。
    let mut t = BlockTracker::new();
    // Pre-session blocks loaded from SQLite on startup.
    t.load_blocks(vec![Block {
        id: BlockId(1),
        command: "old".into(),
        cwd: None,
        output: String::new().into(),
        styled_output: None,
        exit_code: Some(0),
        started_at: SystemTime::UNIX_EPOCH,
        finished_at: Some(SystemTime::UNIX_EPOCH),
        collapsed: false,
        screen_origin: false,
    }]);
    assert_eq!(t.blocks().len(), 1);
    // v1.7.5: session_blocks() 现在包含加载的历史（之前会排除）。
    assert_eq!(t.session_blocks().len(), 1);
    assert_eq!(t.session_blocks()[0].command, "old");

    // A command run this session is appended after loaded history.
    run_one(&mut t, "ls", "", 0);
    assert_eq!(t.blocks().len(), 2);
    assert_eq!(t.session_blocks().len(), 2);
    assert_eq!(t.session_blocks()[0].command, "old");
    assert_eq!(t.session_blocks()[1].command, "ls");
}

#[test]
fn cwd_is_stamped_from_set_cwd_at_command_start() {
    let mut t = BlockTracker::new();
    t.set_cwd(Some("/Users/me/proj".to_string()));
    let b = run_one(&mut t, "pwd", "/Users/me/proj", 0);
    assert_eq!(b.cwd.as_deref(), Some("/Users/me/proj"));
}

#[test]
fn command_output_start_is_harmless_noop() {
    let mut t = BlockTracker::new();
    t.on_prompt_start();
    t.on_command_start("c".to_string());
    t.on_command_output_start();
    t.on_print('x', CapturedStyle::default());
    t.on_command_end(0);
    assert_eq!(t.blocks().last().unwrap().output.as_ref(), "x");
}

// ── T1: additional BlockTracker coverage ──────────────────────────

#[test]
fn multiple_tabs_block_isolation() {
    // Two independent BlockTrackers maintain their own block lists;
    // assigning an id in one must not collide with the other.
    let mut a = BlockTracker::new();
    let mut b = BlockTracker::new();
    run_one(&mut a, "ls", "a", 0);
    run_one(&mut b, "pwd", "b", 0);

    // Each tracker has exactly one block — no cross-contamination.
    assert_eq!(a.blocks().len(), 1);
    assert_eq!(b.blocks().len(), 1);
    // Both start their id sequence at 1 (independent state machines).
    assert_eq!(a.blocks()[0].id, BlockId(1));
    assert_eq!(b.blocks()[0].id, BlockId(1));
    // Commands don't leak across trackers.
    assert_eq!(a.blocks()[0].command, "ls");
    assert_eq!(b.blocks()[0].command, "pwd");

    // Adding a block to `a` doesn't change `b`.
    run_one(&mut a, "echo more", "more", 0);
    assert_eq!(a.blocks().len(), 2);
    assert_eq!(b.blocks().len(), 1);
}

#[test]
fn clear_command_produces_empty_output_block() {
    // The shell's `clear` command typically produces no captured output
    // (it emits control sequences that the VT parser handles, not printable
    // chars). The block should still record the command name and exit code.
    // We drive the tracker directly (without run_one) so no stray newline
    // is appended to the output.
    let mut t = BlockTracker::new();
    t.on_prompt_start();
    t.on_command_start("clear".to_string());
    // No on_print / on_newline calls — clear produces no printable output.
    t.on_command_end(0);
    let block = t.blocks().last().unwrap();
    assert_eq!(block.command, "clear");
    assert!(block.output.is_empty(), "clear should produce no output");
    assert_eq!(block.exit_code, Some(0));
}

#[test]
fn empty_command_produces_block() {
    // v1.0 fix: an empty command ("") still produces a block. The shell
    // may emit 133;B with an empty command line (e.g. user pressed Enter
    // on an empty prompt). The block should be recorded with an empty
    // command string rather than being silently dropped.
    let mut t = BlockTracker::new();
    let block = run_one(&mut t, "", "some output", 0);
    assert_eq!(block.command, "");
    // Trailing '\n' stripped at finalize (PROMPT_SP tail-strip).
    assert_eq!(block.output.as_ref(), "some output");
    assert_eq!(block.exit_code, Some(0));
    assert_eq!(t.blocks().len(), 1);
}

/// v1.10.26 Batch B (FIX_WRAP_EPOCH_AND_VIEWPORT_KEEP B-1): a finalized
/// block whose command took the primary screen (`screen_document_start`
/// became Some) is marked `screen_origin` so the renderer clips its frame
/// rows instead of soft-wrapping them. A plain shell command (no screen
/// document) is not marked.
#[test]
fn finalize_marks_screen_origin_only_for_screen_documents() {
    let mut t = BlockTracker::new();

    // Plain shell command — capture never becomes screen-owned.
    t.on_prompt_start();
    t.on_command_start("echo hi".to_string());
    t.on_print_ascii_run(b"hi", CapturedStyle::default());
    t.on_command_end(0);
    assert!(
        !t.blocks().last().unwrap().screen_origin,
        "ordinary shell output must stay soft-wrappable (screen_origin = false)"
    );

    // A TUI command that takes the primary screen.
    t.on_prompt_start();
    t.on_command_start("omp".to_string());
    t.begin_screen_owned_output(0);
    t.replace_screen_output("banner\nsecond line");
    t.on_command_end(0);
    assert!(
        t.blocks().last().unwrap().screen_origin,
        "a screen-owned TUI block must be marked screen_origin"
    );
}

#[test]
fn command_after_clear_does_not_affect_history() {
    // Running `clear` then another command should leave the history with
    // exactly two blocks — the clear block doesn't erase prior history
    // (that's the shell's job, not the BlockTracker's).
    let mut t = BlockTracker::new();
    run_one(&mut t, "echo first", "first\n", 0);
    run_one(&mut t, "clear", "", 0);
    run_one(&mut t, "echo second", "second\n", 0);

    assert_eq!(t.blocks().len(), 3);
    assert_eq!(t.blocks()[0].command, "echo first");
    assert_eq!(t.blocks()[1].command, "clear");
    assert_eq!(t.blocks()[2].command, "echo second");
    // IDs continue to rise monotonically across the clear.
    assert_eq!(t.blocks()[0].id, BlockId(1));
    assert_eq!(t.blocks()[1].id, BlockId(2));
    assert_eq!(t.blocks()[2].id, BlockId(3));
}
