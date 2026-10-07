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
        VERSION="1.10.4"
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

# PLAN_zoom appendix I: stamp the build hash into the binary so the
# session-start log line can distinguish same-version rebuilds (the version
# number stays frozen at 1.12.15 until the zoom fix is user-confirmed).
# Exported so the child build-app.sh cargo build picks it up via option_env!.
export WEFT_BUILD_HASH="$(git rev-parse --short HEAD 2>/dev/null || echo dev)"

# Rebuild only if .app is missing (lets CI package an already-signed .app).
if [[ ! -d "${APP_DIR}" ]]; then
    echo "==> Building Weft.app"
    ./scripts/build-app.sh "${BUILD_ARGS[@]}"
else
    echo "==> Using existing ${APP_DIR}"
fi

# v1.11.6 (PLAN_v1116 M1/D-h): the .app must carry a signature before the
# DMG is assembled — ad-hoc or Developer ID. UNUserNotificationCenter is
# silently disabled on unsigned bundles (the v1.11.5 notification root
# cause), so this gate prevents packaging a stale unsigned .app again.
# --strict also rejects ad-hoc signatures that only cover part of the bundle.
codesign --verify --strict "${APP_DIR}"

# v1.13.0 (PLAN_v1.13.0_SPARKLE WP4): the DMG is the FINAL gate — the
# "reuse existing .app" bypass above could otherwise package a stale bundle
# without the Sparkle SU keys (EdDSA per-artifact iron law, spike §〇: a
# keyless inner .app can never be replaced by Sparkle later).
DMG_INFO_PLIST="${APP_DIR}/Contents/Info.plist"
for su_key in SUPublicEDKey SUFeedURL SUEnableAutomaticChecks SUScheduledCheckInterval SUAutomaticallyUpdate; do
    if ! plutil -extract "${su_key}" raw "${DMG_INFO_PLIST}" > /dev/null 2>&1; then
        echo "FATAL: ${APP_DIR} Info.plist is missing the ${su_key} key" >&2
        exit 1
    fi
done

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
