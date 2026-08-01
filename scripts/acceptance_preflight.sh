#!/usr/bin/env bash
# Verify the non-interactive part of the current release acceptance contract.
#
# This script intentionally does not claim that GUI, TUI, IME, DPI, or
# visual behavior passed. v1.7 checks live in V17_MANUAL_ACCEPTANCE.md.
# Run after ./scripts/build-app.sh.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

APP_DIR="target/release/osx/Weft.app"
PLIST="$APP_DIR/Contents/Info.plist"
V17_MANUAL_MATRIX="docs/V17_MANUAL_ACCEPTANCE.md"
V18_MANUAL_MATRIX="docs/V18_MANUAL_ACCEPTANCE.md"
V19_RELEASE_MATRIX="docs/V19_RELEASE_ACCEPTANCE.md"
failures=0

workspace_version=$(awk '
    /^\[workspace\.package\]$/ { in_section=1; next }
    /^\[/ && in_section { exit }
    in_section && /^version[[:space:]]*=/ {
        gsub(/.*=[[:space:]]*"|"[[:space:]]*$/, ""); print; exit
    }
' Cargo.toml)

bundle_metadata_version=$(awk '
    /^\[package\.metadata\.bundle\]$/ { in_section=1; next }
    /^\[/ && in_section { exit }
    in_section && /^version[[:space:]]*=/ {
        gsub(/.*=[[:space:]]*"|"[[:space:]]*$/, ""); print; exit
    }
' crates/weft_app/Cargo.toml)

resolved_script_version() {
    script=$1
    if grep -Fq "grep -m1 '^version = ' Cargo.toml" "$script"; then
        printf '%s\n' "$workspace_version"
    else
        awk -F '"' '/^VERSION=/ { print $2; exit }' "$script"
    fi
}

check_equal() {
    label=$1
    expected=$2
    actual=$3
    if [ "$actual" != "$expected" ]; then
        echo "  FAIL: $label is '$actual' (expected '$expected')"
        failures=$((failures + 1))
    else
        echo "  OK: $label = $actual"
    fi
}

echo "==> Acceptance preflight: repository version consistency"
if [ -z "$workspace_version" ]; then
    echo "  FAIL: workspace version could not be read from Cargo.toml"
    failures=$((failures + 1))
else
    check_equal "crates/weft_app bundle metadata" "$workspace_version" "$bundle_metadata_version"
    check_equal "scripts/build-app.sh" "$workspace_version" "$(resolved_script_version scripts/build-app.sh)"
    check_equal "scripts/build-dmg.sh" "$workspace_version" "$(resolved_script_version scripts/build-dmg.sh)"
fi

echo "==> Acceptance preflight: App bundle structure"
if [ ! -d "$APP_DIR" ]; then
    echo "  FAIL: $APP_DIR is missing; run ./scripts/build-app.sh first"
    failures=$((failures + 1))
elif [ ! -f "$PLIST" ]; then
    echo "  FAIL: $PLIST is missing"
    failures=$((failures + 1))
else
    if ! plutil -lint "$PLIST" >/dev/null; then
        echo "  FAIL: Info.plist is invalid"
        failures=$((failures + 1))
    else
        echo "  OK: Info.plist is valid"
    fi

    plist_version=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$PLIST")
    plist_build=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleVersion' "$PLIST")
    plist_id=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$PLIST")
    plist_executable=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$PLIST")

    check_equal "CFBundleShortVersionString" "$workspace_version" "$plist_version"
    check_equal "CFBundleVersion" "$workspace_version" "$plist_build"
    check_equal "CFBundleIdentifier" "dev.weft.terminal" "$plist_id"
    check_equal "CFBundleExecutable" "weft" "$plist_executable"

    if [ ! -x "$APP_DIR/Contents/MacOS/weft" ]; then
        echo "  FAIL: bundle executable is missing or not executable"
        failures=$((failures + 1))
    else
        echo "  OK: bundle executable is present"
    fi
    if [ ! -s "$APP_DIR/Contents/Resources/icon.icns" ]; then
        echo "  FAIL: bundle icon is missing or empty"
        failures=$((failures + 1))
    else
        echo "  OK: bundle icon is present"
    fi
    if [ ! -f "$APP_DIR/Contents/PkgInfo" ] || [ "$(cat "$APP_DIR/Contents/PkgInfo")" != 'APPL????' ]; then
        echo "  FAIL: PkgInfo is missing or invalid"
        failures=$((failures + 1))
    else
        echo "  OK: PkgInfo is valid"
    fi
fi

echo "==> Acceptance preflight: manual-gate contract"
for matrix in "$V17_MANUAL_MATRIX" "$V18_MANUAL_MATRIX" "$V19_RELEASE_MATRIX"; do
    if [ ! -f "$matrix" ]; then
        echo "  FAIL: $matrix is missing"
        failures=$((failures + 1))
    fi
done

if [ -f "$V17_MANUAL_MATRIX" ] && [ -f "$V18_MANUAL_MATRIX" ] && [ -f "$V19_RELEASE_MATRIX" ]; then
    for marker in V17-ANSI-1 V17-SEARCH-1 V17-COMPLETION-1 V17-RUNBOOK-1 V17-RELEASE-1; do
        if ! grep -Fq "**${marker}**" "$V17_MANUAL_MATRIX"; then
            echo "  FAIL: $V17_MANUAL_MATRIX is missing $marker"
            failures=$((failures + 1))
        fi
    done
    for marker in V18-SETTINGS-1 V18-COMMAND-2 V18-DIAGNOSE-2 V18-SAFETY-2 V18-PERF-1; do
        if ! grep -Fq "**${marker}**" "$V18_MANUAL_MATRIX"; then
            echo "  FAIL: $V18_MANUAL_MATRIX is missing $marker"
            failures=$((failures + 1))
        fi
    done
    for marker in V19-VERSION-1 V19-PACKAGE-1 V19-SIGN-1 V19-GUI-1; do
        if ! grep -Fq "**${marker}**" "$V19_RELEASE_MATRIX"; then
            echo "  FAIL: $V19_RELEASE_MATRIX is missing $marker"
            failures=$((failures + 1))
        fi
    done
    if [ "$failures" -eq 0 ]; then
        echo "  OK: v1.7-v1.9 acceptance matrices cover required release areas"
    fi
fi

echo
if [ "$failures" -eq 0 ]; then
    echo "Acceptance preflight: ALL AUTOMATED CHECKS PASSED"
    echo "Manual/distribution acceptance is still required; see v1.7-v1.9 matrices."
    exit 0
fi

echo "Acceptance preflight: $failures FAILURE(S)"
exit 1
