//! P3 wall-clock probes (PLAN_v1136 §3 P3) — split out of
//! `parse_worker::tests` for the line-count gate. Child module via
//! `#[path]`; `use super::*` reaches the test harness imports.
//!
//! Run manually with
//!   cargo test -p weft_app --release -- --ignored --nocapture probe_real_pty

use super::*;
use std::sync::atomic::AtomicU64;
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
///
/// Completeness criteria are mutually exclusive, picked by which expectation
/// is non-zero: `expect_finalized_blocks` (capture-engaged runs), then
/// `expect_min_bytes` (binary payloads whose grid state is unreliable — see
/// the cat10m probes below), then `min_scrollback_lines` (plain text).
fn probe_pipeline(
    program: &str,
    args: &[&str],
    min_scrollback_lines: u64,
    expect_finalized_blocks: usize,
    expect_min_bytes: u64,
) -> std::time::Duration {
    probe_pipeline_sized(
        program,
        args,
        min_scrollback_lines,
        expect_finalized_blocks,
        expect_min_bytes,
        (30, 100),
    )
}

fn probe_pipeline_sized(
    program: &str,
    args: &[&str],
    min_scrollback_lines: u64,
    expect_finalized_blocks: usize,
    expect_min_bytes: u64,
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
    let pty_rx = pty.take_event_rx().expect("probe pty has a receiver");
    // Byte-completeness tee (PLAN_v1138 §2.1): when the payload is binary
    // the terminal state cannot prove ingestion (see the cat10m probe
    // comment below), so the INGEST bytes are counted instead — the plan's
    // "accumulated batch bytes" option. A forwarder thread counts each
    // `Output` batch on its way into the worker; the tee adds one cheap
    // channel hop and is only engaged when `expect_min_bytes` is set, so
    // every other probe keeps the exact production pipeline shape.
    let (rx, byte_total) = if expect_min_bytes > 0 {
        let (forward_tx, forward_rx) = mpsc::channel::<PtyEvent>(1024);
        let total = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&total);
        std::thread::Builder::new()
            .name("probe-byte-tee".to_string())
            .spawn(move || {
                let mut source = pty_rx;
                while let Some(event) = source.blocking_recv() {
                    if let PtyEvent::Output(data) = &event {
                        counter.fetch_add(data.len() as u64, Ordering::Relaxed);
                    }
                    let is_exit = matches!(event, PtyEvent::Exit(_));
                    if forward_tx.blocking_send(event).is_err() {
                        break;
                    }
                    // Exit is the last event the worker needs; dropping the
                    // sender ends the tee thread.
                    if is_exit {
                        break;
                    }
                }
            })
            .expect("probe tee thread");
        (forward_rx, Some(total))
    } else {
        (pty_rx, None)
    };
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
    if expect_finalized_blocks > 0 {
        // Capture-engaged run: under screen capture the scrollback
        // position does not advance; completeness = the 133;D finalized
        // the block (finalize happens inside `process`, worker-side).
        let blocks = terminal.lock().block_tracker().session_blocks().len();
        assert!(
            blocks >= expect_finalized_blocks,
            "block not finalized after PtyExited (blocks {blocks})"
        );
    } else if let Some(total) = &byte_total {
        // Binary payload run: ingest-byte criterion (see the tee above).
        // The counter moves before each batch is forwarded, and PtyExited
        // is FIFO-ordered after every Output on the same channel, so the
        // count is final once the exit is observable.
        let ingested = total.load(Ordering::Relaxed);
        assert!(
            ingested >= expect_min_bytes,
            "output tail missing after PtyExited (ingested {ingested} bytes < {expect_min_bytes})"
        );
    } else {
        let lines = terminal.lock().grid().scrollback.position();
        assert!(
            lines >= min_scrollback_lines,
            "output tail missing after PtyExited (scrollback position {lines})"
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
            0,
        );
    println!("[probe] seq2m integrated (capture ON) wall = {wall:?}");
}

#[test]
#[ignore = "wall-clock probe: cargo test -p weft_app --release -- --ignored --nocapture probe_real_pty"]
fn probe_real_pty_seq2m_big_window() {
    // Desktop-window geometry (≈170×45): rules out grid/scrollback
    // per-line cost scaling with cols as the GUI-vs-headless delta.
    let wall = probe_pipeline_sized(
        "/bin/sh",
        &["-c", "seq 2000000"],
        1_990_000,
        0,
        0,
        (45, 170),
    );
    println!("[probe] seq2m big-window (45x170) wall = {wall:?}");
}

#[test]
#[ignore = "wall-clock probe: cargo test -p weft_app --release -- --ignored --nocapture probe_real_pty"]
fn probe_real_pty_seq2m_single_pane() {
    let wall = probe_pipeline("/bin/sh", &["-c", "seq 2000000"], 1_990_000, 0, 0);
    println!("[probe] seq2m single-pane wall = {wall:?} (headless: no render/present)");
}

