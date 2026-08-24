//! FIX_ORPHAN_PARSE_ERROR_OUTPUT regressions.
//!
//! A syntactically invalid command (`print(x)` at a zsh prompt) makes zsh
//! report the parse error BEFORE preexec — the shell-integration hook never
//! emits `133;B`, so the error text arrived while `phase != CommandExecuting`,
//! fell through every capture gate, and the closing `133;D` found
//! `pending_command == None` and finalized nothing (blocks.db had no record).
//!
//! These tests pin the preexec staging-buffer contract:
//! - bytes printed between editor submit and `133;B` divert into
//!   `Terminal::preexec_staging`, never into a block;
//! - a normal `133;B` discards the staged ZLE echo (zero regression);
//! - an orphan `133;D` (B missing) synthesizes the block from the staging;
//! - alt-screen entry and the next `133;A` clear stale staging.

use crate::blocks::ShellPhase;
use crate::vt::Terminal;

/// Untagged `133;A` bootstraps shell integration (no tagged marker seen yet),
/// leaving the tracker AtPrompt — the state an integrated prompt idles in.
fn boot_prompt(terminal: &mut Terminal) {
    terminal.process(b"\x1b]133;A\x07");
    assert_eq!(terminal.block_tracker().phase(), ShellPhase::AtPrompt);
}

#[test]
fn parse_error_produces_block_with_error_text() {
    let mut t = Terminal::new(24, 80);
    boot_prompt(&mut t);
    t.process(b"\x1b]7;file:///tmp/proj\x07"); // OSC 7 cwd stamp
    t.run_command("print(x)");
    assert!(t.command_from_editor_pending());

    // zsh reports the parse error before preexec — no `133;B` arrives, so
    // these bytes must land in staging (the future block output).
    t.process(b"zsh: parse error near \')\'\r\n");
    assert!(
        t.preexec_staging_len() > 0,
        "error output must divert into staging while phase is AtPrompt"
    );

    // precmd pair closes the failed command: orphan D + prompt A.
    t.process(b"\x1b]133;D;1\x07");
    t.process(b"\x1b]133;A\x07");

    let tracker = t.block_tracker();
    assert_eq!(tracker.blocks().len(), 1, "exactly one synthesized block");
    let block = tracker.blocks().last().unwrap();
    assert_eq!(block.command, "print(x)", "editor command titles the block");
    assert_eq!(block.cwd.as_deref(), Some("/tmp/proj"), "latest OSC 7 cwd");
    assert_eq!(block.exit_code, Some(1));
    let output = block.output.as_ref();
    assert!(output.contains("parse error"), "got: {output:?}");
    assert!(
        !output.contains("print(x)"),
        "command echo must not leak into output: {output:?}"
    );
    assert_eq!(
        t.block_tracker_mut().drain_unpersisted().len(),
        1,
        "synthesized block must be queued for SQLite persistence"
    );
}

#[test]
fn normal_command_staging_discarded_on_b() {
    let mut t = Terminal::new(24, 80);
    boot_prompt(&mut t);
    t.run_command("echo hi");

    // Simulated ZLE accept-line repaint: CR + erase-line + echoed command +
    // newline, all arriving BEFORE preexec (`133;B`).
    t.process(b"\r\x1b[Kecho hi\r\n");
    assert!(
        t.preexec_staging_len() > 0,
        "ZLE repaint bytes must land in staging"
    );

    t.process(b"\x1b]133;B\x07\x1b]133;C\x07");
    t.process(b"hi\r\n");
    t.process(b"\x1b]133;D;0\x07\x1b]133;A\x07");

    let tracker = t.block_tracker();
    assert_eq!(tracker.blocks().len(), 1);
    let block = tracker.blocks().last().unwrap();
    assert_eq!(block.command, "echo hi");
    assert_eq!(block.exit_code, Some(0));
    assert_eq!(
        block.output.as_ref(),
        "hi",
        "staged ZLE echo must be discarded at 133;B — block captures only real output"
    );
    assert_eq!(
        t.block_tracker_mut().drain_unpersisted().len(),
        1,
        "normal pipeline persistence unchanged"
    );
}

#[test]
fn command_end_without_any_context_is_noop() {
    // No editor submission, nothing staged: bare `133;D` must stay a noop
    // (semantics migrated/kept from blocks.rs command_end_without_command_start_is_noop).
    let mut t = Terminal::new(24, 80);
    boot_prompt(&mut t);
    t.process(b"\x1b]133;D;0\x07");

    assert!(t.block_tracker().blocks().is_empty());
    assert_eq!(t.block_tracker().phase(), ShellPhase::AtPrompt);
    assert!(t.block_tracker_mut().drain_unpersisted().is_empty());
}

#[test]
fn staging_cleared_on_alt_screen_enter() {
    let mut t = Terminal::new(24, 80);
    boot_prompt(&mut t);
    t.run_command("vim");
    t.process(b"some staged bytes");
    assert!(t.preexec_staging_len() > 0);

    // DEC 1049 entry: a TUI taking the alt screen invalidates the staged
    // pre-exec bytes (the shell line they belong to is gone).
    t.process(b"\x1b[?1049h");
    assert_eq!(
        t.preexec_staging_len(),
        0,
        "alt-screen entry clears staging"
    );

    // Back on the primary screen the orphan path must have nothing to work
    // with: the later `133;D` stays a plain noop.
    t.process(b"\x1b[?1049l");
    t.process(b"\x1b]133;D;0\x07");
    assert!(t.block_tracker().blocks().is_empty());
}

#[test]
fn missing_d_staging_dropped_on_next_prompt() {
    let mut t = Terminal::new(24, 80);
    boot_prompt(&mut t);
    t.run_command("sleep 100");
    t.process(b"staged but never closed\r\n");
    assert!(t.preexec_staging_len() > 0);

    // Abnormal sequence: the shell re-prompts via `133;A` without any
    // `133;D`. The A fallback drops the stale staging so it can never leak
    // into a later command's block.
    t.process(b"\x1b]133;A\x07");

    assert_eq!(
        t.preexec_staging_len(),
        0,
        "A fallback must drop stale staging"
    );
    assert!(
        !t.command_from_editor_pending(),
        "A fallback clears the submit flag alongside staging"
    );
    assert!(t.block_tracker().blocks().is_empty());
    assert!(t.block_tracker_mut().drain_unpersisted().is_empty());
}

/// Empty Enter must not double-spacer: `submit_command` synthesizes the
/// spacer block immediately and leaves `command_from_editor = Some("")`, so
/// an empty-string submission must NOT arm the staging buffer — otherwise
/// the shell's echo of the bare newline lands in staging and the precmd's
/// unconditional `133;D;0` synthesizes a SECOND empty block.
#[test]
fn empty_enter_spacer_does_not_duplicate_via_orphan_d() {
    let mut t = Terminal::new(24, 80);
    boot_prompt(&mut t);
    let _ = t.run_command("");
    assert_eq!(
        t.block_tracker().blocks().len(),
        1,
        "spacer block synthesized at submit time"
    );

    // The shell echoes the bare Enter; with staging armed for "" these bytes
    // would divert into staging and feed the orphan synthesis below.
    t.process(b"\r\n");

    // precmd pair: bare `133;D;0` + prompt.
    t.process(b"\x1b]133;D;0\x07");
    t.process(b"\x1b]133;A\x07");

    assert_eq!(
        t.block_tracker().blocks().len(),
        1,
        "orphan D must stay noop for an empty submission — no second spacer"
    );
}
