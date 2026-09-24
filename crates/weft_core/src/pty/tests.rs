//! Inline tests for `pty.rs`, extracted to keep the production file
//! under the 800-line architecture gate (repo convention: `tab.rs`
//! -> `tab/tests.rs`, `vt/mod.rs` -> `vt/tests.rs`). Declared in
//! `pty.rs` via `#[cfg(test)] #[path = "pty/tests.rs"] mod tests;`.

use super::read_loop::{read_batch, BatchStop, EVENT_CAP, READ_BATCH_TARGET};
use super::*;

#[test]
fn resize_ioctl_failure_is_propagated() {
    assert!(matches!(resize_ioctl_result(-1), Err(PtyError::Resize(_))));
    assert!(resize_ioctl_result(0).is_ok());
}

#[test]
fn child_env_drops_inherited_no_color_policy() {
    let mut env = std::collections::HashMap::from([
        ("NO_COLOR".into(), "1".into()),
        ("TERM".into(), "dumb".into()),
    ]);
    strip_launcher_presentation_env(&mut env);
    assert!(!env.contains_key(std::ffi::OsStr::new("NO_COLOR")));
    assert_eq!(env.get(std::ffi::OsStr::new("TERM")), Some(&"dumb".into()));
}

/// Test that spawning a PTY with /bin/cat works and we can read/write.
#[tokio::test]
async fn spawn_and_echo() {
    let mut pty = Pty::spawn("/bin/cat", (24, 80), || {}).expect("failed to spawn PTY");

    // Give the child a moment to start.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Write something.
    pty.write(b"hello\n").await.expect("write failed");

    // Read it back (cat echoes input).
    let output = tokio::time::timeout(std::time::Duration::from_secs(2), pty.recv())
        .await
        .expect("timeout waiting for output")
        .expect("channel closed");

    match output {
        PtyEvent::Output(data) => {
            let s = String::from_utf8_lossy(&data);
            assert!(
                s.contains("hello"),
                "expected output to contain 'hello', got: {s:?}"
            );
        }
        PtyEvent::Exit(code) => {
            panic!("child exited unexpectedly: {code:?}");
        }
    }

    // Verify child is alive.
    assert!(pty.is_alive());
}

/// Test that resize doesn't error.
#[tokio::test]
async fn resize_works() {
    let pty = Pty::spawn("/bin/cat", (24, 80), || {}).expect("failed to spawn PTY");

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    pty.resize(50, 120).expect("resize failed");
}

// ── argv NUL rejection is a pre-fork failure (audit VULN-006) ──────────
// Both tests are sync on purpose: a regression that re-forks before the
// CString build would leave a live child/hung test instead of an Err.

#[test]
fn spawn_rejects_nul_in_program_with_invalid_program() {
    // Match instead of formatting the whole Result: Pty is not Debug, and
    // the Err arm only needs the (Debug) error for the failure message.
    match Pty::spawn_with_args("/bin/za\0sh", &[], (24, 80), &[], None, new_flag(), || {}) {
        Err(err @ PtyError::InvalidProgram(_)) => {
            assert!(err.to_string().contains("NUL"), "{err}");
        }
        Err(err) => panic!("expected Err(InvalidProgram), got {err:?}"),
        Ok(_) => panic!("spawn with a NUL program must fail pre-fork"),
    }
}

#[test]
fn spawn_rejects_nul_in_arg_with_invalid_program() {
    match Pty::spawn_with_args(
        "/bin/echo",
        &["a\0b"],
        (24, 80),
        &[],
        None,
        new_flag(),
        || {},
    ) {
        Err(err @ PtyError::InvalidProgram(_)) => {
            assert!(err.to_string().contains("NUL"), "{err}");
        }
        Err(err) => panic!("expected Err(InvalidProgram), got {err:?}"),
        Ok(_) => panic!("spawn with a NUL arg must fail pre-fork"),
    }
}

