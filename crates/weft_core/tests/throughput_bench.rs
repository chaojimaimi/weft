//! v1.12.17 T0 吞吐基准（docs/PLAN_v11217_PERF_TRAIN.md §3.1）。
//!
//! 无 GUI、纯 CPU 度量 `Terminal::process` 的消化速率，为 T1/T2 提供
//! before/after、为 T5/T7 提供归因靶场。沿袭 `bench_v110_scale.rs` 的
//! `#[test] #[ignore]` 计时惯例（不引入 criterion/bench crate），新增价值：
//! 1. OSC 133;A/B/C/D 序列驱动 BlockTracker 进入 CommandExecuting 捕获相位
//!    （生产形态——`on_print_ascii_run`/`on_newline` 捕获路径参与测量）；
//! 2. 256KB 分块喂入（生产 per-message 形态：PTY 读缓冲与
//!    `MAX_BYTES_PER_MESSAGE` 均为 256KB），区别于既有 bench 的整块一次喂。
//!
//! 手动运行（必须 --release，debug 数字无意义）：
//!
//! ```sh
//! cargo test -p weft_core --test throughput_bench --release -- --ignored --nocapture --test-threads=1
//! ```
//!
//! 断言纪律：只设极宽松地板（<2MB/s panic，防灾难性回退），禁止精确时序
//! 断言（flake 纪律）。主数字靠人读，3 轮取中位数回填 PLAN §4。

use std::time::Instant;
use weft_core::vt::Terminal;

/// 生产 per-message 形态：PTY 读缓冲 == MAX_BYTES_PER_MESSAGE == 256KB，
/// 单条 PtyOutput 永不超限，按此切块喂 `Terminal::process`。
const CHUNK_BYTES: usize = 256 * 1024;

/// 长行基准目标总量：16 MiB（行数由此推算取整，余数舍去）。
const LONG_LINE_TARGET_BYTES: usize = 16 * 1024 * 1024;

/// 长行每行恰好 45 字节 = 44 内容 + '\n'（模拟 cat 大文件）。
const LONG_LINE_BYTES: usize = 45;

/// 长行行数：16 MiB / 45 B，余数舍去。
const LONG_LINES: usize = LONG_LINE_TARGET_BYTES / LONG_LINE_BYTES;

/// 短行基准规模：30 万行（模拟 seq 输出；按行数定规模，v1.12.16 速率下
/// 约 6s/轮，避免按字节定规模导致基准过重——PLAN §7 P3 修订）。
const SHORT_LINES: usize = 300_000;

/// 生产级网格尺寸 40×120 + 10k scrollback（区别于 bench_v110_scale 的
/// 24×80）。尺寸影响 encode_row 每行成本：列数越多，逐格编码越贵，
/// 后续 T2（encode_row 快速通道）的收益与本尺寸绑定。
fn production_terminal() -> Terminal {
    Terminal::with_scrollback(40, 120, 10_000)
}

/// 16 MiB 长行流：每行 45 字节，零垫行号 + 'x' 填充，确定性内容（无 RNG）。
fn long_line_payload() -> Vec<u8> {
    let mut buf = Vec::with_capacity(LONG_LINES * LONG_LINE_BYTES);
    for i in 0..LONG_LINES {
        // 8 位零垫行号（ LONG_LINES < 10^8，宽度恒定），'x' 补齐到 44 内容字节。
        let prefix = format!("{:08}", i);
        let fill = LONG_LINE_BYTES - 1 - prefix.len();
        buf.extend_from_slice(prefix.as_bytes());
        buf.extend(std::iter::repeat(b'x').take(fill));
        buf.push(b'\n');
    }
    buf
}

/// 30 万行短行流：`format!("{}\n", i)`，i = 1..=300_000，与 seq 输出同形态。
fn short_line_payload() -> Vec<u8> {
    let mut buf = Vec::with_capacity(SHORT_LINES * 8);
    for i in 1..=SHORT_LINES {
        buf.extend_from_slice(format!("{}\n", i).as_bytes());
    }
    buf
}

