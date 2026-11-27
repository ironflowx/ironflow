#!/usr/bin/env bash
# Usage: ci/release-checksum.sh <archive> (checks the adjacent .sha256 file).
set -euo pipefail
cd "$(dirname "${1:?archive required}")"
archive=$(basename "$1")
if ! shasum -a 256 "$archive" | cmp - "$archive.sha256"; then
  echo "archive SHA-256 checksum does not match: $archive" >&2
  exit 1
fi
echo "$archive: SHA-256 matches"
