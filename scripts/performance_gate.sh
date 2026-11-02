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

# WP3 anchor budgets (PLAN_v1.17.0_WP3_PERF_GATE §2), enforced by the
# local-only tripwire block below (skipped when WEFT_PERF_SLACK != 1.0).
#   B: seq_2m production pipeline, min-of-3 (WP2-F2 measured 2.80s, +6%
#      headroom for the min-of-3 estimator).
#   A: cjk_50k headless probe, median-of-3 (WP1 measured 117.2ms).
SEQ2M_PROBE_BUDGET_MS=3000
CJK_50K_BUDGET_MS=130

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
# WW_BENCH 2026-10 external cross-check: cold start to first frame 0.43s
# (manual measurement on the reference machine — local-only metric, not
# wired into CI; the automated budget below stays the 485ms p95).
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

# ── WP3 anchor tripwires (PLAN_v1.17.0_WP3_PERF_GATE §3-2) ──────────
# local-only: both anchors are single-threaded probes calibrated on the
# dev reference machine, so CI (WEFT_PERF_SLACK=3) skips this block and
# keeps only the scaled legacy budgets above.
if [ "${WEFT_PERF_SLACK:-1.0}" = "1.0" ]; then
    # B anchor: seq_2m production pipeline, min-of-3 (WP2-F2 estimator).
    # The probe prints one
    #   V110_METRIC name=probe_seq2m_pipeline_ms value=<ms>
    # line per run (parse_worker_probes.rs); run_logged lands it in
    # PERF_LOG and the python report below carries it into baseline.json.
    # Multi-threaded pipeline, but 2026-11-01 load testing still saw
    # min-of-3 at 3650-3696ms under ~64% background CPU (+21% vs the
    # clean-window 2786ms) — so a first red triggers exactly one
    # noisy-neighbour retry (same shape as the A anchor); a red that
    # survives the retry logs the load averages for triage.
    echo "==> Performance gate: seq2m production pipeline probe (WP3 B anchor, min-of-3 <= ${SEQ2M_PROBE_BUDGET_MS}ms)"

    seq2m_probe_round() {
        # 3 probe runs; output tees to PERF_LOG and the gate log verbatim.
        # MUST run outside command substitution — run_logged's tee writes
        # the cargo output to stdout, which a $( ) would swallow into the
        # captured value (2026-11-01: that exact bug put a cargo warning
        # into the PASS line's min=).
        for _seq2m_run in 1 2 3; do
            run_logged cargo test --release -p weft_app probe_real_pty_seq2m_single_pane -- \
                --ignored --nocapture --test-threads=1
        done
    }

    seq2m_round_min() {
        # Min over the V110_METRIC lines appended to PERF_LOG after line
        # $1 (the total log line count taken BEFORE the round). Pure
        # reader — safe under command substitution. Scoping to the round's
        # appended lines keeps a retry round from being masked by a faster
        # first round.
        tail -n +"$(( $1 + 1 ))" "$PERF_LOG" | awk '
            BEGIN { min = -1 }
            /V110_METRIC name=probe_seq2m_pipeline_ms/ {
                if (match($0, /value=[0-9]+/)) {
                    v = substr($0, RSTART + 6, RLENGTH - 6) + 0
                    if (min < 0 || v < min) { min = v }
                }
            }
            END { if (min >= 0) { print min } }
        '
    }

    SEQ2M_BEFORE="$(wc -l < "$PERF_LOG" | tr -d ' ')"
    seq2m_probe_round
    SEQ2M_MIN_MS="$(seq2m_round_min "$SEQ2M_BEFORE")"
    if [ -z "$SEQ2M_MIN_MS" ]; then
        echo "seq2m pipeline probe emitted no V110_METRIC line" >&2
        exit 1
    fi
    if [ "$SEQ2M_MIN_MS" -gt "$SEQ2M_PROBE_BUDGET_MS" ]; then
        printf 'PERF_GATE RETRY: probe_seq2m_pipeline_ms min=%sms > budget %sms (background-load suspect)\n' \
            "$SEQ2M_MIN_MS" "$SEQ2M_PROBE_BUDGET_MS" | tee -a "$PERF_LOG"
        SEQ2M_BEFORE="$(wc -l < "$PERF_LOG" | tr -d ' ')"
        seq2m_probe_round
        SEQ2M_MIN_MS="$(seq2m_round_min "$SEQ2M_BEFORE")"
        if [ -z "$SEQ2M_MIN_MS" ]; then
            echo "seq2m pipeline probe emitted no V110_METRIC line on retry" >&2
            exit 1
        fi
    fi
    if [ "$SEQ2M_MIN_MS" -gt "$SEQ2M_PROBE_BUDGET_MS" ]; then
        {
            printf 'PERF_GATE FAIL: probe_seq2m_pipeline_ms min=%sms > budget %sms\n' \
                "$SEQ2M_MIN_MS" "$SEQ2M_PROBE_BUDGET_MS"
            printf 'context: %s\n' "$(uptime)"
        } | tee -a "$PERF_LOG"
        exit 1
    fi
    printf 'PERF_GATE PASS: probe_seq2m_pipeline_ms min=%sms <= budget %sms\n' \
        "$SEQ2M_MIN_MS" "$SEQ2M_PROBE_BUDGET_MS" | tee -a "$PERF_LOG"

    # A anchor: cjk_50k headless (WP1 fast lane), median-of-3 gate-side.
    # Each probe_cjk invocation already reports the in-binary median of 3
    # runs on its "cjk_50k (44 fullwide/line)" line. Single-threaded, so a
    # first red triggers exactly one noisy-neighbour retry; a red that
    # survives the retry logs the load averages for triage (background
    # load degraded this probe +63% on 2026-11-01 while the multi-
    # threaded seq2m probe moved only +3.5%).
    echo "==> Performance gate: cjk_50k headless probe (WP3 A anchor, median-of-3 <= ${CJK_50K_BUDGET_MS}ms)"
    mkdir -p /tmp/ww_bench/data
    cp docs/perf/ww-bench-2026-10/data/cjk_50k_pty.bin /tmp/ww_bench/data/cjk_50k_pty.bin

    cjk_probe_once() {
        # One probe_cjk invocation; echoes the parsed ms value.
        local out ms
        if ! out="$(cargo run --release --bin probe_cjk --manifest-path \
            docs/perf/ww-bench-2026-10/probe_cjk/Cargo.toml 2>&1)"; then
            echo "probe_cjk invocation failed" >&2
            return 1
        fi
        printf '%s\n' "$out" >> "$PERF_LOG"
        ms="$(printf '%s\n' "$out" | grep 'cjk_50k (44 fullwide/line)' \
            | sed -E 's/.*[[:space:]]([0-9]+(\.[0-9]+)?) ms.*/\1/' || true)"
        [ -n "$ms" ] || { echo "probe_cjk emitted no cjk_50k timing line" >&2; return 1; }
        printf '%s\n' "$ms"
    }

    cjk_attempt_median() {
        # Median of 3 probe_cjk invocations.
        local samples="" ms
        for _cjk_run in 1 2 3; do
            ms="$(cjk_probe_once)" || return 1
            samples="$samples $ms"
        done
        # shellcheck disable=SC2086  # word splitting is the point
        printf '%s\n' $samples | sort -n | sed -n 2p
    }

    CJK_MEDIAN_MS="$(cjk_attempt_median)" || {
        echo "cjk_50k probe failed to report a timing" >&2
        exit 1
    }
    if awk -v v="$CJK_MEDIAN_MS" -v b="$CJK_50K_BUDGET_MS" 'BEGIN { exit !(v > b) }'; then
        printf 'PERF_GATE RETRY: cjk_50k median=%sms > budget %sms (background-load suspect)\n' \
            "$CJK_MEDIAN_MS" "$CJK_50K_BUDGET_MS" | tee -a "$PERF_LOG"
        CJK_MEDIAN_MS="$(cjk_attempt_median)" || {
            echo "cjk_50k probe failed to report a timing on retry" >&2
            exit 1
        }
    fi
    if awk -v v="$CJK_MEDIAN_MS" -v b="$CJK_50K_BUDGET_MS" 'BEGIN { exit !(v > b) }'; then
        {
            printf 'PERF_GATE FAIL: cjk_50k median=%sms > budget %sms\n' \
                "$CJK_MEDIAN_MS" "$CJK_50K_BUDGET_MS"
            printf 'context: %s\n' "$(uptime)"
        } | tee -a "$PERF_LOG"
        exit 1
    fi
    printf 'PERF_GATE PASS: cjk_50k median=%sms <= budget %sms\n' \
        "$CJK_MEDIAN_MS" "$CJK_50K_BUDGET_MS" | tee -a "$PERF_LOG"

    # ── WP1 D6 zoom rig segment (PLAN_v1180_WP1_ZOOM_ROOT §2-D6 + WP1.5 §7-5) ──
    # MUST be run from Terminal.app, never inside Weft (nested-instance
    # discipline). Scope limitation (WP1.5 R1 review P3, explicit): the
    # criteria below run on a FRESH config (no session restore), where
    # documents are tiny and the G4 block reconcile is structurally cheap —
    # this segment pins the WP1 frame-supply mechanics (steps band +
    # per-frame draw_ms + drawable waits) but is BLIND to a G4 reconcile
    # regression; the restored-session protection is the manual rig
    # (acceptance 1/3) plus the F4 sequence tests.
    echo "==> Performance gate: zoom self-zoom rig (WP1 D6, steps in [10,15], draw_ms < 33)"

    if [ ! -x target/release/weft ]; then
        echo "zoom rig: target/release/weft missing (build release first)" >&2
        exit 1
    fi

    zoom_rig_round() {
        # Run one rig session and emit the log slice to stdout. The offset
        # is taken BEFORE the launch (a concurrently running dev instance
        # may append to the same log); if the log rotated mid-run (file
        # smaller than the offset), fall back to parsing the whole current
        # file instead of reporting a false red.
        local offset size zoom_tmp
        if [ -f "${HOME}/Library/Logs/Weft/weft.log" ]; then
            offset="$(wc -c < "${HOME}/Library/Logs/Weft/weft.log" | tr -d ' ')"
        else
            offset=0
        fi
        zoom_tmp="$(mktemp -d "${TMPDIR:-/tmp}/weft-zoom-rig.XXXXXX")"
        # WEFT_ALLOW_SECOND_INSTANCE=1: the user's production Weft holds the
        # single-instance flock (ds-worker finding, WP1.5); the documented
        # dev/nested-test escape hatch lets the rig session start beside it.
        XDG_CACHE_HOME="$zoom_tmp/cache" \
            XDG_CONFIG_HOME="$zoom_tmp/config" \
            WEFT_ALLOW_SECOND_INSTANCE=1 \
            WEFT_SELF_ZOOM_TEST=1 \
            RUST_LOG=info \
            target/release/weft &
        local rig_pid=$!
        # 11s covers both rig fires (2s zoom-in + 6s zoom-out) plus the
        # animation and convergence margins.
        sleep 11
        kill "$rig_pid" 2>/dev/null || true
        wait "$rig_pid" 2>/dev/null || true
        rm -rf "$zoom_tmp"
        if [ -f "${HOME}/Library/Logs/Weft/weft.log" ]; then
            size="$(wc -c < "${HOME}/Library/Logs/Weft/weft.log" | tr -d ' ')"
            if [ "$size" -ge "$offset" ]; then
                tail -c "+$((offset + 1))" "${HOME}/Library/Logs/Weft/weft.log"
            else
                cat "${HOME}/Library/Logs/Weft/weft.log"
            fi
        fi
    }

    zoom_rig_verdict() {
        # Criteria (WP1 §2-D6): >= 2 `self-zoom complete steps=N` lines
        # (both rig directions) with every N in [10,15] (upper bound 14 is
        # structurally capped by the pacing floor; the tripwire is the lower
        # bound 10 = late wakes merging steps); every `zoom frame draw`
        # draw_ms < 33; zero `zoom frame draw slow`; zero `next drawable
        # wait`. Emits a PASS/FAIL verdict line; exits nonzero on FAIL.
        awk '
            /self-zoom complete steps=/ {
                n = $0
                sub(/.*steps=/, "", n)
                sub(/[^0-9].*$/, "", n)
                completes++
                if (n + 0 < 10 || n + 0 > 15) {
                    bad_steps++
                    printf("zoom-rig: out-of-band steps=%s\n", n)
                } else {
                    printf("zoom-rig: self-zoom complete steps=%s\n", n)
                }
            }
            /zoom frame draw draw_ms=/ {
                ms = $0
                sub(/.*draw_ms=/, "", ms)
                sub(/[^0-9].*$/, "", ms)
                draws++
                if (ms + 0 >= 33) slow_draw++
                if (ms + 0 > max_draw) max_draw = ms + 0
            }
            /zoom frame draw slow/ { slow_warn++ }
            /next drawable wait/ { wait_warn++ }
            END {
                summary = sprintf("zoom-rig: completes=%d draws=%d max_draw_ms=%d slow_draws=%d slow_warn=%d wait_warn=%d", completes + 0, draws + 0, max_draw + 0, slow_draw + 0, slow_warn + 0, wait_warn + 0)
                print summary
                if (completes < 2) {
                    print "zoom-rig: FAIL fewer than 2 self-zoom complete lines"
                    exit 1
                }
                if (bad_steps > 0) {
                    print "zoom-rig: FAIL steps outside [10,15]"
                    exit 1
                }
                if (slow_draw > 0) {
                    print "zoom-rig: FAIL draw_ms >= 33 present"
                    exit 1
                }
                if (slow_warn > 0) {
                    print "zoom-rig: FAIL zoom frame draw slow present"
                    exit 1
                }
                if (wait_warn > 0) {
                    print "zoom-rig: FAIL next drawable wait present"
                    exit 1
                }
                print "zoom-rig: PASS"
            }
        ' "$1"
    }

    zoom_rig_attempt() {
        # One round: run the rig, slice the log, verify. The verdict and the
        # raw slice land in PERF_LOG either way (evidence for triage); the
        # function's exit code carries the verdict (pipefail propagates the
        # awk exit).
        local slice rc
        slice="$(mktemp "${TMPDIR:-/tmp}/weft-zoom-slice.XXXXXX")"
        zoom_rig_round > "$slice"
        if zoom_rig_verdict "$slice" | tee -a "$PERF_LOG"; then
            rc=0
        else
            rc=1
        fi
        cat "$slice" >> "$PERF_LOG"
        rm -f "$slice"
        return "$rc"
    }

    if ! zoom_rig_attempt; then
        # First red: exactly one noisy-neighbour retry (load perturbs the
        # rig — the historical low-load samples reach 5 steps), with the
        # load averages recorded for triage either way.
        {
            printf 'PERF_GATE RETRY: zoom rig red, retrying once\n'
            printf 'context: %s\n' "$(uptime)"
        } | tee -a "$PERF_LOG"
        if ! zoom_rig_attempt; then
            {
                printf 'PERF_GATE FAIL: zoom rig criteria not met after retry\n'
                printf 'context: %s\n' "$(uptime)"
            } | tee -a "$PERF_LOG"
            exit 1
        fi
    fi
    printf 'PERF_GATE PASS: zoom rig criteria met\n' | tee -a "$PERF_LOG"

    # ── WP2 Phase A giant rig sub-variants (PLAN_v1181_WP2 §4-A2/A3) ──────
    # local-only, opt-in: WEFT_ZOOM_RIG_GIANT=1 (shaped — the 32MB giant
    # block plus 5 seq shaping rounds) or WEFT_ZOOM_RIG_GIANT_INBAND=1
    # (giant only). Neither env set → this block is inert and everything
    # above stays byte-identical to v1.18.0.
    if [ "${WEFT_ZOOM_RIG_GIANT:-0}" = "1" ] || [ "${WEFT_ZOOM_RIG_GIANT_INBAND:-0}" = "1" ]; then
        if [ "${WEFT_ZOOM_RIG_GIANT:-0}" = "1" ] && [ "${WEFT_ZOOM_RIG_GIANT_INBAND:-0}" = "1" ]; then
            echo "zoom giant rig: set only ONE of WEFT_ZOOM_RIG_GIANT / WEFT_ZOOM_RIG_GIANT_INBAND" >&2
            exit 1
        fi
        if [ "${WEFT_ZOOM_RIG_GIANT:-0}" = "1" ]; then
            GIANT_VARIANT=shaped
        else
            GIANT_VARIANT=inband
        fi

        giant_rig_round() {
            # One giant rig session. The fresh temp config is raised to the
            # legal capture cap (plan §4-A2 ①: the default output_cap_mib=1
            # would truncate the 32MB precharge block at 1MiB; 64 is
            # blocks.rs OUTPUT_CAP_MAX_MIB), the in-app precharge state
            # machine is armed (WEFT_ZOOM_RIG_PRECHARGE=1), and the window
            # is 90s = the precharge total budget (60s) + both rig fires
            # (2s/6s from the shaping anchor) + animation + convergence
            # margin (plan R2 P2 closure). WEFT_ZOOM_RIG_GIANT_INBAND is
            # inherited by the app process (it selects the inband sequence).
            local offset size zoom_tmp
            if [ -f "${HOME}/Library/Logs/Weft/weft.log" ]; then
                offset="$(wc -c < "${HOME}/Library/Logs/Weft/weft.log" | tr -d ' ')"
            else
                offset=0
            fi
            zoom_tmp="$(mktemp -d "${TMPDIR:-/tmp}/weft-zoom-rig.XXXXXX")"
            mkdir -p "$zoom_tmp/cache" "$zoom_tmp/config/weft"
            printf '[blocks]\noutput_cap_mib = 64\n' > "$zoom_tmp/config/weft/config.toml"
            XDG_CACHE_HOME="$zoom_tmp/cache" \
                XDG_CONFIG_HOME="$zoom_tmp/config" \
                WEFT_ALLOW_SECOND_INSTANCE=1 \
                WEFT_SELF_ZOOM_TEST=1 \
                WEFT_ZOOM_RIG_PRECHARGE=1 \
                RUST_LOG=info \
                target/release/weft &
            local rig_pid=$!
            sleep 90
            kill "$rig_pid" 2>/dev/null || true
            wait "$rig_pid" 2>/dev/null || true
            rm -rf "$zoom_tmp"
            if [ -f "${HOME}/Library/Logs/Weft/weft.log" ]; then
                size="$(wc -c < "${HOME}/Library/Logs/Weft/weft.log" | tr -d ' ')"
                if [ "$size" -ge "$offset" ]; then
                    tail -c "+$((offset + 1))" "${HOME}/Library/Logs/Weft/weft.log"
                else
                    cat "${HOME}/Library/Logs/Weft/weft.log"
                fi
            fi
        }

        giant_rig_verdict() {
            # Criteria (plan §7-2): both rig directions complete
            # (completes >= 2); a precharge error line (timeout / shell not
            # integrated / PTY write failure) is an immediate FAIL — the
            # giant signature is missing and the red would be
            # unattributable to the mechanism. The animation-feel budgets
            # apply to the SHAPED variant only: every steps >= 3 (inband's
            # steps ~1-2 IS the T2a signature and must not red) and zero
            # >= 1s steps. The fresh variant's [10,15] band and 33ms draw
            # budget are deliberately NOT applied — giant rebuilds pause
            # zoom steps by design; their counts are the A3 attribution
            # evidence. Probe counters on the summary line: rebuild_slow
            # (P1) / pump_slow (P2) / degrades (P3) / exempt (P5).
            awk -v variant="$1" '
                /block rebuild slow/ { rebuild_slow++ }
                /pump drain slow/ { pump_slow++ }
                /budget degrade/ { degrades++ }
                /exempt tables/ { exempt++ }
                /zoom rig precharge timeout/ { precharge_errors++ }
                /zoom rig precharge: shell not integrated/ { precharge_errors++ }
                /zoom rig precharge: PTY write failed/ { precharge_errors++ }
                /self-zoom complete steps=/ {
                    n = $0
                    sub(/.*steps=/, "", n)
                    sub(/[^0-9].*$/, "", n)
                    completes++
                    if (!steps_seen || n + 0 < min_steps) {
                        min_steps = n + 0
                        steps_seen = 1
                    }
                    printf("zoom-giant-rig(%s): self-zoom complete steps=%s\n", variant, n)
                }
                /zoom frame draw draw_ms=/ {
                    ms = $0
                    sub(/.*draw_ms=/, "", ms)
                    sub(/[^0-9].*$/, "", ms)
                    draws++
                    if (ms + 0 > max_draw) max_draw = ms + 0
                    if (ms + 0 >= 1000) second_steps++
                }
                END {
                    printf("zoom-giant-rig(%s): completes=%d min_steps=%d max_draw_ms=%d second_steps=%d rebuild_slow=%d pump_slow=%d degrades=%d exempt=%d precharge_errors=%d\n", variant, completes + 0, min_steps + 0, max_draw + 0, second_steps + 0, rebuild_slow + 0, pump_slow + 0, degrades + 0, exempt + 0, precharge_errors + 0)
                    if (precharge_errors + 0 > 0) {
                        print "zoom-giant-rig(" variant "): FAIL precharge error present"
                        exit 1
                    }
                    if (completes + 0 < 2) {
                        print "zoom-giant-rig(" variant "): FAIL fewer than 2 self-zoom complete lines"
                        exit 1
                    }
                    if (variant == "shaped") {
                        if (min_steps + 0 < 3) {
                            print "zoom-giant-rig(" variant "): FAIL steps < 3 (animation feel)"
                            exit 1
                        }
                        if (second_steps + 0 > 0) {
                            print "zoom-giant-rig(" variant "): FAIL >= 1s step present"
                            exit 1
                        }
                    }
                    print "zoom-giant-rig(" variant "): PASS"
                }
            ' "$2"
        }

        giant_rig_attempt() {
            # One round: run the giant rig, slice the log, verify. The
            # verdict and the raw slice land in PERF_LOG either way
            # (A3 evidence); the exit code carries the verdict.
            local slice rc
            slice="$(mktemp "${TMPDIR:-/tmp}/weft-zoom-slice.XXXXXX")"
            giant_rig_round > "$slice"
            if giant_rig_verdict "$GIANT_VARIANT" "$slice" | tee -a "$PERF_LOG"; then
                rc=0
            else
                rc=1
            fi
            cat "$slice" >> "$PERF_LOG"
            rm -f "$slice"
            return "$rc"
        }

        echo "==> Performance gate: zoom giant rig ($GIANT_VARIANT variant, WP2 Phase A)"
        if ! giant_rig_attempt; then
            # No auto-retry: the pre-F1 baseline red IS the expected evidence
            # (docs/perf/v1181-wp2-phase-a-baseline.md); triage order is
            # precharge error lines first, then the probe counters.
            {
                printf 'PERF_GATE FAIL: zoom giant rig (%s) criteria not met\n' "$GIANT_VARIANT"
                printf 'context: %s\n' "$(uptime)"
            } | tee -a "$PERF_LOG"
            exit 1
        fi
        printf 'PERF_GATE PASS: zoom giant rig (%s) criteria met\n' "$GIANT_VARIANT" | tee -a "$PERF_LOG"
    fi

    # ── F6 double-tab rig sub-variant (PLAN_v1183_F6_CASCADE_TAB_SCOPE §2-F6-4) ──
    # local-only, opt-in: WEFT_ZOOM_RIG_NEWTAB=1 (requires
    # WEFT_ZOOM_RIG_PRECHARGE=1 — the shaped giant precharge runs first,
    # then the app opens a fresh tab, and the zoom fires measure the
    # two-tab topology of the v1.18.3 field report: a large background
    # document tab behind an active empty tab). Unset → inert; everything
    # above stays byte-identical to the giant section's behavior.
    if [ "${WEFT_ZOOM_RIG_NEWTAB:-0}" = "1" ]; then
        if [ "${WEFT_ZOOM_RIG_PRECHARGE:-0}" != "1" ]; then
            echo "zoom newtab rig: WEFT_ZOOM_RIG_NEWTAB requires WEFT_ZOOM_RIG_PRECHARGE=1" >&2
            exit 1
        fi

        newtab_rig_round() {
            # One double-tab rig session — the GIANT round's window shape,
            # NOT the zoom rig round's 11s (the precharge budget 60s +
            # AwaitNewTab <= 10s + both rig fires 2s/6s from the newtab
            # anchor + animation/convergence margin; an 11s window would
            # truncate the sequence into a false red). The fresh temp
            # config is raised to the legal capture cap (the 32MB giant
            # precharge block, same as the giant round). RUST_LOG adds
            # weft=debug: criterion ② counts DEBUG-level RESIZE_PROBE
            # ioctl_commit lines (tab=/pane_id= fields, app_runtime.rs) —
            # info alone cannot attribute them.
            local offset size zoom_tmp
            if [ -f "${HOME}/Library/Logs/Weft/weft.log" ]; then
                offset="$(wc -c < "${HOME}/Library/Logs/Weft/weft.log" | tr -d ' ')"
            else
                offset=0
            fi
            zoom_tmp="$(mktemp -d "${TMPDIR:-/tmp}/weft-zoom-rig.XXXXXX")"
            mkdir -p "$zoom_tmp/cache" "$zoom_tmp/config/weft"
            printf '[blocks]\noutput_cap_mib = 64\n' > "$zoom_tmp/config/weft/config.toml"
            XDG_CACHE_HOME="$zoom_tmp/cache" \
                XDG_CONFIG_HOME="$zoom_tmp/config" \
                WEFT_ALLOW_SECOND_INSTANCE=1 \
                WEFT_SELF_ZOOM_TEST=1 \
                WEFT_ZOOM_RIG_PRECHARGE=1 \
                WEFT_ZOOM_RIG_NEWTAB=1 \
                RUST_LOG=info,weft=debug \
                target/release/weft &
            local rig_pid=$!
            sleep 90
            kill "$rig_pid" 2>/dev/null || true
            wait "$rig_pid" 2>/dev/null || true
            rm -rf "$zoom_tmp"
            if [ -f "${HOME}/Library/Logs/Weft/weft.log" ]; then
                size="$(wc -c < "${HOME}/Library/Logs/Weft/weft.log" | tr -d ' ')"
                if [ "$size" -ge "$offset" ]; then
                    tail -c "+$((offset + 1))" "${HOME}/Library/Logs/Weft/weft.log"
                else
                    cat "${HOME}/Library/Logs/Weft/weft.log"
                fi
            fi
        }

        newtab_rig_verdict() {
            # Criteria (plan §2-F6-4), extraction shape copied from the
            # giant verdict:
            # ① both rig directions complete (completes >= 2) with every
            #   steps >= 8 (fresh double-tab band lower bound; giant
            #   rebuilds pause steps by design, so no upper band), and
            #   every ANIMATION-phase `zoom frame draw` draw_ms < 33.
            #   Settle-tail draws (lines between `self-zoom complete` and
            #   `zoom settled:` — the SELF_ZOOM_WINDOW spans arm→flush) get
            #   the single-frame exemption (settle_exempt / settle_slow):
            #   the background giant tab's one settle commit reflow may
            #   bill one flush frame per direction and a 9.2MB-scale
            #   reflow must not贴线假红. They stay on the summary as
            #   evidence.
            # ② the background GIANT tab's RESIZE_PROBE ioctl_commit count
            #   <= 2, counting window = AFTER the `zoom rig precharge:
            #   newtab ready` marker line (the startup-convergence ioctl
            #   lands OUTSIDE the window; the new tab's own per-step
            #   commits are the ACTIVE tab and excluded by the tab-number
            #   filter). The giant tab's number is learned from the ioctl
            #   lines BEFORE the marker (fresh config: exactly one tab
            #   until the rig opens the second). Expected composition:
            #   zoom-in settle 1 + zoom-out settle 1 = exactly 2; the
            #   retired force mechanism emitted ~18 (per-step) and must
            #   red.
            # ③ probe counters on the summary line: rebuild_slow (P1) /
            #   pump_slow (P2) / degrades (P3) / exempt (P5) ride as
            #   attribution evidence (the giant's own rebuild/drain costs
            #   are the experiment's subject — same rationale as the
            #   giant verdict's counters); `next drawable wait`
            #   (wait_warn) HARD-fails: a drawable stall is exactly what
            #   F6 exists to prevent. Any precharge/newtab error line is
            #   an immediate FAIL — the red must carry attribution.
            awk '
                /block rebuild slow/ { rebuild_slow++ }
                /pump drain slow/ { pump_slow++ }
                /budget degrade/ { degrades++ }
                /exempt tables/ { exempt++ }
                /next drawable wait/ { wait_warn++ }
                /zoom rig precharge timeout/ { precharge_errors++ }
                /zoom rig precharge: shell not integrated/ { precharge_errors++ }
                /zoom rig precharge: PTY write failed/ { precharge_errors++ }
                /zoom rig precharge: new tab shell not integrated/ { precharge_errors++ }
                /zoom rig precharge: newtab ready/ { newtab_ready++ }
                /self-zoom complete steps=/ {
                    n = $0
                    sub(/.*steps=/, "", n)
                    sub(/[^0-9].*$/, "", n)
                    completes++
                    if (!steps_seen || n + 0 < min_steps) {
                        min_steps = n + 0
                        steps_seen = 1
                    }
                    printf("zoom-newtab-rig: self-zoom complete steps=%s\n", n)
                    in_settle = 1
                }
                /zoom settled:/ { in_settle = 0 }
                /zoom frame draw draw_ms=/ {
                    ms = $0
                    sub(/.*draw_ms=/, "", ms)
                    sub(/[^0-9].*$/, "", ms)
                    draws++
                    if (ms + 0 >= 33) {
                        if (in_settle) { settle_slow++ } else { slow_draw++ }
                    }
                    if (in_settle) {
                        settle_exempt++
                    } else {
                        if (ms + 0 > max_draw) max_draw = ms + 0
                    }
                    if (ms + 0 > max_overall_draw) max_overall_draw = ms + 0
                }
                /RESIZE_PROBE ioctl_commit/ {
                    tab = $0
                    sub(/^.* tab=/, "", tab)
                    sub(/[^0-9].*$/, "", tab)
                    if (tab == "") next
                    if (newtab_ready + 0 == 0) {
                        giant_tab = tab + 0
                        giant_tab_seen = 1
                    } else if (giant_tab_seen && tab + 0 == giant_tab) {
                        giant_ioctl++
                    }
                }
                END {
                    printf("zoom-newtab-rig: completes=%d min_steps=%d draws=%d max_draw_ms=%d slow_draw=%d settle_exempt=%d settle_slow=%d newtab_ready=%d giant_tab=%s giant_ioctl=%d rebuild_slow=%d pump_slow=%d degrades=%d exempt=%d wait_warn=%d precharge_errors=%d\n", completes + 0, min_steps + 0, draws + 0, max_draw + 0, slow_draw + 0, settle_exempt + 0, settle_slow + 0, newtab_ready + 0, (giant_tab_seen ? giant_tab : "none"), giant_ioctl + 0, rebuild_slow + 0, pump_slow + 0, degrades + 0, exempt + 0, wait_warn + 0, precharge_errors + 0)
                    if (precharge_errors + 0 > 0) {
                        print "zoom-newtab-rig: FAIL precharge error present"
                        exit 1
                    }
                    if (newtab_ready + 0 < 1) {
                        print "zoom-newtab-rig: FAIL missing newtab ready marker"
                        exit 1
                    }
                    if (!giant_tab_seen) {
                        print "zoom-newtab-rig: FAIL giant tab not identified (no pre-window ioctl_commit line)"
                        exit 1
                    }
                    if (completes + 0 < 2) {
                        print "zoom-newtab-rig: FAIL fewer than 2 self-zoom complete lines"
                        exit 1
                    }
                    if (min_steps + 0 < 8) {
                        print "zoom-newtab-rig: FAIL steps < 8 (animation feel)"
                        exit 1
                    }
                    if (slow_draw + 0 > 0) {
                        print "zoom-newtab-rig: FAIL animation draw_ms >= 33 present"
                        exit 1
                    }
                    if (giant_ioctl + 0 > 2) {
                        print "zoom-newtab-rig: FAIL giant tab ioctl_commit > 2 (cross-tab cascade leaked)"
                        exit 1
                    }
                    if (wait_warn + 0 > 0) {
                        print "zoom-newtab-rig: FAIL next drawable wait present"
                        exit 1
                    }
                    print "zoom-newtab-rig: PASS"
                }
            ' "$1"
        }

        newtab_rig_attempt() {
            # One round: run the rig, slice the log, verify. The verdict and
            # the raw slice land in PERF_LOG either way (F6 evidence); the
            # exit code carries the verdict.
            local slice rc
            slice="$(mktemp "${TMPDIR:-/tmp}/weft-zoom-slice.XXXXXX")"
            newtab_rig_round > "$slice"
            if newtab_rig_verdict "$slice" | tee -a "$PERF_LOG"; then
                rc=0
            else
                rc=1
            fi
            cat "$slice" >> "$PERF_LOG"
            rm -f "$slice"
            return "$rc"
        }

        echo "==> Performance gate: zoom double-tab rig (F6, local-only)"
        if ! newtab_rig_attempt; then
            # No auto-retry: a red here IS the F6 regression evidence.
            # Triage order: precharge/newtab error lines first, then the
            # giant_ioctl count, then the settle-frame draws.
            {
                printf 'PERF_GATE FAIL: zoom double-tab rig criteria not met\n'
                printf 'context: %s\n' "$(uptime)"
            } | tee -a "$PERF_LOG"
            exit 1
        fi
        printf 'PERF_GATE PASS: zoom double-tab rig criteria met\n' | tee -a "$PERF_LOG"
    fi
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
