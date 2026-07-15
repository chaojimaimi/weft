//! Real PTY/TUI acceptance tests.
//!
//! These tests exercise the installed macOS Vim/less/nano binaries through Weft's
//! own PTY and VT parser. They complement deterministic unit tests: hardware
//! trackpad feel still needs one manual check, while protocol, Grid, mouse
//! mode, search prompt, CJK invariants, and exit behavior run automatically.

mod support;

use std::time::Duration;

use support::{require_command, sandbox, TuiSession};
use weft_core::input::{InputHandler, Modifiers, MouseProtocol};

const START_TIMEOUT: Duration = Duration::from_secs(4);
const UPDATE_TIMEOUT: Duration = Duration::from_secs(3);

#[tokio::test(flavor = "current_thread")]
async fn less_search_prompt_match_and_exit_roundtrip() {
    const LESS: &str = "/usr/bin/less";
    if !require_command(LESS) {
        return;
    }

    let dir = sandbox("less");
    let fixture = dir.join("lines.txt");
    let content = (1..=400)
        .map(|line| format!("weft line {line:04}\n"))
        .collect::<String>();
    std::fs::write(&fixture, content).expect("write less fixture");
    let fixture_text = fixture.to_string_lossy().into_owned();

    let mut session = TuiSession::spawn(LESS, &[&fixture_text], 24, 80, &dir);
    assert!(
        session
            .wait_until(START_TIMEOUT, |s| s.terminal.is_alt_screen_active())
            .await,
        "less did not enter alt screen; output={:?}",
        String::from_utf8_lossy(&session.raw_output)
    );

    session.send(b"/weft line 0250");
    assert!(
        session
            .wait_until(UPDATE_TIMEOUT, |s| {
                s.bottom_line().contains("/weft line 0250")
            })
            .await,
        "less search prompt was not visible on the bottom row; screen:\n{}",
        session.visible_text()
    );

    session.send(b"\r");
    assert!(
        session
            .wait_until(UPDATE_TIMEOUT, |s| {
                s.visible_text().contains("weft line 0250")
            })
            .await,
        "less did not reveal the requested match; screen:\n{}",
        session.visible_text()
    );

    session.resize(16, 60);
    session.send(b"\x0c"); // redraw at the new PTY size
    session.pump_for(Duration::from_millis(150)).await;
    assert_eq!(session.terminal.grid().num_rows, 16);
    assert_eq!(session.terminal.grid().num_cols, 60);
    session.send(b"/weft line 0300");
    assert!(
        session
            .wait_until(UPDATE_TIMEOUT, |s| {
                s.bottom_line().contains("/weft line 0300")
            })
            .await,
        "less search prompt left the bottom row after resize; screen:\n{}",
        session.visible_text()
    );
    session.send(b"\r");

    session.send(b"q");
    assert!(
        session.wait_until(UPDATE_TIMEOUT, |s| s.exited).await,
        "less did not exit; status={:?}",
        session.exit_status
    );
    assert!(!session.terminal.is_alt_screen_active());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "current_thread")]
async fn vim_mouse_search_cjk_grid_and_exit_roundtrip() {
    const VIM: &str = "/usr/bin/vim";
    if !require_command(VIM) {
        return;
    }

    let dir = sandbox("vim");
    let fixture = dir.join("cjk.txt");
    let content = (1..=300)
        .map(|line| format!("自动化目标行 {line:04} 中文与 ASCII mixed content\n"))
        .collect::<String>();
    std::fs::write(&fixture, content).expect("write Vim fixture");
    let fixture_text = fixture.to_string_lossy().into_owned();
    let args = ["--clean", "-n", "-i", "NONE", &fixture_text];

    let mut session = TuiSession::spawn(VIM, &args, 24, 80, &dir);
    assert!(
        session
            .wait_until(START_TIMEOUT, |s| s.terminal.is_alt_screen_active())
            .await,
        "Vim did not enter alt screen; output={:?}",
        String::from_utf8_lossy(&session.raw_output)
    );
    assert!(session.visible_text().contains("0001"));
    session.assert_no_orphaned_wide_cells();

    session.send(b":set mouse=a\r");
    assert!(
        session
            .wait_until(UPDATE_TIMEOUT, |s| {
                s.terminal.mouse_protocol == MouseProtocol::ButtonEvent && s.terminal.sgr_mouse
            })
            .await,
        "Vim mouse mode was not negotiated; protocol={:?}, sgr={}",
        session.terminal.mouse_protocol,
        session.terminal.sgr_mouse
    );

    let mut input = InputHandler::new();
    input.mouse_protocol = session.terminal.mouse_protocol;
    input.sgr_mouse = session.terminal.sgr_mouse;
    let wheel_down = input
        .encode_scroll(false, 10, 10, Modifiers::empty())
        .expect("encode Vim wheel-down report");
    let mut gesture = Vec::with_capacity(wheel_down.len() * 3);
    for _ in 0..3 {
        gesture.extend_from_slice(&wheel_down);
    }
    session.send(&gesture);
    assert!(
        session
            .wait_until(UPDATE_TIMEOUT, |s| !s
                .visible_text()
                .contains("目标行 0001"))
            .await,
        "Vim wheel reports did not move the viewport; screen:\n{}",
        session.visible_text()
    );
    session.assert_no_orphaned_wide_cells();

    session.resize(18, 64);
    session.send(b"\x0c"); // Vim redraw
    session.pump_for(Duration::from_millis(150)).await;
    assert_eq!(session.terminal.grid().num_rows, 18);
    assert_eq!(session.terminal.grid().num_cols, 64);

    session.send("/目标行 0150".as_bytes());
    assert!(
        session
            .wait_until(UPDATE_TIMEOUT, |s| {
                s.bottom_line().contains("/目标行 0150")
            })
            .await,
        "Vim search command was not visible on the bottom row; screen:\n{}",
        session.visible_text()
    );
    session.send(b"\r");
    assert!(
        session
            .wait_until(UPDATE_TIMEOUT, |s| s.visible_text().contains("目标行 0150"))
            .await,
        "Vim did not reveal the requested CJK match; screen:\n{}",
        session.visible_text()
    );
    session.assert_no_orphaned_wide_cells();

    session.send(b":q!");
    assert!(
        session
            .wait_until(UPDATE_TIMEOUT, |s| s.bottom_line().contains(":q!"))
            .await,
        "Vim quit command was not visible on the bottom row; screen:\n{}",
        session.visible_text()
    );
    session.send(b"\r");
    assert!(
        session.wait_until(UPDATE_TIMEOUT, |s| s.exited).await,
        "Vim did not exit; status={:?}",
        session.exit_status
    );
    assert!(!session.terminal.is_alt_screen_active());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "current_thread")]
