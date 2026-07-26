#!/usr/bin/env bash
# v1.4.0 baseline: reproducible frame-trace probe + summarizer.
#
# Runs the release Weft.app with the GUI perf probe enabled and frame_trace
# logs at debug level, then pipes the captured `frame=...` lines through a
# Python summarizer that emits p50/p95/avg for every counter.
#
# Usage:
#   scripts/v14-perf-probe.sh [sample_seconds] [output_md]
#
#   sample_seconds  Sample window in seconds (default 30). Warm-up is fixed
#                   at 5s via WEFT_GUI_PROBE_WARMUP_SECS, so total runtime is
#                   sample_seconds + 5. Must be >= 30 for a valid baseline.
#   output_md       Path to write the Markdown summary (default stdout).
#
# Environment:
#   WEFT_GUI_PERF_PROBE=1 is set automatically.
#   RUST_LOG=weft_app::frame_trace=debug is set automatically.
#   WEFT_GUI_PROBE_WARMUP_SECS=5  (covers first redraws without eating the sample)
#   WEFT_GUI_PROBE_SAMPLE_SECS=$sample_seconds
#
# Exit codes:
#   0  probe ran and the summarizer produced a report
#   1  build failed, app exited non-zero, or no frame lines were captured
#
# Reproducibility: the script captures commit hash, OS, scale, and the exact
# env vars into the report so a later run can be compared apples-to-apples.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

SAMPLE_SECS="${1:-30}"
OUTPUT_MD="${2:-}"
WARMUP_SECS=5

if [[ "$SAMPLE_SECS" -lt 30 ]]; then
    echo "v14-perf-probe: sample_seconds must be >= 30 (got $SAMPLE_SECS)" >&2
    exit 1
fi

LOG_FILE="/tmp/weft.log"
PROBE_STDERR="${TMPDIR:-/tmp}/weft-v14-perf.$$.stderr"
trap 'rm -f "$PROBE_STDERR"' EXIT

echo "==> v1.4 perf probe: building release binary (skip with WEFT_V14_NO_BUILD=1)"
if [[ -z "${WEFT_V14_NO_BUILD:-}" ]]; then
    cargo build --release -p weft_app 2>&1 | tail -5
fi

# Prefer the .app bundle if present (matches the shipped layout); fall back
# to the raw cargo release binary (matches scripts/performance_gate.sh).
APP_PATH=""
if [[ -x "target/release/osx/Weft.app/Contents/MacOS/weft" ]]; then
    APP_PATH="target/release/osx/Weft.app/Contents/MacOS/weft"
elif [[ -x "target/release/weft" ]]; then
    APP_PATH="target/release/weft"
else
    echo "v14-perf-probe: release binary not found (run cargo build --release -p weft_app)" >&2
    exit 1
fi

COMMIT_HASH="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
OS_VERSION="$(sw_vers -productVersion 2>/dev/null || echo unknown)"
SCALE_HINT="$(system_profiler SPDisplaysDataType 2>/dev/null | grep -i 'retina' | head -1 || echo 'unknown')"

# Clear the previous run's log so the probe captures only this session. The
# app truncates /tmp/weft.log on startup, but clearing here lets us detect a
# failed launch (empty log = app never started).
rm -f "$LOG_FILE"

echo "==> v1.4 perf probe: starting (warmup ${WARMUP_SECS}s + sample ${SAMPLE_SECS}s)"
echo "    commit=$COMMIT_HASH  macos=$OS_VERSION  binary=$APP_PATH  log=$LOG_FILE"

# Run the app with the perf probe and frame trace logging. The app writes
# tracing output to /tmp/weft.log (see install_runtime_diagnostics). The
# WEFT_GUI_PERF_PROBE status line goes to stdout/stderr. The app exits after
# warmup + sample (see performance_probe.rs).
WEFT_GUI_PERF_PROBE=1 \
WEFT_GUI_PROBE_WARMUP_SECS="$WARMUP_SECS" \
WEFT_GUI_PROBE_SAMPLE_SECS="$SAMPLE_SECS" \
RUST_LOG=weft::frame_trace=debug \
"$APP_PATH" 2>"$PROBE_STDERR" || true

cat "$PROBE_STDERR"

# Check that frame lines were captured.
FRAME_COUNT=$(grep -c ' frame$' "$LOG_FILE" 2>/dev/null || true)
if [[ "$FRAME_COUNT" -eq 0 ]]; then
    echo "v14-perf-probe: no 'frame' trace lines captured in $LOG_FILE" >&2
    echo "    (ensure RUST_LOG=weft_app::frame_trace=debug and WEFT_GUI_PERF_PROBE=1)" >&2
    exit 1
fi

echo "==> v1.4 perf probe: captured $FRAME_COUNT frame lines; summarizing"

SUMMARY="$(
    python3 scripts/v14-frame-summary.py \
        --log "$LOG_FILE" \
        --commit "$COMMIT_HASH" \
        --os "$OS_VERSION" \
        --sample-secs "$SAMPLE_SECS" \
        --warmup-secs "$WARMUP_SECS" \
        --scale-hint "$SCALE_HINT"
)"

if [[ -n "$OUTPUT_MD" ]]; then
    mkdir -p "$(dirname "$OUTPUT_MD")"
    printf '%s\n' "$SUMMARY" >"$OUTPUT_MD"
    echo "==> v1.4 perf probe: report written to $OUTPUT_MD"
else
    printf '%s\n' "$SUMMARY"
fi

echo "==> v1.4 perf probe: done"
