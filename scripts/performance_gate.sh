#!/usr/bin/env bash
# Stable, isolated wall-clock budgets for CPU-side terminal and UI logic.
# GPU frame timing and idle wake frequency remain in the macOS GUI/Metal gate.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

echo "==> Performance gate: VT parsing and 10k-scrollback resize"
cargo test -p weft_core --lib perf_ -- \
    --ignored --nocapture --test-threads=1

echo "==> Performance gate: 10k history filtering and virtualization"
cargo test -p weft_app perf_panel_10k_history_filter_and_virtualize -- \
    --ignored --nocapture --test-threads=1

echo
echo "Performance gate: ALL BUDGETS PASSED"
