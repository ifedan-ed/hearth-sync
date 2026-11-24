#!/bin/bash
# tools/check-large-files.sh [--staged] - refuse any file over 5 MB.
#
# Why: two crash dumps of 13 MB each were once committed by accident. Source,
# docs and test data here are all small; a file over the limit is a mistake
# (a dump, a build product, a capture) until someone raises MAX_BYTES in
# this file, with the reason, in a commit of its own.
#
# Without arguments it checks every tracked file (CI). With --staged it
# checks the blobs about to be committed, not the working tree (the
# pre-commit hook, tools/hooks/pre-commit).
set -euo pipefail
MAX_BYTES=$((5 * 1024 * 1024))
cd "$(git rev-parse --show-toplevel)"
if [ "${1:-}" = --staged ]; then
  list() { git diff --cached --name-only --diff-filter=ACMR -z; }
  size_of() { git cat-file -s ":$1"; }
else
  list() { git ls-files -z; }
  size_of() { git cat-file -s "HEAD:$1" 2>/dev/null || stat -c %s "$1"; }
fi
bad=0
while IFS= read -r -d '' f; do
  size="$(size_of "$f")"
  if [ "$size" -gt "$MAX_BYTES" ]; then
    echo "too large ($size bytes): $f"
    bad=1
  fi
done < <(list)
[ "$bad" = 0 ] || { echo "files over $MAX_BYTES bytes are refused (tools/check-large-files.sh)" >&2; exit 1; }