/// Test that child exit is detected.
/// Uses `sleep 0` (exits immediately with code 0).
#[tokio::test]
async fn detects_child_exit() {
    let mut pty =
        Pty::spawn_with_args("/bin/sleep", &["0"], (24, 80), &[], None, new_flag(), || {})
            .expect("failed to spawn PTY");

    // Collect events until we get an Exit.
    let mut got_exit = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), pty.recv()).await;

        match event {
            Ok(Some(PtyEvent::Exit(result))) => {
                // /bin/true exits with code 0.
                assert!(result.is_ok(), "expected clean exit, got: {result:?}");
                assert_eq!(result.unwrap(), 0, "sleep 0 should exit 0");
                got_exit = true;
                break;
            }
            Ok(Some(PtyEvent::Output(_))) => continue,
            Ok(None) => break, // channel closed
            Err(_) => break,   // timeout
        }
    }
    assert!(got_exit, "never received Exit event from /bin/true");
}

/// Test sync write.
#[tokio::test]
async fn sync_write_works() {
    let pty = Pty::spawn("/bin/cat", (24, 80), || {}).expect("failed to spawn PTY");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    pty.write_sync(b"test\n").expect("sync write failed");
    assert!(pty.is_alive());
}

/// Ctrl+C must remain a PTY byte for raw-mode consumers such as SSH and
/// terminal REPLs. With ISIG disabled the child can observe the literal
/// ETX value; a direct SIGINT implementation would never produce BYTE:3.
#[tokio::test]
async fn interrupt_delivers_exactly_one_etx_to_raw_mode() {
    let script = concat!(
        "stty -isig -icanon -echo; ",
        "printf 'READY\\r\\n'; ",
        "byte=$(/bin/dd bs=1 count=1 2>/dev/null | /usr/bin/od -An -tu1); ",
        "printf 'BYTE:%s\\r\\n' \"$byte\""
    );
    let mut pty = Pty::spawn_with_args(
        "/bin/sh",
        &["-c", script],
        (24, 80),
        &[],
        None,
        new_flag(),
        || {},
    )
    .expect("failed to spawn raw-mode PTY fixture");

    let mut output = String::new();
    let ready_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while !output.contains("READY") && tokio::time::Instant::now() < ready_deadline {
        if let Ok(Some(PtyEvent::Output(bytes))) =
            tokio::time::timeout(std::time::Duration::from_millis(200), pty.recv()).await
        {
            output.push_str(&String::from_utf8_lossy(&bytes));
        }
    }
    assert!(
        output.contains("READY"),
        "raw fixture did not become ready: {output:?}"
    );
    assert!(pty.send_interrupt(), "ETX write should succeed");

    let exit_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while tokio::time::Instant::now() < exit_deadline {
        match tokio::time::timeout(std::time::Duration::from_millis(200), pty.recv()).await {
            Ok(Some(PtyEvent::Output(bytes))) => {
                output.push_str(&String::from_utf8_lossy(&bytes));
            }
            Ok(Some(PtyEvent::Exit(result))) => {
                assert_eq!(result, Ok(0));
                break;
            }
            Ok(None) => break,
            Err(_) => continue,
        }
    }
    let byte = output
        .split("BYTE:")
        .nth(1)
        .and_then(|tail| tail.split_whitespace().next());
    assert_eq!(
        byte,
        Some("3"),
        "raw child must receive one literal ETX byte: {output:?}"
    );
    assert_eq!(
        output.matches("BYTE:").count(),
        1,
        "one key must deliver once"
    );
}

/// Test that `extra_env` overrides reach the child via the `execve` path.
/// `/usr/bin/env` prints its environment and exits; our override must appear.
#[tokio::test]
async fn extra_env_reaches_child() {
    let mut pty = Pty::spawn_with_args(
        "/usr/bin/env",
        &[],
        (24, 80),
        &[("WEFT_TEST_OVERRIDE", "sentinel-12345")],
        None,
        new_flag(),
        || {},
    )
    .expect("failed to spawn PTY");

    let mut buf = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(std::time::Duration::from_millis(200), pty.recv()).await {
            Ok(Some(PtyEvent::Output(data))) => buf.extend_from_slice(&data),
            Ok(Some(PtyEvent::Exit(_))) => break,
            Ok(None) => break,
            Err(_) => break,
        }
    }
    let s = String::from_utf8_lossy(&buf);
    assert!(
        s.contains("WEFT_TEST_OVERRIDE=sentinel-12345"),
        "override missing from child env; got:\n{s}"
    );
}

