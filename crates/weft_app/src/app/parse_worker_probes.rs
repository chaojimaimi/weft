//! P3 wall-clock probes (PLAN_v1136 §3 P3) — split out of
//! `parse_worker::tests` for the line-count gate. Child module via
//! `#[path]`; `use super::*` reaches the test harness imports.
//!
//! Run manually with
//!   cargo test -p weft_app --release -- --ignored --nocapture probe_real_pty

use super::*;
use std::sync::Mutex as StdMutex;

// ── P3 wall-clock probes (PLAN_v1136 §3 P3) ─────────────────────────
// #[ignore] benchmarks: run manually with
//   cargo test -p weft_app --release -- --ignored --nocapture probe_real_pty
// Real PTY + real reader (T8/T5' batching) + real parse worker — the
// full production pipeline minus GUI render/present. The user-facing
// `time seq 2000000` acceptance number includes render; this isolates
// the scheduling win.

/// One production pipeline: real Pty -> parse worker -> FairMutex
/// Terminal. Returns wall time from spawn until PtyExited (FIFO
/// guarantees the terminal is fully caught up at that point).
fn probe_pipeline(
    program: &str,
    args: &[&str],
    min_scrollback_lines: u64,
    expect_finalized_blocks: usize,
) -> std::time::Duration {
    probe_pipeline_sized(
        program,
        args,
        min_scrollback_lines,
        expect_finalized_blocks,
        (30, 100),
    )
}

fn probe_pipeline_sized(
    program: &str,
    args: &[&str],
    min_scrollback_lines: u64,
    expect_finalized_blocks: usize,
    size: (u16, u16),
) -> std::time::Duration {
    use weft_core::input::mouse_suppress::new_flag;
    use weft_core::pty::Pty;

    // Pty::spawn_with_args tokio::spawns its reader, so the probe needs
    // a runtime context (the real app provides main's runtime).
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("probe runtime");
    let _runtime = rt.enter();

    let t0 = Instant::now();
    let mut pty = Pty::spawn_with_args(program, args, size, &[], None, new_flag())
        .expect("probe spawns a shell");
    let rx = pty.take_event_rx().expect("probe pty has a receiver");
    let writer = pty.writer();
    let terminal = Arc::new(FairMutex::new(Terminal::with_scrollback(
        size.0 as usize,
        size.1 as usize,
        10_000,
    )));
    let (ctrl_tx, ctrl_rx) = crossbeam_channel::unbounded();
    let had_output = Arc::new(AtomicBool::new(false));
    // Record the LAST wake: the worker wakes (throttled ~2ms) per batch
    // drain, so `last_wake - t0` ≈ the `time seq` drain semantics of the
    // comparison reports (seq exits once the reader has drained the
    // pty), while the returned wall time is the stricter full-parse
    // completion (PtyExited fires after the FIFO tail is parsed).
    let last_wake: Arc<StdMutex<Option<Instant>>> = Arc::new(StdMutex::new(None));
    let wake_sink = Arc::clone(&last_wake);
    let wake: WakeFn = Box::new(move || {
        *wake_sink.lock().unwrap() = Some(Instant::now());
    });
    spawn(
        0,
        rx,
        Arc::clone(&terminal),
        writer,
        ctrl_tx,
        had_output,
        wake,
    );
    loop {
        match ctrl_rx
            .recv_timeout(std::time::Duration::from_secs(120))
            .expect("probe pipeline reports PtyExited")
        {
            AppMsg::PtyExited(_) => break,
            AppMsg::AltFlipped { .. } => continue,
        }
    }
    let wall = t0.elapsed();
    // FIFO + exit contract: at PtyExited the terminal has every byte.
    // Bare /bin/sh emits no OSC 133, so completeness is proven by the
    // scrollback's monotonic rows-ever-pushed counter (`position`
    // survives truncation) — seq 2M must have pushed ~2M lines.
    if expect_finalized_blocks == 0 {
        let lines = terminal.lock().grid().scrollback.position();
        assert!(
            lines >= min_scrollback_lines,
            "output tail missing after PtyExited (scrollback position {lines})"
        );
    } else {
        // Capture-engaged run: under screen capture the scrollback
        // position does not advance; completeness = the 133;D finalized
        // the block (finalize happens inside `process`, worker-side).
        let blocks = terminal.lock().block_tracker().session_blocks().len();
        assert!(
            blocks >= expect_finalized_blocks,
            "block not finalized after PtyExited (blocks {blocks})"
        );
    }
    if let Some(lw) = *last_wake.lock().unwrap() {
        println!(
            "[probe]   drain-to-last-wake = {:?}  full-parse = {wall:?}",
            lw.duration_since(t0)
        );
    }
    wall
}

