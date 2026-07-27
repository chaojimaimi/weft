#!/usr/bin/env bash
# Run every non-interactive v1.4 acceptance gate. GUI probes are opt-in because
# they take about a minute and require a logged-in macOS desktop session.
#
# Usage:
#   scripts/v14-automated-acceptance.sh
#   WEFT_V14_RUN_GUI=1 scripts/v14-automated-acceptance.sh

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

run() {
    echo
    echo "==> $*"
    "$@"
}

run cargo fmt --check
run cargo test --workspace
run cargo clippy --workspace --all-targets -- -D warnings
run scripts/architecture_gate.sh
run scripts/performance_gate.sh
run scripts/build-app.sh
run scripts/acceptance_preflight.sh
run scripts/build-dmg.sh
run hdiutil verify target/release/osx/Weft-1.4.3.dmg
run cargo test --release -p weft_app --bin weft bench_build_grid_instances -- \
    --nocapture --ignored --test-threads=1

if [[ "${WEFT_V14_RUN_GUI:-0}" == "1" ]]; then
    run scripts/v14-perf-probe.sh 30 docs/perf/v1.4/v1.4.3-idle.md
    run scripts/v14-baseline-with-load.sh 30 docs/perf/v1.4/v1.4.1-loaded-current.md
    run python3 -m unittest scripts/test_v14_cache_compare.py
    run python3 scripts/v14-cache-compare.py \
        docs/perf/v1.4/v1.4.0-loaded-baseline.md \
        docs/perf/v1.4/v1.4.1-loaded-current.md \
        docs/perf/v1.4/v1.4.1-loaded-comparison.md
    run scripts/v14-grid-load-probe.sh 30 docs/perf/v1.4/v1.4.3-grid-load.md
else
    echo
    echo "==> GUI probes skipped (set WEFT_V14_RUN_GUI=1 to enable)"
fi

echo
echo "v1.4 automated acceptance: ALL REQUESTED GATES PASSED"
