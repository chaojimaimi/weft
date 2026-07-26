#!/usr/bin/env bash
# v1.4.1 baseline: drive Weft with synthetic Claude-like load to capture
# styled_paint_us data, then summarize. Required for the v1.4.1 GO/NO-GO
# decision (V14_IMPLEMENTATION_PLAN.md §5.1).
#
# The default v14-perf-probe.sh only captures idle/light-load frames where
# styled_paint_us=0, which can't inform the styled-line cache decision. This
# wrapper:
#   1. Launches Weft.app with the perf probe enabled
#   2. Waits for warmup to complete
#   3. Uses osascript System Events to type a command that emits ANSI-colored
#      multi-line output (mimicking a Claude transcript) repeatedly during
#      the sample window
#   4. Lets the probe exit on its own
#   5. Pipes captured frame lines through v14-frame-summary.py
#
# Usage:
#   scripts/v14-baseline-with-load.sh [sample_seconds] [output_md]
#
# Exit codes:
#   0  probe ran and produced a report
#   1  build failed, app missing, osascript permission denied, or no frames
#
# Requires: System Events permission for the controlling terminal (grant via
# System Settings → Privacy & Security → Accessibility).

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

SAMPLE_SECS="${1:-30}"
OUTPUT_MD="${2:-docs/perf/v1.4/v1.3.5-baseline.md}"
WARMUP_SECS=5

if [[ "$SAMPLE_SECS" -lt 30 ]]; then
    echo "v14-baseline-with-load: sample_seconds must be >= 30 (got $SAMPLE_SECS)" >&2
    exit 1
fi

LOG_FILE="/tmp/weft.log"
PROBE_STDERR="${TMPDIR:-/tmp}/weft-v14-baseline.$$.stderr"
trap 'rm -f "$PROBE_STDERR"' EXIT

# Build release binary if needed.
echo "==> v1.4 baseline-with-load: building release (skip with WEFT_V14_NO_BUILD=1)"
if [[ -z "${WEFT_V14_NO_BUILD:-}" ]]; then
    cargo build --release -p weft_app 2>&1 | tail -3
fi

APP_PATH=""
if [[ -x "target/release/osx/Weft.app/Contents/MacOS/weft" ]]; then
    APP_PATH="target/release/osx/Weft.app/Contents/MacOS/weft"
elif [[ -x "target/release/weft" ]]; then
    APP_PATH="target/release/weft"
else
    echo "v14-baseline-with-load: release binary not found" >&2
    exit 1
fi

COMMIT_HASH="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
OS_VERSION="$(sw_vers -productVersion 2>/dev/null || echo unknown)"
SCALE_HINT="$(system_profiler SPDisplaysDataType 2>/dev/null | grep -i 'retina' | head -1 || echo 'unknown')"

rm -f "$LOG_FILE"

echo "==> v1.4 baseline-with-load: starting (warmup ${WARMUP_SECS}s + sample ${SAMPLE_SECS}s)"
echo "    commit=$COMMIT_HASH  macos=$OS_VERSION  binary=$APP_PATH  log=$LOG_FILE"

# A multi-line ANSI-colored command that mimics a Claude transcript: header
# lines, bullet lists, code blocks, and a long wrapped paragraph. Pasting this
# repeatedly into the prompt produces steady-state styled BlockView output
# during the sample window. We keep each paste under 4 KiB so the PTY doesn't
# throttle.
LOAD_CMD='for i in $(seq 1 20); do printf "\x1b[1;36m# Section %d\x1b[0m\n" "$i"; printf "\x1b[2m- bullet one with some longer text to force wrapping across the terminal width so we exercise the styled output path\x1b[0m\n"; printf "\x1b[2m- bullet two with similar long text that should wrap and produce multiple styled rows in the block view\x1b[0m\n"; printf "\x1b[32m$ \x1b[0m\x1b[1mcargo test --workspace\x1b[0m\n"; printf "\x1b[2m    Compiling weft_core v1.4.0 (/Users/test/weft/crates/weft_core)\x1b[0m\n"; printf "\x1b[2m    Compiling weft_app v1.4.0 (/Users/test/weft/crates/weft_app)\x1b[0m\n"; printf "\x1b[32m     Running tests/tui_integration.rs\x1b[0m\n"; printf "\n"; printf "\x1b[1;33mnote: \x1b[0m\x1b[3mthis is a longer note that should wrap across multiple lines and exercise the styled-output path with foreground color and italic styling applied to the same BlockView row\x1b[0m\n"; printf "\n"; done; printf "\x1b[1;35m=== DONE ===\x1b[0m\n"'

# Start the app in the background.
WEFT_GUI_PERF_PROBE=1 \
WEFT_GUI_PROBE_WARMUP_SECS="$WARMUP_SECS" \
WEFT_GUI_PROBE_SAMPLE_SECS="$SAMPLE_SECS" \
RUST_LOG=weft::frame_trace=debug \
"$APP_PATH" 2>"$PROBE_STDERR" &
APP_PID=$!

# Wait for warmup + a small buffer so the first paint completes before we
# start driving the prompt. This ensures the typed command lands in a
# prompt that's already visible (BlockView ready).
sleep $((WARMUP_SECS + 2))

echo "==> v1.4 baseline-with-load: typing load command every 2s during sample"

# Drive the prompt during the sample window. Each iteration types the load
# command and presses Enter; the next iteration starts after 2s so the
# shell has time to render the output (and so the BlockView records multiple
# completed blocks).
SAMPLE_END=$(( $(date +%s) + SAMPLE_SECS - 5 ))
while [[ $(date +%s) -lt $SAMPLE_END ]]; do
    # Activate Weft window and type the command. The `delay 0.2` after
    # activation gives the window server time to focus Weft before keystrokes
    # start, so they don't end up in whatever app was previously frontmost.
    osascript -e "tell application \"Weft\" to activate" \
              -e "delay 0.2" \
              -e "tell application \"System Events\" to keystroke \"$LOAD_CMD\"" \
              -e "tell application \"System Events\" to keystroke return" \
        2>/dev/null || true
    sleep 2
done

# Wait for the probe to exit on its own (it auto-quits after warmup+sample).
wait $APP_PID 2>/dev/null || true

cat "$PROBE_STDERR" >&2 || true

FRAME_COUNT=$(grep -c ' frame$' "$LOG_FILE" 2>/dev/null || true)
if [[ "$FRAME_COUNT" -eq 0 ]]; then
    echo "v14-baseline-with-load: no 'frame' trace lines captured in $LOG_FILE" >&2
    exit 1
fi

STYLED_FRAMES=$(grep 'frame frame_id' "$LOG_FILE" | awk -F 'styled_paint_us=' '{print $2}' | awk '{print $1}' | grep -v '^0$' | wc -l | tr -d ' ')
echo "==> v1.4 baseline-with-load: captured $FRAME_COUNT frames ($STYLED_FRAMES with styled_paint_us > 0)"

SUMMARY="$(
    python3 scripts/v14-frame-summary.py \
        --log "$LOG_FILE" \
        --commit "$COMMIT_HASH" \
        --os "$OS_VERSION" \
        --sample-secs "$SAMPLE_SECS" \
        --warmup-secs "$WARMUP_SECS" \
        --scale-hint "$SCALE_HINT" \
        --scenario "synthetic Claude-like transcript (osascript-driven)"
)"

mkdir -p "$(dirname "$OUTPUT_MD")"
printf '%s\n' "$SUMMARY" >"$OUTPUT_MD"
echo "==> v1.4 baseline-with-load: report written to $OUTPUT_MD"
echo "==> v1.4 baseline-with-load: done"
