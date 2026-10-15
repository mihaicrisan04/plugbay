#!/usr/bin/env bash
# Renders packaging/homebrew/plugbay.rb into a real formula on stdout.
#
#   render.sh <version> <SHA256SUMS> [download-base-url]
#
# SHA256SUMS holds "<sha256>  plugbay-v<version>-<target>.tar.gz" lines, as published
# with each release. The optional base URL replaces the GitHub release download URL,
# e.g. file:///tmp/plugbay-dist for testing tarballs before they are released.
set -euo pipefail

if [ $# -lt 2 ]; then
  echo "usage: $0 <version> <SHA256SUMS> [download-base-url]" >&2
  exit 2
fi

version=$1
sums=$2
base_url=${3:-}
template="$(dirname "$0")/plugbay.rb"
targets=(aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu)

sha_for() {
  local archive="plugbay-v$version-$1.tar.gz"
  local sha
  sha=$(awk -v archive="$archive" '$2 == archive { print $1 }' "$sums")
  if [ -z "$sha" ]; then
    echo "no checksum for $archive in $sums" >&2
    exit 1
  fi
  echo "$sha"
}

substitutions=(-e "s|@VERSION@|$version|g")
for target in "${targets[@]}"; do
  placeholder="@SHA256_$(echo "$target" | tr '[:lower:]-' '[:upper:]_')@"
  substitutions+=(-e "s|$placeholder|$(sha_for "$target")|")
done
if [ -n "$base_url" ]; then
  substitutions+=(-e "s|https://github.com/mihaicrisan04/plugbay/releases/download/v$version|$base_url|")
fi

# The leading comment block only describes the template.
sed "${substitutions[@]}" "$template" | sed '1,/^class /{/^#/d;}'
