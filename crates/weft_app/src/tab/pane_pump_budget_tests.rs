//! T1 flood-aware frame budget tests (PLAN_v11217 §3.2 测试验收, 四层).
//!
//! Declared as a CHILD module of `pane_pump` (`#[cfg(test)] #[path]` at the
//! bottom of pane_pump.rs) so the pure strategy fn [`super::frame_budget`],
//! its constants, and the clock seam
//! `super::Tab::process_messages_with_clock` all stay private while being
//! pinned end to end.
//!
//! Layer map (真值表钉选择层、缝断言钉排水回路接线 — 两层合并才全覆盖,
//! 评审第三轮 §3.2 测试验收):
//! 1. truth table — frame_budget 四象限（flood==base / 预算颠倒 / 阈值
//!    反转三类策略映射缺陷在本层失败）;
//! 2. base unconditional — 机器无关底线（字节上限单帧内必延期）;
//! 3. seam exact stops — 冻结/跳变时钟钉排水停止点（观测量 =
//!    msg_rx.len() 前后差）;
//! 4. cross-pane Σ — 跨窗格求和端到端钉（防 active-only 复辟,
//!    FIX_background_pane_pump 历史 bug 形态）.

use std::cell::Cell;
use std::time::{Duration, Instant};

use super::super::Tab;
use super::{
    frame_budget, BASE_BYTES_PER_FRAME, BASE_TIME_BUDGET, FLOOD_BYTES_PER_FRAME, FLOOD_TIME_BUDGET,
};
use crate::pane::Pane;
use crate::AppMsg;
use weft_core::pane_layout::{PaneId, SplitDirection};

const KB: usize = 1024;

/// Same harness as `pane_pump::tests::two_pane_tab` (duplicated on purpose:
/// the budget items stay private to the pane_pump subtree). Returns
/// `(tab, background_id, active_id)` — `split_active_pane_test` focuses the
/// NEW pane, so `first` (the original root, smaller id) ends up in the
/// background.
fn two_pane_tab() -> (Tab, PaneId, PaneId) {
    let mut tab = Tab::with_single_pane(Pane::with_terminal_only(1000));
    let first = tab.active_pane_id();
    let second = tab
        .split_active_pane_test(SplitDirection::Vertical, 0.5, 1000)
        .expect("test split is infallible for a single-leaf tree");
    (tab, first, second)
}

fn inject(pane_id: PaneId, msg: AppMsg) -> impl FnOnce(&mut Tab) {
    move |tab: &mut Tab| {
        tab.pane(pane_id)
            .unwrap_or_else(|| panic!("pane {pane_id:?} vanished"))
            .msg_tx
            .send(msg)
            .expect("test channel is empty and bounded(1024)");
    }
}

/// A clock that never advances: every seam read returns the frame start,
/// so the time criteria can never fire and stop points are byte-determined.
fn frozen_clock() -> impl Fn() -> Instant {
    let t0 = Instant::now();
    move || t0
}

/// 层 1 — §3.2 测试验收 1（真值表，钉选择层）：frame_budget 四象限 +
/// T15c ×pane_count 扩容/饱和（编译级加参：N=1 时语义与 T1 逐位一致）。
#[test]
fn frame_budget_truth_table_four_quadrants() {
    let base = frame_budget(0, false, 1);
    assert_eq!(base.time, BASE_TIME_BUDGET, "0 积压 → 基础时间档");
    assert_eq!(base.bytes, BASE_BYTES_PER_FRAME, "0 积压 → 基础字节档");

    let base = frame_budget(15, false, 1);
    assert_eq!(
        base.time, BASE_TIME_BUDGET,
        "15 < 16 → 仍基础（阈值反转在此红）"
    );
    assert_eq!(base.bytes, BASE_BYTES_PER_FRAME, "15 < 16 → 仍基础字节档");

    let flood = frame_budget(16, false, 1);
    assert_eq!(
        flood.time, FLOOD_TIME_BUDGET,
        "16 ≥ 16 → 洪水（flood==base 在此红）"
    );
    assert_eq!(flood.bytes, FLOOD_BYTES_PER_FRAME, "16 ≥ 16 → 洪水字节档");

    let flood = frame_budget(0, true, 1);
    assert_eq!(
        flood.time, FLOOD_TIME_BUDGET,
        "零积压但有切分尾 → 洪水（tail 信号独立生效）"
    );
    assert_eq!(
        flood.bytes, FLOOD_BYTES_PER_FRAME,
        "零积压但有切分尾 → 洪水字节档"
    );

    // T15c（PLAN_v11217 §3.10）：字节档 ×pane_count、饱和于 4；时间档不动
    // （R3 修订：仅放宽字节维）。
    let duo = frame_budget(0, false, 2);
    assert_eq!(
        duo.time, BASE_TIME_BUDGET,
        "时间档不随 N 放大（tab 级共享红线不动）"
    );
    assert_eq!(duo.bytes, BASE_BYTES_PER_FRAME * 2, "N=2 基础字节档 ×2");
    let trio = frame_budget(16, false, 3);
    assert_eq!(trio.bytes, FLOOD_BYTES_PER_FRAME * 3, "N=3 洪水字节档 ×3");
    let eight = frame_budget(16, false, 8);
    assert_eq!(
        eight.bytes,
        FLOOD_BYTES_PER_FRAME * 4,
        "N=8 洪水字节档饱和于 ×4（单屏可读性上限 ~4 pane）"
    );
    let huge = frame_budget(0, false, 100);
    assert_eq!(huge.bytes, BASE_BYTES_PER_FRAME * 4, "基础档同样饱和于 ×4");
}

