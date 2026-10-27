use super::Terminal;

pub(crate) fn line_a(n: usize) -> String {
    format!("line {n:05} the quick brown fox")
}

pub(crate) fn line_b(n: usize) -> String {
    format!("line {n:05} {}", "x".repeat(189))
}

/// Bootstrap the block tracker and activate primary-screen TUI ownership.
/// Equivalents: `repeated_primary_screen_addressing_temporarily_owns_the_grid_view`
/// and `primary_screen_history_view_snapshot_refresh_is_explicitly_coalesced`.
pub(crate) fn activate_primary_screen_tui(terminal: &mut Terminal) {
    terminal.process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
    // Two absolute CUP moves => primary_screen_cursor_ops >= 2 =>
    // primary_screen_app_active() true, begin_screen_owned_output(0) runs
    // with the (pre-stream) empty grid => document_start = 0.
    terminal.process(b"\x1b[2;1H\x1b[3;1H");
    assert!(
        terminal.primary_screen_app_active(),
        "test setup: primary-screen TUI ownership not established"
    );
}

pub(crate) fn stream_lines(
    terminal: &mut Terminal,
    from: usize,
    to: usize,
    line_fn: fn(usize) -> String,
) {
    let mut buf = String::with_capacity((to - from + 1) * 64);
    for n in from..=to {
        buf.push_str(&line_fn(n));
        buf.push_str("\r\n");
    }
    terminal.process(buf.as_bytes());
}

/// Extract the `line NNNNN` number embedded in a snapshot line.
pub(crate) fn line_number_of(text: &str) -> Option<usize> {
    let after = text.split("line ").nth(1)?;
    after.split([' ', '\r', '\n', '\0']).next()?.parse().ok()
}
