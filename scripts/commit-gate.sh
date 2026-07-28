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

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

MARKER=".zcode/review-passed"
MAX_LINES=800
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
