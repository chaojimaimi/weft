#!/usr/bin/env bash
# Drive a real Vim alternate-screen workload and require frame-trace evidence
# that the v1.4.2 background/glyph streams were exercised.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

SAMPLE_SECS="${1:-30}"
OUTPUT_MD="${2:-docs/perf/v1.4/v1.4.3-grid-load.md}"
WARMUP_SECS=5
LOG_FILE="/tmp/weft.log"
PROBE_STDERR="${TMPDIR:-/tmp}/weft-v14-grid.$$.stderr"
trap 'rm -f "$PROBE_STDERR"' EXIT

if [[ "$SAMPLE_SECS" -lt 30 ]]; then
    echo "v14-grid-load-probe: sample_seconds must be >= 30" >&2
    exit 1
fi

if [[ -z "${WEFT_V14_NO_BUILD:-}" ]]; then
    cargo build --release -p weft_app
fi

APP_PATH="${WEFT_V14_APP_PATH:-target/release/osx/Weft.app/Contents/MacOS/weft}"
if [[ ! -x "$APP_PATH" ]]; then
    APP_PATH="target/release/weft"
fi
if [[ ! -x "$APP_PATH" ]]; then
    echo "v14-grid-load-probe: release binary not found" >&2
    exit 1
fi

COMMIT_HASH="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
OS_VERSION="$(sw_vers -productVersion 2>/dev/null || echo unknown)"
SCALE_HINT="$(system_profiler SPDisplaysDataType 2>/dev/null | grep -i 'retina' | head -1 || echo unknown)"
VIM_COMMAND="vim -Nu NONE -n -c 'set nowrap' -c 'syntax on' crates/weft_app/src/paint/grid_instances.rs"

rm -f "$LOG_FILE"
WEFT_GUI_PERF_PROBE=1 \
WEFT_GUI_PROBE_WARMUP_SECS="$WARMUP_SECS" \
WEFT_GUI_PROBE_SAMPLE_SECS="$SAMPLE_SECS" \
RUST_LOG=weft::frame_trace=debug \
"$APP_PATH" 2>"$PROBE_STDERR" &
APP_PID=$!

sleep $((WARMUP_SECS + 2))
if ! osascript \
    -e 'on run argv' \
    -e 'set appPid to (item 2 of argv) as integer' \
    -e 'tell application "System Events" to set frontmost of first process whose unix id is appPid to true' \
    -e 'delay 0.2' \
    -e 'tell application "System Events" to keystroke (item 1 of argv)' \
    -e 'delay 0.8' \
    -e 'tell application "System Events" to key code 36 using command down' \
    -e 'end run' \
    -- "$VIM_COMMAND" "$APP_PID" >/dev/null; then
    echo "v14-grid-load-probe: failed to launch Vim through Accessibility" >&2
    kill "$APP_PID" 2>/dev/null || true
    wait "$APP_PID" 2>/dev/null || true
    exit 1
fi

sleep 2
VIM_STARTED=0
for _ in 1 2 3; do
    if grep -Eq 'grid_glyph_instances=[1-9][0-9]*' "$LOG_FILE" 2>/dev/null; then
        VIM_STARTED=1
        break
    fi
    osascript \
        -e 'on run argv' \
        -e 'set appPid to (item 1 of argv) as integer' \
        -e 'tell application "System Events" to set frontmost of first process whose unix id is appPid to true' \
        -e 'tell application "System Events" to key code 36 using command down' \
        -e 'end run' \
        -- "$APP_PID" >/dev/null
    sleep 2
done
if [[ "$VIM_STARTED" -ne 1 ]]; then
    echo "v14-grid-load-probe: Vim did not enter the glyph grid path after submission retries" >&2
    kill "$APP_PID" 2>/dev/null || true
    wait "$APP_PID" 2>/dev/null || true
    exit 1
fi

SAMPLE_END=$(( $(date +%s) + SAMPLE_SECS - 10 ))
KEY_CODE=121
while [[ $(date +%s) -lt "$SAMPLE_END" ]]; do
    osascript \
        -e 'on run argv' \
        -e 'set appPid to (item 2 of argv) as integer' \
        -e 'tell application "System Events" to set frontmost of first process whose unix id is appPid to true' \
        -e 'tell application "System Events" to key code ((item 1 of argv) as integer)' \
        -e 'end run' \
        -- "$KEY_CODE" "$APP_PID" >/dev/null || break
    if [[ "$KEY_CODE" -eq 121 ]]; then KEY_CODE=116; else KEY_CODE=121; fi
    sleep 0.25
done

wait "$APP_PID" 2>/dev/null || true
cat "$PROBE_STDERR" >&2 || true

FRAME_COUNT=$(grep -c 'frame_trace: frame ' "$LOG_FILE" 2>/dev/null || true)
GRID_FRAMES=$(awk -F 'grid_bg_instances=' '
    /frame_trace: frame / { split($2, bg, " "); split($0, glyph_part, "grid_glyph_instances="); split(glyph_part[2], glyph, " "); if (bg[1] > 0 || glyph[1] > 0) count++ }
    END { print count + 0 }
' "$LOG_FILE")
BUILD_FRAMES=$(awk -F 'grid_build_us=' '
    /frame_trace: frame / { split($2, value, " "); if (value[1] > 0) count++ }
    END { print count + 0 }
' "$LOG_FILE")
GLYPH_FRAMES=$(awk -F 'grid_glyph_instances=' '
    /frame_trace: frame / { split($2, value, " "); if (value[1] > 0) count++ }
    END { print count + 0 }
' "$LOG_FILE")

if [[ "$FRAME_COUNT" -eq 0 || "$GRID_FRAMES" -lt 10 || "$GLYPH_FRAMES" -lt 10 || "$BUILD_FRAMES" -lt 5 ]]; then
    echo "v14-grid-load-probe: Vim did not exercise the grid pipeline" >&2
    echo "    frames=$FRAME_COUNT grid_frames=$GRID_FRAMES glyph_frames=$GLYPH_FRAMES build_frames=$BUILD_FRAMES" >&2
    exit 1
fi

SUMMARY="$(python3 scripts/v14-frame-summary.py \
    --log "$LOG_FILE" \
    --commit "$COMMIT_HASH" \
    --os "$OS_VERSION" \
    --sample-secs "$SAMPLE_SECS" \
    --warmup-secs "$WARMUP_SECS" \
    --scale-hint "$SCALE_HINT" \
    --title "v1.4.3 Vim Grid-Load Probe")"

mkdir -p "$(dirname "$OUTPUT_MD")"
{
    printf '%s\n' "$SUMMARY"
    printf '\n## Grid-load acceptance\n\n'
    printf -- '- Total frame traces: %s\n' "$FRAME_COUNT"
    printf -- '- Frames with grid instances: %s\n' "$GRID_FRAMES"
    printf -- '- Frames with glyph instances: %s\n' "$GLYPH_FRAMES"
    printf -- '- Frames with non-zero `grid_build_us`: %s\n' "$BUILD_FRAMES"
    printf -- '- Grid pipeline verdict: **PASS**\n'
} >"$OUTPUT_MD"

echo "v14-grid-load-probe: PASS frames=$FRAME_COUNT grid_frames=$GRID_FRAMES build_frames=$BUILD_FRAMES"
echo "v14-grid-load-probe: report written to $OUTPUT_MD"
