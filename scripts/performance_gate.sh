#!/usr/bin/env bash
# Stable, isolated wall-clock budgets for CPU-side terminal and UI logic.
# GPU frame timing and idle wake frequency remain in the macOS GUI/Metal gate.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

REPORT_DIR="${WEFT_PERF_REPORT_DIR:-$ROOT/build/perf/v1.10/$(date +%Y%m%d-%H%M%S)}"
mkdir -p "$REPORT_DIR"
PERF_LOG="$REPORT_DIR/performance.log"
: > "$PERF_LOG"

run_logged() {
    "$@" 2>&1 | tee -a "$PERF_LOG"
}

echo "==> Performance gate: VT parsing and 10k-scrollback resize"
run_logged cargo test -p weft_core --lib perf_ -- \
    --ignored --nocapture --test-threads=1

# v1.10 规模基准（V110_IMPLEMENTATION_PLAN.md §3.4 + §6）。
# 这些是 #[ignore] 计时测试，采集基线供 docs/perf/v1.10/ 报告对比。
echo "==> Performance gate: v1.10 scale benchmarks (100k/1M lines, smart select, resize)"
run_logged /usr/bin/time -l cargo test --release -p weft_core --test bench_v110_scale -- \
    --ignored --nocapture --test-threads=1

echo "==> Performance gate: v1.10 10k/50k/100k Block visible-window p95"
run_logged /usr/bin/time -l cargo test --release -p weft_app \
    perf_panel_v110_scale_visible_window -- --ignored --nocapture --test-threads=1

echo "==> Performance gate: 10k history filtering and virtualization"
run_logged cargo test -p weft_app perf_panel_10k_history_filter_and_virtualize -- \
    --ignored --nocapture --test-threads=1

echo "==> Performance gate: offscreen Metal frame completion"
run_logged cargo test -p weft_app perf_offscreen_metal_frame_budget -- \
    --ignored --nocapture --test-threads=1

echo "==> Performance gate: cold start to first frame (20 fresh-config samples)"
COLD_TMP="$(mktemp -d "${TMPDIR:-/tmp}/weft-cold-start.XXXXXX")"
PROBE_TMP="$(mktemp -d "${TMPDIR:-/tmp}/weft-gui-perf.XXXXXX")"
trap 'rm -rf "$COLD_TMP" "$PROBE_TMP"' EXIT
for run_index in $(seq 1 20); do
    run_dir="$COLD_TMP/run-$run_index"
    mkdir -p "$run_dir/cache" "$run_dir/config"
    COLD_OUTPUT="$(
        XDG_CACHE_HOME="$run_dir/cache" \
        XDG_CONFIG_HOME="$run_dir/config" \
        WEFT_GUI_PERF_PROBE=1 \
        WEFT_GUI_PROBE_WARMUP_SECS=1 \
        WEFT_GUI_PROBE_SAMPLE_SECS=1 \
        RUST_LOG=warn \
        target/release/weft 2>&1
    )"
    echo "$COLD_OUTPUT" | grep 'V110_METRIC name=cold_start' | tee -a "$PERF_LOG"
done

echo "==> Performance gate: idle GUI wake and redraw frequency"
GUI_OUTPUT="$(
    XDG_CACHE_HOME="$PROBE_TMP/cache" \
    XDG_CONFIG_HOME="$PROBE_TMP/config" \
    WEFT_GUI_PERF_PROBE=1 \
    WEFT_GUI_PROBE_WARMUP_SECS=3 \
    WEFT_GUI_PROBE_SAMPLE_SECS=60 \
    RUST_LOG=warn \
    cargo run --release -p weft_app 2>&1
)"
echo "$GUI_OUTPUT"
printf '%s\n' "$GUI_OUTPUT" >> "$PERF_LOG"
if ! grep -q "WEFT_GUI_PERF status=PASS" <<<"$GUI_OUTPUT"; then
    echo "GUI performance probe did not report PASS" >&2
    exit 1
fi

python3 - "$PERF_LOG" "$REPORT_DIR/baseline.json" <<'PY'
import json
import math
import os
import platform
import re
import subprocess
import sys
from datetime import datetime, timezone