#[test]
#[ignore = "wall-clock probe: cargo test -p weft_app --release -- --ignored --nocapture probe_real_pty"]
fn probe_real_pty_seq2m_two_pane_parallel() {
    // Two independent pipelines at once — the multi-pane amplification
    // probe (§5.1: amplification <= +15% over single-pane).
    let (t1, t2) = std::thread::scope(|scope| {
        let a = scope.spawn(|| probe_pipeline("/bin/sh", &["-c", "seq 2000000"], 1_990_000, 0, 0));
        let b = scope.spawn(|| probe_pipeline("/bin/sh", &["-c", "seq 2000000"], 1_990_000, 0, 0));
        (a.join().unwrap(), b.join().unwrap())
    });
    let worst = t1.max(t2);
    println!("[probe] seq2m two-pane walls = {t1:?} / {t2:?} (worst {worst:?})");
}

#[test]
#[ignore = "wall-clock probe: cargo test -p weft_app --release -- --ignored --nocapture probe_real_pty"]
fn probe_real_pty_echo_and_cat_sanity() {
    let echo = probe_pipeline("/bin/sh", &["-c", "echo hi"], 0, 0, 0);
    let cat = probe_pipeline(
        "/bin/sh",
        &["-c", "yes AB0123456789 | head -c 1048576"],
        70_000,
        0,
        0,
    );
    println!("[probe] echo wall = {echo:?}   cat 1MiB wall = {cat:?}");
}

// ── cat_10mb_bin variance attribution (PLAN_v1138 §2.1) ─────────────
// GUI bench variance on this payload is 0.123/0.142/0.175s (median
// 0.142, v1.13.5/6/7 consistent) while plain-text cat is a constant
// ~0.025s; the only differing variable is the RANDOM BINARY stream
// (control bytes + high-entropy bytes). Both variants feed
// `head -c 10485760 /dev/urandom` through a real PTY — bare (no OSC
// 133, no capture) and integrated (133;A/B/C … D, capture swallows the
// whole ~10 MB, matching the GUI bench's integrated shell). Attribution
// rule: neither variant reproducing the variance ⇒ GUI render/atlas
// side; either reproducing ⇒ parse/storage/capture side, localized by
// variant.
//
// Completeness criteria — the plan's "bare/integrated pick-one" maps
// onto the tee's ingest-byte counter for BOTH variants, and here is why
// the alternative criteria are unusable on this payload:
//   - Scrollback lines: urandom has no newline guarantee (plan-noted).
//   - FlatStorage content bytes: the stream contains ~160 expected
//     `ESC c` (RIS full resets) per 10 MiB (1/256 × 1/256 × 10M) —
//     each RIS replaces the whole Terminal (`vt/mod.rs reset()`),
//     so any grid-state read only observes the post-last-RIS tail
//     (~64 KiB mean).
//   - `expect_finalized_blocks = 1` for the integrated variant: same
//     RIS hazard — a reset clears `pending_command`, and finalize
//     (blocks/continuation.rs) builds no block without it, so the
//     trailing 133;D would (correctly) produce zero blocks almost
//     every run. The ingest-byte counter is immune (it counts the
//     wire, not the grid) and is symmetric across both variants, so
//     the bare-vs-integrated delta stays a pure capture/tracker cost.
// Probe safety: NUL (0x00) hits vte execute and is ignored
// (perform.rs:384 only traces); random control bytes only churn grid
// state; the MouseDisableScanner is a passive FSM whose false-hit
// probability on random bytes is negligible (mouse_suppress.rs:105-111).

/// 5 trials of one cat10m-binary variant in this process, one
/// `[probe] cat10m-binary <variant> trial N wall = ...` line per round
/// and a median line after the fifth.
fn cat10m_binary_trials(variant: &str, command: &str, expect_min_bytes: u64) {
    let mut walls = [std::time::Duration::ZERO; 5];
    for (index, slot) in walls.iter_mut().enumerate() {
        let wall = probe_pipeline("/bin/sh", &["-c", command], 0, 0, expect_min_bytes);
        println!(
            "[probe] cat10m-binary {variant} trial {} wall = {wall:?}",
            index + 1
        );
        *slot = wall;
    }
    walls.sort();
    println!(
        "[probe] cat10m-binary {variant} median wall = {:?}",
        walls[2]
    );
}

#[test]
#[ignore = "wall-clock probe: cargo test -p weft_app --release -- --ignored --nocapture probe_real_pty"]
fn probe_real_pty_cat10m_binary_bare() {
    cat10m_binary_trials("bare", "head -c 10485760 /dev/urandom", 10_485_760);
}

#[test]
#[ignore = "wall-clock probe: cargo test -p weft_app --release -- --ignored --nocapture probe_real_pty"]
fn probe_real_pty_cat10m_binary_integrated() {
    // NOTE: `\\033` (not `\033`) — Rust has no 3-digit octal escape;
    // `\07` would embed a NUL and CString::new rejects the argv. The
    // literal backslash reaches sh's printf, which does the octal →
    // ESC/BEL conversion itself (see probe_real_pty_seq2m_shell_integrated).
    cat10m_binary_trials(
        "integrated",
        "printf '\\033]133;A\\007\\033]133;B\\007\\033]133;C\\007'; head -c 10485760 /dev/urandom; printf '\\033]133;D\\007'",
        10_485_760,
    );
}
