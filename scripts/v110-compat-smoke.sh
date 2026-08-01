#!/usr/bin/env bash
# v110-compat-smoke.sh — Weft v1.10 兼容性证据采集入口（V110_IMPLEMENTATION_PLAN.md §4）。
#
# 职责：
#   1. 采集环境基线（版本/字体/locale/macOS/屏幕/shell）写入日志目录。
#   2. 运行可重放 VT/TUI fixture（纯字节，不依赖 PTY/真实时钟）。
#   3. 运行 workspace 自动化回归（fmt/clippy/test/architecture/perf gate）。
#   4. 列出只能人工验证的项，提示执行者填写 docs/V110_MANUAL_ACCEPTANCE.md。
#
# 不做：
#   - 不启动 GUI（GUI/IME/硬件矩阵由 V110_MANUAL_ACCEPTANCE.md 四态记录）。
#   - 不伪造 PASS：未实测项只能 WAIVED 或 BLOCKED。
#
# 用法：
#   ./scripts/v110-compat-smoke.sh [--no-build]
#   输出目录：./build/v110-compat-<timestamp>/

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

NO_BUILD=0
[[ "${1:-}" == "--no-build" ]] && NO_BUILD=1

TS="$(date +%Y%m%d-%H%M%S)"
OUT_DIR="$ROOT/build/v110-compat-$TS"
mkdir -p "$OUT_DIR"
LOG="$OUT_DIR/smoke.log"
ENV_JSON="$OUT_DIR/environment.json"
exec > >(tee -a "$LOG") 2>&1

echo "==> Weft v1.10 compatibility smoke @ $TS"
echo "==> Output: $OUT_DIR"

# ----------------------------------------------------------------------------
# 1. 环境基线采集
# ----------------------------------------------------------------------------
echo "==> [1/4] Collecting environment baseline"

# 版本：从 Cargo.toml 取 workspace package version
VERSION=$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/' || echo 'unknown')
MACOS_VER=$(sw_vers -productVersion 2>/dev/null || echo 'non-macos')
ARCH=$(uname -m)
SHELL_BIN=$(basename "${SHELL:-/bin/zsh}")
LOCALE=$(defaults read -g AppleLocale 2>/dev/null || echo "${LANG:-unknown}")
XCODE_VER=$(xcodebuild -version 2>/dev/null | head -1 || echo 'n/a')

# 字体（v1.10 关注 CJK 字体可用性）
FONT_INFO="n/a"
if [[ "$MACOS_VER" != "non-macos" ]]; then
    FONT_INFO=$(system_profiler SPFontsDataType 2>/dev/null \
        | grep -iE "PingFang|Hiragino|Noto.*CJK|Source Han" | head -3 \
        | tr '\n' '|' || echo 'probe-failed')
fi

# 屏幕（DPI 矩阵相关）
SCREEN_INFO="n/a"
if [[ "$MACOS_VER" != "non-macos" ]]; then
    SCREEN_INFO=$(system_profiler SPDisplaysDataType 2>/dev/null \
        | grep -iE 'Resolution|Retina' | head -4 | tr '\n' '|' || echo 'probe-failed')
fi

cat > "$ENV_JSON" <<EOF
{
  "weft_version": "$VERSION",
  "macos": "$MACOS_VER",
  "arch": "$ARCH",
  "shell": "$SHELL_BIN",
  "locale": "$LOCALE",
  "xcode": "$XCODE_VER",
  "cjk_fonts": "$FONT_INFO",
  "displays": "$SCREEN_INFO",
  "timestamp": "$TS",
  "git_commit": "$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
}
EOF
echo "    environment → $ENV_JSON"
cat "$ENV_JSON"

# ----------------------------------------------------------------------------
# 2. 构建（除非 --no-build）
# ----------------------------------------------------------------------------
if [[ "$NO_BUILD" -eq 0 ]]; then
    echo "==> [2/4] Building workspace (release)"
    cargo build --release --workspace 2>&1 | tail -5
else
    echo "==> [2/4] Skipping build (--no-build)"
fi

# ----------------------------------------------------------------------------
# 3. 可重放 VT/TUI fixture（纯字节，确定性，无 PTY/时钟依赖）
#    这些是 V110_PLAN §4 "先写测试/fixture" 的自动化部分。
# ----------------------------------------------------------------------------
echo "==> [3/4] Running replayable VT/TUI fixtures (weft_core)"

# v1.10 新增的兼容 fixture（见 crates/weft_core/tests/replay_fixtures.rs::fixture_v110_*）
cargo test -p weft_core --test replay_fixtures fixture_v110 -- --nocapture 2>&1 | tee "$OUT_DIR/replay-fixtures.log" | tail -20

# 既有 fixture 回归（防止修复 A 破坏 B）
echo "    -- full replay_fixtures regression --"
cargo test -p weft_core --test replay_fixtures -- --nocapture 2>&1 | tee -a "$OUT_DIR/replay-fixtures.log" | tail -10

# TUI 真 PTY 集成（若依赖已安装；install_tui_test_deps.sh）
echo "    -- tui_integration (real PTY; may skip if deps missing) --"
cargo test -p weft_core --test tui_integration -- --nocapture 2>&1 | tee "$OUT_DIR/tui-integration.log" | tail -10 || \
    echo "    [WARN] tui_integration skipped or failed — record in V110_MANUAL_ACCEPTANCE.md"

# ----------------------------------------------------------------------------
# 4. workspace 自动化门禁（与 v1.9.0 RC 同口径）
# ----------------------------------------------------------------------------
echo "==> [4/4] Running workspace automation gates"

echo "    -- cargo test --workspace --"
cargo test --workspace 2>&1 | tee "$OUT_DIR/workspace-test.log" | tail -15

echo "    -- cargo fmt --check --"
if ! cargo fmt --check 2>&1 | tee "$OUT_DIR/fmt.log"; then
    echo "    [FAIL] fmt check"
fi

echo "    -- cargo clippy (all-features, strict) --"
cargo clippy --all-features --all-targets -- \
    -D warnings -D clippy::pedantic 2>&1 | tee "$OUT_DIR/clippy.log" | tail -10 || \
    echo "    [WARN] clippy had warnings — review $OUT_DIR/clippy.log"

echo "    -- architecture gate --"
./scripts/architecture_gate.sh 2>&1 | tee "$OUT_DIR/architecture.log" | tail -5 || \
    echo "    [WARN] architecture gate failed"

# ----------------------------------------------------------------------------
# 5. 提示人工矩阵
# ----------------------------------------------------------------------------
cat <<EOF

==> Automated smoke complete. Reports in: $OUT_DIR

==> MANUAL VERIFICATION REQUIRED (V110_PLAN §4, cannot automate):
    - IME: system pinyin / WeType compose, candidate window, undo, focus switch
    - TUI: vim/neovim fullscreen scroll, tmux alt screen, fzf, less, top
    - SSH: login, remote vim/tmux, SIGWINCH, disconnect/reconnect, CJK/emoji
    - DPI: 1x/2x/cross-screen, resize, fullscreen, font scale
    - Recovery: abnormal exit, unresponsive process, db busy, corrupted record

    Fill results (PASS/FAIL/BLOCKED/WAIVED) in:
      docs/V110_MANUAL_ACCEPTANCE.md

    Reminder: WAIVED requires documented risk + trigger condition. Never fake PASS.

EOF
