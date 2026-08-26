//! Headless "real zsh → command block" integration matrix.
//!
//! `shell_integration.rs` proves the OSC 133 boundary markers arrive on the
//! raw PTY stream. These tests go one step further: they pump the live stream
//! into `weft_core::vt::Terminal` so the markers drive `BlockTracker` parsing
//! end-to-end, then assert on the finished [`Block`]s (exit codes, output,
//! styled output) and on the live grid cells.
//!
//! Every test spawns its own sandboxed zsh (ZDOTDIR rc-redirect + faked HOME,
//! so the user's real rcs never load — deterministic and side-effect-free).
//! zsh startup is slow and jittery, so tests poll for the expected block
//! completion instead of sleeping a fixed duration; each test's total runtime
//! is bounded by `WAIT_TIMEOUT` (+ a short drain after `exit`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use weft_core::blocks::{Block, ANSI_ATTRIBUTE_MASK};
use weft_core::grid::{CellColor, CellFlags};
use weft_core::pty::{Pty, PtyEvent};
use weft_core::shell::Integration;
use weft_core::vt::Terminal;

/// How long a test waits for a predicate before failing. zsh startup +
/// four commands is usually < 2 s; the floor leaves room for slow CI.
const WAIT_TIMEOUT: Duration = Duration::from_secs(10);
/// Extra budget to drain the stream after the final `exit`.
const EXIT_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// PTY size: large enough that short probe outputs never scroll off screen.
const ROWS: u16 = 30;
const COLS: u16 = 100;

/// A sandboxed zsh whose PTY output is pumped into a live `Terminal`.
///
/// Drop removes the sandbox dir, and the `Pty`'s Drop SIGHUPs the child, so
/// no process or temp state leaks between tests.
struct IntegratedSession {
    pty: Pty,
    term: Terminal,
    /// Everything zsh wrote, kept for failure-message diagnostics.
    raw: Vec<u8>,
    /// True once the child exited or the PTY hit EOF.
    exited: bool,
    sandbox: PathBuf,
}

impl IntegratedSession {
    /// Spawn `/bin/zsh` with the ZDOTDIR rc-redirect wired and HOME faked.
    /// Returns `None` when `/bin/zsh` is unavailable (skip pattern shared
    /// with `shell_integration.rs`).
    fn spawn_integrated_session() -> Option<Self> {
        if !Path::new("/bin/zsh").exists() {
            eprintln!("skipping: /bin/zsh not present on this system");
            return None;
        }
        // Unique sandbox per spawn so parallel tests never collide.
        let sandbox = std::env::temp_dir().join(format!(
            "weft-block-matrix-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let redirect_dir = sandbox.join("zsh"); // ZDOTDIR → holds our .zshenv
        let fake_home = sandbox.join("home"); // restored ZDOTDIR/HOME → empty
        std::fs::create_dir_all(&redirect_dir).unwrap();
        std::fs::create_dir_all(&fake_home).unwrap();
        let fake_home_str = fake_home.to_string_lossy().into_owned();

        let (_, file) = Integration::Zsh.rc_redirect().expect("zsh has rc redirect");
        std::fs::write(redirect_dir.join(file.filename), file.body).unwrap();

        let mut env: Vec<(&str, String)> = Integration::Zsh
            .child_env(Some(&fake_home_str))
            .into_iter()
            .collect();
        env.push(("ZDOTDIR", redirect_dir.to_string_lossy().into_owned()));
        env.push(("HOME", fake_home_str));
        let env_refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();

        let pty = Pty::spawn_with_args("/bin/zsh", &[], (ROWS, COLS), &env_refs, None, || {})
            .expect("failed to spawn zsh");
        Some(Self {
            pty,
            term: Terminal::new(ROWS as usize, COLS as usize),
            raw: Vec::new(),
            exited: false,
            sandbox,
        })
    }

    /// Submit one command line, followed by `\n` (the zsh line editor accepts
    /// newline — the reference test writes `\n` too, never a bare `\r`).
    async fn send_line(&self, line: &str) {
        let mut bytes = line.as_bytes().to_vec();
        bytes.push(b'\n');
        self.pty.write(&bytes).await.expect("write to zsh pty");
    }

    /// Drain PTY output into the Terminal until the deadline, the child
    /// exits, or EOF — the `collect` helper of shell_integration.rs, but
    /// feeding the live parser instead of only an accumulating buffer.
    async fn pump_until(&mut self, deadline: tokio::time::Instant) {
        while tokio::time::Instant::now() < deadline && !self.exited {
            match tokio::time::timeout(Duration::from_millis(200), self.pty.recv()).await {
                Ok(Some(PtyEvent::Output(bytes))) => {
                    self.raw.extend_from_slice(&bytes);
                    self.term.process(&bytes);
                }
                Ok(Some(PtyEvent::Exit(_))) | Ok(None) => {
                    self.exited = true;
                }
                Err(_) => {} // no data yet — keep draining until deadline
            }
        }
    }

    /// Poll `predicate` on the session every 50 ms while pumping, until it
    /// holds, the child exits, or `timeout` elapses. Returns whether the
    /// predicate held. Polling (not `sleep` + one-shot assert) is what keeps
    /// these tests immune to zsh's startup latency.
    async fn wait_for<F>(&mut self, timeout: Duration, predicate: F) -> bool
    where
        F: Fn(&Self) -> bool,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if predicate(self) {
                return true;
            }
            if self.exited {
                break;
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline.saturating_duration_since(now);
            let step = remaining.min(Duration::from_millis(50));
            self.pump_until(now + step).await;
        }
        predicate(self)
    }

    /// The session's finished blocks (newest last).
    fn blocks(&self) -> &[Block] {
        self.term.block_tracker().blocks()
    }

    /// Find the finished block whose (possibly prompt-prefixed) command line
    /// contains `needle`. Command extraction is best-effort, so matchers use
    /// substring contains, never exact equality.
    fn find_block(&self, needle: &str) -> Option<&Block> {
        self.blocks()
            .iter()
            .find(|block| block.command.contains(needle))
    }
}

impl Drop for IntegratedSession {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.sandbox);
    }
}