/// 用 OSC 133;A/B/C/D 包裹输出载荷（仿 bench_v110_scale::synthetic_shell_output
/// 的帧序列）：133;A 提示符起 → 133;B 命令起（捕获自此开始，相位
/// CommandExecuting）→ 133;C 输出起 → 载荷 → 133;D;0 命令终。
/// 这是 shell 集成下的生产形态，捕获路径参与测量。
fn shell_integrated(payload: &[u8], command: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(payload.len() + 64);
    buf.extend_from_slice(b"\x1b]133;A\x07$ \x1b]133;B\x07");
    buf.extend_from_slice(command.as_bytes());
    buf.extend_from_slice(b"\n\x1b]133;C\x07");
    buf.extend_from_slice(payload);
    buf.extend_from_slice(b"\x1b]133;D;0\x07");
    buf
}

/// 共享计时 harness：两轮取第二轮（首轮预热：页错误、分支预测、分配器
/// 冷启动均为一次性成本），256KB 分块循环 `terminal.process(chunk)`。
/// 打印 elapsed_ms / 总字节 / MB/s / ns每字节 / µs每行，并输出
/// V11217_METRIC 行（沿 V110_METRIC 惯例）。
fn run_stream_bench(name: &str, bytes: &[u8], lines: usize) {
    let mut elapsed_ms = 0.0;
    for round in 1..=2 {
        let mut terminal = production_terminal();
        let start = Instant::now();
        for chunk in bytes.chunks(CHUNK_BYTES) {
            terminal.process(chunk);
        }
        elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        if round == 1 {
            println!("  [{name}] warm-up: {elapsed_ms:.2} ms (discarded)");
        }
    }

    let total_bytes = bytes.len();
    let secs = (elapsed_ms / 1000.0).max(1e-9);
    let mbps = (total_bytes as f64 / 1_048_576.0) / secs;
    let ns_per_byte = (elapsed_ms * 1_000_000.0) / total_bytes as f64;
    let us_per_line = (elapsed_ms * 1_000.0) / lines as f64;
    println!(
        "  [{name}] {elapsed_ms:.2} ms | {total_bytes} bytes | {lines} lines | \
         {mbps:.2} MB/s | {ns_per_byte:.1} ns/byte | {us_per_line:.3} µs/line"
    );
    println!("V11217_METRIC name={name} elapsed_ms={elapsed_ms:.3} mbps={mbps:.2}");

    // 极宽松地板：只防灾难性回退（如意外把生产路径变成 O(n²)），
    // 真实回归判读靠人对比 PLAN §4 基线，不做精确时序断言。
    assert!(
        mbps > 2.0,
        "{name}: {mbps:.2} MB/s below disaster floor of 2 MB/s — catastrophic regression"
    );
}

/// 16MB 长行流（捕获开）：模拟 `cat` 大文件。单命令块输出超 1MiB 时
/// Block 捕获缓冲按 MAX_OUTPUT_BYTES 截断——这是生产行为（网格始终持有
/// 完整显示输出，截断只影响块摘录），本基准刻意保留该路径参与测量。
#[test]
#[ignore]
fn bench_long_line_stream() {
    println!(
        "\n=== v1.12.17 T0 bench: 16 MiB long-line stream \
         (45 B/line, {} lines, capture on, 40x120) ===",
        LONG_LINES
    );
    let bytes = shell_integrated(&long_line_payload(), "cat weft-16m.bin");
    run_stream_bench("long_line_stream", &bytes, LONG_LINES);
}

/// 30 万行短行流（捕获开）：模拟 shell 集成下的 `seq 1 300000`。
#[test]
#[ignore]
fn bench_short_line_stream() {
    println!(
        "\n=== v1.12.17 T0 bench: {} short lines (seq form, capture on, 40x120) ===",
        SHORT_LINES
    );
    let bytes = shell_integrated(&short_line_payload(), "seq 1 300000");
    run_stream_bench("short_line_stream", &bytes, SHORT_LINES);
}

/// 同短行内容，但不进入 CommandExecuting 相位：无 OSC 133 标记（shell 未
/// 集成，NotIntegrated），捕获路径（on_print_ascii_run/on_newline）被门控
/// 跳过。与 bench_short_line_stream 的差 = 捕获 on/off 归因，服务后续
/// profile 立项（T5）。长行不做此变体。
#[test]
#[ignore]
fn bench_short_line_no_capture() {
    println!(
        "\n=== v1.12.17 T0 bench: {} short lines (seq form, capture OFF, 40x120) ===",
        SHORT_LINES
    );
    let bytes = short_line_payload();
    run_stream_bench("short_line_no_capture", &bytes, SHORT_LINES);
}
