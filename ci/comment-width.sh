#!/usr/bin/env bash
# Usage: ci/comment-width.sh <path>...   (files or directories)
# Fails if a comment line in a Rust source or shell script under <path> runs past 100 columns,
# the width rustfmt gives code. rustfmt does not wrap comments, so nothing else catches one.
# A line holding a URL is exempt: a link cannot wrap. Columns are characters, not bytes, the same
# on every OS (awk's length differs between macOS and Linux), hence perl.
set -euo pipefail
# The perl program is single-quoted on purpose: its $ARGV is perl's, not the shell's.
# shellcheck disable=SC2016
found=$(find "$@" -type f \( -name '*.rs' -o -name '*.sh' \) -not -path '*/target/*' -print0 \
  | xargs -0 perl -CSD -ne '
      my $comment = $ARGV =~ /\.rs$/ ? qr{^\s*//} : qr{^\s*#};
      chomp;
      print "$ARGV:$.: ", length, " columns\n" if /$comment/ && length > 100 && !m{https?://};
      close ARGV if eof;
    ')
if [ -n "$found" ]; then
  echo "comment lines over 100 columns:"
  echo "$found"
  exit 1
fi
