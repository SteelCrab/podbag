#!/usr/bin/env bash
# Run the end-to-end tests of the image store (nginx has a run of its own: nginx.sh).
#
#   tests/e2e/run.sh                     rootless, podbag from PATH
#   PODBAG_E2E_ROOT=1 tests/e2e/run.sh   as root
#   PODBAG=/path/to/podbag tests/e2e/run.sh
set -euo pipefail
dir="$(cd "$(dirname "$0")" && pwd)"
for t in files repair; do
  echo "== $t"
  "$dir/$t.sh"
done
