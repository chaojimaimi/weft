#!/usr/bin/env bash
# Validate a v1.9 release candidate. Local mode validates reproducible bundle
# and package structure; --distribution additionally requires Developer ID,
# notarization, Gatekeeper acceptance, and a pinned Homebrew Cask checksum.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

mode="local"
if [[ "${1:-}" == "--distribution" ]]; then
    mode="distribution"
elif [[ $# -ne 0 ]]; then
    echo "Usage: $0 [--distribution]" >&2
    exit 2
fi

version=$(awk '
    /^\[workspace\.package\]$/ { in_section=1; next }
    /^\[/ && in_section { exit }
    in_section && /^version[[:space:]]*=/ {
        gsub(/.*=[[:space:]]*"|"[[:space:]]*$/, ""); print; exit
    }
' Cargo.toml)
app="target/release/osx/Weft.app"
plist="$app/Contents/Info.plist"
zip="target/release/osx/Weft-v${version}.zip"
dmg="target/release/osx/Weft-${version}.dmg"
cask="Casks/weft.rb"
failures=0

pass() { echo "  PASS: $*"; }
fail() { echo "  FAIL: $*"; failures=$((failures + 1)); }

equal() {
    local label=$1 expected=$2 actual=$3
    if [[ "$actual" == "$expected" ]]; then
        pass "$label = $actual"
    else
        fail "$label is '$actual' (expected '$expected')"
    fi
}

echo "==> v1.9 release acceptance ($mode)"
[[ "$version" == 1.9.* ]] && pass "workspace version $version" || fail "workspace version is not v1.9.x: $version"

bundle_version=$(awk '
    /^\[package\.metadata\.bundle\]$/ { in_section=1; next }
    /^\[/ && in_section { exit }
    in_section && /^version[[:space:]]*=/ {
        gsub(/.*=[[:space:]]*"|"[[:space:]]*$/, ""); print; exit
    }
' crates/weft_app/Cargo.toml)
cask_version=$(awk -F '"' '/^  version / { print $2; exit }' "$cask")
equal "bundle metadata version" "$version" "$bundle_version"
equal "Cask version" "$version" "$cask_version"

echo "==> App bundle"
if [[ ! -x "$app/Contents/MacOS/weft" || ! -f "$plist" ]]; then
    fail "$app is missing or incomplete"
else
    plutil -lint "$plist" >/dev/null && pass "Info.plist syntax" || fail "Info.plist syntax"
    equal "CFBundleShortVersionString" "$version" \
        "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$plist")"
    equal "CFBundleVersion" "$version" \
        "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleVersion' "$plist")"
    equal "bundle identifier" "dev.weft.terminal" \
        "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$plist")"
    file "$app/Contents/MacOS/weft" | grep -q 'arm64' \
        && pass "release executable is arm64" \
        || fail "release executable is not arm64"
fi

echo "==> ZIP and DMG"
if [[ -f "$zip" ]]; then
    unzip -tq "$zip" >/dev/null && pass "ZIP integrity" || fail "ZIP integrity"
else
    fail "$zip is missing"
fi
if [[ -f "$dmg" ]]; then
    hdiutil verify "$dmg" >/dev/null && pass "DMG integrity" || fail "DMG integrity"
else
    fail "$dmg is missing"
fi

if [[ "$mode" == "distribution" ]]; then
    echo "==> Distribution trust chain"
    if codesign --verify --deep --strict --verbose=2 "$app"; then
        pass "Developer ID code signature structure"
    else
        fail "Developer ID code signature structure"
    fi
    codesign -dv --verbose=4 "$app" 2>&1 | grep -q '^Authority=Developer ID Application:' \
        && pass "Developer ID authority" \
        || fail "Developer ID authority"
    xcrun stapler validate "$app" >/dev/null 2>&1 \
        && pass "notarization ticket" \
        || fail "notarization ticket"
    spctl --assess --type execute --verbose=2 "$app" >/dev/null 2>&1 \
        && pass "Gatekeeper assessment" \
        || fail "Gatekeeper assessment"

    pinned_sha=$(awk -F '"' '/^  sha256 / { print $2; exit }' "$cask")
    if [[ -f "$zip" ]]; then
        actual_sha=$(shasum -a 256 "$zip" | awk '{print $1}')
        equal "Cask signed ZIP sha256" "$actual_sha" "$pinned_sha"
    fi
else
    echo "==> Distribution trust chain"
    if security find-identity -v -p codesigning | grep -q 'Developer ID Application'; then
        pass "Developer ID identity available for distribution run"
    else
        echo "  BLOCKED: no Developer ID Application identity; run --distribution in signed CI"
    fi
    grep -q 'sha256 :no_check' "$cask" \
        && echo "  BLOCKED: Cask checksum awaits the final signed ZIP" \
        || pass "Cask checksum is pinned"
fi

echo
if [[ $failures -ne 0 ]]; then
    echo "v1.9 release acceptance: $failures FAILURE(S)"
    exit 1
fi
echo "v1.9 release acceptance: LOCAL/STRUCTURAL CHECKS PASSED"
if [[ "$mode" == "local" ]]; then
    echo "Distribution trust checks remain mandatory in signed CI."
fi