/// Four commands with distinct exit codes and output, then `exit`. Asserts
/// the finished blocks appear in execution order with the right exit codes,
/// and that nothing is left in flight once the last prompt landed.
#[tokio::test]
async fn command_block_lifecycle_and_exit_codes() {
    let Some(mut session) = IntegratedSession::spawn_integrated_session() else {
        return;
    };

    for command in [
        "echo weft_probe_alpha",
        "false",
        "printf 'a\\nb\\nc\\n'",
        "true",
    ] {
        session.send_line(command).await;
    }

    // Wait until all four blocks are finished AND the tracker is back at a
    // prompt (the precmd's `133;D` + `133;A` pair finalized the `true` block).
    let completed = session
        .wait_for(WAIT_TIMEOUT, |s| {
            s.blocks().len() >= 4 && s.term.block_tracker().in_flight().is_none()
        })
        .await;
    assert!(
        completed,
        "four blocks never completed; commands: {:?};\nstream:\n{}",
        session
            .blocks()
            .iter()
            .map(|b| b.command.as_str())
            .collect::<Vec<_>>(),
        String::from_utf8_lossy(&session.raw)
    );

    // Execution order must be preserved (blocks append newest-last): each
    // probe's block must sit after the previous probe's block.
    let probes: [(&str, Option<i32>); 4] = [
        ("weft_probe_alpha", Some(0)),
        ("false", Some(1)),
        ("printf", Some(0)),
        ("true", Some(0)),
    ];
    let mut previous_index = None;
    for (needle, expected_code) in probes {
        let block = session.find_block(needle).unwrap_or_else(|| {
            panic!(
                "no block for {needle:?}; commands: {:?}",
                session
                    .blocks()
                    .iter()
                    .map(|b| b.command.as_str())
                    .collect::<Vec<_>>()
            )
        });
        let index = session
            .blocks()
            .iter()
            .position(|b| b.id == block.id)
            .expect("found block must be in blocks()");
        if let Some(previous) = previous_index {
            assert!(
                previous < index,
                "{needle:?} block finished before the previous probe's block"
            );
        }
        previous_index = Some(index);
        assert_eq!(
            block.exit_code, expected_code,
            "{needle:?} exit code captured from 133;D"
        );
    }

    // The echo probe's output must be captured in its own block…
    let echo_block = session.find_block("weft_probe_alpha").expect("echo block");
    assert!(echo_block.output.contains("weft_probe_alpha"));
    // …while `false` produced no output but still carries its failing code.
    let false_block = session.find_block("false").expect("false block");
    assert_eq!(false_block.exit_code, Some(1));

    // After the last real prompt nothing is in flight: the `true` block was
    // closed by its precmd's `133;D` and the tracker idles AtPrompt.
    assert!(
        session.term.block_tracker().in_flight().is_none(),
        "in_flight must be None at the idle prompt"
    );

    // Close the session; the child must actually exit (not hang).
    session.send_line("exit").await;
    let deadline = tokio::time::Instant::now() + EXIT_DRAIN_TIMEOUT;
    session.pump_until(deadline).await;
    assert!(
        session.exited,
        "zsh did not exit after `exit`; stream:\n{}",
        String::from_utf8_lossy(&session.raw)
    );
}