/// Test error type display.
#[test]
fn error_display() {
    let err = PtyError::ChildExited(1);
    assert!(err.to_string().contains("exited with code 1"));

    let err = PtyError::ChildSignaled("SIGHUP".into());
    assert!(err.to_string().contains("killed by signal"));
}

// ── T4: additional PTY coverage ────────────────────────────────────

/// Spawning a non-existent program: forkpty succeeds (the fork itself
/// works), the child's exec fails, and the child exits with code 127
/// (the POSIX convention for "command not found"). The parent sees an
/// `Exit` event rather than a spawn-time error.
///
/// Marked `#[ignore]` because it spawns a real subprocess (needs a PTY
/// and a working fork). Run with `cargo test -- --ignored`.
#[tokio::test]
#[ignore]
async fn spawn_unknown_command_child_exits() {
    let mut pty = Pty::spawn("/no/such/binary/xyzzy", (24, 80), || {})
        .expect("forkpty itself should succeed even if the program doesn't exist");

    // The child's exec will fail → it exits with code 127. Collect
    // events until we see the Exit.
    let mut got_exit = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), pty.recv()).await;
        match event {
            Ok(Some(PtyEvent::Exit(result))) => {
                // exec failure → child exits with 127.
                assert!(
                    result.is_ok(),
                    "expected exit code 127, got error: {result:?}"
                );
                assert_eq!(result.unwrap(), 127, "exec failure should exit 127");
                got_exit = true;
                break;
            }
            Ok(Some(PtyEvent::Output(_))) => continue,
            Ok(None) => break,
            Err(_) => break,
        }
    }
    assert!(
        got_exit,
        "should receive an Exit event for an unknown command"
    );
}

// Restored verbatim from HEAD (rust-reviewer v1.11.2 Major-1): the
// interrupted worker had dropped these write-budget behavior anchors
// while restructuring the tests module; the production functions they
// pin (write_all_nonblocking / WriteOutcome / write_sync) are unchanged
// by v1.11.2, so the anchors must survive too.

// ── FIX_TERMINAL_CAPABILITY_HARDENING: bounded-EAGAIN write loop ──

/// EWOULDBLOCK twice, then success → the loop must poll-wait and retry,
/// ending in WrittenAll with all three calls observed.
#[test]
fn write_all_nonblocking_retries_wouldblock_then_succeeds() {
    let devnull = std::fs::File::open("/dev/null").expect("open /dev/null");
    let mut calls = 0;
    let outcome = write_all_nonblocking(
        &mut |buf: &[u8]| {
            calls += 1;
            if calls <= 2 {
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            } else {
                Ok(buf.len())
            }
        },
        devnull.as_raw_fd(),
        b"hello",
        Duration::from_millis(50),
    );
    assert!(
        matches!(outcome, WriteOutcome::WrittenAll),
        "expected WrittenAll, got {outcome:?}"
    );
    assert_eq!(calls, 3, "must retry after each EWOULDBLOCK");
}

/// A writer that stays blocked must burn through the whole budget and
/// then give up with an observable TimedOut result (the caller warns
/// about the dropped bytes).
#[test]
fn write_all_nonblocking_gives_up_after_budget() {
    let devnull = std::fs::File::open("/dev/null").expect("open /dev/null");
    let started = std::time::Instant::now();
    let outcome = write_all_nonblocking(
        &mut |_buf: &[u8]| Err(io::Error::from(io::ErrorKind::WouldBlock)),
        devnull.as_raw_fd(),
        b"data",
        Duration::from_millis(40),
    );
    assert!(
        matches!(outcome, WriteOutcome::TimedOut { written: 0 }),
        "continuously-blocked writer must time out, got {outcome:?}"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(30),
        "must spend the budget waiting before giving up"
    );
}

