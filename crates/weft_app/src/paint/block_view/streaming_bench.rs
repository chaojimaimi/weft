//! T16a step 1 (PLAN_v11217 §3.11): headless streaming micro-bench — the
//! PRIMARY measurement instrument for the seq 7.25s-vs-5s attribution.
//!
//! Feeds an OSC 133 `CommandExecuting` stream into a production-shaped
//! `Terminal` in ~15 KB batches (the T5' hold/consume production shape),
//! and after EVERY batch times the render-side hot segments:
//!
//! 1. `process` — `Terminal::process` whole chain: VT parse and block
//!    capture plus the 100ms-throttled live-styled publish (core side; only
//!    the styled variant can confirm candidate ①).
//! 2. `block_view_live` — full `build_block_view_vertices` with the live
//!    head (active pane, CommandExecuting form — the draw_phases.rs model
//!    verbatim), including the LiveLayoutCache sync + visible-window slice.
//! 3. `block_view_nolive` — same model, `live: None` — the delta vs (2)
//!    isolates the live-head cost from finished-block painting.
//! 4. `tui_cursor_scan` — the `block_view_tui_cursor` formula fallback:
//!    tail-line index + display-column scan. Plain shell commands have NO
//!    cursor snapshot (screen-exit-only), so this branch runs EVERY frame
//!    (pre-fix it was a double O(text) scan — candidate ② shape).
//! 5. `grid_bg` — `build_grid_instances_for_background_pane` on the same
//!    terminal (B3-2 incremental row cache).
//!
//! Dual variants (review P2): PLAIN text stream (seq shape) and SGR-styled
//! stream (grep --color shape: a color pair every 7th line). Candidate ①
//! (live-styled 100ms O(text) rebuild) can ONLY be confirmed on the styled
//! variant — the plain stream's `peek_styled` early-exits on empty runs.
//!
//! NOT covered (user GUI acceptance, frame_trace): the Metal encode /
//! buffer-upload / GPU segments and the vsync-paced real frame cadence.
//! This bench attributes CPU-side per-batch cost only.
//!
//! Run (release 口径 — debug numbers are meaningless):
//! `cargo test --release -p weft_app --bin weft streaming_bench -- --ignored --nocapture --test-threads=1`
//!
//! 断言纪律：只设结构性/极宽松地板断言，禁止精确时序断言（flake 纪律）。
//! 主数字靠人读，前后对照回填 PLAN 报告。
//!
//! Precedent: `paint::block_view::bench` (four-phase), `weft_core`
//! `throughput_bench.rs` (generator shape, copied here per plan — no
//! cross-crate bench dependency).

use std::collections::HashMap;
use std::time::Instant;

use weft_core::selection::SelectionHandler;
use weft_core::vt::Terminal;

use crate::paint::block_view_model::BlockViewPaintModel;

/// T5' production consumption shape: ~15 KB per batch (~15KB/4ms supply).
const BATCH_BYTES: usize = 15 * 1024;

/// Stream scale: enough batches to cross several 100ms live-styled publish
/// windows (~300 batches ≈ multi-second wall time in release).
const TOTAL_LINES: usize = 750_000;

/// Production grid shape (throughput_bench precedent).
fn production_terminal() -> Terminal {
    Terminal::with_scrollback(40, 120, 10_000)
}

/// seq 形态行：`{i}\n`（与 seq 输出同形态，行号宽度自然增长）。
fn plain_stream() -> Vec<u8> {
    let mut buf = Vec::with_capacity(TOTAL_LINES * 8);
    for i in 1..=TOTAL_LINES {
        buf.extend_from_slice(format!("{i}\n").as_bytes());
    }
    buf
}

/// grep --color 形态：每 7 行一次 SGR 色对（红行号），其余行为纯文本。
/// 行结构与 plain 变体逐行对齐，仅插入 `ESC[31m`/`ESC[0m` 包裹。
fn styled_stream() -> Vec<u8> {
    let mut buf = Vec::with_capacity(TOTAL_LINES * 16);
    for i in 1..=TOTAL_LINES {
        if i % 7 == 0 {
            buf.extend_from_slice(format!("\x1b[31m{i}\x1b[0m\n").as_bytes());
        } else {
            buf.extend_from_slice(format!("{i}\n").as_bytes());
        }
    }
    buf
}

