#!/usr/bin/env bash
# Run the netlink kernel tests as root in a throwaway network namespace (inside the VM).
set -euo pipefail
bin=$(cargo test --lib --no-run --message-format=json 2>/dev/null \
  | grep -o '"executable":"[^"]*"' | grep -v null | head -1 | cut -d'"' -f4)
sudo -n unshare --net "$bin" --ignored --test-threads=1 netlink::kernel_tests