/// Partial writes must advance and re-issue with the unsent remainder.
#[test]
fn write_all_nonblocking_chains_partial_writes() {
    let devnull = std::fs::File::open("/dev/null").expect("open /dev/null");
    let mut seen: Vec<Vec<u8>> = Vec::new();
    let outcome = write_all_nonblocking(
        &mut |buf: &[u8]| {
            seen.push(buf.to_vec());
            Ok(buf.len().min(3)) // claim 3 bytes written per call
        },
        devnull.as_raw_fd(),
        b"hello",
        Duration::from_millis(50),
    );
    assert!(
        matches!(outcome, WriteOutcome::WrittenAll),
        "partial writes must converge, got {outcome:?}"
    );
    assert_eq!(
        seen,
        vec![b"hello".to_vec(), b"lo".to_vec()],
        "remainder must be re-issued"
    );
}

/// A writer claiming `Ok(0)` with data remaining must fail fast with a
/// WriteZero error (std `write_all` contract) instead of spinning out
/// the whole budget.
#[test]
fn write_all_nonblocking_fails_fast_on_zero_write() {
    let devnull = std::fs::File::open("/dev/null").expect("open /dev/null");
    let started = std::time::Instant::now();
    let outcome = write_all_nonblocking(
        &mut |_buf: &[u8]| Ok(0),
        devnull.as_raw_fd(),
        b"data",
        Duration::from_millis(50),
    );
    assert!(
        matches!(outcome, WriteOutcome::Error(ref e) if e.kind() == io::ErrorKind::WriteZero),
        "zero-write must fail fast with WriteZero, got {outcome:?}"
    );
    assert!(
        started.elapsed() < Duration::from_millis(30),
        "zero-write must not spin to the budget deadline"
    );
}

/// A writer that makes partial progress and THEN blocks forever must
/// report `TimedOut` with the bytes that actually made it out — the
/// caller (`write_sync`) warns with the dropped remainder, so partial
/// drops must stay observable via `written`, never masked as success.
#[test]
fn write_all_nonblocking_times_out_with_partial_writes() {
    let devnull = std::fs::File::open("/dev/null").expect("open /dev/null");
    let mut writes = 0;
    let outcome = write_all_nonblocking(
        &mut |buf: &[u8]| {
            writes += 1;
            if writes == 1 {
                Ok(buf.len().min(4)) // claim the first 4 bytes
            } else {
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            }
        },
        devnull.as_raw_fd(),
        b"abcdef",
        Duration::from_millis(30),
    );
    match outcome {
        WriteOutcome::TimedOut { written } => {
            assert_eq!(written, 4, "partial progress must survive the timeout");
        }
        other => panic!("expected TimedOut with partial write, got {other:?}"),
    }
}

/// A REAL kernel-backed EAGAIN, no fake closures: a Unix pipe whose
/// buffer is full and whose reader never drains. `write_all_nonblocking`
/// must burn its budget waiting for POLLOUT (which cannot fire) and
/// report `TimedOut { written: 0 }`.
#[test]
fn write_all_nonblocking_times_out_against_real_full_pipe() {
    let (reader, writer) = nix::unistd::pipe().expect("pipe");
    let _reader = reader; // held open so the pipe stays full for the whole test
    nix::fcntl::fcntl(
        writer.as_raw_fd(),
        nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
    )
    .expect("set pipe write end non-blocking");

    // Saturate: write 4 KiB chunks until the kernel refuse (EAGAIN).
    let mut accepted = 0usize;
    loop {
        match nix::unistd::write(&writer, &[0u8; 4096]) {
            Ok(n) => accepted += n,
            Err(nix::errno::Errno::EAGAIN) => break,
            Err(error) => panic!("unexpected pipe fill error: {error}"),
        }
        if accepted > 4 * 1024 * 1024 {
            panic!("pipe never filled; unexpected kernel behavior");
        }
    }
    assert!(accepted > 0, "pipe must accept then refuse writes");

    let mut kernel_writes = 0usize;
    let outcome = write_all_nonblocking(
        &mut |buf: &[u8]| match nix::unistd::write(&writer, buf) {
            Ok(n) => {
                kernel_writes += n;
                Ok(n)
            }
            Err(nix::errno::Errno::EAGAIN) => Err(io::Error::from(io::ErrorKind::WouldBlock)),
            Err(error) => Err(io::Error::from(error)),
        },
        writer.as_raw_fd(),
        b"overflow payload that can never fit",
        Duration::from_millis(40),
    );
    match outcome {
        WriteOutcome::TimedOut { written } => {
            assert_eq!(written, 0, "full pipe must accept nothing");
        }
        other => panic!("expected TimedOut against a real full pipe, got {other:?}"),
    }
    assert_eq!(kernel_writes, 0, "no byte may sneak into a full pipe");
}

