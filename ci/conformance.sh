#!/usr/bin/env bash
# Usage: ci/conformance.sh <path-to-foundation>
# Runs this repository's suite (the `test` job of .github/workflows/ci.yml) against the foundation
# checkout at <path>, in place of the commit Cargo.toml pins, through a [patch] in a temporary
# .cargo/config.toml. Foundation's CI calls it for each foundation PR. The suite still runs the
# vendored feature files and test helpers (vendor/foundation/), which follow the pin, not <path>.
# Cargo.lock is restored on exit; the build goes to target/conformance unless CARGO_TARGET_DIR is set.
set -euo pipefail
foundation=$(cd "${1:?usage: ci/conformance.sh <path-to-foundation>}" && pwd -P)
cd "$(dirname "$0")/.."
[ -f "$foundation/crates/ironflowx-core/Cargo.toml" ] || {
  echo "conformance: $foundation is not a foundation checkout"
  exit 2
}
if [ -e .cargo/config.toml ]; then
  echo "conformance: .cargo/config.toml exists; move it aside first, as this run writes its own"
  exit 2
fi
lock=$(mktemp)
cp Cargo.lock "$lock"
mkdir -p .cargo
trap 'rm -f .cargo/config.toml; rmdir .cargo 2>/dev/null || true; mv "$lock" Cargo.lock' EXIT
{
  echo '[patch."https://github.com/ironflowx/foundation"]'
  # Every foundation crate Cargo.lock names, so none is left at the pin.
  sed -n 's/^name = "\(ironflowx[a-z-]*\)"$/\1/p' Cargo.lock | sort -u | while read -r crate; do
    [ -d "crates/$crate" ] || echo "$crate = { path = \"$foundation/crates/$crate\" }"
  done
} >.cargo/config.toml
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$PWD/target/conformance}
# Resolve once with the patch, so the suite's --locked calls take the patched graph.
cargo metadata --format-version 1 >/dev/null
cargo test --workspace --locked
ci/serve-lifecycle.sh
ci/server-crash.sh
cargo test -p ironflowx-cli --locked --features serde_json/arbitrary_precision
echo "conformance: the suite passes against $foundation"
