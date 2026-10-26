//! v1.14.3 (T3, PLAN_v1143 §2) spawn-failure classification + resource-
//! exhaustion keep-path tests. Sibling test module (repo convention,
//! block_component/tests.rs precedent) so `pane_spawn.rs` stays within its
//! architecture budget.

use super::{
    fork_error_is_resource_exhausted, resource_exhausted_diagnostic_bytes,
    spawn_failure_resource_keep, worker_spawn_error_is_resource_exhausted, SpawnRequest,
};
use crate::app_state::SessionManager;
use crate::pane::Pane;
use crate::tab::Tab;

/// A placeholder pane with its spawn request armed (bypasses `spawn` — that
/// would fire a real fork thread; these tests are wiring-only).
fn placeholder_with_pending_spawn() -> Pane {
    let mut pane = Pane::placeholder(24, 80, 1000);
    pane.pending_spawn = Some(SpawnRequest::for_new_pane(
        pane.pane_session_id,
        24,
        80,
        None,
        pane.mouse_suppress.clone(),
    ));
    pane
}

// ── failure classification truth tables (T3-1) ─────────────────────────

/// fork Err 臂 errno 真值表: ENXIO/EMFILE/ENFILE/EAGAIN → true (kernel PTY /
/// process pool exhaustion); other errnos and other PtyError variants → false.
#[test]
fn fork_errno_truth_table() {
    use nix::errno::Errno;
    use weft_core::pty::PtyError;
    for errno in [Errno::ENXIO, Errno::EMFILE, Errno::ENFILE, Errno::EAGAIN] {
        assert!(
            fork_error_is_resource_exhausted(&PtyError::Fork(errno)),
            "{errno:?} must classify as resource exhaustion"
        );
    }
    for errno in [Errno::EIO, Errno::EACCES, Errno::ENOMEM, Errno::EINVAL] {
        assert!(
            !fork_error_is_resource_exhausted(&PtyError::Fork(errno)),
            "{errno:?} must NOT classify as resource exhaustion"
        );
    }
    // Non-fork variants never classify (even when carrying an errno).
    assert!(!fork_error_is_resource_exhausted(&PtyError::Resize(
        Errno::ENOMEM
    )));
    assert!(!fork_error_is_resource_exhausted(&PtyError::ExecFailed));
}

/// worker 创建失败 io::Error 真值表: EAGAIN-class (WouldBlock / raw EAGAIN)
/// → true; everything else → false.
#[test]
fn worker_spawn_error_truth_table() {
    assert!(worker_spawn_error_is_resource_exhausted(
        &std::io::Error::from(std::io::ErrorKind::WouldBlock)
    ));
    assert!(worker_spawn_error_is_resource_exhausted(
        &std::io::Error::from_raw_os_error(nix::libc::EAGAIN)
    ));
    assert!(!worker_spawn_error_is_resource_exhausted(
        &std::io::Error::from(std::io::ErrorKind::PermissionDenied)
    ));
    assert!(!worker_spawn_error_is_resource_exhausted(
        &std::io::Error::from_raw_os_error(nix::libc::EACCES)
    ));
}

// ── resource-exhaustion keep-kernel (T3-2, 假件测试) ────────────────────

/// 资源耗尽保留 tab: the tab STAYS (including the last-tab case — 应用不退
/// 出), pending_spawn is cleared (防误 adopt), and the diagnostic text rides
/// back for the caller's `Terminal::process`.
#[test]
fn resource_keep_retains_even_the_last_tab_and_clears_pending_spawn() {
    let mut sessions = SessionManager::new();
    let only = placeholder_with_pending_spawn();
    let only_id = only.pane_session_id;
    sessions.push_tab(Tab::with_single_pane(only));

    // Last tab: the old path closed + requested Exit; the keep-kernel keeps.
    let (tab_idx, pane_id, diagnostic) =
        spawn_failure_resource_keep(&mut sessions, only_id).expect("pane must be located");
    assert_eq!(tab_idx, 0);
    assert_eq!(diagnostic, super::RESOURCE_EXHAUSTED_DIAGNOSTIC);
    assert_eq!(sessions.len(), 1, "the last tab must NOT be torn down");
    let pane = sessions
        .tab(0)
        .and_then(|tab| tab.pane(pane_id))
        .expect("the diagnostic pane stays in place");
    assert!(pane.pending_spawn.is_none(), "防误 adopt — pending cleared");
    assert!(pane.pty.is_none(), "still a placeholder (no PTY to adopt)");
}

/// 多 pane 场景: a resource failure in ONE pane of a healthy+placeholder tab
/// keeps the whole tab and only clears THAT pane's pending spawn.
#[test]
fn resource_keep_in_a_mixed_tab_touches_only_the_failed_pane() {
    use weft_core::pane_layout::SplitDirection;
    let mut sessions = SessionManager::new();
    let healthy = Pane::with_terminal_only(1000);
    let healthy_id = healthy.pane_session_id;
    let mut tab = Tab::with_single_pane(healthy);
    // split_active_pane_test focuses the NEW pane — the background pane id
    // returned here is the failed placeholder's slot.
    let failed_pane_id = tab
        .split_active_pane_test(SplitDirection::Vertical, 0.5, 1000)
        .expect("test split is infallible for a single-leaf tree");
    let failed_pane = tab.pane_mut(failed_pane_id).expect("new pane exists");
    failed_pane.pending_spawn = Some(SpawnRequest::for_new_pane(
        failed_pane.pane_session_id,
        24,
        80,
        None,
        failed_pane.mouse_suppress.clone(),
    ));
    let failed_session_id = failed_pane.pane_session_id;
    sessions.push_tab(tab);

    let (_, kept_pane_id, _) = spawn_failure_resource_keep(&mut sessions, failed_session_id)
        .expect("pane must be located");
    assert_eq!(kept_pane_id, failed_pane_id);
    assert_eq!(sessions.len(), 1, "the mixed tab must NOT be torn down");
    let failed_pane = sessions.tab(0).unwrap().pane(failed_pane_id).unwrap();
    assert!(
        failed_pane.pending_spawn.is_none(),
        "only the failed pane clears"
    );
    let healthy_pane = sessions
        .tab(0)
        .unwrap()
        .pane(weft_core::pane_layout::PaneId(healthy_id))
        .expect("the healthy pane survives");
    assert!(
        healthy_pane.pty.is_none(),
        "the healthy pane's state is untouched by the failed sibling"
    );
}

