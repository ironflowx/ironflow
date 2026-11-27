#!/usr/bin/env bash
# Proves each ELF check on the native Linux archive runners without executing the fixtures.
set -euo pipefail
cd "$(dirname "$0")/.."
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
printf 'void _start(void) { for (;;) {} }\n' >"$work/start.c"
cc -nostdlib -static "$work/start.c" -o "$work/static"
cc -nostdlib -fPIE -pie "$work/start.c" -o "$work/interpreter"
printf '#include <stdio.h>\nvoid fixture(void) { puts("fixture"); }\n' >"$work/library.c"
cc -fPIC -shared "$work/library.c" -o "$work/library"

ci/release-static.sh "$work/static"
reason="archive's Linux binary needs a dynamic loader or library"
ci/must-reject.sh 'INTERP' "$reason" -- ci/release-static.sh "$work/interpreter"
ci/must-reject.sh '(NEEDED)' "$reason" -- ci/release-static.sh "$work/library"
