#!/usr/bin/env bash
# Usage: ci/vendored.sh
# vendor/foundation/ holds foundation's files that this repository runs but Cargo does not hand it:
# events.feature and tenancy.feature, and the test helpers the binary's tests include. Each must
# match, byte for byte, the file at the same path in foundation at the commit Cargo.lock pins; a
# feature file's comment lines aside, since a copy may leave some out.
set -euo pipefail
cd "$(dirname "$0")/.."
pin='^source = "git+https://github.com/ironflowx/foundation?rev=[0-9a-f]*#\([0-9a-f]*\)"$'
rev=$(sed -n "s|$pin|\1|p" Cargo.lock | sort -u)
if [ -z "$rev" ] || [ "$(wc -l <<<"$rev")" -ne 1 ]; then
  echo "vendored: Cargo.lock pins foundation at '${rev//$'\n'/ }', not at one commit"
  exit 1
fi
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
# A feature file less its comment lines; any other file as it is.
body() {
  case $1 in
    *.feature) grep -v '^[[:space:]]*#' "$2" || true ;;
    *) cat "$2" ;;
  esac
}
status=0
while IFS= read -r file; do
  path=${file#vendor/foundation/}
  if ! curl -fsSL -o "$work/upstream" \
    "https://raw.githubusercontent.com/ironflowx/foundation/$rev/$path"; then
    echo "vendored: $path is not in foundation at $rev"
    status=1
  elif ! cmp -s <(body "$path" "$work/upstream") <(body "$path" "$file"); then
    diff -u <(body "$path" "$work/upstream") <(body "$path" "$file") || true
    echo "vendored: $file differs from foundation's at $rev"
    status=1
  fi
done < <(find vendor/foundation -type f | sort)
[ "$status" -ne 0 ] || echo "vendored: every file matches foundation at $rev"
exit $status
