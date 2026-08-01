//! v1.10.2 规模性能基准骨架（V110_IMPLEMENTATION_PLAN.md §3.4 + §6）。
//!
//! 沿用既有 `#[test] #[ignore]` 计时风格（见 vt/tests.rs::bench_*），不引入
//! criterion/bench crate。运行方式：
//!
//! ```sh
//! cargo test -p weft_core --test bench_v110_scale -- --ignored --nocapture --test-threads=1
//! ```
//!
//! 覆盖 V110_PLAN §3.4 预算表的 VT 层子集：
//! - 大体量 VT 输出解析（10 万 / 100 万行）
//! - 持续输出下 scrollback 滚动
//! - Smart Select 在长行上的延迟（无 panic + 时间预算）
//!
//! 注：Block 视图（10 万 Block）、Metal 帧耗时、GUI 控件基准属于 weft_app 层，
//! 需 Release App + Instruments，本文件不覆盖——记录在 docs/V110_MANUAL_ACCEPTANCE.md。

use std::time::Instant;
use weft_core::smart_select::match_at;
use weft_core::vt::Terminal;

/// 生成 `n` 行 `seq` 风格输出（带 ANSI 色 + OSC 133 标记），逼近真实 shell 输出。
fn synthetic_shell_output(n: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(n * 20);
    buf.extend_from_slice(b"\x1b]133;A\x07$ \x1b]133;B\x07seq 1 ");
    buf.extend_from_slice(n.to_string().as_bytes());
    buf.extend_from_slice(b"\n\x1b]133;C\x07");
    for i in 1..=n {
        if i % 7 == 0 {
            buf.extend_from_slice(b"\x1b[31m");
        }
        buf.extend_from_slice(i.to_string().as_bytes());
        buf.extend_from_slice(b"\n");
        if i % 7 == 0 {
            buf.extend_from_slice(b"\x1b[0m");
        }
    }
    buf.extend_from_slice(b"\x1b]133;D;0\x07\x1b]133;A\x07$ ");
    buf
}

#[test]
#[ignore]
fn bench_v110_vt_parse_100k_lines() {
    println!("\n=== v1.10 bench: parse 100k lines of shell output ===");
    let bytes = synthetic_shell_output(100_000);
    let mut t = Terminal::with_scrollback(24, 80, 10_000);
    let start = Instant::now();
    t.process(&bytes);
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    let mbps = (bytes.len() as f64 / 1_048_576.0) / (elapsed_ms / 1000.0).max(1e-9);
    println!(
        "  100k lines: {:.2} ms | {} bytes | {:.1} MB/s",
        elapsed_ms,
        bytes.len(),
        mbps
    );
    println!("V110_METRIC name=vt_parse size=100000 elapsed_ms={elapsed_ms:.3}");
    // 预算参考（V110_PLAN §3.4 间接）：大体量解析不应阻塞首屏。
    // 此处只记录数值，硬阈值留给 v1.10.0 基线测量后定（§3.4 末段）。
    assert!(
        elapsed_ms < 5000.0,
        "100k lines took {elapsed_ms:.0}ms, suspiciously slow"
    );
}

#[test]
#[ignore]
fn bench_v110_vt_parse_1m_lines_scrollback() {
    println!("\n=== v1.10 bench: parse 1M lines with scrollback ===");
    // 1M 行会触发 scrollback 淘汰——测量内存与解析综合表现
    let bytes = synthetic_shell_output(1_000_000);
    let mut t = Terminal::with_scrollback(24, 80, 10_000);
    let start = Instant::now();
    t.process(&bytes);
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    let mbps = (bytes.len() as f64 / 1_048_576.0) / (elapsed_ms / 1000.0).max(1e-9);
    println!(
        "  1M lines: {:.2} ms | {} bytes | {:.1} MB/s",
        elapsed_ms,
        bytes.len(),
        mbps
    );
    println!("V110_METRIC name=vt_parse size=1000000 elapsed_ms={elapsed_ms:.3}");
    // 预算：V110_PLAN §3.4 "百万行历史搜索首批 < 250ms"——此处测解析非搜索，
    // 仅保证不爆炸式增长（< 30s 为 sanity check）
    assert!(
        elapsed_ms < 30_000.0,
        "1M lines took {elapsed_ms:.0}ms, leak suspected"
    );
}

#[test]
#[ignore]
fn bench_v110_smart_select_on_long_line() {
    println!("\n=== v1.10 bench: Smart Select match_at on long line ===");
    // 模拟编译器输出：长行含多个 path:line:column + URL
    let mut line = String::with_capacity(10_000);
    for i in 0..200 {
        let next = i + 1;
        line.push_str(&format!("src/module_{i}/file.rs:{i}:{next} "));
    }
    line.push_str("see https://example.com/long/url/with/many/segments?q=1&v=2 ");

    // 在不同位置调用 match_at，测平均延迟
    let positions: Vec<usize> = (0..100)
        .map(|i| (line.len() / 100) * i)
        .filter(|&p| p < line.len())
        .collect();

    let start = Instant::now();
    let mut matches = 0usize;
    for &pos in &positions {
        if match_at(&line, pos).is_some() {
            matches += 1;
        }
    }
    let elapsed_us = start.elapsed().as_secs_f64() * 1_000_000.0;
    let per_call_us = elapsed_us / positions.len() as f64;
    println!(
        "  {} calls on {}-char line: {:.1} µs total, {:.2} µs/call, {} matches",
        positions.len(),
        line.len(),
        elapsed_us,
        per_call_us,
        matches
    );
    println!("V110_METRIC name=smart_select per_call_us={per_call_us:.3}");
    // 预算参考：Smart Select 应远快于人眼感知（< 1ms/call 是宽裕上限）
    assert!(
        per_call_us < 1000.0,
        "match_at too slow: {per_call_us:.1}µs/call"
    );
}

#[test]
#[ignore]
fn bench_v110_resize_under_load() {
    println!("\n=== v1.10 bench: resize during/after large output ===");
    // V110_PLAN §3.4 "8 Tab × 2 Pane 切换 p95 < 16ms"——此处测单 grid resize。
    let bytes = synthetic_shell_output(50_000);
    let mut t = Terminal::with_scrollback(40, 120, 10_000);
    t.process(&bytes);

    // 反复 resize，测平均延迟
    let sizes: [(usize, usize); 8] = [
        (24, 80),
        (40, 120),
        (50, 200),
        (30, 100),
        (60, 250),
        (24, 80),
        (10, 40),
        (40, 120),
    ];
    let start = Instant::now();
    for &(rows, cols) in &sizes {
        t.resize(rows, cols);
    }
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    let per_resize_ms = elapsed_ms / sizes.len() as f64;
    println!(
        "  {} resizes after 50k lines: {:.2} ms total, {:.2} ms/resize",
        sizes.len(),
        elapsed_ms,
        per_resize_ms
    );
    println!("V110_METRIC name=resize_after_output per_resize_ms={per_resize_ms:.3}");
    // V110_PLAN §3.3 性能决策门：此处只采集基线，不强阈值。
    // 50k 行 scrollback 下 resize ~75ms 是当前观测值。是否优化取决于：
    // (a) profile 是否指向 reflow/scrollback 拷贝为热点
    // (b) GUI 实测是否感知到卡顿（v1.10.3 dogfood）
    // 100ms 为 sanity check（防爆），真实预算待基线测量后按 §3.4 末段定。
    assert!(
        per_resize_ms < 100.0,
        "resize suspiciously slow: {per_resize_ms:.1}ms/resize — profile needed"
    );
}
