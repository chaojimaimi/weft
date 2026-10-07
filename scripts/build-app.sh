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

# v1.13.0 (PLAN_v1.13.0_SPARKLE WP4): Sparkle auto-update keys. EVERY
# produced artifact must carry SUPublicEDKey — the spike §〇 iron law: a
# version whose inner .app lacks the key can NEVER be replaced by Sparkle
# (the "removal of EdDSA keys" policy rejects it with an unfriendly error).
# Fail fast instead of shipping an unupdatable build.
SU_PUBKEY_FILE="scripts/sparkle/ed_public_key.b64"
if [[ ! -f "${SU_PUBKEY_FILE}" ]]; then
    echo "FATAL: ${SU_PUBKEY_FILE} missing — the Sparkle public key is mandatory" >&2
    exit 1
fi
SU_PUBLIC_ED_KEY="$(tr -d '[:space:]' < "${SU_PUBKEY_FILE}")"
# Default feed = GitHub Pages appcast (plan D1); WEFT_FEED_URL is the
# single-point override for local rehearsal / feed relocation.
SU_FEED_URL="${WEFT_FEED_URL:-https://chaojimaimi.github.io/weft/appcast.xml}"

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
    <key>SUPublicEDKey</key>
    <string>${SU_PUBLIC_ED_KEY}</string>
    <key>SUFeedURL</key>
    <string>${SU_FEED_URL}</string>
    <key>SUEnableAutomaticChecks</key>
    <true/>
    <key>SUScheduledCheckInterval</key>
    <integer>86400</integer>
    <key>SUAutomaticallyUpdate</key>
    <false/>
</dict>
</plist>
PLIST

# PkgInfo (8-byte signature: APPL + 4 reserved bytes)
printf "APPL????" > "${CONTENTS_DIR}/PkgInfo"

# v1.13.0 (WP4): embed Sparkle.framework — the runtime NSBundle load in
# updater/mod.rs resolves it from Contents/Frameworks/. Commit-invariant:
# the framework ships with every artifact (same iron law as SUPublicEDKey).
if [[ ! -d "vendor/Sparkle.framework" ]]; then
    echo "FATAL: vendor/Sparkle.framework missing — the update feature cannot ship" >&2
    exit 1
fi
echo "==> Embedding Sparkle.framework"
mkdir -p "${CONTENTS_DIR}/Frameworks"
rm -rf "${CONTENTS_DIR}/Frameworks/Sparkle.framework"
cp -R vendor/Sparkle.framework "${CONTENTS_DIR}/Frameworks/"

# v1.13.0 (WP4): packaging self-check — fail fast on a missing/empty SU key
# (plutil reads the ACTUAL generated plist; set -e aborts on any mismatch).
for su_key in SUPublicEDKey SUFeedURL SUEnableAutomaticChecks SUScheduledCheckInterval SUAutomaticallyUpdate; do
    if ! plutil -extract "${su_key}" raw "${CONTENTS_DIR}/Info.plist" > /dev/null 2>&1; then
        echo "FATAL: Info.plist is missing the ${su_key} key" >&2
        exit 1
    fi
done
if [[ -z "$(plutil -extract SUPublicEDKey raw "${CONTENTS_DIR}/Info.plist")" ]]; then
    echo "FATAL: SUPublicEDKey is empty" >&2
    exit 1
fi

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
    # v1.13.0 (WP4): --deep is MANDATORY now — the embedded
    # Sparkle.framework carries nested code (Sparkle dylib + Updater.app +
    # XPCServices) that a plain ad-hoc sign leaves unsigned, and
    # codesign --verify --strict then fails on the nested bundles.
    echo "==> Ad-hoc DEEP signing bundle (UNUserNotificationCenter requires a signed bundle)"
    codesign --force --deep --sign - --identifier "${BUNDLE_ID}" "${APP_DIR}"
    codesign --verify --strict "${APP_DIR}"
fi

echo "==> Done: ${APP_DIR}"
echo "    Open with: open ${APP_DIR}"