/// AUDIT_v1.10.39 (write-budget behavior anchor): `write_sync` maps a
/// budget-exhausted write to `Ok(())` — dropping the overflow with
/// only a warn! (see the doc comment on `write_sync`). The plan was
/// to trigger that arm with a real PTY, but on macOS it is
/// unreachable: the n_tty line discipline silently DISCARDS input
/// overflow instead of holding the queue full (probed empirically:
/// 512 MiB into a never-reading child returned full-count writes,
/// never EAGAIN), so the real kernel never surfaces a persistent
/// full buffer to the master fd. The `TimedOut → Ok(())` mapping is
/// therefore pinned by the injected-writer unit tests above, and this
/// test pins what IS reachable on real hardware — the audit's
/// "silent drop": a huge write into a saturated child returns Ok(())
/// with the overflow invisible to the caller. Both pieces document,
/// not endorse, the status quo.
#[tokio::test]
async fn write_sync_large_write_into_saturated_child_reports_ok() {
    // `/bin/sleep` never reads stdin, so the child's input queue is
    // permanently saturated; the kernel discards the overflow.
    let pty = Pty::spawn("/bin/sleep", (24, 80), || {}).expect("failed to spawn PTY");
    let payload = vec![0x55u8; 1024 * 1024];
    let result = pty.write_sync(&payload);
    assert!(
        result.is_ok(),
        "write_sync must stay Ok() into a saturated child, got {result:?}"
    );
}

/// EINTR is retried like EWOULDBLOCK (asymmetric with the poll loop's
/// EINTR handling, which already continues).
#[test]
fn write_all_nonblocking_retries_interrupted_like_wouldblock() {
    let devnull = std::fs::File::open("/dev/null").expect("open /dev/null");
    let mut calls = 0;
    let outcome = write_all_nonblocking(
        &mut |buf: &[u8]| {
            calls += 1;
            if calls <= 2 {
                Err(io::Error::from(io::ErrorKind::Interrupted))
            } else {
                Ok(buf.len())
            }
        },
        devnull.as_raw_fd(),
        b"hi",
        Duration::from_millis(50),
    );
    assert!(
        matches!(outcome, WriteOutcome::WrittenAll),
        "Interrupted must be retried, got {outcome:?}"
    );
    assert_eq!(calls, 3, "must retry after each Interrupted");
}

// ── v1.11.15 (FIX E): honest reported-write mapping + Pty::write loop ──

/// The pure outcome→report mapping: a full write reports the total; a
/// budget timeout reports exactly the bytes that made it out; an error
/// is an error. The TimedOut arm is unreachable against a real macOS
/// master (n_tty silently discards input overflow), so the contract is
/// pinned at the injected-writer level.
#[test]
fn map_write_outcome_reports_written_bytes_honestly() {
    assert!(matches!(
        map_write_outcome(WriteOutcome::WrittenAll, 7),
        Ok(7)
    ));
    assert!(
        matches!(
            map_write_outcome(WriteOutcome::TimedOut { written: 4 }, 10),
            Ok(4)
        ),
        "partial progress must be reported, never masked as success"
    );
    assert!(map_write_outcome(WriteOutcome::Error(io::Error::other("boom")), 10).is_err());
}

