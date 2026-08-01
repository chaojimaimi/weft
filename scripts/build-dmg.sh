#!/bin/bash
# scripts/build-dmg.sh
#
# v1.2: Build a distributable Weft.dmg disk image with drag-to-Applications
# layout. Runs scripts/build-app.sh first to ensure Weft.app is up to date.
#
# Usage: ./scripts/build-dmg.sh
#        ./scripts/build-dmg.sh --sign "Developer ID: ..."  (passed to build-app.sh)
#
# Produces: target/release/osx/Weft-<version>.dmg

set -e

cd "$(dirname "$0")/.."

APP_NAME="Weft"
# v1.8.3: 默认从 workspace Cargo.toml 读取版本号，保持与 bundle 一致。
# 仍允许通过 DMG_VERSION 环境变量覆盖（CI 手动触发不同 tag 时使用）。
if [[ -z "${DMG_VERSION:-}" ]]; then
    VERSION="$(grep -m1 '^version = ' Cargo.toml | sed 's/^version = "\(.*\)"$/\1/')"
    if [[ -z "${VERSION}" ]]; then
        VERSION="1.8.10"
    fi
else
    VERSION="${DMG_VERSION}"
fi
RELEASE_DIR="target/release"
OSX_DIR="${RELEASE_DIR}/osx"
APP_DIR="${OSX_DIR}/${APP_NAME}.app"
DMG_PATH="${OSX_DIR}/${APP_NAME}-${VERSION}.dmg"

# Forward --sign to build-app.sh if provided.
BUILD_ARGS=()
if [[ "$1" == "--sign" && -n "$2" ]]; then
    BUILD_ARGS=("$1" "$2")
    shift 2
fi

# Rebuild only if .app is missing (lets CI package an already-signed .app).
if [[ ! -d "${APP_DIR}" ]]; then
    echo "==> Building Weft.app"
    ./scripts/build-app.sh "${BUILD_ARGS[@]}"
else
    echo "==> Using existing ${APP_DIR}"
fi

echo "==> Preparing DMG staging directory"
STAGING_DIR="${OSX_DIR}/dmg-staging"
rm -rf "${STAGING_DIR}"
mkdir -p "${STAGING_DIR}"

# Copy the app bundle.
cp -R "${APP_DIR}" "${STAGING_DIR}/"

# Create a symlink to /Applications for drag-to-install.
ln -s /Applications "${STAGING_DIR}/Applications"

echo "==> Creating ${DMG_PATH}"
# Remove any previous DMG at this path.
rm -f "${DMG_PATH}"

# Create a read-only UDZO-compressed DMG.
hdiutil create \
    -volname "${APP_NAME}" \
    -srcfolder "${STAGING_DIR}" \
    -ov \
    -format UDZO \
    "${DMG_PATH}"

# Clean up staging.
rm -rf "${STAGING_DIR}"

echo "==> DMG created: ${DMG_PATH}"
echo "    Mount with: open ${DMG_PATH}"
