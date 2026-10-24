#!/bin/sh
# Print the Homebrew formula for a release, using its SHA256SUMS.
#
# Usage: scripts/homebrew-formula.sh v0.1.0 dist/SHA256SUMS > Formula/entropeek.rb
#
# The release workflow runs this and publishes the result to the
# vorhey/homebrew-tap repository, so `brew install vorhey/tap/entropeek` works.

set -eu

[ $# -eq 2 ] || { echo "usage: $0 <tag> <SHA256SUMS>" >&2; exit 2; }
tag="$1"
sums="$2"
printf '%s' "$tag" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$' || { echo "invalid tag: $tag" >&2; exit 2; }
version="${tag#v}"
base="https://github.com/vorhey/entropeek/releases/download/${tag}"

sha() {
    file="entropeek_${tag}_$1.tar.gz"
    value=$(sed -n "s/^\([0-9a-f]\{64\}\)[[:space:]]\{1,\}\*\{0,1\}${file}\$/\1/p" "$sums" | head -n 1)
    [ -n "$value" ] || { echo "no checksum for ${file} in ${sums}" >&2; exit 1; }
    printf '%s' "$value"
}

darwin_arm64=$(sha darwin_arm64)
darwin_amd64=$(sha darwin_amd64)
linux_arm64=$(sha linux_arm64)
linux_amd64=$(sha linux_amd64)

cat <<EOF
class Entropeek < Formula
  desc "Fast terminal UI to browse, preview, resume and delete coding-agent sessions"
  homepage "https://github.com/vorhey/entropeek"
  version "${version}"
  license "MIT"

  on_macos do
    on_arm do
      url "${base}/entropeek_${tag}_darwin_arm64.tar.gz"
      sha256 "${darwin_arm64}"
    end
    on_intel do
      url "${base}/entropeek_${tag}_darwin_amd64.tar.gz"
      sha256 "${darwin_amd64}"
    end
  end

  on_linux do
    on_arm do
      url "${base}/entropeek_${tag}_linux_arm64.tar.gz"
      sha256 "${linux_arm64}"
    end
    on_intel do
      url "${base}/entropeek_${tag}_linux_amd64.tar.gz"
      sha256 "${linux_amd64}"
    end
  end

  def install
    bin.install "entropeek"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/entropeek --version")
  end
end
EOF
