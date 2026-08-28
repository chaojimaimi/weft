#!/bin/bash
# scripts/build-app.sh
#
# v1.2: Build Weft.app bundle from release binary.
# Usage: ./scripts/build-app.sh [--sign "Developer ID: ..."]
#
# Produces: target/release/osx/Weft.app
# (Uses a custom .app layout instead of cargo-bundle for full control.)

set -e

cd "$(dirname "$0")/.."

APP_NAME="Weft"
BUNDLE_ID="dev.weft.terminal"
# v1.8.3: read version from workspace Cargo.toml so the bundle stays in sync
# without manual edits. Falls back to a hardcoded version if parsing fails.
VERSION="$(grep -m1 '^version = ' Cargo.toml | sed 's/^version = "\(.*\)"$/\1/')"
if [[ -z "${VERSION}" ]]; then
    VERSION="1.10.4"
fi
MIN_OS="12.0"

# Paths
RELEASE_DIR="target/release"
APP_DIR="${RELEASE_DIR}/osx/${APP_NAME}.app"
CONTENTS_DIR="${APP_DIR}/Contents"
MACOS_DIR="${CONTENTS_DIR}/MacOS"
RESOURCES_DIR="${CONTENTS_DIR}/Resources"

# Optional: signing identity passed via --sign "Developer ID: ..."
SIGN_IDENTITY=""
if [[ "$1" == "--sign" && -n "$2" ]]; then
    SIGN_IDENTITY="$2"
    shift 2
fi

echo "==> Building release binary"
cargo build --release

echo "==> Assembling ${APP_NAME}.app bundle"
rm -rf "${APP_DIR}"
mkdir -p "${MACOS_DIR}" "${RESOURCES_DIR}"

# Copy executable (binary name is `weft`, CFBundleExecutable is `weft`)
cp "${RELEASE_DIR}/weft" "${MACOS_DIR}/weft"
chmod +x "${MACOS_DIR}/weft"

# Copy icon
cp assets/icon.icns "${RESOURCES_DIR}/icon.icns"

# Generate Info.plist from template (keeps version in sync)
cat > "${CONTENTS_DIR}/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>${APP_NAME}</string>
    <key>CFBundleDisplayName</key>
    <string>${APP_NAME}</string>
    <key>CFBundleIdentifier</key>
    <string>${BUNDLE_ID}</string>
    <key>CFBundleVersion</key>
    <string>${VERSION}</string>
    <key>CFBundleShortVersionString</key>
    <string>${VERSION}</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleExecutable</key>
    <string>weft</string>
    <key>CFBundleIconFile</key>
    <string>icon.icns</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>LSMinimumSystemVersion</key>
    <string>${MIN_OS}</string>
    <key>NSAllowsArbitraryLoads</key>
    <false/>
    <key>NSSupportsAutomaticGraphicsSwitching</key>
    <true/>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.developer-tools</string>
    <key>NSPrincipalClass</key>
    <string>NSApplication</string>
</dict>
</plist>
PLIST

# PkgInfo (8-byte signature: APPL + 4 reserved bytes)
printf "APPL????" > "${CONTENTS_DIR}/PkgInfo"

echo "==> Bundle assembled at ${APP_DIR}"

# Optional code signing
if [[ -n "${SIGN_IDENTITY}" ]]; then
    echo "==> Signing with identity: ${SIGN_IDENTITY}"
    codesign --force --deep --options runtime \
        --identifier "${BUNDLE_ID}" \
        --sign "${SIGN_IDENTITY}" \
        --entitlements scripts/Weft.entitlements \
        "${APP_DIR}"
    # Verify
    codesign --verify --verbose=2 "${APP_DIR}"
else
    # v1.11.6 (PLAN_v1116 M1/D-h): ad-hoc signing. macOS 26 silently rejects
    # UNUserNotificationCenter requestAuthorization on unsigned bundles
    # (granted=false, no dialog — the v1.11.5 notification root cause), so an
    # unsigned .app can never show the permission prompt. Ad-hoc is the
    # minimum signature that satisfies the check; it is NOT notarized and
    # carries no hardened runtime (--options runtime / entitlements are
    # intentionally absent — no benefit without notarization).
    # Side effect: every repackaged DMG has a new cdhash, so macOS re-prompts
    # for notification permission on the first launch of each build.
    echo "==> Ad-hoc signing bundle (v1.11.6: UNUserNotificationCenter requires a signed bundle)"
    codesign --force --sign - --identifier "${BUNDLE_ID}" "${APP_DIR}"
    codesign --verify "${APP_DIR}"
fi

echo "==> Done: ${APP_DIR}"
echo "    Open with: open ${APP_DIR}"
