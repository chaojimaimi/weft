//! R1-4: Deterministic PTY byte-fixture replay tests.
//!
//! These tests drive `Terminal::process(bytes)` with inline byte literals
//! (no real PTY, no wall-clock dependency) to cover four high-value scenarios
//! that were previously only exercisable via manual GUI testing:
//!
//! 1. CJK / ANSI / Block Elements rendering
//! 2. Claude double-Ctrl+C primary-screen exit + late resume tail
//! 3. Late resume tail replacing stale row suffixes
//! 4. Primary-screen TUI resize survival
//!
//! The byte shapes mirror real TUI output (OSC 133 markers, DEC 2026 sync,
//! absolute cursor moves) and are discovered via the `WEFT_PTY_CAPTURE`
//! env-gated tee in `pty.rs`. Fixtures use inline literals (not binary
//! recordings) so they are auditable and diff-friendly.

use weft_core::grid::{CellFlags, CellWidth};
use weft_core::input::MouseProtocol;
use weft_core::vt::Terminal;

/// Assert no orphaned wide cells: every `WIDE_SPACER` must follow a `Full`
/// width cell, and every `Full` width cell must be followed by a `WIDE_SPACER`.
fn assert_no_orphaned_wide_cells(terminal: &Terminal) {
    let grid = terminal.grid();
    for row in 0..grid.num_rows {
        for col in 0..grid.num_cols {
            let cell = grid.cell(row, col);
            if cell.flags.contains(CellFlags::WIDE_SPACER) {
                assert!(col > 0, "wide spacer at left edge: {row}:{col}");
                assert_eq!(
                    grid.cell(row, col - 1).width,
                    CellWidth::Full,
                    "orphaned wide spacer at {row}:{col}"
                );
            }
            if cell.width == CellWidth::Full {
                assert!(
                    col + 1 < grid.num_cols,
                    "wide lead at right edge: {row}:{col}"
                );
                assert!(
                    grid.cell(row, col + 1)
                        .flags
                        .contains(CellFlags::WIDE_SPACER),
                    "orphaned wide lead at {row}:{col}"
                );
            }
        }
    }
}

/// Fixture 1: CJK full-width characters, ANSI 256-color sequences, and
/// Unicode box-drawing / block elements all render correctly through
/// `Terminal::process`. This mirrors the `opentui_drawing_symbols` test in
/// `vt/tests.rs` but is structured as a replay fixture.
#[test]
fn fixture_cjk_ansi_block_elements() {
    let mut t = Terminal::new(6, 40);

    // ANSI 256-color + CJK full-width text.
    t.process(b"\x1b[38;5;208m\xe6\xb5\x8b\xe8\xaf\x95\x1b[0m");
    assert_eq!(t.grid().row_text(0), "测试");
    assert_eq!(t.grid().cursor.col, 4);
    assert_eq!(t.grid().cell(0, 0).width, CellWidth::Full);
    assert_eq!(t.grid().cell(0, 2).width, CellWidth::Full);
    assert_no_orphaned_wide_cells(&t);

    // Box-drawing and block elements (half-width, no wide pairs).
    t.process(b"\x1b[2;1H\xe2\x94\x83\xe2\x96\x84\xe2\x94\x82");
    let row1 = t.grid().row_text(1);
    assert!(row1.contains('┃'), "box drawing: {row1}");
    assert!(row1.contains('▄'), "block element: {row1}");
    assert_no_orphaned_wide_cells(&t);

    // DEC 2026 synchronized output with CJK inside.
    t.process(b"\x1b[?2026h\x1b[3;1H\xe5\x85\xa8\xe8\xa7\x92\x1b[?2026l");
    assert_eq!(t.grid().row_text(2), "全角");
    assert_no_orphaned_wide_cells(&t);
}

