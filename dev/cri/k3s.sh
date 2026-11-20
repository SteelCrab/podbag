#!/bin/sh
# A single-node k3s cluster with podbag as its container runtime: start it, stop it, or
# remove what it left. For tests and benchmarks; runs on Linux as root.
#
#   sudo dev/cri/k3s.sh start PODBAG K3S   # podbag cri serve, then k3s; waits for the node
#   sudo dev/cri/k3s.sh stop               # k3s, its pods, then podbag cri serve
#   sudo dev/cri/k3s.sh clean              # stop, and remove k3s's data and the images
#
# The kubeconfig is /etc/rancher/k3s/k3s.yaml (`k3s kubectl` reads it). Logs:
# /var/log/podbag-cri.log and /var/log/k3s-podbag.log. PODBAG_K3S_ARGS adds arguments to
# `k3s server` (on a nearly full disk: --kubelet-arg eviction-hard=nodefs.available<1%).
set -eu
sock=/run/podbag/cri.sock
run=/run/podbag-k3s
k3s_data=/var/lib/rancher/k3s

crictl_() { crictl --config /dev/null -r "unix://$sock" -i "unix://$sock" "$@"; }

start() {
  podbag=${1:?usage: k3s.sh start PODBAG K3S}
  k3s=${2:?usage: k3s.sh start PODBAG K3S}
  mkdir -p "$run" /run/podbag
  echo "$k3s" >"$run/k3s-path"
  # k3s writes flannel's CNI configuration here and unpacks the CNI plugins there.
  setsid "$podbag" cri serve --socket "$sock" \
    --cni-conf-dir "$k3s_data/agent/etc/cni/net.d" \
    --cni-bin-dir "$k3s_data/data/cni:$k3s_data/data/current/bin" \
    --cgroup-driver cgroupfs >/var/log/podbag-cri.log 2>&1 </dev/null &
  echo $! >"$run/cri.pid"
  i=0
  while [ ! -S "$sock" ] && [ $i -lt 100 ]; do sleep 0.1; i=$((i + 1)); done
  setsid "$k3s" server \
    --container-runtime-endpoint "unix://$sock" \
    --disable traefik,servicelb,metrics-server,local-storage \
    --write-kubeconfig-mode 644 \
    --kubelet-arg cgroup-driver=cgroupfs ${PODBAG_K3S_ARGS:-} \
    >/var/log/k3s-podbag.log 2>&1 </dev/null &
  echo $! >"$run/k3s.pid"
  echo "waiting for the node"
  i=0
  until "$k3s" kubectl get nodes 2>/dev/null | grep -q ' Ready'; do
    i=$((i + 1))
    if [ $i -gt 180 ]; then
      echo "the node did not become ready; see /var/log/k3s-podbag.log" >&2
      exit 1
    fi
    sleep 1
  done
  "$k3s" kubectl get nodes -o wide
}

stop() {
  if [ -f "$run/k3s.pid" ]; then
    kill "$(cat "$run/k3s.pid")" 2>/dev/null || true
    i=0
    while kill -0 "$(cat "$run/k3s.pid")" 2>/dev/null && [ $i -lt 30 ]; do sleep 1; i=$((i + 1)); done
    kill -9 "$(cat "$run/k3s.pid")" 2>/dev/null || true
  fi
  # The pods outlive k3s: podbag removes them (their networks too), then stops.
  if [ -S "$sock" ]; then
    crictl_ rmp -fa >/dev/null 2>&1 || true
  fi
  if [ -f "$run/cri.pid" ]; then
    kill "$(cat "$run/cri.pid")" 2>/dev/null || true
    sleep 1
  fi
  # What k3s's own killall script removes: its network devices and mounts.
  for link in flannel.1 cni0 flannel-v6.1 flannel-wg; do
    ip link del "$link" 2>/dev/null || true
  done
  grep -o " $k3s_data[^ ]*\| /var/lib/kubelet[^ ]*\| /run/k3s[^ ]*" /proc/mounts | sort -r |
    while read -r m; do umount -l "$m" 2>/dev/null || true; done
  iptables-save 2>/dev/null | grep -qE 'KUBE-|FLANNEL|CNI-' &&
    iptables-save | grep -vE 'KUBE-|FLANNEL|CNI-|flannel|cni0' | iptables-restore || true
  rm -rf "$run"
}

clean() {
  # The images the cluster pulled go too, while podbag still serves.
  if [ -f "$run/k3s.pid" ]; then
    kill "$(cat "$run/k3s.pid")" 2>/dev/null || true
    sleep 2
  fi
  if [ -S "$sock" ]; then
    crictl_ rmp -fa >/dev/null 2>&1 || true
    crictl_ rmi -a >/dev/null 2>&1 || true
  fi
  stop
  rm -rf "$k3s_data" /etc/rancher/k3s /var/lib/kubelet /var/log/pods /var/log/containers \
    /run/k3s /run/flannel /var/lib/cni /var/log/k3s-podbag.log /var/log/podbag-cri.log
}

case "${1:-}" in
  start) shift; start "$@" ;;
  stop) stop ;;
  clean) clean ;;
  *) echo "usage: k3s.sh start PODBAG K3S | stop | clean" >&2; exit 2 ;;
esac