/// Against a real PTY whose child never reads, the master still accepts
/// the full payload (macOS n_tty discards overflow silently), so
/// `write_sync_reported` must report the exact byte count.
#[tokio::test]
async fn write_sync_reported_returns_full_len_into_saturated_child() {
    let pty = Pty::spawn("/bin/sleep", (24, 80), || {}).expect("failed to spawn PTY");
    let payload = vec![0x41u8; 64 * 1024];
    let reported = pty
        .write_sync_reported(&payload)
        .expect("reported write must stay Ok into a saturated child");
    assert_eq!(
        reported,
        payload.len(),
        "master writes must report full len"
    );
    assert!(
        matches!(pty.write_sync_reported(b""), Ok(0)),
        "empty write reports 0"
    );
}

/// `Pty::write` must loop on the write offset: a large single write into
/// a slow-draining raw-mode child is accepted IN FULL — the child
/// (`dd of=/dev/null bs=1`, one syscall per byte, deliberately slower
/// than the writer) signals DONE only after consuming exactly
/// `payload.len()` bytes. (Raw mode is required because macOS canonical
/// mode would discard input beyond the line buffer at the KERNEL level —
/// invisible to the master; `of=/dev/null` because a copying dd would
/// fill the output side and deadlock the drain; `bs=1` because dd counts
/// short reads as full blocks, so `bs>1` would finish early.)
#[tokio::test]
async fn pty_write_large_payload_is_fully_delivered() {
    let mut pty = Pty::spawn_with_args(
            "/bin/sh",
            &[
                "-c",
                "stty raw -echo; printf READY; dd of=/dev/null bs=1 count=262144 2>/dev/null; printf DONE",
            ],
            (24, 80),
            &[],
            None,
            new_flag(),
            || {},
        )
        .expect("failed to spawn PTY");
    // Wait for the child to be in raw mode before writing.
    let mut output = Vec::new();
    let ready_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !contains_subslice(&output, b"READY") && tokio::time::Instant::now() < ready_deadline {
        if let Ok(Some(PtyEvent::Output(data))) =
            tokio::time::timeout(Duration::from_millis(200), pty.recv()).await
        {
            output.extend_from_slice(&data);
        }
    }
    assert!(contains_subslice(&output, b"READY"), "fixture not ready");

    // Printable bytes only; the kernel FIFO preserves order, so dd
    // consuming exactly the payload length proves full delivery.
    let payload: Vec<u8> = (0..256 * 1024u32).map(|i| b'A' + (i % 26) as u8).collect();
    pty.write(&payload)
        .await
        .expect("large write must loop until fully accepted");
    // Drain until the child's DONE marker (it cannot fire early: dd only
    // finishes after exactly payload.len() one-byte reads).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !contains_subslice(&output, b"DONE") && tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), pty.recv()).await {
            Ok(Some(PtyEvent::Output(data))) => output.extend_from_slice(&data),
            Ok(Some(PtyEvent::Exit(_))) | Ok(None) => break,
            Err(_) => {}
        }
    }
    assert!(
        contains_subslice(&output, b"DONE"),
        "child must consume every byte of the payload before signaling DONE"
    );
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

// ── v1.11.15 (FIX A): reader-side mouse suppression wiring ────────

/// The read loop's scanner flips the flag when the byte stream carries a
/// mouse-disable DECRST — here driven by a CHILD writing the sequence to
/// its own PTY (exactly how a real TUI emits its teardown disables; the
/// pane's flag is passed via spawn_with_args like Pane::spawn wires it).
#[tokio::test]
async fn read_loop_sets_suppress_flag_on_mouse_disable_bytes() {
    let flag = new_flag();
    assert!(!crate::input::is_suppressed(&flag));
    let _pty = Pty::spawn_with_args(
        "/bin/sh",
        &[
            "-c",
            "sleep 0.2; printf '\\033[?1003l\\033[?1006l'; sleep 2",
        ],
        (24, 80),
        &[],
        None,
        flag.clone(),
        || {},
    )
    .expect("failed to spawn PTY");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !crate::input::is_suppressed(&flag) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        crate::input::is_suppressed(&flag),
        "reader thread must flip the flag on a mouse-disable sequence"
    );
}

