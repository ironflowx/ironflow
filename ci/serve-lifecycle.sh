#!/usr/bin/env bash
# SRV-15: run the fixture outside Cargo's unwind-only test harness, in both shipped profiles.
set -euo pipefail
cd "$(dirname "$0")/.."
ulimit -c 0
for profile in dev release; do
  cargo run --locked --profile "$profile" -p ironflowx-bin --features __fault-points \
    --example serve_lifecycle
done
ci/must-reject.sh "serve fixture must use the abort panic strategy" -- \
  cargo run --locked --config 'profile.dev.panic="unwind"' -p ironflowx-bin \
    --features __fault-points --example serve_lifecycle