async fn nano_edit_search_save_resize_and_exit_roundtrip() {
    const NANO: &str = "/usr/bin/nano";
    if !require_command(NANO) {
        return;
    }

    let dir = sandbox("nano");
    let fixture = dir.join("notes.txt");
    let content = (1..=120)
        .map(|line| format!("nano 自动化行 {line:04} mixed content\n"))
        .collect::<String>();
    std::fs::write(&fixture, content).expect("write nano fixture");
    let fixture_text = fixture.to_string_lossy().into_owned();

    let mut session = TuiSession::spawn(NANO, &[&fixture_text], 24, 80, &dir);
    assert!(
        session
            .wait_until(START_TIMEOUT, |s| s.terminal.is_alt_screen_active())
            .await,
        "nano did not enter alt screen; output={:?}",
        String::from_utf8_lossy(&session.raw_output)
    );
    assert!(
        session
            .wait_until(UPDATE_TIMEOUT, |s| {
                s.visible_text().contains("notes.txt")
                    && s.visible_text().contains("Get Help")
                    && s.bottom_line().contains("Exit")
            })
            .await,
        "nano title and shortcut bar did not render; screen:\n{}",
        session.visible_text()
    );
    session.assert_no_orphaned_wide_cells();

    session.send(b"\x17nano "); // Ctrl-W: Where Is
    session.send("自动化行 0075".as_bytes());
    assert!(
        session
            .wait_until(UPDATE_TIMEOUT, |s| {
                let lines = s.visible_lines();
                lines
                    .get(lines.len().saturating_sub(3))
                    .is_some_and(|line| line.contains("Search: nano 自动化行 0075"))
            })
            .await,
        "nano search prompt was not visible above the shortcut rows; screen:\n{}",
        session.visible_text()
    );
    session.send(b"\r");
    assert!(
        session
            .wait_until(UPDATE_TIMEOUT, |s| {
                s.visible_lines()
                    .iter()
                    .any(|line| line == "nano 自动化行 0075 mixed content")
            })
            .await,
        "nano did not reveal the requested match; screen:\n{}",
        session.visible_text()
    );
    session.assert_no_orphaned_wide_cells();

    session.send(b"\x05 appended-by-weft"); // Ctrl-E, then edit the matched line.
    session.send(b"\x0f"); // Ctrl-O: WriteOut
    assert!(
        session
            .wait_until(UPDATE_TIMEOUT, |s| {
                let lines = s.visible_lines();
                lines
                    .get(lines.len().saturating_sub(3))
                    .is_some_and(|line| line.contains("File Name"))
            })
            .await,
        "nano write-out prompt was not visible above the shortcut rows; screen:\n{}",
        session.visible_text()
    );
    session.send(b"\r");
    assert!(
        session
            .wait_until(UPDATE_TIMEOUT, |_s| {
                std::fs::read_to_string(&fixture)
                    .is_ok_and(|text| text.contains("0075 mixed content appended-by-weft"))
            })
            .await,
        "nano did not save the edited fixture; screen:\n{}",
        session.visible_text()
    );

    session.resize(18, 64);
    session.send(b"\x0c");
    assert!(
        session
            .wait_until(UPDATE_TIMEOUT, |s| {
                s.terminal.grid().num_rows == 18
                    && s.terminal.grid().num_cols == 64
                    && s.bottom_line().contains("Exit")
            })
            .await,
        "nano shortcut bar was not on the bottom row after resize; screen:\n{}",
        session.visible_text()
    );

    session.send(b"\x18"); // Ctrl-X: Exit
    assert!(
        session.wait_until(UPDATE_TIMEOUT, |s| s.exited).await,
        "nano did not exit; status={:?}, screen:\n{}",
        session.exit_status,
        session.visible_text()
    );
    assert!(!session.terminal.is_alt_screen_active());
    let _ = std::fs::remove_dir_all(dir);
}
