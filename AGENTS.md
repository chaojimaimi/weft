# Weft 项目专属 Agent 指令

> 适用于 ZCode Harness（GLM-5.2）| 位置：项目根目录，覆盖全局 `~/.zcode/AGENTS.md`
> 本文件在 SessionStart 时自动注入上下文，与全局指令叠加生效。

## 项目概况

Weft 是原生 macOS 终端模拟器（Rust + winit 0.30 + Metal + objc2-app-kit + nix PTY）。
**非** Electron/xterm.js。所有 UI 由 Metal GPU 顶点直接绘制，无 CSS/DOM。

## 强制纪律（不可绕过，除非显式说明）

### 1. 调试纪律：先证伪假设，再改代码

bug 修复**禁止凭猜直接改代码**。必须遵循 `superpowers:systematic-debugging` 方法论：

1. **收集证据**：加诊断日志（`tracing::info!`）或断言，确认问题出在数据层还是渲染层
2. **形成多个假设**：列出 2-3 个可能根因
3. **逐个证伪**：用日志/测试数据排除不可能的假设
4. **确认根因后再动手改**

教训记录（2026-07-09 vim CJK 滚动乱码）：连续 4 轮凭猜修改全部失败（猜渲染 blit→猜 Ime redraw→猜 mouse_protocol→猜 scroll-blit），最终靠 `DL -> grid content snapshot` 日志证明 grid 内容本身就错了（CJK splat bug），一次修对。**先看证据，再改代码。**

### 2. 代码审查：Rust 改动必须经过 rust-reviewer

任何 `.rs` 文件修改后、`git commit` 前，**必须**调用 `rust-reviewer` agent 审查。

- `commit-gate.sh` hook 会检查 `.zcode/review-passed` 标记文件
- 审查通过后 marker 自动消费（一个 commit 一个审查）
- 文档/注释类提交可 `touch .zcode/review-passed` 跳过

### 3. 测试要求：纯逻辑函数必须先写测试

- 纯逻辑函数（如 `resize_dims`、`action_from_isize`、`encode_scroll`）**必须**有单元测试
- GUI 功能（标题栏、菜单栏）至少补集成测试桩或手动验证清单
- 新增/修改的 VT 行为（print/scroll/CSI）必须有回归测试
- 测试命令：`cargo test --workspace`

### 4. 文件规模：模块化 + 行数治理

- `main.rs` 已完成拆分（2026-08 重构 6600+ → 490 行；v1.12.25 3-B-2 四个 session I/O 方法外移到 `app/session_pump.rs` 后实际 530 行），只保留模块声明、App struct/new/tab 与启动编排，**禁止重新膨胀**（gate 专项阈值 MAIN_RS_MAX=530）
- 新功能必须拆到独立模块（参考 `app/`、`paint/`、`glyph/`、`layout/`、`tab/`、`block_view/` 子模块）
- `commit-gate.sh` 会检查暂存区 `.rs` 文件行数：超 800 行且不在 `scripts/architecture_allowlist.txt` 中直接 block；allowlist 变更需审计理由

### 5. 验证：未实测不算修复

声称"已修复"前，必须满足以下之一：
- 用户实测确认（最佳）
- 单元测试覆盖该场景
- 集成测试验证

`cargo build` + `cargo test` 通过**不等于**功能修复——只能证明编译和已有测试不回归。

## 工具使用指引

### 大文件分析用 context-mode

分析 >500 行的文件时，优先用 `ctx_execute_file`（sandbox 内运行代码分析，只返回摘要），而非 `Read` 整个文件。
当前大文件集中在 `vt/tests.rs`(3080)、`config/tests.rs`(2373)、`pane_layout.rs`(1867)、`paint/block_view.rs`(1189)、`mouse_controller.rs`(1159) 等，直接 Read 会消耗大量上下文。

### 命令绕行

当 Bash 工具被 wrapper 拦截（`git diff` 输出被渲染为摘要而非原始 patch），用 `python3 -c "import subprocess; ..."` 子进程绕行获取原始输出。

## 项目结构

```
crates/
  weft_app/     — 主应用（窗口、事件循环、渲染器、菜单）
    src/main.rs     — 模块声明 + 启动编排（530 行，禁止膨胀）
    src/app/        — App 结构体 + Action
    src/paint/      — Metal 绘制（block_view/ grid_cache/ grid_instances/ prompt/ 等）
    src/glyph/      — 字形图集（atlas/ font/ rasterize/ style/ query/）
    src/layout/     — 布局上下文（含 drag.rs 纯函数）
    src/tab/        — Tab 管理（含 scroll.rs）
    src/*controller — 事件控制器（mouse/ geometry/ redraw/ lifecycle/ …）
    src/menu.rs     — 原生 NSMenu 菜单栏（v1.1）
  weft_core/    — 核心（VT 解析、Grid、PTY、输入、配置）
    src/vt/         — Terminal + vte::Perform（perform.rs/ screen_exit/ osc.rs/ …）
    src/grid/       — Grid + Row + Cell（snapshot/ scrollback/ …）
    src/blocks/     — 命令块模型（output_capture/ continuation/ …）
    src/pty.rs      — forkpty + async read loop
    src/input/      — 键盘/鼠标编码
    src/config/     — Action enum + KeyBindings + Config（多 profile/ 主题）
    src/completion/ — 补全（排序/ 提供器）
    src/selection/  — 选择模型（block_view 行快照）
docs/
  PROGRESS.md       — 开发进度（每次改动后更新）
  ROADMAP.md        — 版本规划
  ARCHITECTURE.md   — 架构文档
  FIX_*.md          — 每个 bug 修复的根因分析与实施方案
```

## 编码规范（项目专属）

- unsafe objc2 调用：优先用类型安全的 `objc2-app-kit` 方法（如 `NSWindow::setStyleMask`），而非裸 `msg_send!`
- `msg_send!` 必须 `catch_unwind` 保护（教训：`set_dock_icon` 的 nounwind abort）
- CJK 全角字符：print 路径必须清理被覆盖的旧全角对（splat 处理）
- alt 屏幕：resize 只改尺寸不 reflow（`resize_dims`），让 TUI app 自己重绘