/// OSC 133 CommandExecuting 帧：133;A 提示符起 → 133;B 命令起（捕获自此
/// 开始）→ 133;C 输出起 → 载荷。刻意不结束（无 133;D）——测量的是流式
/// in-flight 形态，与 `seq` 运行中的真实 GUI 帧一致。
fn command_executing_frame(payload: &[u8], command: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(payload.len() + 64);
    buf.extend_from_slice(b"\x1b]133;A\x07$ \x1b]133;B\x07");
    buf.extend_from_slice(command.as_bytes());
    buf.extend_from_slice(b"\n\x1b]133;C\x07");
    buf.extend_from_slice(payload);
    buf
}

/// One per-batch sample: µs per segment, u32 is plenty (a pathological
/// batch is tens of ms = tens of thousands of µs).
#[derive(Default, Clone, Copy)]
struct BatchSample {
    process_us: u32,
    block_view_live_us: u32,
    block_view_nolive_us: u32,
    tui_cursor_scan_us: u32,
    grid_bg_us: u32,
}

fn summarize(name: &str, label: &str, samples: &[u32]) -> (f64, f64, f64) {
    let mut sorted: Vec<u32> = samples.to_vec();
    sorted.sort_unstable();
    let n = sorted.len().max(1);
    let total: u64 = samples.iter().map(|&s| s as u64).sum();
    let avg = total as f64 / n as f64;
    let p50 = sorted[n / 2] as f64;
    let max = sorted.last().copied().unwrap_or(0) as f64;
    println!(
        "  [{name}] {label:<22} total {total:>9} µs | avg {avg:>9.1} | p50 {p50:>9.1} | max {max:>9.1} ({} batches)",
        samples.len()
    );
    (avg, p50, max)
}

fn metric_line(name: &str, variant: &str, seg: &str, s: (f64, f64, f64)) {
    println!(
        "V11217_METRIC bench=streaming variant={variant} seg={seg} name={name} avg_us={:.1} p50_us={:.1} max_us={:.1}",
        s.0, s.1, s.2
    );
}

/// Mirror of `MetalRenderer::block_view_tui_cursor`'s formula fallback —
/// the POST-T16a production shape: O(1) tail-line index from the tracker's
/// newline ledger + an O(tail line) display-column scan (pre-fix this was
/// `lines().count()` + `lines().nth()` — two O(capture) passes per frame).
fn tui_cursor_formula_scan(terminal: &Terminal) -> usize {
    let Some(live) = terminal.block_tracker().in_flight() else {
        return 0;
    };
    let line = terminal.block_tracker().live_cursor_tail_line();
    let _ = crate::paint::tui_caret::tui_cursor_tail_display_col(live.output, 120);
    line
}

/// Active-pane CommandExecuting model (draw_phases.rs verbatim shape),
/// with `live` switchable to isolate the live-head cost.
fn active_pane_model<'a>(
    terminal: &'a Terminal,
    live: Option<weft_core::blocks::InFlightBlock<'a>>,
    diagnose: &'a HashMap<weft_core::blocks::BlockId, crate::app_state::BlockDiagnoseState>,
) -> BlockViewPaintModel<'a> {
    BlockViewPaintModel {
        blocks: terminal.block_tracker().session_blocks(),
        live_head_lines: terminal.screen_head_lines(),
        region_bottom_y: 600.0,
        cwd: terminal.cwd(),
        git_branch: terminal.git_branch(),
        live,
        block_scroll: 0.0,
        viewport_rows: terminal.grid().num_rows,
        block_hovered: None,
        block_selected: None,
        block_action_hovered: None,
        spinner_phase: -1.0,
        find_block_highlight: None,
        palette: terminal.palette(),
        cache_namespace: 1,
        block_diagnose_state: diagnose,
        ai_configured: false,
        tui_cursor: None,
        tui_preedit: None,
        cursor_blink_on: false,
        is_alt: terminal.is_alt_screen_active(),
        now: std::time::SystemTime::now(),
    }
}

