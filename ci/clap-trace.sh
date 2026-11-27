#!/usr/bin/env bash
# Usage: ci/clap-trace.sh <ironflowx>
# TEN-3: <ironflowx>, built with clap's `debug` feature, which prints each argument as it is parsed,
# refuses to start: one envelope on stderr, nothing on stdout, exit 2. A build that reads the
# arguments first prints the stray one, or says it is unexpected.
set -euo pipefail
out=$(mktemp)
err=$(mktemp)
trap 'rm -f "$out" "$err"' EXIT
code=0
"$1" whoami a-stray-argument >"$out" 2>"$err" || code=$?
envelope='{"code":"invalid","message":"clap is built with its debug feature","retryable":false}'
if [ "$code" -ne 2 ] || [ -s "$out" ] || ! printf '%s\n' "$envelope" | cmp -s - "$err"; then
  cat "$out" "$err"
  echo "clap-trace: $1 exits $code, with the above and not its one envelope"
  exit 1
fi
