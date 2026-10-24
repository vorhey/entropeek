#!/bin/sh
# Install the latest entropeek release from GitHub.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/vorhey/entropeek/main/scripts/install.sh | sh
#
# Environment:
#   ENTROPEEK_INSTALL_DIR  install directory (default: ~/.local/bin, falling
#                          back to /usr/local/bin with sudo)
#   ENTROPEEK_VERSION      release tag to install, e.g. v0.1.0 (default: latest)
#   ENTROPEEK_RELEASE_URL  where release files are downloaded from (default:
#                          GitHub releases; for testing and mirrors)

set -eu

REPO="vorhey/entropeek"
MAX_METADATA_BYTES=1048576
MAX_CHECKSUM_BYTES=1048576
MAX_ARCHIVE_BYTES=67108864

fail() {
    echo "install.sh: $1" >&2
    exit 1
}

command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v tar >/dev/null 2>&1 || fail "tar is required"

download() {
    curl -fL --retry 3 --retry-delay 1 --connect-timeout 10 --max-time 300 "$@"
}

case "$(uname -s)" in
    Linux) os="linux" ;;
    Darwin) os="darwin" ;;
    *) fail "unsupported OS: $(uname -s) (build from source: cargo install --git https://github.com/${REPO})" ;;
esac

case "$(uname -m)" in
    x86_64 | amd64) arch="amd64" ;;
    aarch64 | arm64) arch="arm64" ;;
    *) fail "unsupported architecture: $(uname -m) (build from source: cargo install --git https://github.com/${REPO})" ;;
esac

tag="${ENTROPEEK_VERSION:-}"
if [ -z "$tag" ]; then
    tag=$(download --max-filesize "$MAX_METADATA_BYTES" -sS "https://api.github.com/repos/${REPO}/releases/latest" |
        sed -n 's/^ *"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
fi
[ -n "$tag" ] || fail "could not determine the latest release tag"
printf '%s' "$tag" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$' || fail "invalid release tag: ${tag}"

archive="entropeek_${tag}_${os}_${arch}.tar.gz"
base_url="${ENTROPEEK_RELEASE_URL:-https://github.com/${REPO}/releases/download}/${tag}"

tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT INT TERM

echo "Downloading ${archive}..."
download --max-filesize "$MAX_ARCHIVE_BYTES" -sS -o "${tmpdir}/${archive}" "${base_url}/${archive}" ||
    fail "download failed: ${base_url}/${archive}"
download --max-filesize "$MAX_CHECKSUM_BYTES" -sS -o "${tmpdir}/SHA256SUMS" "${base_url}/SHA256SUMS" ||
    fail "download failed: ${base_url}/SHA256SUMS"

expected=$(sed -n "s/^\([0-9a-f]\{64\}\)[[:space:]]\{1,\}\*\{0,1\}${archive}\$/\1/p" "${tmpdir}/SHA256SUMS" | head -n 1)
[ -n "$expected" ] || fail "no SHA256SUMS entry for ${archive}"
if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "${tmpdir}/${archive}" | cut -d ' ' -f 1)
elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "${tmpdir}/${archive}" | cut -d ' ' -f 1)
else
    fail "sha256sum or shasum is required to verify the download"
fi
[ "$actual" = "$expected" ] || fail "checksum mismatch for ${archive}: expected ${expected}, got ${actual}"

tar -xzf "${tmpdir}/${archive}" -C "$tmpdir" entropeek || fail "the archive has no entropeek binary"

[ -n "${HOME:-}" ] || [ -n "${ENTROPEEK_INSTALL_DIR:-}" ] || fail "HOME is not set; set ENTROPEEK_INSTALL_DIR"
use_sudo=""
if [ -n "${ENTROPEEK_INSTALL_DIR:-}" ]; then
    install_dir="$ENTROPEEK_INSTALL_DIR"
    mkdir -p "$install_dir" 2>/dev/null || fail "cannot create ENTROPEEK_INSTALL_DIR: ${install_dir}"
    [ -w "$install_dir" ] || fail "ENTROPEEK_INSTALL_DIR is not writable: ${install_dir}"
else
    install_dir="${HOME}/.local/bin"
    if ! mkdir -p "$install_dir" 2>/dev/null || [ ! -w "$install_dir" ]; then
        install_dir="/usr/local/bin"
        if [ ! -w "$install_dir" ]; then
            command -v sudo >/dev/null 2>&1 ||
                fail "cannot write to ${install_dir} and sudo is unavailable; set ENTROPEEK_INSTALL_DIR"
            use_sudo="sudo"
            echo "Installing to ${install_dir} (requires sudo)..."
        fi
    fi
fi

$use_sudo install -m 0755 "${tmpdir}/entropeek" "${install_dir}/entropeek"

echo "Installed entropeek ${tag} to ${install_dir}/entropeek"
case ":${PATH}:" in
    *":${install_dir}:"*) echo "Run: entropeek" ;;
    *) echo "Note: ${install_dir} is not on your PATH; add it, then run: entropeek" ;;
esac