/// One variant sweep: feed the stream batch-by-batch, sampling every
/// segment after each batch. Fresh Terminal per variant (clean tracker),
/// shared renderer (production caches behave like a real session).
fn run_variant(
    name: &'static str,
    payload: &[u8],
    renderer: &crate::renderer::MetalRenderer,
) -> Vec<BatchSample> {
    let mut terminal = production_terminal();
    let mut selection = SelectionHandler::new();
    let mut samples: Vec<BatchSample> = Vec::new();

    let mut first = true;
    let mut offset = 0usize;
    while offset < payload.len() {
        let end = (offset + BATCH_BYTES).min(payload.len());
        let batch = &payload[offset..end];
        offset = end;

        let mut s = BatchSample::default();

        let t = Instant::now();
        terminal.process(batch);
        s.process_us = t.elapsed().as_micros() as u32;

        // Structural floor: the OSC frame must have armed CommandExecuting,
        // or every downstream segment measures nothing.
        if first
            && terminal.block_tracker().phase() != weft_core::blocks::ShellPhase::CommandExecuting
        {
            panic!("{name}: stream did not reach CommandExecuting (OSC frame broken)");
        }
        first = false;

        // Active pane: live head present (CommandExecuting).
        let diagnose: HashMap<weft_core::blocks::BlockId, crate::app_state::BlockDiagnoseState> =
            HashMap::new();
        let t = Instant::now();
        let built = renderer.build_block_view_vertices(
            active_pane_model(&terminal, terminal.block_tracker().in_flight(), &diagnose),
            &mut selection,
        );
        s.block_view_live_us = t.elapsed().as_micros() as u32;
        if first && built.0.is_empty() {
            panic!("{name}: live block-view build produced no vertices");
        }

        // Same frame minus the live head (finished blocks only).
        let t = Instant::now();
        let _ = renderer.build_block_view_vertices(
            active_pane_model(&terminal, None, &diagnose),
            &mut selection,
        );
        s.block_view_nolive_us = t.elapsed().as_micros() as u32;

        // Per-frame O(text) formula fallback (plain commands: always).
        let t = Instant::now();
        let scanned = tui_cursor_formula_scan(&terminal);
        s.tui_cursor_scan_us = t.elapsed().as_micros() as u32;
        if first && scanned == 0 {
            panic!("{name}: cursor scan saw no lines mid-stream");
        }

        // Background grid pane counterpart (B3-2 incremental cache).
        let t = Instant::now();
        let (_batch, _rebuilt) = renderer.build_grid_instances_for_background_pane(&terminal, 1);
        s.grid_bg_us = t.elapsed().as_micros() as u32;

        samples.push(s);
    }
    samples
}

/// T16a step 1: dual-variant streaming micro-bench. Prints per-segment
/// avg/p50/max per variant; the styled-vs-plain `process` delta plus the
/// styled `max` spikes decide candidate ①, the `tui_cursor_scan` and
/// live-head deltas decide candidate ②.
#[test]
#[ignore = "T16a attribution bench; run with --release --ignored --nocapture --test-threads=1"]
fn bench_streaming_render_hotspots() {
    let Some(_device) = metal::Device::system_default() else {
        eprintln!("skipping streaming bench: no Metal device available");
        return;
    };
    let renderer =
        crate::renderer::MetalRenderer::new_headless_paint(weft_core::config::Theme::weft_dark());

    println!(
        "== T16a streaming bench: {TOTAL_LINES} lines, {} KB/batch, dual variant ==",
        BATCH_BYTES / 1024
    );

    for (variant, raw_payload) in [("plain", plain_stream()), ("styled", styled_stream())] {
        // CommandExecuting frame wraps the WHOLE payload; the markers land
        // in the first batch (production: shell integration is armed before
        // the command runs).
        let payload =
            command_executing_frame(raw_payload.as_slice(), &format!("seq {TOTAL_LINES}"));
        let borrowed = &payload[..];
        let samples = run_variant(variant, borrowed, &renderer);
        println!("-- variant {variant} ({} bytes) --", payload.len());
        metric_line(
            "streaming_hotspots",
            variant,
            "process",
            summarize(
                variant,
                "process (parse+capture+publish)",
                &samples.iter().map(|s| s.process_us).collect::<Vec<_>>(),
            ),
        );
        metric_line(
            "streaming_hotspots",
            variant,
            "block_view_live",
            summarize(
                variant,
                "block_view (live head)",
                &samples
                    .iter()
                    .map(|s| s.block_view_live_us)
                    .collect::<Vec<_>>(),
            ),
        );
        metric_line(
            "streaming_hotspots",
            variant,
            "block_view_nolive",
            summarize(
                variant,
                "block_view (no live)",
                &samples
                    .iter()
                    .map(|s| s.block_view_nolive_us)
                    .collect::<Vec<_>>(),
            ),
        );
        metric_line(
            "streaming_hotspots",
            variant,
            "tui_cursor_scan",
            summarize(
                variant,
                "tui_cursor formula scan",
                &samples
                    .iter()
                    .map(|s| s.tui_cursor_scan_us)
                    .collect::<Vec<_>>(),
            ),
        );
        metric_line(
            "streaming_hotspots",
            variant,
            "grid_bg",
            summarize(
                variant,
                "grid bg pane build",
                &samples.iter().map(|s| s.grid_bg_us).collect::<Vec<_>>(),
            ),
        );
    }
}
