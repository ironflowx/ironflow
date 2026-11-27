#!/usr/bin/env bash
# Usage: ci/release-archive.sh <target> (after the native release build in release.yml).
set -euo pipefail
cd "$(dirname "$0")/.."

target=${1:?target triple required}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml)
archive=ironflowx-$version-$target.tar.gz
binary=${CARGO_TARGET_DIR:-target}/$target/release/ironflowx
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

mkdir -p dist
install -m 755 "$binary" "$work/ironflowx"
install -m 644 LICENSE "$work/LICENSE"
COPYFILE_DISABLE=1 tar -czf "dist/$archive" -C "$work" ironflowx LICENSE
(
  cd dist
  shasum -a 256 "$archive" >"$archive.sha256"
)
ci/verify-release-archive.sh "$target" "$version" "dist/$archive"
if [[ "$target" == aarch64-apple-darwin ]]; then
  ci/release-archive-fixtures.sh "$target" "$version" "dist/$archive"
fi
