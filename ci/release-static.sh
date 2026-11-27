#!/usr/bin/env bash
# Usage: ci/release-static.sh <Linux release binary>
set -euo pipefail
elf=$(readelf -lW -dW "${1:?binary required}")
if grep -E '^[[:space:]]*INTERP[[:space:]]|\(NEEDED\)' <<<"$elf" >&2; then
  echo "archive's Linux binary needs a dynamic loader or library" >&2
  exit 1
fi
