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
#   2. 等沉降（首帧 + 预热；默认 25s，MEM_GATE_SETTLE 可调）
#   3. 断言：footprint ≤ 阈值（默认 200MB，MEM_GATE_MAX_FOOTPRINT_MB 可调）
#      且采样窗内 LARGE_ALLOC(≥32MiB) = 0 条（font-kit slurp 回归锁）
#
# 用法：scripts/mem_gate.sh [weft 二进制路径]   # 默认 ./target/release/weft
# 退出码：0 = PASS，1 = FAIL。窗口会短暂出现（门禁在打包时跑，属预期）。
#
# 确定性口径（v1.10.33 起）：默认给被测实例注入临时 XDG_CACHE_HOME
# （weft 的 cache dir 解析优先 XDG_CACHE_HOME，见 crates/weft_app/src/
# app/helpers.rs `weft_cache_dir`），blocks.db/recovery 全落临时目录——
# 每次干净状态、无 tab 恢复，数值可跨机器/跨时间复现。
# MEM_GATE_KEEP_STATE=1 保留旧行为：用真实 ~/.cache/weft 状态（含 tab
# 恢复 + blocks.db 历史，footprint 随使用量漂移，仅作参考值，不作门禁依据）。
# 注意：脚本不编译、直接测 $BIN——须在 `cargo build --release` 之后运行。

set -u
BIN="${1:-./target/release/weft}"
MAX_MB="${MEM_GATE_MAX_FOOTPRINT_MB:-200}"
SETTLE="${MEM_GATE_SETTLE:-25}"
LOG=~/Library/Logs/Weft/weft.log
MARKER=~/.cache/weft/recovery/.clean_shutdown
KEEP_STATE="${MEM_GATE_KEEP_STATE:-0}"

[[ -x "$BIN" ]] || { echo "mem_gate: binary not found: $BIN"; exit 1; }

# 确定性口径：默认注入临时 XDG_CACHE_HOME（干净状态，无 tab 恢复/历史）。
STATE_ARGS=()
STATE_DIR=""
if [[ "$KEEP_STATE" != "1" ]]; then
  STATE_DIR=$(mktemp -d /tmp/weft-memgate.XXXXXX)
  STATE_ARGS=(XDG_CACHE_HOME="$STATE_DIR")
fi

MARK=$(wc -l < "$LOG")
# 经由 `env` 传递 XDG_CACHE_HOME：zsh/bash 都不把「展开产生的 VAR=val 词」
# 识别为环境赋值（会当命令名执行），字面参数给 env 才可靠。
WEFT_ALLOC_PROBE=1 WEFT_ALLOC_PROBE_MIN=33554432 nohup env "${STATE_ARGS[@]}" "$BIN" >/dev/null 2>&1 &
PID=$!
cleanup() {
  kill "$PID" >/dev/null 2>&1
  sleep 1
  if [[ "$KEEP_STATE" == "1" ]]; then
    mkdir -p ~/.cache/weft/recovery && touch "$MARKER"   # 门禁 kill 不算 unclean
  else
    rm -rf "$STATE_DIR"   # 临时状态目录整体丢弃
  fi
}
trap cleanup EXIT INT TERM

if [[ "$KEEP_STATE" == "1" ]]; then
  STATE_DESC="real"
else
  STATE_DESC="clean(${STATE_DIR})"
fi
echo "mem_gate: pid=$PID settle=${SETTLE}s max_footprint=${MAX_MB}MB state=${STATE_DESC}"
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
