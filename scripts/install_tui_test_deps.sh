#!/usr/bin/env bash
set -euo pipefail

TMUX_VERSION="3.7b"
TMUX_SHA256="87f2e99e3b685973f2ca002ffd6ed7e51a5744f7009daae5a15670b6d532db96"
TMUX_URL="https://github.com/tmux/tmux/releases/download/${TMUX_VERSION}/tmux-${TMUX_VERSION}.tar.gz"
TMUX_PREFIX="${WEFT_TMUX_PREFIX:-${RUNNER_TEMP:-${TMPDIR:-/tmp}}/weft-tmux-${TMUX_VERSION}}"

# v1.11.16: glyph tests assert Fira Code's "no true italic face" behavior
# (`fira_code_without_italic_variant_is_rejected` — Core Text would otherwise
# approximate Italic with Regular). GitHub runners don't ship the font, and the
# test now skips when it is absent, so install it here to keep the assertion
# live in CI. Runs before the tmux early-exit below.
install_fira_code() {
    if system_profiler SPFontsDataType 2>/dev/null | grep -qi "Fira Code"; then
        echo "Fira Code already installed"
        return 0
    fi
    echo "Installing Fira Code (glyph variant tests)"
    # Best-effort: the test skips if the font ends up missing, so a brew/cask
    # hiccup must not fail the whole dependency step.
    brew install --cask font-fira-code || \
        echo "warning: Fira Code install failed — glyph variant tests will skip"
}

install_fira_code

add_to_github_path() {
    if [[ -n "${GITHUB_PATH:-}" ]]; then
        echo "$1" >> "${GITHUB_PATH}"
    fi
}

if [[ "${WEFT_FORCE_TMUX_INSTALL:-0}" != "1" ]] && command -v tmux >/dev/null 2>&1 && [[ "$(tmux -V)" == "tmux ${TMUX_VERSION}" ]]; then
    tmux_bin="$(command -v tmux)"
    echo "tmux ${TMUX_VERSION} already available at ${tmux_bin}"
    add_to_github_path "$(dirname "${tmux_bin}")"
    exit 0
fi

if [[ -x "${TMUX_PREFIX}/bin/tmux" ]] && [[ "$("${TMUX_PREFIX}/bin/tmux" -V)" == "tmux ${TMUX_VERSION}" ]]; then
    echo "reusing tmux ${TMUX_VERSION} from ${TMUX_PREFIX}"
    add_to_github_path "${TMUX_PREFIX}/bin"
    exit 0
fi

brew install pkgconf libevent ncurses utf8proc

archive="$(mktemp "${TMPDIR:-/tmp}/weft-tmux.tar.gz.XXXXXX")"
build_dir="$(mktemp -d "${TMPDIR:-/tmp}/weft-tmux-build.XXXXXX")"
trap 'rm -f "${archive}"; rm -rf "${build_dir}"' EXIT

curl --fail --location --retry 3 --retry-all-errors --http1.1 --silent --show-error --output "${archive}" "${TMUX_URL}"
echo "${TMUX_SHA256}  ${archive}" | shasum -a 256 --check
tar -xzf "${archive}" -C "${build_dir}" --strip-components=1

export PKG_CONFIG_PATH="$(brew --prefix libevent)/lib/pkgconfig:$(brew --prefix ncurses)/lib/pkgconfig:$(brew --prefix utf8proc)/lib/pkgconfig"
(
    cd "${build_dir}"
    ./configure --prefix="${TMUX_PREFIX}" --enable-utf8proc
    make -j2
    make install
)

actual="$("${TMUX_PREFIX}/bin/tmux" -V)"
[[ "${actual}" == "tmux ${TMUX_VERSION}" ]] || {
    echo "expected tmux ${TMUX_VERSION}, got ${actual}" >&2
    exit 1
}
add_to_github_path "${TMUX_PREFIX}/bin"
echo "installed ${actual} at ${TMUX_PREFIX}"