/// 层 2 — §3.2 测试验收 2（基础模式无条件断言，机器无关底线）：15×80KB
/// 积压自动选基础（15 < 16；80KB < 256KB 切分阈值不触发 oversize），
/// 生产入口单次 process_messages 后通道必有剩余——256KB 字节上限是
/// 无条件检查，任何机器速度下必延期。
#[test]
fn base_mode_byte_cap_defers_within_one_production_frame() {
    let mut tab = Tab::with_single_pane(Pane::with_terminal_only(1000));
    let only = tab.active_pane_id();
    for _ in 0..15 {
        inject(only, AppMsg::PtyOutput(vec![b'x'; 80 * KB]))(&mut tab);
    }

    let _ = tab.process_messages(); // 生产入口（真实时钟）

    let remaining = tab.pane(only).map(|p| p.msg_rx.len()).unwrap_or_default();
    assert!(
        remaining > 0,
        "base-mode 256KB byte cap must defer within a single frame on any machine"
    );
}

/// 层 3a — §3.2 测试验收 3（缝·基础·冻结时钟）：冻结时钟下时间判据
/// 永不触发，停止点完全由字节上限决定：4×80KB=320KB ≥ 256KB 停 →
/// 恰排 4 条余 11。
#[test]
fn frozen_clock_base_mode_stops_exactly_at_256k() {
    let mut tab = Tab::with_single_pane(Pane::with_terminal_only(1000));
    let only = tab.active_pane_id();
    for _ in 0..15 {
        inject(only, AppMsg::PtyOutput(vec![b'x'; 80 * KB]))(&mut tab);
    }

    let _ = tab.process_messages_with_clock(frozen_clock());

    assert_eq!(
        tab.pane(only).map(|p| p.msg_rx.len()),
        Some(11),
        "3×80KB=240KB < 256KB 不停；4×80KB=320KB ≥ 256KB 停 → 恰排 4 条，余 11"
    );
}

/// 层 3b — §3.2 测试验收 3（缝·洪水·冻结时钟）：Σ=150 ≥ 16 自动选
/// 洪水，128×8KB=1MiB 恰好触顶 → 恰排 128 条余 22（洪水延期语义钉死；
/// 若预算未生效得 150 → 红，字节上限失灵也得 150 → 红）。
#[test]
fn frozen_clock_flood_mode_stops_exactly_at_1mib() {
    let mut tab = Tab::with_single_pane(Pane::with_terminal_only(1000));
    let only = tab.active_pane_id();
    for _ in 0..150 {
        inject(only, AppMsg::PtyOutput(vec![b'x'; 8 * KB]))(&mut tab);
    }

    let _ = tab.process_messages_with_clock(frozen_clock());

    assert_eq!(
        tab.pane(only).map(|p| p.msg_rx.len()),
        Some(22),
        "128×8KB = 1MiB 洪水字节上限 → 恰排 128 条，余 22 条"
    );
}

/// 层 3c — §3.2 测试验收 3（缝·时间路径·跳变时钟）：帧首后时钟跳变
/// 1s（≫ 8ms/16ms 两档预算）；首条 80KB ≥ 32KB 粒度即武装时间检查，
/// 时间判据先于字节判据 → 恰排 1 条余 14（时间路径断裂时得 4 → 红）。
#[test]
fn jumping_clock_time_check_fires_before_the_byte_cap() {
    let mut tab = Tab::with_single_pane(Pane::with_terminal_only(1000));
    let only = tab.active_pane_id();
    for _ in 0..15 {
        inject(only, AppMsg::PtyOutput(vec![b'x'; 80 * KB]))(&mut tab);
    }
    let t0 = Instant::now();
    let calls = Cell::new(0usize);
    let clock = move || {
        calls.set(calls.get() + 1);
        if calls.get() == 1 {
            t0
        } else {
            t0 + Duration::from_secs(1)
        }
    };

    let _ = tab.process_messages_with_clock(clock);

    assert_eq!(
        tab.pane(only).map(|p| p.msg_rx.len()),
        Some(14),
        "1s ≫ 任一档预算：武装后的时间判据先于字节判据 → 恰排 1 条，余 14"
    );
}

/// 层 4 — §3.2 测试验收 4（多窗格 Σ 接线端到端钉）+ T15c ×N 扩容钉：
/// 背景窗格灌 300×8KB、active 空 + 冻结时钟 → 洪水 1MiB×2（N=2）= 2MiB →
/// 恰排 256 条余 44。三重判别：Σ 误只算 active（FIX_background_pane_pump
/// 修掉的历史 bug 形态）→ 基础 256KB×2 → 排 64 条余 236 → 红；×N 扩容
/// 缺失（恒 1MiB）→ 排 128 条余 172 → 红。
#[test]
fn background_only_backlog_sums_across_panes_into_flood_budget() {
    let (mut tab, bg, _active) = two_pane_tab();
    for _ in 0..300 {
        inject(bg, AppMsg::PtyOutput(vec![b'x'; 8 * KB]))(&mut tab);
    }

    let _ = tab.process_messages_with_clock(frozen_clock());

    assert_eq!(
        tab.pane(bg).map(|p| p.msg_rx.len()),
        Some(44),
        "Σ 所有窗格 = 300 → 洪水 1MiB×2 → 恰排 256 条余 44（active-only 得 236、无 ×N 得 172）"
    );
}
