#!/usr/bin/env bash
# Usage: ci/must-reject.sh <pattern>... -- <command...>
# Proves a check still works: the command must fail, and each pattern must be in exactly one line
# of its output, and no two in the same line, so a fixture that fails for an unrelated reason (a
# typo, a missing tool) does not count, a longer name one pattern is for does not stand in for
# another's, and one fixture can prove several bans in a single run. A longer name no pattern is for
# does: end a name's pattern with what follows it.
set -uo pipefail
patterns=()
while [ $# -gt 0 ] && [ "$1" != "--" ]; do
  patterns+=("$1")
  shift
done
if [ $# -lt 2 ] || [ ${#patterns[@]} -eq 0 ]; then
  echo "usage: ci/must-reject.sh <pattern>... -- <command...>"
  exit 2
fi
shift
output=$("$@" 2>&1)
code=$?
if [ "$code" -eq 0 ]; then
  echo "$output"
  echo "must-reject: '$*' passed, but it must reject its fixture"
  exit 1
fi
wrong=0
taken=" "
for pattern in "${patterns[@]}"; do
  lines=$(grep -nF -- "$pattern" <<<"$output" | cut -d: -f1)
  count=$(grep -c . <<<"$lines")
  if [ "$count" -ne 1 ]; then
    echo "must-reject: '$*' failed with '$pattern' in $count lines, not one"
    wrong=1
  elif [[ "$taken" == *" $lines "* ]]; then
    echo "must-reject: '$*' failed with '$pattern' only in another pattern's line"
    wrong=1
  fi
  taken+="$lines "
done
if [ "$wrong" -ne 0 ]; then
  echo "$output"
  exit 1
fi
echo "must-reject: '$*' rejected its fixture with all ${#patterns[@]} expected findings"
