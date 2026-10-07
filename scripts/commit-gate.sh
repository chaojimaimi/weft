#!/usr/bin/env bash
# commit-gate.sh — pre-commit hook enforcing AGENTS.md §2 and §4.
#
# Checks:
#   1. Rust changes must have a fresh .zcode/review-passed marker
#      (consumed on pass — one commit per review).
#   2. Staged .rs files must not exceed 800 lines (except tests.rs and
#      files in scripts/architecture_allowlist.txt).
#   3. Non-Rust changes (docs, configs) may skip the review requirement
#      by touching the marker manually: `touch .zcode/review-passed`.
#
# Exit codes:
#   0 — all checks passed (marker consumed if present)
#   1 — one or more checks failed
#
# Install:  ln -s ../../scripts/commit-gate.sh .git/hooks/pre-commit
# Run manually: ./scripts/commit-gate.sh

set -euo pipefail

# v1.10.34 修复：hook 经符号链接 .git/hooks/pre-commit 调用时 $0 是链接路径，
# `dirname $0/..` 解析到 .git/ 并 cd 进去——.git 内无 work tree，git diff --cached
# 静默返回空，Check 1/2 全部漏检（07-28 装 hook 起所有 .rs 提交绕过审查门）。
# 改用 git rev-parse 定位 worktree 根（hook 启动时 cwd 即仓库根），保留原逻辑回退。
ROOT="$(git rev-parse --show-toplevel 2>/dev/null || true)"
if [ -z "$ROOT" ]; then
  ROOT="$(cd "$(dirname "$0")/.." && pwd)"
fi
cd "$ROOT"

MARKER=".zcode/review-passed"
MAX_LINES=800
# v1.12.25 (3-B-2 P2-01): main.rs 专项阈值 — spawn_pty/pump_pty/process_messages/
# request_redraw 四方法已外移到 app/session_pump.rs；App struct/new()/tab() 与
# AppEvent/AppMsg 定义留守使 main.rs 无法回到 AGENTS.md §4 的 490，阈值钉在实际
# 审计值（进位取整），只防回弹、不允许借道 allowlist 越限。
MAIN_RS_MAX=540
# v1.12.27b: raised 530->540 (+6 actual: `mod window_event;` + `mod mouse_press;`
# registration blocks with WHY comments — the debt-split train's inevitable
# product; module declarations are exactly the content AGENTS.md §4 allows)
MAIN_RS_FILE="crates/weft_app/src/main.rs"
BUDGET_FILE="scripts/architecture_allowlist.txt"
failures=0

# --- Check 1: review-passed marker for .rs changes ---
staged_rs=$(git diff --cached --name-only --diff-filter=ACMR -- '*.rs' | head -1)
if [ -n "$staged_rs" ]; then
    if [ ! -f "$MARKER" ]; then
        echo "FAIL: staged .rs changes but $MARKER is missing."
        echo "      Run rust-reviewer on the changes, then touch $MARKER."
        echo "      (AGENTS.md §2: Rust 改动必须经过 rust-reviewer)"
        failures=$((failures + 1))
    else
        # Verify the marker is newer than the last commit (not stale).
        last_commit_time=$(git log -1 --format=%ct 2>/dev/null || echo 0)
        marker_time=$(stat -f %m "$MARKER" 2>/dev/null || stat -c %Y "$MARKER" 2>/dev/null || echo 0)
        if [ "$marker_time" -le "$last_commit_time" ]; then
            echo "FAIL: $MARKER is stale (older than last commit)."
            echo "      Re-run rust-reviewer and touch $MARKER."
            echo "      (AGENTS.md §2: one review per commit)"
            failures=$((failures + 1))
        else
            echo "OK: $MARKER is fresh — consuming marker for this commit."
            rm -f "$MARKER"
        fi
    fi
else
    echo "OK: no staged .rs changes — review marker not required."
fi

# --- Check 2: staged .rs file line counts ---
while IFS= read -r -d '' file; do
    # Skip pure test files.
    case "$file" in
        */tests.rs) continue ;;
        */tests/*.rs) continue ;;
    esac

    # Only check staged files (file must exist in working tree).
    [ -f "$file" ] || continue

    lines=$(wc -l < "$file" | tr -d ' ')
    # v1.12.25 (3-B-2 P2-01): main.rs has its own stricter ceiling (see MAIN_RS_MAX).
    if [ "$file" = "$MAIN_RS_FILE" ] && [ "$lines" -gt "$MAIN_RS_MAX" ]; then
        echo "FAIL: $file is $lines lines (main.rs limit $MAIN_RS_MAX)"
        echo "       Split new methods into a module (e.g. app/), do not re-inflate."
        echo "       (AGENTS.md §4: main.rs 只保留模块声明与启动编排，禁止重新膨胀)"
        failures=$((failures + 1))
    fi
    if [ "$lines" -gt "$MAX_LINES" ]; then
        entry=$(awk -F '|' -v path="$file" '$1 == path { print; exit }' "$BUDGET_FILE" 2>/dev/null || true)
        if [ -z "$entry" ]; then
            echo "FAIL: $file is $lines lines (limit $MAX_LINES)"
            echo "       Add an audited ceiling to $BUDGET_FILE or split the file."
            echo "       (AGENTS.md §4: main.rs 不再膨胀)"
            failures=$((failures + 1))
        else
            IFS='|' read -r _ allowed_max reason <<< "$entry"
            if [ "$lines" -gt "$allowed_max" ]; then
                echo "FAIL: $file grew to $lines lines (audited ceiling $allowed_max)"
                echo "       Reason: $reason"
                failures=$((failures + 1))
            else
                echo "OK (budgeted): $file ($lines/$allowed_max lines)"
            fi
        fi
    fi
done < <(git diff --cached --name-only --diff-filter=ACMR -z -- '*.rs')

if [ "$failures" -eq 0 ]; then
    echo "commit-gate: ALL CHECKS PASSED"
    exit 0
else
    echo "commit-gate: $failures FAILURE(S)"
    exit 1
fi
