#!/usr/bin/env bash
# Usage: ci/no-host-engine-deps.sh <Cargo.toml>
# A host reaches the control plane, ledger, event rules and credentials through the engine.
# Check this package's direct normal dependencies by their declared Cargo package names.
# Cargo metadata keeps those names for aliases and for optional or target-specific dependencies.
# A fork with a different package name escapes the check; this workspace uses the four names below.
set -euo pipefail
manifest=$(cd "$(dirname "$1")" && pwd -P)/$(basename "$1")
forbidden=$(cargo metadata --locked --no-deps --format-version 1 --manifest-path "$manifest" |
  jq -r --arg manifest "$manifest" '
    (first(.packages[] | select(.manifest_path == $manifest))
      // error("package not found in \($manifest)"))
    | [.dependencies[] | select(.kind == null) | .name
        | select(test("^ironflowx-(cell|store|events|authz)$"))]
    | unique[]
  ')
if [ -n "$forbidden" ]; then
  while IFS= read -r dep; do
    echo "$1: direct engine dependency: '$dep'"
  done <<<"$forbidden"
  exit 1
fi