/// Fixture 2: Claude double-Ctrl+C primary-screen exit with a late resume
/// tail. The TUI paints a primary screen, emits OSC 133;D on exit, then
/// streams "Press Ctrl-C again to exit" + "claude --resume" before the
/// shell settles. `settle_primary_screen_exit()` is called explicitly
/// (no 200ms wall-clock wait).
#[test]
fn fixture_clude_double_ctrl_c_exit() {
    let mut terminal = Terminal::new(6, 64);
    terminal.process(b"\x1b]7;file://localhost/Users/me/project\x07");
    terminal.process(b"\x1b]133;A\x07claude\x1b]133;B\x07\x1b]133;C\x07");
    terminal.process(b"\x1b[H\x1b[2;1H");
    assert!(terminal.primary_screen_app_active());

    // TUI paints a colored answer, then exits (OSC 133;D).
    terminal.process(b"\x1b[2J\x1b[H\x1b[38;2;222;120;80manswer\x1b[0m");
    terminal.process(b"\x1b]133;D;0\x07\x1b]133;A\x07");
    assert!(terminal.primary_screen_exit_pending());
    assert!(terminal.block_tracker().blocks().is_empty());

    // Late resume tail arrives before settle.
    terminal.process(
        b"\x1b[3;1HPress Ctrl-C again to exit\x1b[4;1HResume this session with:\x1b[5;1Hclaude --resume late-id",
    );
    terminal.settle_primary_screen_exit();

    let block = terminal.block_tracker().blocks().last().unwrap();
    assert_eq!(block.cwd.as_deref(), Some("/Users/me/project"));
    assert!(block.output.contains("answer"));
    assert!(block.output.contains("Press Ctrl-C again to exit"));
    assert!(block.output.contains("claude --resume late-id"));

    // Styled output preserves the RGB color from the TUI paint.
    let styled = block
        .styled_output
        .as_ref()
        .expect("screen snapshot styles");
    assert!(styled
        .line(0)
        .and_then(|line| line.foreground_at(0))
        .is_some_and(|color| matches!(color, weft_core::grid::CellColor::Rgb(_))));
}

/// Fixture 3: Late resume tail replaces stale row suffixes. The TUI exits,
/// a stale suffix lingers on the resume line, then the real resume command
/// overwrites it. `settle_primary_screen_exit()` confirms the stale suffix
/// is gone and the clean resume command is captured.
#[test]
fn fixture_late_resume_tail_replaces_stale_suffix() {
    let mut terminal = Terminal::new(7, 72);
    terminal.process(b"\x1b]133;A\x07claude\x1b]133;B\x07\x1b]133;C\x07");
    terminal.process(b"\x1b[H\x1b[2;1H");
    assert!(terminal.primary_screen_app_active());

    // Stale suffix on the resume line.
    terminal.process(b"\x1b[2J\x1b[Hanswer\x1b[4;1HResume this session with: stale answer suffix");
    terminal.process(b"\x1b]133;D;0\x07\x1b]133;A\x07");
    assert!(terminal.primary_screen_exit_pending());

    // Clean resume command overwrites the stale suffix.
    terminal.process(b"\x1b[4;1HResume this session with:\x1b[5;1Hclaude --resume clean-id");
    assert_eq!(terminal.grid().row_text(3), "Resume this session with:");
    terminal.settle_primary_screen_exit();

    let block = terminal.block_tracker().blocks().last().unwrap();
    assert!(
        !block.output.contains("stale answer suffix"),
        "stale suffix survived: {:?}",
        block.output
    );
    assert!(block.output.contains("claude --resume clean-id"));
}