/// Gone pane → idempotent None (no panic, no state change).
#[test]
fn resource_keep_for_a_gone_pane_is_a_no_op() {
    let mut sessions = SessionManager::new();
    assert_eq!(spawn_failure_resource_keep(&mut sessions, 424_242), None);
    assert_eq!(sessions.len(), 0);
}

// ── diagnostics text into the grid (T3-2) ──────────────────────────────

/// The diagnostic bytes carry the mandated wording with \r\n discipline, and
/// a placeholder's terminal renders them onto the grid (the same
/// `Terminal::process` call `handle_spawn_failed` makes — the placeholder has
/// no parse worker, so the main-thread lock is uncontended).
#[test]
fn diagnostic_bytes_land_on_the_placeholder_grid() {
    let bytes = resource_exhausted_diagnostic_bytes();
    let text = String::from_utf8(bytes.clone()).expect("utf-8 diagnostic");
    assert!(text.contains("系统终端资源(PTY)已耗尽"), "mandated wording");
    assert!(text.contains("Cmd+T"), "recovery hint");
    assert!(
        text.starts_with("\r\n") && text.ends_with("\r\n"),
        "line discipline"
    );

    let pane = Pane::placeholder(24, 80, 1000);
    let mut terminal = pane.lock_terminal().expect("placeholder holds a terminal");
    terminal.process(&bytes);
    let rendered: String = (0..terminal.grid().num_rows)
        .map(|row| terminal.grid().row_text(row))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        rendered.contains("系统终端资源(PTY)已耗尽"),
        "the diagnostic text must be visible in the grid, got: {rendered:?}"
    );
}

// ── child_pid capture timing (T3-3) ────────────────────────────────────

/// Independent dup of /dev/null for the fake's master/read/writer fds
/// (pane_spawn inline-test twin).
fn devnull_fd() -> std::os::unix::io::OwnedFd {
    use std::os::unix::io::{AsRawFd, FromRawFd};
    let file = std::fs::File::open("/dev/null").expect("open /dev/null");
    let duped = unsafe { nix::libc::dup(file.as_raw_fd()) };
    assert!(duped >= 0, "dup failed");
    drop(file);
    unsafe { std::os::unix::io::OwnedFd::from_raw_fd(duped) }
}

/// Fake `ForkedPty` (pane_spawn inline-test twin): /dev/null master, a real
/// already-exiting `/usr/bin/true` child. NO forkpty happens here.
#[allow(clippy::zombie_processes)]
fn fake_forked() -> weft_core::pty::ForkedPty {
    let child = std::process::Command::new("/usr/bin/true")
        .spawn()
        .expect("spawn /usr/bin/true");
    weft_core::pty::ForkedPty::from_parts(
        devnull_fd(),
        nix::unistd::Pid::from_raw(child.id() as nix::libc::pid_t),
        devnull_fd(),
        devnull_fd(),
    )
}

/// child_pid 捕获时序: the pid is captured BEFORE `release_pty` drops the
/// Pty — after teardown the pane carries BOTH the captured pid AND a dropped
/// pty. A PTY-less pane captures nothing.
#[test]
fn begin_close_teardown_captures_child_pid_before_release() {
    // start_read_loop contains the ONLY tokio::spawn; the test enters a
    // runtime exactly like the adoption contract (pane_spawn inline test).
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let _guard = rt.enter();

    let mut pane = placeholder_with_pending_spawn();
    assert!(pane.install_spawned_pty(fake_forked(), Box::new(|| {})));
    assert!(pane.pty.is_some());
    assert!(
        pane.child_pid.is_none(),
        "a live pane does not track the pid yet"
    );

    pane.begin_close_teardown();
    assert!(
        pane.child_pid.is_some(),
        "the pid must be captured BEFORE release_pty drops the Pty"
    );
    assert!(pane.pty.is_none(), "the PTY itself is released");
    assert!(
        pane.close_deadline.is_some(),
        "the watchdog deadline is stamped"
    );

    // A PTY-less pane (already torn down) captures nothing.
    let mut bare = Pane::with_terminal_only(1000);
    bare.begin_close_teardown();
    assert!(bare.child_pid.is_none());
}

// ── v1.15.1 (T3, PLAN_v1151): 诊断 bytes 含 R 重试提示 ─────────────────

#[test]
fn diagnostic_bytes_include_retry_hint() {
    // 纯函数级：诊断 bytes 必须带第二行「按 R 重试启动 shell」，且以 \r\n
    // 独立成段（grid 渲染为单独一行）。
    let text = String::from_utf8(resource_exhausted_diagnostic_bytes())
        .expect("diagnostic bytes are UTF-8");
    assert!(text.contains("按 R 重试启动 shell"), "text = {text:?}");
    assert!(
        text.contains("Cmd+T 新建。\r\n按 R 重试启动 shell。"),
        "the hint must be its own paragraph; text = {text:?}"
    );
}
