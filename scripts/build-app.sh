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
VERSION="1.3.0"
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
    echo "==> Skipping code signing (pass --sign \"Developer ID: ...\" to enable)"
fi

echo "==> Done: ${APP_DIR}"
echo "    Open with: open ${APP_DIR}"