/// Fixture 4: Primary-screen TUI resize survival. A primary-screen app is
/// active, then the terminal is resized. The ownership mask and primary
/// screen state must survive the resize so subsequent output is rendered
/// correctly (not treated as shell output).
#[test]
fn fixture_primary_screen_tui_resize_survival() {
    let mut terminal = Terminal::new(6, 40);
    terminal.process(b"\x1b]133;A\x07claude\x1b]133;B\x07\x1b]133;C\x07");
    terminal.process(b"\x1b[H\x1b[2;1H");
    assert!(terminal.primary_screen_app_active());

    // Ownership mask exists before resize.
    let ownership_before = terminal
        .primary_screen_viewport_ownership()
        .map(|slice| slice.to_vec());
    assert!(
        ownership_before.is_some(),
        "ownership mask should exist before resize"
    );

    // Resize: the terminal must sync the ownership mask to the new dimensions.
    terminal.resize(10, 60);
    assert!(
        terminal.primary_screen_app_active(),
        "primary screen must still be active after resize"
    );

    // Ownership mask survives resize (resized to new row count).
    let ownership_after = terminal
        .primary_screen_viewport_ownership()
        .map(|slice| slice.to_vec());
    assert!(
        ownership_after.is_some(),
        "ownership mask should survive resize"
    );
    assert_eq!(
        ownership_after.as_ref().unwrap().len(),
        10,
        "ownership mask must match new row count"
    );

    // Post-resize output is still processed correctly.
    terminal.process(b"\x1b[5;1Hpost-resize line");
    assert!(
        terminal.grid().row_text(4).contains("post-resize line"),
        "post-resize output should be rendered: {:?}",
        terminal.grid().row_text(4)
    );
    assert!(terminal.primary_screen_app_active());
}

/// Fixture 5 (v1.7.4 Phase A): Verify vte 0.13.1 safely handles APC sequences
/// (ESC _ ... ST) without breaking OSC/DCS/UTF-8/print.
///
/// Kitty Graphics protocol uses `ESC _ G <payload> ESC \` (APC with 'G'
/// identifier). vte 0.13.1 routes ESC _ into the `SosPmApcString` state,
/// which uses `Ignore` action for all payload bytes — they are silently
/// dropped, NOT dispatched to any handler. This fixture confirms:
///
/// 1. APC payload does not leak onto the grid as text
/// 2. APC does not break subsequent OSC 133 / print / CSI sequences
/// 3. APC does not corrupt CJK UTF-8 handling
/// 4. BEL (0x07) does NOT terminate APC in vte 0.13.1 (only ST does)
///
/// This is the Phase A decision-gate test per V17_IMPLEMENTATION_PLAN §6.
#[test]
fn fixture_apc_kitty_graphics_safety() {
    let mut t = Terminal::new(6, 40);

    // --- Test 1: APC with Kitty-style payload, ST-terminated ---
    // ESC _ G a=T,f=24,s=1,v=1;<base64-payload> ESC \
    // The entire payload must be silently dropped — no text on grid.
    t.process(b"\x1b_G a=T,f=24,s=1,v=1;iVBORw0KGgoAAAANS\x1b\\");
    assert_eq!(
        t.grid().row_text(0),
        "",
        "APC payload must not leak onto grid"
    );
    assert_eq!(t.grid().cursor.col, 0, "cursor unchanged by APC");

    // --- Test 2: APC does not break subsequent OSC 133 ---
    t.process(b"\x1b]133;A\x07echo hello\x1b]133;B\x07");
    assert_eq!(t.grid().row_text(0), "echo hello");

    // --- Test 3: APC does not break CJK UTF-8 ---
    t.process(b"\x1b[2;1H");
    t.process(b"\x1b_G q=1\x1b\\\xe6\xb5\x8b\xe8\xaf\x95");
    assert_eq!(
        t.grid().row_text(1),
        "测试",
        "CJK after APC renders correctly"
    );
    assert_no_orphaned_wide_cells(&t);

    // --- Test 4: APC does not break CSI cursor movement ---
    t.process(b"\x1b[3;1H");
    t.process(b"\x1b_G t=d\x1b\\\x1b[5GABC");
    assert_eq!(t.grid().row_text(2), "    ABC");

    // --- Test 5: Multiple consecutive APCs are all dropped ---
    t.process(b"\x1b[4;1H");
    t.process(b"\x1b_G a=T\x1b\\\x1b_G a=T\x1b\\\x1b_G a=T\x1b\\text");
    assert_eq!(t.grid().row_text(3), "text");

    // --- Test 6: APC with BEL termination ---
    // vte 0.13.1's SosPmApcString state treats 0x07 as Ignore, NOT as a
    // terminator. So BEL does NOT end the APC — the payload continues
    // until ST (ESC \). Verify this by checking that text after BEL-but-
    // before-ST is still consumed by the APC state.
    t.process(b"\x1b[5;1H");
    t.process(b"\x1b_G payload\x07still-inside-apc\x1b\\visible");
    assert_eq!(
        t.grid().row_text(4),
        "visible",
        "BEL does not terminate APC; only ST does"
    );

    // --- Test 7: APC interspersed with DCS (ESC P) ---
    // Both must be independently handled.
    t.process(b"\x1b[6;1H");
    t.process(b"\x1b_G apc_payload\x1b\\\x1bPdcs_payload\x1b\\after");
    assert_eq!(t.grid().row_text(5), "after");
}