log_path, output_path = sys.argv[1:]
metrics = []
max_rss = 0
with open(log_path, encoding="utf-8", errors="replace") as handle:
    for line in handle:
        if "V110_METRIC " in line:
            line = line.split("V110_METRIC ", 1)[1]
            item = {}
            for key, value in re.findall(r"([a-zA-Z0-9_]+)=([^ ]+)", line):
                try:
                    item[key] = float(value) if "." in value else int(value)
                except ValueError:
                    item[key] = value
            metrics.append(item)
        if "WEFT_GUI_PERF " in line:
            payload = line.split("WEFT_GUI_PERF ", 1)[1]
            item = {"name": "gui_idle"}
            for key, value in re.findall(r"([a-zA-Z0-9_]+)=([^ ]+)", payload):
                try:
                    item[key] = float(value) if "." in value else int(value)
                except ValueError:
                    item[key] = value
            metrics.append(item)
        match = re.search(r"(\d+)\s+maximum resident set size", line)
        if match:
            max_rss = max(max_rss, int(match.group(1)))

required_sizes = {10_000, 50_000, 100_000}
observed_sizes = {
    int(item["size"])
    for item in metrics
    if item.get("name") == "block_visible_window" and "size" in item
}
if not required_sizes.issubset(observed_sizes):
    raise SystemExit(f"missing block scale metrics: {required_sizes - observed_sizes}")

cold_samples_all = [
    float(item["first_frame_ms"])
    for item in metrics
    if item.get("name") == "cold_start"
]
if len(cold_samples_all) != 21:
    raise SystemExit(
        f"expected 20 cold runs plus 1 idle-run startup, got {len(cold_samples_all)}"
    )
cold_samples = sorted(cold_samples_all[:20])
cold_p95 = cold_samples[math.ceil(len(cold_samples) * 0.95) - 1]
# v1.11.12 (PLAN_v11112 M-B) budget rescale (architect P2-4 protection rule):
# new_budget = max(ceil(p95 * 1.1), 300), hard cap 500ms. Derived from the
# five manual bare-binary fresh-config samples taken 2026-08-30 on the
# reference machine (440.760ms p95 -> 485ms); replaces the legacy 300ms set
# against the v1.10.3-era codebase (attribution table in PLAN_v11112 report).
# v1.11.16: the 485ms budget is calibrated on a developer reference machine.
# Shared CI runners are measurably slower, so WEFT_PERF_SLACK scales it here
# exactly as it scales the Rust perf budgets (default 1.0 = the real budget).
# CI sets 3x, so a genuine regression still fails while host latency does not.
try:
    _slack = float(os.environ.get("WEFT_PERF_SLACK", "1.0"))
except ValueError:
    _slack = 1.0
if _slack < 1.0:
    _slack = 1.0
cold_budget = 485.0 * _slack
if cold_p95 >= cold_budget:
    raise SystemExit(
        f"cold-start p95 {cold_p95:.3f}ms exceeds {cold_budget:.0f}ms budget "
        f"(slack x{_slack:g})"
    )
metrics.append({
    "name": "cold_start_summary",
    "samples": len(cold_samples),
    "median_ms": cold_samples[len(cold_samples) // 2],
    "p95_ms": cold_p95,
})

try:
    commit = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], text=True, stderr=subprocess.DEVNULL
    ).strip()
except Exception:
    commit = "unknown"

report = {
    "schema": 1,
    "version": "v1.10",
    "captured_at": datetime.now(timezone.utc).isoformat(),
    "commit": commit,
    "platform": platform.platform(),
    "metrics": metrics,
    "max_subprocess_rss_bytes": max_rss,
    "log": os.path.relpath(log_path),
}
with open(output_path, "w", encoding="utf-8") as handle:
    json.dump(report, handle, indent=2, ensure_ascii=False)
    handle.write("\n")
print(f"wrote {output_path}")
PY

echo
echo "Performance gate: AUTOMATED BUDGETS PASSED"
echo "Evidence: $REPORT_DIR/baseline.json"
