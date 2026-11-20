#!/bin/sh
# A smoke test of `podbag cri serve` with crictl: runtime information and images.
# Runs on Linux as root; needs crictl (cri-tools) on PATH and network for the pull.
#
# usage: sudo dev/cri/crictl-smoke.sh PODBAG
set -eu
podbag=${1:?usage: crictl-smoke.sh PODBAG}
dir=$(mktemp -d /tmp/pbg-cri.XXXXXX)
sock=$dir/cri.sock
mkdir "$dir/net.d"
"$podbag" cri serve --socket "$sock" --cni-conf-dir "$dir/net.d" >"$dir/serve.log" 2>&1 &
pid=$!
trap 'kill $pid 2>/dev/null || true; wait $pid 2>/dev/null || true; rm -rf "$dir"' EXIT
i=0
while [ ! -S "$sock" ] && [ $i -lt 50 ]; do sleep 0.1; i=$((i + 1)); done

c() {
  echo "+ crictl $*"
  crictl --runtime-endpoint "unix://$sock" --image-endpoint "unix://$sock" --timeout 120s "$@"
}
c version
c info
c pull busybox:1.36
c images
c inspecti busybox:1.36
c imagefsinfo
c rmi busybox:1.36
c images
echo "--- serve log"
cat "$dir/serve.log"
