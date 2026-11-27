#!/usr/bin/env bash
# Usage: ci/verify-release-archive.sh <target> <version> <archive>
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
target=${1:?target required}
version=${2:?version required}
archive=${3:?archive required}
commit=$(git -C "$repo" rev-parse HEAD)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

"$repo/ci/release-checksum.sh" "$archive"
members=$(tar -tzf "$archive")
if [ "$members" != "$(printf '%s\n' ironflowx LICENSE)" ]; then
  echo "unexpected archive members: $members" >&2
  exit 1
fi
tar -xzf "$archive" -C "$work"
if ! cmp "$repo/LICENSE" "$work/LICENSE"; then
  echo "archive licence does not match LICENSE" >&2
  exit 1
fi
file "$work/ironflowx"
if [[ "$target" == *-linux-musl ]]; then
  "$repo/ci/release-static.sh" "$work/ironflowx"
fi
if ! "$work/ironflowx" version | jq -es --arg version "$version" --arg commit "$commit" \
  '. == [{version: $version, commit: $commit, protocol: "ironflowx.v1"}]' >/dev/null; then
  echo "archive binary did not report version $version, commit $commit and protocol ironflowx.v1" >&2
  exit 1
fi
python3 "$repo/ci/release-https.py" "$work/ironflowx" "$work"
