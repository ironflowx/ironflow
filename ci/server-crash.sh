#!/usr/bin/env bash
# The fault-enabled host is built once in a target directory reserved for this fixture.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo test --locked --target-dir "${CARGO_TARGET_DIR:-target}/server-crash" \
  -p ironflowx-bin --features __fault-points --test server_crash "$@"
