#!/bin/bash
# scripts/sparkle/build-update.sh
#
# v1.13.0 (PLAN_v1.13.0_SPARKLE WP5): LOCAL update-feed rehearsal — replicates
# the sparkle-release.yml signature + appcast steps without GitHub (发版前
# 自测，等价 spike 手工流程; docs/SPIKE_V1.13.0_SPARKLE_BRIDGE.md §〇).
#
# Usage:
#   UPDATE_VERSION=1.13.1 ./scripts/sparkle/build-update.sh [--serve [port]]
#
#   - UPDATE_VERSION: the version the FEED advertises (must be GREATER than
#     the built app's CFBundleVersion for Sparkle to see an update; defaults
#     to the workspace Cargo.toml version, which yields "no update" — useful
#     for the "已是最新" feedback check).
#   - --serve [port]: after assembling the feed, serve it over HTTP
#     (default port 9988) so a locally built Weft.app can check against it.
#
# Outputs under target/release/osx/sparkle-feed/:
#   appcast.xml, notes.html, Weft-<UPDATE_VERSION>.dmg
#
# Keys: the EdDSA seed is read from WEFT_SPARKLE_KEY_FILE
# (default ~/.weft-sparkle/seed.b64, chmod 600 — NEVER commit it; see
# docs/RELEASE_SPARKLE.md).

set -e

cd "$(dirname "$0")/../.."

KEY_FILE="${WEFT_SPARKLE_KEY_FILE:-$HOME/.weft-sparkle/seed.b64}"
PORT="9988"
SERVE=0
if [[ "${1:-}" == "--serve" ]]; then
    SERVE=1
    PORT="${2:-9988}"
fi

if [[ ! -f "${KEY_FILE}" ]]; then
    echo "FATAL: signing key not found at ${KEY_FILE}" >&2
    echo "       (generate per docs/RELEASE_SPARKLE.md: openssl genpkey -algorithm ED25519 …)" >&2
    exit 1
fi

APP_VERSION="$(grep -m1 '^version = ' Cargo.toml | sed 's/^version = "\(.*\)"$/\1/')"
UPDATE_VERSION="${UPDATE_VERSION:-${APP_VERSION}}"
FEED_DIR="target/release/osx/sparkle-feed"
URL_BASE="${WEFT_FEED_URL_BASE:-http://127.0.0.1:${PORT}}"

echo "==> Building app + DMG (app stays ${APP_VERSION}; feed advertises ${UPDATE_VERSION})"
./scripts/build-app.sh
DMG_VERSION="${UPDATE_VERSION}" ./scripts/build-dmg.sh

DMG_PATH="target/release/osx/Weft-${UPDATE_VERSION}.dmg"
[[ -f "${DMG_PATH}" ]] || { echo "FATAL: DMG missing at ${DMG_PATH}" >&2; exit 1; }

echo "==> Signing DMG (EdDSA)"
# gitignored local extraction — a fresh clone won't have it (RELEASE_SPARKLE.md §四).
[[ -x "vendor/bin/sign_update" ]] || { echo "FATAL: vendor/bin/sign_update missing — extract vendor/Sparkle-2.10.0.tar.xz locally (see docs/RELEASE_SPARKLE.md)" >&2; exit 1; }
SIGN_OUT="$(vendor/bin/sign_update --ed-key-file "${KEY_FILE}" "${DMG_PATH}")"
echo "    ${SIGN_OUT}"

echo "==> Generating appcast item"
mkdir -p "${FEED_DIR}"
ITEM="$(./scripts/sparkle/appcast-item.sh "${DMG_PATH}" "${UPDATE_VERSION}" "${SIGN_OUT}" "${URL_BASE}")"

# ── Assemble appcast.xml: bootstrap when no feed exists, else replace the
# item for THIS version (idempotent re-runs never duplicate items). ──────
ITEM_FILE="$(mktemp)"
printf '%s\n' "${ITEM}" > "${ITEM_FILE}"
python3 - "${FEED_DIR}/appcast.xml" "${ITEM_FILE}" <<'PYEOF'
import os, re, sys

feed_path, item_path = sys.argv[1], sys.argv[2]
with open(item_path, encoding="utf-8") as f:
    item = f.read().rstrip("\n")
skeleton = '''<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle" xmlns:dc="http://purl.org/dc/elements/1.1/">
    <channel>
        <title>Weft</title>
        <link>https://chaojimaimi.github.io/weft/appcast.xml</link>
        <description>Most recent changes with links to updates.</description>
        <language>en</language>
{items}
    </channel>
</rss>
'''

if os.path.exists(feed_path):
    with open(feed_path, encoding="utf-8") as f:
        doc = f.read()
    # Replace any existing item advertising this version, else prepend.
    version = re.search(r"<sparkle:version>([^<]+)</sparkle:version>", item).group(1)
    item_re = re.compile(
        r"[ \t]*<item>(?:(?!</item>).)*<sparkle:version>"
        + re.escape(version)
        + r"</sparkle:version>(?:(?!</item>).)*</item>\n",
        re.S,
    )
    if item_re.search(doc):
        doc = item_re.sub(item + "\n", doc)
    else:
        doc = doc.replace("<channel>\n", "<channel>\n" + item + "\n", 1)
        doc = doc.replace("<channel>\r\n", "<channel>\r\n" + item + "\r\n", 1)
else:
    print("==> No existing feed — bootstrapping appcast.xml (first-run path)")
    doc = skeleton.format(items=item)

with open(feed_path, "w", encoding="utf-8") as f:
    f.write(doc)
PYEOF
rm -f "${ITEM_FILE}"

# Feed-side notes.html (Sparkle fetches <link>-adjacent release notes via the
# item description; the spike served notes.html as standalone evidence).
cat > "${FEED_DIR}/notes.html" <<HTML_EOF
<!DOCTYPE html>
<html><head><meta charset="utf-8"><title>Weft ${UPDATE_VERSION}</title></head>
<body><h1>Weft ${UPDATE_VERSION}</h1>
<p>Local rehearsal feed (built $(date '+%Y-%m-%d %H:%M:%S')). App version: ${APP_VERSION}.</p>
</body></html>
HTML_EOF

# xmllint the FINAL feed (not just the single item) — the assembly step is
# the last writer before the client sees it.
xmllint --noout "${FEED_DIR}/appcast.xml" || { echo "FATAL: assembled appcast.xml is invalid" >&2; exit 1; }

cp "${DMG_PATH}" "${FEED_DIR}/" 2>/dev/null || true

echo "==> Feed ready: ${FEED_DIR}/appcast.xml"
echo "    Launch the app with: WEFT_FEED_URL=${URL_BASE}/appcast.xml open target/release/osx/Weft.app"
if [[ "${SERVE}" -eq 1 ]]; then
    echo "==> Serving ${FEED_DIR} on http://127.0.0.1:${PORT}/ (Ctrl-C to stop)"
    cd "${FEED_DIR}"
    exec python3 -m http.server "${PORT}"
fi
