#!/usr/bin/env bash
# Run the library tests that need real root (#[ignore], names ending in _as_root) as root.
# The test binary is the one cargo builds for the test user, so the target directory stays
# owned by the test user. Runs inside the VM (`dev/vm.sh root-lib [filter]`).
#
# usage: dev/root-lib-tests.sh [filter]      (default filter: _as_root)
set -euo pipefail
exe="$(cargo test --lib --no-run --message-format=json 2>/dev/null \
  | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | tail -1)"
if [ -z "$exe" ]; then
  echo "cannot find the library test binary (does \`cargo test --lib --no-run\` build?)" >&2
  exit 1
fi
sudo -n "$exe" --ignored --test-threads=1 "${1:-_as_root}"
