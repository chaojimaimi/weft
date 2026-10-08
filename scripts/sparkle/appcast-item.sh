#!/bin/bash
# scripts/sparkle/appcast-item.sh
#
# v1.13.0 (PLAN_v1.13.0_SPARKLE WP5): emit one Sparkle 2 <item> XML block for
# a release DMG. Used by .github/workflows/sparkle-release.yml and by
# scripts/sparkle/build-update.sh (local rehearsal).
#
# Usage: appcast-item.sh <Weft-x.y.z.dmg> <version> <sign_update-output> [url]
#   - The third argument is sign_update's raw stdout line:
#       sparkle:edSignature="..." sparkle:length="..."
#     (parsed here, emitted as the enclosure's attributes — Sparkle 2 EdDSA
#     signatures live on the <enclosure>, NOT in a separate element).
#   - The 4th argument overrides the download URL base (CI/ Pages default:
#     https://chaojimaimi.github.io/weft/).
#
# Spike 踩坑 1 (docs/SPIKE_V1.13.0_SPARKLE_BRIDGE.md §四): the item MUST carry
# `sparkle:version` AND `sparkle:shortVersionString` — a missing attribute
# leaves the client stuck on "Checking for updates…" forever.
# All interpolated values are XML-escaped (& < >) before use; the emitted
# fragment is xmllint-validated before it reaches stdout (fail-fast: a
# malformed item must never reach the feed).

set -e

if [[ $# -lt 3 ]]; then
    echo "usage: $0 <dmg-path> <version> <sign_update-output> [url-base]" >&2
    exit 1
fi

DMG_PATH="$1"
VERSION="$2"
SIGN_UPDATE_OUT="$3"
URL_BASE="${4:-https://chaojimaimi.github.io/weft}"

if [[ ! -f "${DMG_PATH}" ]]; then
    echo "FATAL: DMG not found: ${DMG_PATH}" >&2
    exit 1
fi
# Peel the two quoted attribute values out of sign_update's output.
# (sign_update 2.10 emits `sparkle:edSignature="…" length="…"` — the length
# attribute carries no sparkle: prefix; both spellings parse identically
# because the prefix is consumed by the leading .*.)
ED_SIGNATURE="$(sed -n 's/.*edSignature="\([^"]*\)".*/\1/p' <<< "${SIGN_UPDATE_OUT}")"
LENGTH="$(sed -n 's/.*length="\([^"]*\)".*/\1/p' <<< "${SIGN_UPDATE_OUT}")"
if [[ -z "${ED_SIGNATURE}" || -z "${LENGTH}" ]]; then
    echo "FATAL: could not parse sparkle:edSignature/sparkle:length from: ${SIGN_UPDATE_OUT}" >&2
    exit 1
fi

DMG_NAME="$(basename "${DMG_PATH}")"
DOWNLOAD_URL="${URL_BASE}/${DMG_NAME}"
# Short version = the same release version (CFBundleShortVersionString);
# sparkle:version (CFBundleVersion) feeds Sparkle's comparison.
SHORT_VERSION="${VERSION}"

# ── XML escaping (attributes + text): & first, then < > ────────────────
escape_xml() {
    local s="$1"
    s="${s//&/&amp;}"
    s="${s//</&lt;}"
    s="${s//>/&gt;}"
    printf '%s' "${s}"
}
DMG_NAME_ESC="$(escape_xml "${DMG_NAME}")"
ED_SIGNATURE_ESC="$(escape_xml "${ED_SIGNATURE}")"
DOWNLOAD_URL_ESC="$(escape_xml "${DOWNLOAD_URL}")"
VERSION_ESC="$(escape_xml "${VERSION}")"
SHORT_VERSION_ESC="$(escape_xml "${SHORT_VERSION}")"

ITEM_FILE="$(mktemp)"
trap 'rm -f "${ITEM_FILE}"' EXIT
cat > "${ITEM_FILE}" <<ITEM_EOF
        <item>
            <title>Weft ${VERSION_ESC}</title>
            <sparkle:version>${VERSION_ESC}</sparkle:version>
            <sparkle:shortVersionString>${SHORT_VERSION_ESC}</sparkle:shortVersionString>
            <link>${DOWNLOAD_URL_ESC}</link>
            <enclosure url="${DOWNLOAD_URL_ESC}" length="${LENGTH}" type="application/x-bzip2-diskimage" sparkle:edSignature="${ED_SIGNATURE_ESC}" />
            <description><![CDATA[
                <h2>Weft ${VERSION_ESC}</h2>
                <p>See the <a href="https://github.com/chaojimaimi/weft/releases/tag/v${VERSION_ESC}">release notes</a>.</p>
            ]]></description>
        </item>
ITEM_EOF
if ! grep -q "sparkle:version" "${ITEM_FILE}"; then
    echo "FATAL: generated item lost sparkle:version (generator bug)" >&2
    exit 1
fi
if ! grep -q "sparkle:edSignature" "${ITEM_FILE}"; then
    echo "FATAL: generated item lost sparkle:edSignature (generator bug)" >&2
    exit 1
fi
# xmllint gate: wrap the fragment in a document that declares the sparkle
# namespace, exactly like the real appcast does.
if ! xmllint --noout <(cat <<VALIDATE_EOF
<?xml version="1.0" standalone="yes"?>
<rss xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle" xmlns:dc="http://purl.org/dc/elements/1.1/" version="2.0">
    <channel>
$(cat "${ITEM_FILE}")
    </channel>
</rss>
VALIDATE_EOF
); then
    echo "FATAL: generated appcast item is not well-formed XML" >&2
    exit 1
fi

cat "${ITEM_FILE}"