/// Fixture 6 (v1.7.4 Phase A): Verify APC with large payload (exceeding
/// any reasonable limit) does not cause panic or unbounded memory growth
/// in vte 0.13.1. Since vte uses `Ignore` action for APC bytes, no buffer
/// accumulates — the payload is discarded byte-by-byte.
#[test]
fn fixture_apc_large_payload_no_panic() {
    let mut t = Terminal::new(4, 40);

    // Construct a 256 KiB APC payload — far larger than any real Kitty
    // Graphics image chunk. vte should discard all bytes without panic.
    let mut bytes = Vec::with_capacity(256 * 1024 + 4);
    bytes.extend_from_slice(b"\x1b_G a=T;");
    bytes.extend(std::iter::repeat(b'X').take(256 * 1024));
    bytes.extend_from_slice(b"\x1b\\");
    t.process(&bytes);

    // Grid must be clean — no payload leaked.
    for row in 0..4 {
        assert_eq!(t.grid().row_text(row), "", "row {row} leaked APC payload");
    }

    // Subsequent output must work normally.
    t.process(b"after-apc");
    assert_eq!(t.grid().row_text(0), "after-apc");
}

// ===========================================================================
// v1.10 compatibility fixtures (V110_IMPLEMENTATION_PLAN.md §4)
//
// 这些 fixture 是 v1.10.0 批次的"先写测试"产出，覆盖 IME/TUI/SSH/DPI 中
// 可在纯字节层确定性复现的部分。需要真实 GUI/PTY/硬件的场景写入
// docs/V110_MANUAL_ACCEPTANCE.md 四态矩阵，不在本文件伪造。
// ===========================================================================

/// Fixture v1.10-IME-1: CJK compose 在 print 路径正确落格，splat 旧全角对。
///
/// 模拟系统拼音输入"你好"后光标回退重新输入 ASCII 的字节流。
/// 回归点：print 路径必须清理被覆盖的旧全角对（AGENTS.md CJK splat 纪律）。
///
/// 注意 VT 语义：光标移到行首写入 2 字节只覆盖前 2 列，后续列保持。
/// 关键不变量是：覆盖全角 lead cell 时，其 WIDE_SPACER 必须被清理（splat），
/// 不能残留孤儿 spacer；且不产生半个全角字符。
#[test]
fn fixture_v110_ime_cjk_overwrite_splat() {
    let mut t = Terminal::new(2, 20);

    // 输入 "你好"（两个全角字符，占 4 列）
    t.process("你好".as_bytes());
    assert_eq!(t.grid().row_text(0), "你好");
    assert_no_orphaned_wide_cells(&t);

    // 光标回到行首，用 ASCII 覆盖第 1 个全角字符（占其 lead + spacer 两列）
    t.process(b"\x1b[1;1HAB");
    // VT 语义：写入 AB 覆盖 col 0/1（原"你"的 lead + spacer），col 2/3 仍是"好"
    let row = t.grid().row_text(0);
    assert!(row.starts_with("AB"), "row: {row}");
    assert_no_orphaned_wide_cells(&t);
    // 关键回归点：col 0 不再是 Full width lead，col 1 不再是 WIDE_SPACER
    assert_ne!(
        t.grid().cell(0, 0).width,
        CellWidth::Full,
        "lead splat must clear"
    );
    assert!(
        !t.grid().cell(0, 1).flags.contains(CellFlags::WIDE_SPACER),
        "wide spacer must be cleared when lead is overwritten"
    );

    // 进一步：覆盖到第 2 个全角对，整个全角对应清零
    t.process(b"\x1b[1;3HCD");
    let row2 = t.grid().row_text(0);
    assert!(row2.starts_with("ABCD"), "row: {row2}");
    assert_no_orphaned_wide_cells(&t);
}

