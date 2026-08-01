#!/usr/bin/env bash
# Stable, isolated wall-clock budgets for CPU-side terminal and UI logic.
# GPU frame timing and idle wake frequency remain in the macOS GUI/Metal gate.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

echo "==> Performance gate: VT parsing and 10k-scrollback resize"
cargo test -p weft_core --lib perf_ -- \
    --ignored --nocapture --test-threads=1

# v1.10 规模基准（V110_IMPLEMENTATION_PLAN.md §3.4 + §6）。
# 这些是 #[ignore] 计时测试，采集基线供 docs/perf/v1.10/ 报告对比。
echo "==> Performance gate: v1.10 scale benchmarks (100k/1M lines, smart select, resize)"
cargo test -p weft_core --test bench_v110_scale -- \
    --ignored --nocapture --test-threads=1

echo "==> Performance gate: 10k history filtering and virtualization"
cargo test -p weft_app perf_panel_10k_history_filter_and_virtualize -- \
    --ignored --nocapture --test-threads=1

echo "==> Performance gate: offscreen Metal frame completion"
cargo test -p weft_app perf_offscreen_metal_frame_budget -- \
    --ignored --nocapture --test-threads=1

echo "==> Performance gate: idle GUI wake and redraw frequency"
PROBE_TMP="$(mktemp -d "${TMPDIR:-/tmp}/weft-gui-perf.XXXXXX")"
trap 'rm -rf "$PROBE_TMP"' EXIT
GUI_OUTPUT="$(
    XDG_CACHE_HOME="$PROBE_TMP/cache" \
    XDG_CONFIG_HOME="$PROBE_TMP/config" \
    WEFT_GUI_PERF_PROBE=1 \
    RUST_LOG=warn \
    cargo run --release -p weft_app 2>&1
)"
echo "$GUI_OUTPUT"
if ! grep -q "WEFT_GUI_PERF status=PASS" <<<"$GUI_OUTPUT"; then
    echo "GUI performance probe did not report PASS" >&2
    exit 1
fi

echo
echo "Performance gate: ALL BUDGETS PASSED"
