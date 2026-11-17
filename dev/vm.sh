#!/usr/bin/env bash
# Control the openpod-dev lima VM.
set -euo pipefail

name="openpod-dev"
repo="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"

# Run a command inside the VM, in the repository, with the test environment.
vm_run() {
  local cmd
  printf -v cmd '%q ' "$@"
  limactl shell --workdir "$repo" "$name" bash -lc "
    . \"\$HOME/.cargo/env\"
    export CARGO_TARGET_DIR=\"\$HOME/openpod-target\"
    export OPENPOD_TEST_ROOTFS=\"\$HOME/openpod-fixtures/alpine\"
    ${OPENPOD_TEST_ROOT:+export OPENPOD_TEST_ROOT=1}
    $cmd"
}

case "${1:-}" in
  up)
    if limactl list -q | grep -qx "$name"; then
      limactl start "$name"
    else
      limactl start --name="$name" --tty=false \
        --set ".mounts=[{\"location\":\"$repo\",\"writable\":true}]" \
        "$repo/dev/lima.yaml"
    fi
    limactl shell --workdir "$repo" "$name" bash "$repo/dev/provision.sh"
    ;;
  shell) limactl shell --workdir "$repo" "$name" ;;
  run) shift; vm_run "$@" ;;
  test) shift; vm_run cargo test "$@" ;;
  test-root) shift; OPENPOD_TEST_ROOT=1 vm_run cargo test "$@" ;;
  static)
    target="$(limactl shell "$name" uname -m)-unknown-linux-musl"
    vm_run cargo build --release --target "$target"
    vm_run bash -c "dev/check-binary.sh \"\$CARGO_TARGET_DIR/$target/release/openpod\""
    ;;
  down) limactl stop "$name" ;;
  *)
    echo "usage: dev/vm.sh up|shell|run <cmd...>|test [args]|test-root [args]|static|down" >&2
    exit 2
    ;;
esac