/// The exit path force-sets the flag: a child that dies WITHOUT ever
/// emitting a disable sequence (kill window coverage) still leaves the
/// flag set once the reader observes the exit.
#[tokio::test]
async fn read_loop_sets_suppress_flag_on_child_exit() {
    let flag = new_flag();
    assert!(!crate::input::is_suppressed(&flag));
    let mut pty = Pty::spawn_with_args(
        "/bin/sleep",
        &["0"],
        (24, 80),
        &[],
        None,
        flag.clone(),
        || {},
    )
    .expect("failed to spawn PTY");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let event = tokio::time::timeout(Duration::from_secs(2), pty.recv()).await;
        match event {
            Ok(Some(PtyEvent::Exit(_))) => break,
            Ok(Some(PtyEvent::Output(_))) => {}
            Ok(None) | Err(_) => break,
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
    }
    assert!(
        crate::input::is_suppressed(&flag),
        "EOF/EIO exit path must force-set the suppression flag"
    );
}

// ── v1.11.16 (Fix B1): exit-race zombie leak on receiver drop ──────

/// Dropping the `Pty` (which drops the only event receiver) must NOT
/// leave the child as a zombie. The read loop's `tx.send()` failure arm
/// used to `return` early, skipping `waitpid_safe` and leaking the child
/// until weft exited. After the fix it sends a defensive SIGHUP and
/// `break`s into the shared exit tail, which reaps the child.
#[tokio::test]
async fn receiver_drop_during_flood_reaps_child() {
    // A child that floods output so the read loop is busy in the
    // send arm when we drop the Pty. `yes` is spawned directly (not via
    // `sh -c`) so SIGHUP kills the child itself rather than leaving a
    // grandchild holding the PTY open.
    let mut pty = Pty::spawn_with_args("/usr/bin/yes", &[], (24, 80), &[], None, new_flag(), || {})
        .expect("failed to spawn flooding PTY");

    // Pump a few events so the read loop is running and the channel
    // is live, then capture the pid and drop the whole Pty — this
    // drops the receiver, which makes the next `tx.send()` fail.
    for _ in 0..5 {
        let _ = tokio::time::timeout(Duration::from_millis(500), pty.recv()).await;
    }
    let pid = pty.child_pid();
    drop(pty);

    // Poll until the child has been fully reaped. If the read loop did
    // its job, `waitpid(WNOHANG)` returns ECHILD (no zombie left). The
    // loop converges to ECHILD either because the read loop reaped it or
    // because our own WNOHANG reaped a transient zombie — both prove no
    // permanent zombie.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut reaped = false;
    while tokio::time::Instant::now() < deadline {
        match nix::sys::wait::waitpid(pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG)) {
            Err(nix::errno::Errno::ECHILD) => {
                reaped = true;
                break;
            }
            _ => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
    assert!(
        reaped,
        "child must be reaped (waitpid => ECHILD), not left as a zombie on receiver drop"
    );
}

/// The same receiver-drop path must force-set the shared mouse
/// suppression flag — consistent with the EOF/EIO exit path.
#[tokio::test]
async fn receiver_drop_sets_mouse_suppressed() {
    let flag = new_flag();
    assert!(!crate::input::is_suppressed(&flag));

    let mut pty = Pty::spawn_with_args(
        "/usr/bin/yes",
        &[],
        (24, 80),
        &[],
        None,
        flag.clone(),
        || {},
    )
    .expect("failed to spawn flooding PTY");

    for _ in 0..5 {
        let _ = tokio::time::timeout(Duration::from_millis(500), pty.recv()).await;
    }
    drop(pty);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if crate::input::is_suppressed(&flag) {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("receiver-drop path must force-set the suppression flag");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// T8 tests (read_batch stop conditions, wake table, flood integrity)
// live in a child module file to stay under the 800-line gate cap;
// `super::*` from the child reaches everything imported above.
#[path = "wake_and_batch_tests.rs"]
mod wake_and_batch_tests;