/// `printf '\033[38;5;196mRED\033[0m\n'` — the block's `StyledOutput` must
/// keep the indexed (256-color palette) foreground through the capture, so
/// the block view can render history with program-emitted colors even after
/// the live grid scrolled away.
#[tokio::test]
async fn styled_output_preserves_indexed_color() {
    let Some(mut session) = IntegratedSession::spawn_integrated_session() else {
        return;
    };
    session
        .send_line("printf '\\033[38;5;196mRED\\033[0m\\n'")
        .await;

    let done = session
        .wait_for(WAIT_TIMEOUT, |s| {
            s.find_block("38;5;196").is_some() && s.term.block_tracker().in_flight().is_none()
        })
        .await;
    assert!(
        done,
        "RED block never completed; stream:\n{}",
        String::from_utf8_lossy(&session.raw)
    );

    let block = session.find_block("38;5;196").expect("checked above");
    // A captured SGR stream always yields a StyledOutput (the style RLE is
    // non-empty); None would mean the color was lost on the way through.
    let styled = block
        .styled_output
        .as_deref()
        .expect("block must carry styled output");
    assert!(styled.has_colors(), "styled output has no captured spans");

    // Locate every Palette(196) span and check it covers exactly the "RED"
    // characters of the captured line text. Char coordinates in StyledLine
    // are per-line text indices (the trailing `\n` is not part of any run).
    let captured_lines: Vec<&str> = block.output.split('\n').collect();
    let mut red_spans_seen = 0usize;
    for styled_line in &styled.lines {
        let text = captured_lines
            .get(styled_line.line as usize)
            .copied()
            .unwrap_or_default();
        for span in &styled_line.foregrounds {
            if span.color == CellColor::Palette(196) {
                red_spans_seen += 1;
                let chars: Vec<char> = text.chars().collect();
                let span_text: String = (span.start..span.end)
                    .filter_map(|i| chars.get(i as usize))
                    .collect();
                assert_eq!(
                    span_text, "RED",
                    "Palette(196) span must cover exactly the RED glyphs; line {} text = {text:?}",
                    styled_line.line
                );
            }
        }
    }
    assert!(
        red_spans_seen >= 1,
        "no Palette(196) span captured; block output = {:?}",
        block.output.as_ref()
    );
}

/// `printf '\033[4;58;2;255;0;0mU\033[0mX\n'` — end-to-end guard for the
/// v1.10 SGR-58 fix: the underline-color params (58;2;r;g;b) must be consumed
/// as one parameter set so the following `\033[0m` still resets everything.
/// On the grid, 'U' must carry UNDERLINE (and nothing else) while the
/// following 'X' — printed after the reset — must be fully plain.
#[tokio::test]
async fn sgr58_underline_survives_shell_roundtrip() {
    let Some(mut session) = IntegratedSession::spawn_integrated_session() else {
        return;
    };
    session
        .send_line("printf '\\033[4;58;2;255;0;0mU\\033[0mX\\n'")
        .await;

    let done = session
        .wait_for(WAIT_TIMEOUT, |s| {
            s.find_block("4;58;2;255;0;0").is_some() && s.term.block_tracker().in_flight().is_none()
        })
        .await;
    assert!(
        done,
        "SGR-58 probe never completed; stream:\n{}",
        String::from_utf8_lossy(&session.raw)
    );

    let grid = session.term.grid();
    // The echoed command line also contains literal 'U'/'X' characters (in
    // the escape text), so key the scan on the UNDERLINE flag, never on the
    // character alone.
    let underlined: Vec<(usize, usize)> = (0..grid.num_rows)
        .flat_map(|row| {
            (0..grid.num_cols).filter_map(move |col| {
                let cell = grid.cell(row, col);
                (cell.character == 'U' && cell.flags.contains(CellFlags::UNDERLINE))
                    .then_some((row, col))
            })
        })
        .collect();
    assert_eq!(
        underlined.len(),
        1,
        "exactly one underlined 'U' expected; found {}; stream:\n{}",
        underlined.len(),
        String::from_utf8_lossy(&session.raw)
    );

    let (row, col) = underlined[0];
    // The v1.10 SGR-58 audit: 4;58;2;255;0;0 must set ONLY underline — the
    // 58 params previously bled into following attributes (DIM/ITALIC here).
    let u_flags = grid.cell(row, col).flags;
    assert!(
        !u_flags.contains(CellFlags::DIM) && !u_flags.contains(CellFlags::ITALIC),
        "U must not pick up DIM/ITALIC from the SGR-58 params: {u_flags:?}"
    );

    // The reset `\033[0m` between U and X must have cleared every attribute:
    // X is the very next cell and carries no SGR attribute (freshly printed
    // cells legitimately keep grid-internal bits like DIRTY, so compare only
    // the ANSI-visible attribute mask) and no color.
    let x_cell = grid.cell(row, col + 1);
    assert_eq!(
        x_cell.character, 'X',
        "X must directly follow the underlined U"
    );
    assert!(
        x_cell.flags.intersection(ANSI_ATTRIBUTE_MASK).is_empty(),
        "X must be plain after the reset; got {:?}",
        x_cell.flags
    );
    assert_eq!(
        x_cell.fg,
        CellColor::Default,
        "X must not keep the underline-color SGR state"
    );
}

