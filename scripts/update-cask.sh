#!/usr/bin/env bash
# Update the in-repository Homebrew Cask from the final signed release ZIP.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CASK="${WEFT_CASK_PATH:-$ROOT/Casks/weft.rb}"

if [[ $# -ne 2 ]]; then
    echo "Usage: $0 <version> <signed-release-zip>" >&2
    exit 2
fi

version="${1#v}"
artifact="$2"
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "Error: version must be semantic x.y.z, got '$1'" >&2
    exit 2
fi
if [[ ! -f "$artifact" ]]; then
    echo "Error: release ZIP not found: $artifact" >&2
    exit 2
fi

sha=$(shasum -a 256 "$artifact" | awk '{print $1}')
CASK_VERSION="$version" CASK_SHA="$sha" ruby -i -pe '
  if $_ =~ /^  version /
    $_ = "  version \"#{ENV.fetch("CASK_VERSION")}\"\n"
  elsif $_ =~ /^  sha256 /
    $_ = "  sha256 \"#{ENV.fetch("CASK_SHA")}\"\n"
  end
' "$CASK"

echo "Updated $CASK"
echo "  version: $version"
echo "  sha256:  $sha"
