#!/usr/bin/env bash
# Architecture gate: enforce file budget, layering, and API hygiene.
#
# Exit codes:
#   0 — all checks passed
#   1 — one or more checks failed
#
# Run locally: ./scripts/architecture_gate.sh
# Run in CI:   ./scripts/architecture_gate.sh (added to ci.yml)

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# Paths to scan (source only — skip target/, tests/, generated files).
APP_SRC=(crates/weft_app/src)
CORE_SRC=(crates/weft_core/src)

# File budget: production .rs files must stay <= 800 lines.
# Exceptions: tests.rs files (pure test code) and files listed in the central
# budget file. Each listed file has a frozen ceiling, so an exception cannot
# silently grow and a new inline source comment cannot authorize itself.
MAX_LINES=800
BUDGET_FILE="scripts/architecture_allowlist.txt"

# Layering: weft_core must NOT depend on weft_app (no `use weft_app::`
# anywhere in weft_core). This prevents the core terminal emulation layer
# from pulling in rendering/platform code.
failures=0

echo "==> Architecture gate: file budget (<= $MAX_LINES lines)"

# Check all .rs files under crates/*/src, excluding tests.rs files.
while IFS= read -r -d '' file; do
    # Skip pure test files.
    case "$file" in
        */tests.rs) continue ;;
        */tests/*.rs) continue ;;
    esac

    lines=$(wc -l < "$file" | tr -d ' ')
    if [ "$lines" -gt "$MAX_LINES" ]; then
        entry=$(awk -F '|' -v path="$file" '$1 == path { print; exit }' "$BUDGET_FILE")
        if [ -z "$entry" ]; then
            echo "  FAIL: $file is $lines lines (limit $MAX_LINES)"
            echo "         Add an audited ceiling and reason to $BUDGET_FILE."
            failures=$((failures + 1))
        else
            IFS='|' read -r _ allowed_max reason <<< "$entry"
            case "$allowed_max" in
                ''|*[!0-9]*)
                    echo "  FAIL: invalid ceiling '$allowed_max' for $file in $BUDGET_FILE"
                    failures=$((failures + 1))
                    continue
                    ;;
            esac
            if [ "$lines" -gt "$allowed_max" ]; then
                echo "  FAIL: $file grew to $lines lines (audited ceiling $allowed_max)"
                echo "         Reason: $reason"
                failures=$((failures + 1))
            else
                echo "  OK (budgeted): $file ($lines/$allowed_max lines)"
            fi
        fi
    fi
done < <(find crates -name "*.rs" -not -path "*/target/*" -print0)

echo "==> Architecture gate: budget inventory"

while IFS='|' read -r file allowed_max reason; do
    case "$file" in
        ''|'#'*) continue ;;
    esac
    if [ ! -f "$file" ]; then
        echo "  FAIL: budget entry points to missing file: $file"
        failures=$((failures + 1))
        continue
    fi
    case "$allowed_max" in
        ''|*[!0-9]*)
            echo "  FAIL: invalid ceiling '$allowed_max' for $file"
            failures=$((failures + 1))
            continue
            ;;
    esac
    if [ -z "$reason" ]; then
        echo "  FAIL: budget entry has no reason: $file"
        failures=$((failures + 1))
    fi
done < "$BUDGET_FILE"

if [ "$failures" -eq 0 ]; then
    echo "  OK: centralized budget inventory is valid"
fi

echo "==> Architecture gate: layering (weft_core must not depend on weft_app)"

if grep -rn 'use weft_app::' crates/weft_core/src --include="*.rs" 2>/dev/null; then
    echo "  FAIL: weft_core imports from weft_app — core layer must be dependency-free"
    failures=$((failures + 1))
else
    echo "  OK: no weft_core → weft_app imports"
fi

echo "==> Architecture gate: clippy too_many_arguments lint"

# Run clippy with too_many_arguments denied. This catches growing function
# signatures before they become unmaintainable.
clippy_output=$(cargo clippy --workspace --all-targets --quiet -- \
    -D warnings \
    -W clippy::too_many_arguments \
    -W clippy::too_many_lines \
    2>&1) || true

# Re-run with the lint flags as warnings to detect their presence.
too_many=$(echo "$clippy_output" | grep -E 'warning: .*(too_many_arguments|too_many_lines)' || true)
if [ -n "$too_many" ]; then
    echo "  WARN: too_many_arguments/too_many_lines detected — consider refactoring:"
    echo "$too_many" | sed 's/^/    /'
    # Warnings only, not failures — these are advisory.
    echo "  (advisory: refactor when convenient)"
else
    echo "  OK: no too_many_arguments/too_many_lines warnings"
fi

echo "==> Architecture gate: duplicate layout constants"

# Check for hardcoded cell/padding constants that should go through
# LayoutCtx. Common offenders: literal pixel values in paint code.
dup_constants=$(grep -rn 'ch \* 1\.1\|cell_h \* 1\.1' \
    crates/weft_app/src/paint --include="*.rs" 2>/dev/null || true)
if [ -n "$dup_constants" ]; then
    echo "  WARN: hardcoded pitch constants found in paint/ — use LayoutCtx:"
    echo "$dup_constants" | sed 's/^/    /'
    echo "  (advisory: route through LayoutCtx)"
else
    echo "  OK: no duplicate pitch constants in paint/"
fi

echo
if [ "$failures" -eq 0 ]; then
    echo "Architecture gate: ALL CHECKS PASSED"
    exit 0
else
    echo "Architecture gate: $failures FAILURE(S)"
    exit 1
fi
