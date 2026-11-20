#!/bin/sh
# The CRI validation suite (critest from cri-tools) against `podbag cri serve`, with the
# tests of features podbag does not have skipped (each with its reason below). Runs on
# Linux as root; needs critest, the CNI plugins (bridge, host-local, portmap) and network.
#
# usage: sudo dev/cri/critest.sh PODBAG CRITEST [CNI_BIN_DIR] [extra critest flags...]
set -eu
podbag=${1:?usage: critest.sh PODBAG CRITEST [CNI_BIN_DIR] [flags...]}
critest=${2:?usage: critest.sh PODBAG CRITEST [CNI_BIN_DIR] [flags...]}
cni_bin=${3:-/opt/cni/bin}
shift 3 2>/dev/null || shift $#
dir=$(mktemp -d /tmp/pbg-critest.XXXXXX)
sock=$dir/cri.sock
mkdir "$dir/net.d"
cat >"$dir/net.d/10-critest.conflist" <<EOF
{
  "cniVersion": "1.0.0",
  "name": "pbgcritest",
  "plugins": [
    {"type": "bridge", "bridge": "pbgcrit0", "isGateway": true, "ipMasq": true,
     "ipam": {"type": "host-local", "ranges": [[{"subnet": "10.233.78.0/24"}]],
              "routes": [{"dst": "0.0.0.0/0"}], "dataDir": "$dir/ipam"}},
    {"type": "portmap", "capabilities": {"portMappings": true}}
  ]
}
EOF
"$podbag" cri serve --socket "$sock" --cni-conf-dir "$dir/net.d" --cni-bin-dir "$cni_bin" \
  >"$dir/serve.log" 2>&1 &
pid=$!
cleanup() {
  crictl --config /dev/null -r "unix://$sock" rmp -fa >/dev/null 2>&1 || true
  kill $pid 2>/dev/null || true
  wait $pid 2>/dev/null || true
  ip link del pbgcrit0 2>/dev/null || true
  rm -rf "$dir"
}
trap cleanup EXIT
i=0
while [ ! -S "$sock" ] && [ $i -lt 50 ]; do sleep 0.1; i=$((i + 1)); done

# Skipped, and why:
# - SELinux: podbag sets no SELinux labels.
# - AppArmor: podbag loads no AppArmor profiles for containers.
# - seccomp profiles from files (Localhost; the "profile that blocks setting hostname"
#   tests use one): podbag has its default filter and none.
# - user namespaces (hostUsers: false): not supported for pods yet.
# - checkpoint and restore: not supported.
# - image volumes (mounts of an image): not supported.
# - bidirectional mount propagation: mounts made in a container do not reach the host.
# - runtime handlers other than the default.
skip='SELinux|AppArmor|apparmor|seccomp.*[Ll]ocalhost|[Ll]ocalhost.*seccomp|seccomp profile that blocks|[Uu]ser[ _]?[Nn]amespace|[Cc]heckpoint|[Ii]mage [Vv]olume|image volume|rshared|[Bb]idirectional|runtime handler'
"$critest" --runtime-endpoint "unix://$sock" --image-endpoint "unix://$sock" \
  --ginkgo.skip "$skip" "$@"