/// `printf 'l1\nl2\nl3\n'` — all three lines must land in ONE block's
/// captured output (the 133;B→133;D window must not split the command).
#[tokio::test]
async fn multiline_output_captured_in_one_block() {
    let Some(mut session) = IntegratedSession::spawn_integrated_session() else {
        return;
    };
    session.send_line("printf 'l1\\nl2\\nl3\\n'").await;

    let done = session
        .wait_for(WAIT_TIMEOUT, |s| {
            s.find_block("l1").is_some() && s.term.block_tracker().in_flight().is_none()
        })
        .await;
    assert!(
        done,
        "multiline probe never completed; stream:\n{}",
        String::from_utf8_lossy(&session.raw)
    );

    let block = session.find_block("l1").expect("checked above");
    let captured: Vec<&str> = block.output.lines().collect();
    assert!(
        captured.len() >= 3,
        "expected at least 3 captured lines; output = {:?}",
        block.output.as_ref()
    );
    assert_eq!(&captured[..3], ["l1", "l2", "l3"]);
    // One block only — the command must not be split by any marker.
    assert!(
        session
            .blocks()
            .iter()
            .filter(|b| b.command.contains("l1"))
            .count()
            == 1,
        "printf block split into multiple blocks"
    );
}

/// v1.10.33 PROMPT_SP regression, end-to-end under a real zsh: zsh's
/// prompt-cleanup mechanism emits a full line of spaces before the next
/// prompt. Those bytes used to land inside the capture window and were stored
/// as trailing block output. The finalize tail-strip must leave the captured
/// output of a no-trailing-whitespace command untouched and space-free.
#[tokio::test]
async fn prompt_sp_trailing_space_not_in_output() {
    let Some(mut session) = IntegratedSession::spawn_integrated_session() else {
        return;
    };
    // No trailing newline on purpose: a prompt redraw happens right after,
    // which is exactly when zsh emits the PROMPT_SP space run.
    session.send_line("printf 'solid'").await;

    let done = session
        .wait_for(WAIT_TIMEOUT, |s| {
            s.find_block("solid").is_some() && s.term.block_tracker().in_flight().is_none()
        })
        .await;
    assert!(
        done,
        "probe never completed; stream:\n{}",
        String::from_utf8_lossy(&session.raw)
    );

    let block = session.find_block("solid").expect("checked above");
    assert!(
        block.output.contains("solid"),
        "probe output missing; got {:?}",
        block.output.as_ref()
    );
    // The PROMPT_SP strip must have removed any trailing whitespace run —
    // otherwise the block ends in a wall of spaces.
    assert_eq!(
        block.output.trim_end(),
        "solid",
        "block output must not end with PROMPT_SP whitespace; got {:?}",
        block.output.as_ref()
    );
    assert!(
        !block.output.ends_with(' '),
        "block output must not end with a space; got {:?}",
        block.output.as_ref()
    );
}
