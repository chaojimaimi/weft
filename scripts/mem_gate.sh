#!/bin/zsh
# mem_gate.sh — 发布内存门禁（docs/perf/OPTIMIZATION_PLAN_2026-08-18.md P4）
#
# 口径：Physical footprint（≈ Activity Monitor "内存"列）。ps-RSS 会随换出
# 波动，不作为门禁口径（口径决议见 OPTIMIZATION_PLAN §P4 与
# docs/perf/warp-comparison/2026-08-18-baseline.md §2.4）。
#
# 做三件事：
#   1. 启动一个全新实例（带 WEFT_ALLOC_PROBE=1，阈值 32MiB——大于任何系统
#      字体文件的 slurp 量级、小于 4MiB 级 glyph atlas，专抓字体整文件读入回归）
#   2. 等沉降（恢复 tab + 首帧 + 预热；默认 25s，MEM_GATE_SETTLE 可调）
#   3. 断言：footprint ≤ 阈值（默认 200MB，MEM_GATE_MAX_FOOTPRINT_MB 可调）
#      且采样窗内 LARGE_ALLOC(≥32MiB) = 0 条（font-kit slurp 回归锁）
#
# 用法：scripts/mem_gate.sh [weft 二进制路径]   # 默认 ./target/release/weft
# 退出码：0 = PASS，1 = FAIL。窗口会短暂出现（门禁在打包时跑，属预期）。
#
# 依赖本机状态：blocks.db 的恢复历史（不同机器数值会漂移，阈值留了裕量；
# 若要跨机器复现，先用 scripts/build-app.sh 产的 .app 或同版本二进制）。

set -u
BIN="${1:-./target/release/weft}"
MAX_MB="${MEM_GATE_MAX_FOOTPRINT_MB:-200}"
SETTLE="${MEM_GATE_SETTLE:-25}"
LOG=~/Library/Logs/Weft/weft.log
MARKER=~/.cache/weft/recovery/.clean_shutdown

[[ -x "$BIN" ]] || { echo "mem_gate: binary not found: $BIN"; exit 1; }

MARK=$(wc -l < "$LOG")
WEFT_ALLOC_PROBE=1 WEFT_ALLOC_PROBE_MIN=33554432 nohup "$BIN" >/dev/null 2>&1 &
PID=$!
cleanup() {
  kill "$PID" >/dev/null 2>&1
  sleep 1
  mkdir -p ~/.cache/weft/recovery && touch "$MARKER"   # 门禁 kill 不算 unclean
}
trap cleanup EXIT INT TERM

echo "mem_gate: pid=$PID settle=${SETTLE}s max_footprint=${MAX_MB}MB"
sleep "$SETTLE"
kill -0 "$PID" 2>/dev/null || { echo "mem_gate: FAIL (instance exited during settle)"; exit 1; }

# 1) footprint（Activity Monitor 口径）
FP_MB=$(footprint "$PID" 2>/dev/null | awk -F'Footprint: ' '/Footprint:/{print int($2)}' | head -1)
FP_MB=${FP_MB:-0}

# 2) 大分配回归锁（≥32MiB 的 LARGE_ALLOC 条数）
SLURPS=$(tail -n +$((MARK + 1)) "$LOG" | grep -c "LARGE_ALLOC")

# 3) 摘要输出（诊断用，不参与断言）
vmmap --summary "$PID" 2>/dev/null | grep -E "^MALLOC_LARGE  |^IOSurface|^IOAccelerator \(graphics\)" | sed 's/^/mem_gate:   /'

echo "mem_gate: footprint=${FP_MB}MB (<=${MAX_MB}?) large_allocs=${SLURPS} (==0?)"
FAIL=0
(( FP_MB <= MAX_MB )) || { echo "mem_gate: FAIL footprint ${FP_MB}MB > ${MAX_MB}MB"; FAIL=1; }
(( SLURPS == 0 )) || { echo "mem_gate: FAIL ${SLURPS} allocations >=32MiB (font slurp regression?)"; FAIL=1; }

if (( FAIL == 0 )); then
  echo "mem_gate: PASS"
  exit 0
else
  echo "mem_gate: FAIL — see docs/perf/warp-comparison/2026-08-18-baseline.md §4 for tooling caveats"
  exit 1
fi