#[test]
#[ignore = "wall-clock probe: cargo test -p weft_app --release -- --ignored --nocapture probe_real_pty"]
fn probe_real_pty_seq2m_shell_integrated() {
    // Discriminator (2026-10-09 frame-trace round): the bare-sh probe
    // never engages block capture (no OSC 133), while the GUI benchmark
    // runs zsh WITH shell integration — capture + live styled tracking
    // on every byte. Same payload wrapped in 133;A/B/C isolates the
    // capture/tracker cost on the worker.
    let wall = probe_pipeline(
            "/bin/sh",
            &[
                "-c",
                // NOTE: `\\033` (not `\033`) — Rust has no 3-digit octal
                // escape; `\07` would embed a NUL and CString::new rejects
                // the argv. The literal backslash reaches sh's printf, which
                // does the octal → ESC/BEL conversion itself.
                "printf '\\033]133;A\\007\\033]133;B\\007\\033]133;C\\007'; seq 2000000; printf '\\033]133;D\\007'",
            ],
            0,
            1,
        );
    println!("[probe] seq2m integrated (capture ON) wall = {wall:?}");
}

#[test]
#[ignore = "wall-clock probe: cargo test -p weft_app --release -- --ignored --nocapture probe_real_pty"]
fn probe_real_pty_seq2m_big_window() {
    // Desktop-window geometry (≈170×45): rules out grid/scrollback
    // per-line cost scaling with cols as the GUI-vs-headless delta.
    let wall = probe_pipeline_sized("/bin/sh", &["-c", "seq 2000000"], 1_990_000, 0, (45, 170));
    println!("[probe] seq2m big-window (45x170) wall = {wall:?}");
}

#[test]
#[ignore = "wall-clock probe: cargo test -p weft_app --release -- --ignored --nocapture probe_real_pty"]
fn probe_real_pty_seq2m_single_pane() {
    let wall = probe_pipeline("/bin/sh", &["-c", "seq 2000000"], 1_990_000, 0);
    println!("[probe] seq2m single-pane wall = {wall:?} (headless: no render/present)");
}

#[test]
#[ignore = "wall-clock probe: cargo test -p weft_app --release -- --ignored --nocapture probe_real_pty"]
fn probe_real_pty_seq2m_two_pane_parallel() {
    // Two independent pipelines at once — the multi-pane amplification
    // probe (§5.1: amplification <= +15% over single-pane).
    let (t1, t2) = std::thread::scope(|scope| {
        let a = scope.spawn(|| probe_pipeline("/bin/sh", &["-c", "seq 2000000"], 1_990_000, 0));
        let b = scope.spawn(|| probe_pipeline("/bin/sh", &["-c", "seq 2000000"], 1_990_000, 0));
        (a.join().unwrap(), b.join().unwrap())
    });
    let worst = t1.max(t2);
    println!("[probe] seq2m two-pane walls = {t1:?} / {t2:?} (worst {worst:?})");
}

#[test]
#[ignore = "wall-clock probe: cargo test -p weft_app --release -- --ignored --nocapture probe_real_pty"]
fn probe_real_pty_echo_and_cat_sanity() {
    let echo = probe_pipeline("/bin/sh", &["-c", "echo hi"], 0, 0);
    let cat = probe_pipeline(
        "/bin/sh",
        &["-c", "yes AB0123456789 | head -c 1048576"],
        70_000,
        0,
    );
    println!("[probe] echo wall = {echo:?}   cat 1MiB wall = {cat:?}");
}
