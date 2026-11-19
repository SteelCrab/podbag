#!/bin/sh
# Pods and containers of `podbag cri serve` through crictl: run a pod on a CNI bridge,
# create, start, exec, read logs and stats, stop and remove. Runs on Linux as root; needs
# crictl, the CNI plugins (bridge, host-local, portmap, loopback) and network for the pull.
#
# usage: sudo dev/cri/crictl-pods.sh PODBAG [CNI_BIN_DIR]
set -eu
podbag=${1:?usage: crictl-pods.sh PODBAG [CNI_BIN_DIR]}
cni_bin=${2:-/opt/cni/bin}
dir=$(mktemp -d /tmp/pbg-cri.XXXXXX)
sock=$dir/cri.sock
mkdir "$dir/net.d" "$dir/logs"
cat >"$dir/net.d/10-test.conflist" <<'EOF'
{
  "cniVersion": "1.0.0",
  "name": "pbgtest",
  "plugins": [
    {"type": "bridge", "bridge": "pbgcri0", "isGateway": true, "ipMasq": false,
     "ipam": {"type": "host-local", "ranges": [[{"subnet": "10.233.77.0/24"}]],
              "dataDir": "DATADIR"}},
    {"type": "portmap", "capabilities": {"portMappings": true}}
  ]
}
EOF
sed -i "s|DATADIR|$dir/ipam|" "$dir/net.d/10-test.conflist"
cat >"$dir/pod.json" <<EOF
{"metadata": {"name": "smoke", "namespace": "default", "uid": "pbg-smoke-1", "attempt": 0},
 "hostname": "smoke-pod", "log_directory": "$dir/logs",
 "dns_config": {"servers": ["10.233.77.53"], "searches": ["default.svc"], "options": ["ndots:2"]},
 "linux": {"sysctls": {"net.ipv4.ip_unprivileged_port_start": "80"}}}
EOF
cat >"$dir/ctr.json" <<'EOF'
{"metadata": {"name": "sleeper"}, "image": {"image": "busybox:1.36"},
 "command": ["sh", "-c", "echo hello from the pod; echo to stderr >&2; mkdir -p /tmp/www; echo pong >/tmp/www/index.html; httpd -p 8080 -h /tmp/www; while true; do echo tick; sleep 1; done"],
 "log_path": "sleeper/0.log"}
EOF
"$podbag" cri serve --socket "$sock" --cni-conf-dir "$dir/net.d" --cni-bin-dir "$cni_bin" \
  >"$dir/serve.log" 2>&1 &
pid=$!
cleanup() {
  c rmp -fa >/dev/null 2>&1 || true
  kill $pid 2>/dev/null
  wait $pid 2>/dev/null
  ip link del pbgcri0 2>/dev/null || true
  echo "--- serve log"
  cat "$dir/serve.log"
  rm -rf "$dir"
}
trap cleanup EXIT
i=0
while [ ! -S "$sock" ] && [ $i -lt 50 ]; do sleep 0.1; i=$((i + 1)); done

c() {
  crictl --config /dev/null --runtime-endpoint "unix://$sock" --image-endpoint "unix://$sock" --timeout 120s "$@"
}
say() { echo "+ crictl $*"; c "$@"; }
check() {
  if ! echo "$2" | grep -q -- "$3"; then
    echo "FAIL: $1: expected '$3' in:"
    echo "$2"
    exit 1
  fi
  echo "ok: $1"
}