/// Fixture v1.10-IME-2: CJK 与 emoji 混排，组合字符不破坏全角对。
///
/// 回归点：emoji modifier / ZWJ 序列在 print 路径不污染相邻 CJK 全角对。
#[test]
fn fixture_v110_ime_cjk_emoji_mixed() {
    let mut t = Terminal::new(2, 30);

    // "测试" + emoji 表情 + "结束"
    t.process("测试😀结束".as_bytes());
    let row = t.grid().row_text(0);
    assert!(row.contains("测试"), "row: {row}");
    assert!(row.contains("😀"), "emoji missing: {row}");
    assert!(row.contains("结束"), "row: {row}");
    assert_no_orphaned_wide_cells(&t);
}

/// Fixture v1.10-TUI-1: alt screen 进入/退出 + 鼠标协议开关字节序列。
///
/// 回归点：DECSET 1049 (alt screen) 与 1000/1006 (鼠标) 必须互不干扰，
/// 退出 alt screen 后主屏内容恢复、鼠标协议关闭。
#[test]
fn fixture_v110_tui_alt_screen_and_mouse_protocol() {
    let mut t = Terminal::new(6, 40);

    // 主屏写入内容
    t.process(b"main screen content");
    assert!(!t.is_alt_screen_active());

    // 进入 TUI：alt screen + 鼠标 SGR 模式
    t.process(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h");
    assert!(t.is_alt_screen_active(), "alt screen should be active");
    assert_ne!(
        t.mouse_protocol(),
        MouseProtocol::Off,
        "mouse reporting should be on"
    );
    assert!(t.sgr_mouse(), "SGR mouse mode should be on");

    // TUI 内容写入 alt screen，不污染主屏 scrollback
    t.process(b"\x1b[Htui overlay");
    assert_eq!(t.grid().row_text(0), "tui overlay");

    // 退出 TUI：先关鼠标，再退 alt screen（vim/tmux 实际顺序）
    t.process(b"\x1b[?1006l\x1b[?1000l\x1b[?1049l");
    assert!(!t.is_alt_screen_active(), "alt screen must be exited");
    assert_eq!(
        t.mouse_protocol(),
        MouseProtocol::Off,
        "mouse reporting must be off"
    );
    assert!(!t.sgr_mouse(), "SGR mouse must be off");

    // 主屏内容恢复（row 0 仍为原内容；alt screen 写入不污染）
    assert!(
        t.grid().row_text(0).contains("main screen content"),
        "main screen must restore after alt screen exit: {:?}",
        t.grid().row_text(0)
    );
}

/// Fixture v1.10-TUI-2: alt screen 内 resize 不 reflow，只改尺寸。
///
/// 回归点：AGENTS.md 约定 alt 屏幕 resize 只改尺寸不 reflow，让 TUI 自己重绘。
#[test]
fn fixture_v110_tui_alt_screen_resize_no_reflow() {
    let mut t = Terminal::new(4, 20);

    // 进入 alt screen 并写入一行接近行宽的内容
    t.process(b"\x1b[?1049h");
    t.process(b"\x1b[1;1H0123456789012345"); // 16 chars，行宽 20
    assert!(t.is_alt_screen_active());

    // resize 缩窄到 12 列
    t.resize(4, 12);
    assert_eq!(t.grid().num_cols, 12);
    assert!(t.is_alt_screen_active());

    // alt screen 不 reflow：内容应被裁剪而非换行重排。
    // 关键是不 panic、不产生孤儿全角对、尺寸正确。
    assert_no_orphaned_wide_cells(&t);

    // resize 后 TUI 可继续写入
    t.process(b"\x1b[2;1Hafter");
    assert!(t.grid().row_text(1).contains("after"));
}

/// Fixture v1.10-SSH-1: SIGWINCH 等价的 resize 字节路径在 CJK 行尾不碎裂。
///
/// 回归点：CJK 全角字符恰在 resize 边界时不能产生半个字符（display column vs char）。
#[test]
fn fixture_v110_ssh_resize_cjk_boundary() {
    let mut t = Terminal::new(2, 10);

    // 写入 4 个全角字符（占 8 列，行宽 10）
    t.process("你好世界".as_bytes());
    assert_eq!(t.grid().row_text(0), "你好世界");
    assert_no_orphaned_wide_cells(&t);

    // resize 到奇数列宽 9 —— 全角对不能被切成两半
    t.resize(2, 9);
    assert_no_orphaned_wide_cells(&t);
    assert_eq!(t.grid().num_cols, 9);

    // resize 到 8（恰好容纳 4 全角）
    t.resize(2, 8);
    assert_no_orphaned_wide_cells(&t);
}

/// Fixture v1.10-RECOVERY-1: 记录 alt screen 异常退出时鼠标协议的当前行为。
///
/// 背景：vim/tmux 崩溃或 SSH 断线时可能未发 DEC mouse-mode reset
/// （`\x1b[?1006l\x1b[?1000l`）。本 fixture 采集当前实现的行为基线：
/// DECSET 1049l（退 alt screen）**不**隐含清理鼠标协议——这与 xterm/
/// Alacritty 一致（TUI 负责清理自己的鼠标模式）。
///
/// 正常 TUI 退出路径（发完整 reset 序列）见 fixture_v110_tui_alt_screen_and_mouse_protocol。
/// 异常退出时鼠标协议泄漏到主屏 shell 的风险，记录在
/// docs/V110_MANUAL_ACCEPTANCE.md §3 SSH 矩阵为待评估项（P2），
/// 因为修复需评估对 vim shell-mode 等场景的影响，不在 v1.10.0 强改。
#[test]
fn fixture_v110_recovery_mouse_leak_after_abrupt_alt_exit() {
    let mut t = Terminal::new(4, 40);

    // 进入 TUI：alt screen + SGR 鼠标
    t.process(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h");
    assert!(t.is_alt_screen_active());
    assert_ne!(t.mouse_protocol(), MouseProtocol::Off);
    assert!(t.sgr_mouse());

    // 异常退出：只退 alt screen，不发鼠标 reset
    t.process(b"\x1b[?1049l");
    assert!(!t.is_alt_screen_active());

    // 当前行为基线（与 xterm/Alacritty 一致）：鼠标协议不因 alt 退出而复位。
    // 这是已知行为，非 bug——TUI 应用负责发自己的 reset 序列。
    // 若未来 Weft 决定在 alt 退出时强制清理（参考 settle_primary_screen_exit
    // 在 screen_exit.rs:527 的做法），更新此断言为 Off。
    assert_ne!(
        t.mouse_protocol(),
        MouseProtocol::Off,
        "current behavior: mouse protocol survives alt-screen exit (xterm-compatible)"
    );

    // 但 alt_active 标志必须复位（确保后续渲染走主屏路径）
    assert!(!t.is_alt_screen_active());

    // 后续主屏输出正常
    t.process(b"safe shell input");
    assert!(t.grid().row_text(0).contains("safe shell"));

    // 显式发 reset 后状态清理（TUI 正常责任的等价）
    t.process(b"\x1b[?1006l\x1b[?1000l");
    assert_eq!(t.mouse_protocol(), MouseProtocol::Off);
    assert!(!t.sgr_mouse());
}
