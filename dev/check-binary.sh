#!/usr/bin/env bash
# Check that a binary is statically linked and no larger than 5 MiB.
set -euo pipefail

bin="${1:?usage: dev/check-binary.sh <binary>}"
limit=5242880

kind="$(file -b "$bin")"
case "$kind" in
  *"statically linked"* | *"static-pie linked"*) ;;
  *) echo "FAIL not static: $kind" >&2; exit 1 ;;
esac

size="$(stat -c %s "$bin")"
if [ "$size" -gt "$limit" ]; then
  echo "FAIL too large: $size bytes (limit $limit)" >&2
  exit 1
fi

echo "OK static $size bytes"
