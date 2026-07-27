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
#   0  probe ran, injected load, and produced a report with styled cache data
#   1  build failed, app missing, input injection failed, or no useful frames
#
# Requires: System Events permission for the controlling terminal (grant via
# System Settings → Privacy & Security → Accessibility).

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

SAMPLE_SECS="${1:-30}"
OUTPUT_MD="${2:-docs/perf/v1.4/v1.4.1-loaded-current.md}"
WARMUP_SECS=5
EXPECT_CACHE="${WEFT_V14_EXPECT_CACHE:-1}"

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

APP_PATH="${WEFT_V14_APP_PATH:-}"
if [[ -n "$APP_PATH" && ! -x "$APP_PATH" ]]; then
    echo "v14-baseline-with-load: WEFT_V14_APP_PATH is not executable: $APP_PATH" >&2
    exit 1
elif [[ -n "$APP_PATH" ]]; then
    :
elif [[ -x "target/release/osx/Weft.app/Contents/MacOS/weft" ]]; then
    APP_PATH="target/release/osx/Weft.app/Contents/MacOS/weft"
elif [[ -x "target/release/weft" ]]; then
    APP_PATH="target/release/weft"
else
    echo "v14-baseline-with-load: release binary not found" >&2
    exit 1
fi

COMMIT_HASH="${WEFT_V14_COMMIT_HASH:-$(git rev-parse --short HEAD 2>/dev/null || echo unknown)}"
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
INJECTION_COUNT=0
while [[ $(date +%s) -lt $SAMPLE_END ]]; do
    # Activate Weft window and type the command. The `delay 0.2` after
    # activation gives the window server time to focus Weft before keystrokes
    # start, so they don't end up in whatever app was previously frontmost.
    if ! osascript \
        -e 'on run argv' \
        -e 'set appPid to (item 2 of argv) as integer' \
        -e 'tell application "System Events" to set frontmost of first process whose unix id is appPid to true' \
        -e 'delay 0.2' \
        -e 'tell application "System Events" to keystroke (item 1 of argv)' \
        -e 'delay 0.8' \
        -e 'tell application "System Events" to key code 36 using command down' \
        -e 'end run' \
        -- "$LOAD_CMD" "$APP_PID" >/dev/null; then
        echo "v14-baseline-with-load: input injection failed; grant Accessibility permission" >&2
        kill "$APP_PID" 2>/dev/null || true
        wait "$APP_PID" 2>/dev/null || true
        exit 1
    fi
    INJECTION_COUNT=$((INJECTION_COUNT + 1))
    sleep 2
done

# Wait for the probe to exit on its own (it auto-quits after warmup+sample).
wait $APP_PID 2>/dev/null || true

cat "$PROBE_STDERR" >&2 || true

FRAME_COUNT=$(grep -c 'frame_trace: frame ' "$LOG_FILE" 2>/dev/null || true)
if [[ "$FRAME_COUNT" -eq 0 ]]; then
    echo "v14-baseline-with-load: no frame trace lines captured in $LOG_FILE" >&2
    exit 1
fi

STYLED_FRAMES=$(awk -F 'styled_paint_us=' '
    /frame_trace: frame / { split($2, value, " "); if (value[1] > 0) count++ }
    END { print count + 0 }
' "$LOG_FILE")
CACHE_HITS=$(awk -F 'styled_cache_hits=' '
    /frame_trace: frame / { split($2, value, " "); total += value[1] }
    END { print total + 0 }
' "$LOG_FILE")
CACHE_MISSES=$(awk -F 'styled_cache_misses=' '
    /frame_trace: frame / { split($2, value, " "); total += value[1] }
    END { print total + 0 }
' "$LOG_FILE")
CACHE_BYTES_MAX=$(awk -F 'styled_cache_bytes=' '
    /frame_trace: frame / { split($2, value, " "); if (value[1] > max) max = value[1] }
    END { print max + 0 }
' "$LOG_FILE")

if [[ "$INJECTION_COUNT" -eq 0 || "$STYLED_FRAMES" -eq 0 ]]; then
    echo "v14-baseline-with-load: load did not exercise styled BlockView rendering" >&2
    exit 1
fi
if [[ "$EXPECT_CACHE" == "1" && $((CACHE_HITS + CACHE_MISSES)) -eq 0 ]]; then
    echo "v14-baseline-with-load: no styled cache lookups were recorded" >&2
    exit 1
fi

if [[ "$EXPECT_CACHE" == "1" ]]; then
    CACHE_LOOKUPS=$((CACHE_HITS + CACHE_MISSES))
    CACHE_HIT_RATE_BPS=$((CACHE_HITS * 10000 / CACHE_LOOKUPS))
    if [[ "$CACHE_HIT_RATE_BPS" -lt 7000 ]]; then
        echo "v14-baseline-with-load: styled cache hit rate is below 70%" >&2
        exit 1
    fi
    if [[ "$CACHE_BYTES_MAX" -gt 16777216 ]]; then
        echo "v14-baseline-with-load: styled cache exceeded the 16 MiB budget" >&2
        exit 1
    fi
fi

echo "==> v1.4 baseline-with-load: captured $FRAME_COUNT frames"
echo "    injections=$INJECTION_COUNT styled_frames=$STYLED_FRAMES cache_hits=$CACHE_HITS cache_misses=$CACHE_MISSES"

SUMMARY="$(
    python3 scripts/v14-frame-summary.py \
        --log "$LOG_FILE" \
        --commit "$COMMIT_HASH" \
        --os "$OS_VERSION" \
        --sample-secs "$SAMPLE_SECS" \
        --warmup-secs "$WARMUP_SECS" \
        --scale-hint "$SCALE_HINT" \
        --title "${WEFT_V14_REPORT_TITLE:-v1.4.1 Loaded Styled-Line Cache Probe}"
)"

mkdir -p "$(dirname "$OUTPUT_MD")"
{
    printf '%s\n' "$SUMMARY"
    printf '\n## Load execution\n\n'
    printf -- '- Input injections: %s\n' "$INJECTION_COUNT"
    printf -- '- Frames with `styled_paint_us > 0`: %s\n' "$STYLED_FRAMES"
    if [[ "$EXPECT_CACHE" == "1" ]]; then
        printf '\n## Loaded cache acceptance\n\n'
        printf -- '- Cache hits / misses: %s / %s\n' "$CACHE_HITS" "$CACHE_MISSES"
        printf -- '- Cache hit rate: %d.%02d%% (threshold >= 70%%)\n' \
            $((CACHE_HIT_RATE_BPS / 100)) $((CACHE_HIT_RATE_BPS % 100))
        printf -- '- Maximum cache bytes: %s (threshold <= 16777216)\n' "$CACHE_BYTES_MAX"
        printf -- '- Operational cache verdict: **PASS**\n'
        printf '\nThis operational probe does not replace a same-workload pre-cache comparison.\n'
    else
        printf '\n## Pre-cache baseline\n\n'
        printf -- '- Styled cache counters are intentionally unavailable.\n'
    fi
} >"$OUTPUT_MD"
echo "==> v1.4 baseline-with-load: report written to $OUTPUT_MD"
echo "==> v1.4 baseline-with-load: done"