c pull busybox:1.36 >/dev/null
pod=$(c runp "$dir/pod.json")
echo "pod $pod"
say pods
check "pod is ready" "$(c inspectp "$pod")" '"state": "SANDBOX_READY"'
ip=$(c inspectp -o go-template --template '{{.status.network.ip}}' "$pod")
check "pod has an address" "$ip" '^10\.233\.77\.'
ctr=$(c create "$pod" "$dir/ctr.json" "$dir/pod.json")
echo "container $ctr"
check "container is created" "$(c ps -a)" 'Created'
say start "$ctr"
sleep 1
check "container runs" "$(c ps)" 'Running'
check "exec runs in the container" "$(c exec -s "$ctr" hostname)" '^smoke-pod$'
check "exec sees the pod address" "$(c exec -s "$ctr" ip -4 addr show eth0)" "$ip"
check "exec sees the pod DNS" "$(c exec -s "$ctr" cat /etc/resolv.conf)" 'nameserver 10.233.77.53'
check "pod sysctl" "$(c exec -s "$ctr" cat /proc/sys/net/ipv4/ip_unprivileged_port_start)" '^80$'
check "exec exit code" "$(c exec -s "$ctr" sh -c 'exit 3' 2>&1 || echo "code $?")" 'code'
check "logs" "$(c logs "$ctr" 2>&1)" 'hello from the pod'
check "logs keep stderr" "$(c logs "$ctr" 2>&1)" 'to stderr'
check "log file format" "$(cat "$dir/logs/sleeper/0.log")" ' stdout F hello from the pod'
# Streaming: exec with stdin, an exit code, a terminal, attach and a forwarded port.
check "exec with stdin" "$(echo through stdin | c exec -i "$ctr" cat)" '^through stdin$'
check "exec exit code (streamed)" "$(c exec "$ctr" sh -c 'exit 4' 2>&1; echo "code $?")" 'code 4'
check "exec with a terminal" "$(script -qec "crictl --config /dev/null -r unix://$sock exec -it $ctr tty" /dev/null)" '/dev/pts/'
check "attach" "$(timeout -s KILL 3 crictl --config /dev/null -r "unix://$sock" attach "$ctr" 2>&1 || true)" 'tick'
c port-forward "$pod" 18080:8080 >"$dir/pf.log" 2>&1 &
pf=$!
sleep 1
check "port forward" "$(wget -qO- http://127.0.0.1:18080/ 2>&1)" '^pong$'
check "port forward again" "$(wget -qO- http://127.0.0.1:18080/ 2>&1)" '^pong$'
kill $pf
wait $pf 2>/dev/null || true
check "stats" "$(c stats -o json "$ctr")" 'workingSetBytes'
check "pod stats" "$(c statsp -o json "$pod")" 'workingSetBytes'
check "inspect" "$(c inspect "$ctr")" '"state": "CONTAINER_RUNNING"'
say stop -t 2 "$ctr"
check "container exited" "$(c inspect "$ctr")" '"state": "CONTAINER_EXITED"'
say rm "$ctr"
check "container removed" "$(c ps -a -q)" '^$'
say stopp "$pod"
check "pod not ready" "$(c inspectp "$pod")" '"state": "SANDBOX_NOTREADY"'
say rmp "$pod"
check "pod removed" "$(c pods -q)" '^$'
say stopp "$pod"

# A pod on the host's network with one PID namespace for its containers, and a container
# with a user, groups, a read-only root and a memory limit.
cat >"$dir/hostpod.json" <<EOF
{"metadata": {"name": "hostpod", "namespace": "default", "uid": "pbg-smoke-2", "attempt": 0},
 "log_directory": "$dir/logs",
 "linux": {"security_context": {"namespace_options": {"network": 2, "pid": 0}}}}
EOF
cat >"$dir/user.json" <<'EOF'
{"metadata": {"name": "user"}, "image": {"image": "busybox:1.36"},
 "command": ["sleep", "600"], "log_path": "user/0.log",
 "linux": {"resources": {"memory_limit_in_bytes": 67108864, "cpu_shares": 512},
  "security_context": {"run_as_user": {"value": 1000}, "run_as_group": {"value": 1000},
   "supplemental_groups": [2000], "readonly_rootfs": true, "no_new_privs": true,
   "namespace_options": {"pid": 0}}}}
EOF
pod=$(c runp "$dir/hostpod.json")
ctr=$(c create "$pod" "$dir/user.json" "$dir/hostpod.json")
say start "$ctr"
check "user and groups" "$(c exec -s "$ctr" id)" 'uid=1000 gid=1000 groups=1000,2000'
check "read-only root" "$(c exec -s "$ctr" touch /x 2>&1 || true)" 'Read-only file system'
check "host network" "$(c exec -s "$ctr" cat /proc/net/dev)" "$(ip -o link | awk -F': ' 'NR==2 {print $2}')"
check "shared PID namespace" "$(c exec -s "$ctr" ps)" 'sleep 600'
check "memory limit" "$(c exec -s "$ctr" cat /sys/fs/cgroup/memory.max)" '^67108864$'
check "cpu weight" "$(c exec -s "$ctr" cat /sys/fs/cgroup/cpu.weight)" '^20$'
say rmp -f "$pod"
check "nothing held" "$(ls /run/podbag/cri/ns 2>/dev/null)" '^$'
echo "PASS"
