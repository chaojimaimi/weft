#!/bin/bash
# scripts/notarize.sh
#
# v1.0 V10/V11: Notarize Weft.app for distribution outside the App Store.
#
# Prerequisites:
#   - Apple Developer ID Application certificate in keychain
#   - App-specific password stored in keychain as "AC_PASSWORD"
#     (xcrun notarytool store-credentials AC_PASSWORD \
#        --apple-id you@example.com --team-id TEAMID)
#
# Usage: ./scripts/notarize.sh

set -e

cd "$(dirname "$0")/.."

APP_NAME="Weft"
APP_DIR="target/release/osx/${APP_NAME}.app"
ZIP_PATH="target/release/osx/${APP_NAME}.zip"
BUNDLE_ID="dev.weft.terminal"

if [[ ! -d "${APP_DIR}" ]]; then
    echo "Error: ${APP_DIR} not found. Run scripts/build-app.sh first."
    exit 1
fi

# Discover Developer ID Application identity automatically.
SIGN_IDENTITY=$(security find-identity -v -p codesigning | \
    grep "Developer ID Application" | head -n 1 | \
    sed -E 's/.*"(.*)".*/\1/')

if [[ -z "${SIGN_IDENTITY}" ]]; then
    echo "Error: No 'Developer ID Application' certificate found in keychain."
    echo "       Enroll in Apple Developer Program, then run:"
    echo "       security import developerID_application.p12 -k ~/Library/Keychains/login.keychain-db"
    exit 1
fi

echo "==> Signing with: ${SIGN_IDENTITY}"
./scripts/build-app.sh --sign "${SIGN_IDENTITY}"

echo "==> Creating ZIP for notarization"
ditto -c -k --keepParent "${APP_DIR}" "${ZIP_PATH}"

echo "==> Submitting to Apple notary service"
xcrun notarytool submit "${ZIP_PATH}" \
    --keychain-profile "AC_PASSWORD" \
    --wait

echo "==> Stapling ticket"
xcrun stapler staple "${APP_DIR}"
xcrun stapler validate "${APP_DIR}"

echo "==> Notarization complete: ${APP_DIR}"
