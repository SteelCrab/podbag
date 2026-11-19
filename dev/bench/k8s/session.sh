#!/usr/bin/env bash
# One Kubernetes measurement session in the podbag-dev VM: the same k3s with each container
# runtime in turn (its own containerd, CRI-O, `podbag cri serve`), one round that is not
# counted, then BENCH_ROUNDS rounds. A figure is the median of its rounds.
#
# In each round, every runtime gets a fresh cluster (its images stay from round to round):
#   - memory before k3s starts, and at rest (only the system pods),
#   - one pod: a Deployment of busybox scaled from 0 to 1, until the pod is ready (10 runs),
#   - 30 pods: scaled from 0 to 30, until all are ready; memory with them running,
#   - `kubectl exec POD -- true` (10 runs),
#   - in the last round: what two Python images add to the runtime's image store.
# The order of the runtimes turns from round to round, so that each one comes first once.
#
# usage: sudo dev/bench/k8s/session.sh [results-dir]    (then dev/bench/k8s/report.sh)
# Environment:
#   K3S             the k3s binary (default: ~/k8s-tools/k3s of the user who ran sudo)
#   PODBAG          the podbag binary (default: the static build of `dev/vm.sh static`)
#   BENCH_ROUNDS    counted rounds (default 3)
#   BENCH_RUNTIMES  which runtimes (default: containerd crio podbag)
#   BENCH_ONLY=disk only the disk part, one cluster per runtime, into earlier results
#   BENCH_LOCK=0    do not take the VM's shared locks
# Each round holds /tmp/podbag-bench.lock and /tmp/podbag-root-tests.lock: builds and tests
# of other work in the VM wait meanwhile. Needs CRI-O installed (crio, conmon, crun in
# /usr/libexec/crio) and crictl.
set -u

[ "$(id -u)" = 0 ] || { echo "run as root (sudo)" >&2; exit 2; }
here="$(cd "$(dirname "$0")" && pwd)"
user_home="$(getent passwd "${SUDO_USER:-root}" | cut -d: -f6)"
out="${1:-$user_home/bench-results/k8s}"
k3s="${K3S:-$user_home/k8s-tools/k3s}"
podbag="${PODBAG:-$user_home/podbag-target/$(uname -m)-unknown-linux-musl/release/podbag}"
rounds="${BENCH_ROUNDS:-3}"
runtimes=(${BENCH_RUNTIMES:-containerd crio podbag})
count=30
runs="${BENCH_RUNS:-10}"
mkdir -p "$out/logs"

k3s_data=/var/lib/rancher/k3s
cni_conf="$k3s_data/agent/etc/cni/net.d"
pb_data=/var/lib/podbag-k8s-bench
pb_run=/run/podbag-k8s-bench
crio_store=/var/lib/containers/storage
busybox=docker.io/library/busybox:1.36
py1=docker.io/library/python:3.12-slim-bookworm
py2=docker.io/library/python:3.12-slim-trixie
# The same k3s flags for every runtime; only --container-runtime-endpoint differs. The VM's
# disk is nearly full: no eviction for disk space, and no image garbage collection (it
# would delete busybox between rounds).
k3s_args=(--disable traefik,servicelb,metrics-server,local-storage
  --write-kubeconfig-mode 644
  --kubelet-arg cgroup-driver=cgroupfs
  --kubelet-arg "eviction-hard=nodefs.available<1%,imagefs.available<1%"
  --kubelet-arg image-gc-high-threshold=100
  --kubelet-arg image-gc-low-threshold=99)

k() { "$k3s" kubectl "$@"; }
now_us() { local t="$EPOCHREALTIME"; echo "${t/./}"; }
log() { echo "[$(date +%T)] $*" >&2; }

# --- memory, as in dev/bench/run.sh ---------------------------------------------------
drop_caches() { sync; echo 3 >/proc/sys/vm/drop_caches; sleep 2; }
# The VM's helper (lima's guest agent) and the firmware updater a timer starts now and then
# are left out of the system-wide figures; so is the k3s server, the same for every
# runtime and reported on its own.
bystanders="${BENCH_BYSTANDERS:-^(lima-guestagent|fwupd|fwupdmgr)$}"
k3s_pid=""
pss_of() { awk "/^$2:/{print \$2}" "/proc/$1/smaps_rollup" 2>/dev/null || echo 0; }
bystander_kb() {
  local sum=0 pid kb
  for pid in $(ps -eo pid=,comm= | awk -v re="$bystanders" '$2 ~ re {print $1}') $k3s_pid; do
    kb="$(pss_of "$pid" Pss_Anon)"
    sum=$((sum + ${kb:-0}))
  done
  echo "$sum"
}
# What processes and the kernel hold, without the page cache.
held_kb() {
  local all
  all="$(awk '/^(AnonPages|Shmem|Slab|KernelStack|PageTables|Percpu|VmallocUsed):/{s+=$2} END{print s}' /proc/meminfo)"
  echo $((all - $(bystander_kb)))
}
used_kb() {
  local all
  all="$(awk '/^MemTotal/{t=$2} /^MemAvailable/{a=$2} END{print t-a}' /proc/meminfo)"
  echo $((all - $(bystander_kb)))
}
settle() {
  local prev cur
  prev="$(held_kb)"
  for _ in $(seq 15); do
    sleep 2
    cur="$(held_kb)"
    [ $((prev - cur)) -gt 256 ] || return 0
    prev="$cur"
  done
}
pct() { sort -n | awk -v p="$1" '{v[NR]=$1} END{i=int((NR*p+99)/100); if(i<1)i=1; printf "%.0f", v[i]/1000}'; }

# --- the runtimes ---------------------------------------------------------------------
setup_rt() {
  case "$rt" in
    containerd)
      sock=/run/k3s/containerd/containerd.sock
      endpoint=()
      store=("$k3s_data/agent/containerd")
      # k3s's containerd, its shims and the pause containers. (Another containerd may run
      # as a user: only root's processes count.)
      proc_re='^(containerd|containerd-shim.*|pause)$'
      ;;
    crio)
      sock=/run/crio/crio.sock
      endpoint=(--container-runtime-endpoint "unix://$sock")
      store=("$crio_store")
      proc_re='^(crio|conmon|pause)$'
      ;;
    podbag)
      sock="$pb_run/cri.sock"
      endpoint=(--container-runtime-endpoint "unix://$sock")
      store=("$pb_data")
      # `podbag cri serve`, the keeper, and `podbag __pause` for a pod that shares its PID
      # namespace (none here).
      proc_re='^podbag$'
      ;;
  esac
}
crictl_() { crictl --config /dev/null -r "unix://$sock" -i "unix://$sock" --timeout 300s "$@"; }
# Root's processes, and the pause processes (they run as user 65535).
rt_pids() { ps -eo pid=,uid=,comm= | awk -v re="$proc_re" '($2 == 0 || $3 == "pause") && $3 ~ re {print $1}'; }
rt_procs() { rt_pids | wc -l; }
pause_procs() {
  case "$rt" in
    podbag) for p in $(rt_pids); do tr '\0' ' ' <"/proc/$p/cmdline" 2>/dev/null; echo; done | grep -c ' __pause' ;;
    *) ps -eo comm= | grep -cx pause ;;
  esac
}
rt_pss_kb() { # $1: Pss (default) or Pss_Anon
  local sum=0 pid kb
  for pid in $(rt_pids); do
    kb="$(pss_of "$pid" "${1:-Pss}")"
    sum=$((sum + ${kb:-0}))
  done
  echo "$sum"
}
store_kb() { du -scxk "${store[@]}" 2>/dev/null | tail -1 | cut -f1; }

start_rt() {
  mkdir -p "$cni_conf"
  case "$rt" in
    crio)
      # Short image names (k3s's CoreDNS) resolve on Docker Hub, as with containerd; the
      # system's registries.conf stays as it is.
      CONTAINERS_REGISTRIES_CONF=/etc/crio/registries.conf setsid crio \
        --cgroup-manager cgroupfs --conmon-cgroup pod \
        --cni-config-dir "$cni_conf" \
        --cni-plugin-dir "$k3s_data/data/cni" --cni-plugin-dir "$k3s_data/data/current/bin" \
        --log-level warn >"$out/logs/crio.log" 2>&1 </dev/null &
      ;;
    podbag)
      mkdir -p "$pb_run"
      PODBAG_DATA_DIR="$pb_data" PODBAG_RUNTIME_DIR="$pb_run" setsid "$podbag" cri serve \
        --socket "$sock" --cni-conf-dir "$cni_conf" \
        --cni-bin-dir "$k3s_data/data/cni:$k3s_data/data/current/bin" \
        --cgroup-driver cgroupfs >"$out/logs/podbag-cri.log" 2>&1 </dev/null &
      ;;
  esac
  if [ "$rt" != containerd ]; then
    rt_daemon=$!
    for _ in $(seq 300); do [ -S "$sock" ] && break; sleep 0.1; done
  fi
  setsid "$k3s" server "${endpoint[@]}" "${k3s_args[@]}" >"$out/logs/k3s-$rt.log" 2>&1 </dev/null &
  k3s_pid=$!
  local i=0
  until k get nodes 2>/dev/null | grep -q ' Ready'; do
    i=$((i + 1))
    [ $i -lt 240 ] || { log "the node did not become ready"; return 1; }
    sleep 1
  done
  i=0
  until k -n kube-system get deploy coredns >/dev/null 2>&1; do
    i=$((i + 1)); [ $i -lt 120 ] || return 1; sleep 1
  done
  k -n kube-system rollout status deploy/coredns --timeout=300s >&2 || return 1
  k create namespace bench >/dev/null
  i=0
  until k -n bench get serviceaccount default >/dev/null 2>&1; do
    i=$((i + 1)); [ $i -lt 60 ] || break; sleep 1
  done
  # The image is pulled before anything is timed.
  crictl_ pull "$busybox" >/dev/null || { log "pull of $busybox failed"; return 1; }
  k -n bench apply -f - >/dev/null <<EOF
apiVersion: apps/v1
kind: Deployment
metadata: {name: sleeper}
spec:
  replicas: 0
  selector: {matchLabels: {app: sleeper}}
  template:
    metadata: {labels: {app: sleeper}}
    spec:
      terminationGracePeriodSeconds: 1
      containers:
      - name: sleep
        image: $busybox
        imagePullPolicy: IfNotPresent
        command: [sleep, "1000000"]
EOF
}

# Every pod goes while k3s still runs (so that the kubelet makes no new ones), then k3s,
# the runtime, and what k3s's own killall script removes (network devices, mounts,
# rules). The cluster's state goes; the images stay, unless $1 is "images".
stop_rt() {
  if [ -n "$k3s_pid" ] && kill -0 "$k3s_pid" 2>/dev/null; then
    k delete namespace bench --wait=false >/dev/null 2>&1
    k -n kube-system scale deploy --all --replicas=0 >/dev/null 2>&1
    for _ in $(seq 120); do
      [ -z "$(crictl_ pods -q --state ready 2>/dev/null)" ] && break
      sleep 1
    done
    crictl_ rmp -fa >/dev/null 2>&1
    [ "${1:-}" != images ] || crictl_ rmi -a >/dev/null 2>&1
    kill "$k3s_pid" 2>/dev/null
    for _ in $(seq 30); do kill -0 "$k3s_pid" 2>/dev/null || break; sleep 1; done
    kill -9 "$k3s_pid" 2>/dev/null
  fi
  k3s_pid=""
  case "$rt" in
    containerd)
      pkill -U 0 -f 'containerd-shim-runc-v2 -namespace k8s.io' 2>/dev/null
      pkill -U 0 -f "^$k3s_data/data/[^ ]*/containerd" 2>/dev/null
      ;;
    *)
      if [ -S "$sock" ]; then
        crictl_ rmp -fa >/dev/null 2>&1
        [ "${1:-}" != images ] || crictl_ rmi -a >/dev/null 2>&1
      fi
      [ -z "${rt_daemon:-}" ] || kill "$rt_daemon" 2>/dev/null
      for _ in $(seq 30); do kill -0 "${rt_daemon:-0}" 2>/dev/null || break; sleep 0.5; done
      rt_daemon=""
      [ "$rt" != crio ] || pkill -U 0 -x conmon 2>/dev/null
      ;;
  esac
  sleep 1
  for link in flannel.1 cni0 flannel-v6.1 flannel-wg; do ip link del "$link" 2>/dev/null; done
  grep -o " $k3s_data[^ ]*\| /var/lib/kubelet[^ ]*\| /run/k3s[^ ]*\| /run/netns/[^ ]*" /proc/mounts | sort -r |
    while read -r m; do umount -l "$m" 2>/dev/null; done
  if iptables-save 2>/dev/null | grep -qE 'KUBE-|FLANNEL|CNI-'; then
    iptables-save | grep -vE 'KUBE-|FLANNEL|CNI-|flannel|cni0' | iptables-restore
  fi
  if ip6tables-save 2>/dev/null | grep -qE 'KUBE-|FLANNEL|CNI-'; then
    ip6tables-save | grep -vE 'KUBE-|FLANNEL|CNI-|flannel|cni0' | ip6tables-restore
  fi
  find "$k3s_data/agent" -mindepth 1 -maxdepth 1 ! -name containerd -exec rm -rf {} + 2>/dev/null
  rm -rf "$k3s_data/server" /etc/rancher/k3s /var/lib/kubelet /var/log/pods /var/log/containers \
    /run/k3s /run/flannel /var/lib/cni
}

# What is left of the runtime's store once it stopped.
clean_store() {
  case "$rt" in
    containerd) rm -rf "$k3s_data/agent/containerd" ;;
    crio)
      grep -o " $crio_store[^ ]*\| /run/containers/storage[^ ]*" /proc/mounts | sort -r |
        while read -r m; do umount -l "$m" 2>/dev/null; done
      find "$crio_store" -mindepth 1 -maxdepth 1 ! -name tmp -exec rm -rf {} +
      rm -rf /run/containers/storage /run/crio /var/lib/crio
      ;;
    podbag)
      grep -o " $pb_data[^ ]*\| $pb_run[^ ]*" /proc/mounts | sort -r |
        while read -r m; do umount -l "$m" 2>/dev/null; done
      rm -rf "$pb_data" "$pb_run"
      ;;
  esac
}

# --- timing ---------------------------------------------------------------------------
# Every change of the bench pods, with the time it arrived: "time|name|ready|deleting".
wlog="$out/logs/watch.log"
watch_start() {
  : >"$wlog"
  setsid bash -c '"$1" kubectl -n bench get pods -w --request-timeout=0 -o jsonpath="{.metadata.name}|{.status.conditions[?(@.type==\"Ready\")].status}|{.metadata.deletionTimestamp}{\"\n\"}" |
    while IFS= read -r line; do echo "${EPOCHREALTIME/./}|$line"; done >>"$2"' _ "$k3s" "$wlog" 2>/dev/null &
  watcher=$!
  sleep 2
}
watch_stop() { [ -n "${watcher:-}" ] && kill -- -"$watcher" 2>/dev/null; watcher=""; }
# The time when $1 pods that were not ready before $2 had become ready.
wait_ready() {
  local n="$1" t0="$2" deadline=$((SECONDS + 300)) r
  while :; do
    r="$(awk -F'|' -v t0="$t0" -v n="$n" \
      '$1 >= t0 && $3 == "True" && $4 == "" && !($2 in s) {s[$2] = 1; if (++c == n) {print $1; exit}}' "$wlog")"
    [ -n "$r" ] && { echo "$r"; return 0; }
    [ $SECONDS -lt $deadline ] || return 1
    sleep 0.05
  done
}
# Until the API has no bench pod and the runtime no bench sandbox.
wait_gone() {
  local deadline=$((SECONDS + 300))
  while [ $SECONDS -lt $deadline ]; do
    if [ -z "$(k -n bench get pods -o name 2>/dev/null)" ] &&
      [ -z "$(crictl_ pods -q --namespace bench --state ready 2>/dev/null)" ]; then
      sleep 1
      return 0
    fi
    sleep 0.5
  done
  return 1
}
scale() { k -n bench scale deploy/sleeper --replicas="$1" >/dev/null; }

# --- one round of one runtime -----------------------------------------------------------
record() { echo "$1=$2" >>"$res"; }

versions() {
  local f="$out/versions.env"
  {
    echo "os=$(. /etc/os-release && echo "$PRETTY_NAME")"
    echo "kernel=$(uname -r)"
    echo "arch=$(uname -m)"
    echo "cpus=$(nproc)"
    echo "mem_kb=$(awk '/^MemTotal/{print $2}' /proc/meminfo)"
    echo "k3s=$("$k3s" --version | head -1 | awk '{print $3}')"
    echo "crictl=$(crictl --version | awk '{print $3}')"
  } >"$f.base"
  case "$rt" in
    containerd)
      echo "containerd=$("$k3s_data/data/current/bin/containerd" --version | awk '{print $3}')"
      echo "containerd_runc=$("$k3s_data/data/current/bin/runc" --version | head -1 | awk '{print $3}')"
      ;;
    crio)
      echo "crio=$(crio --version | awk '/^crio version/{print $3}')"
      echo "crio_runtime=$(/usr/libexec/crio/crun --version | head -1 | awk '{print $1, $3}')"
      echo "crio_conmon=$(/usr/libexec/crio/conmon --version | head -1 | awk '{print $3}')"
      ;;
    podbag) echo "podbag=$("$podbag" --version | awk '{print $2}')" ;;
  esac >"$f.$rt"
  crictl_ images --digests 2>/dev/null | awk 'NR > 1 {print $1 ":" $2 " " $3}' >"$out/images-$rt.txt"
}

disk() {
  log "$rt: disk"
  crictl_ rmi "$py1" "$py2" >/dev/null 2>&1
  sleep 2
  local base one two src=docker.io
  base="$(store_kb)"
  pull() {
    crictl_ pull "$1" >/dev/null 2>&1 || crictl_ pull "$1" >/dev/null 2>&1 || {
      src=mirror.gcr.io
      crictl_ pull "mirror.gcr.io/${1#docker.io/}" >/dev/null
    }
  }
  pull "$py1" || return 1
  one="$(store_kb)"
  pull "$py2" || return 1
  two="$(store_kb)"
  record disk_first_kb $((one - base))
  record disk_both_kb $((two - base))
  record disk_source "$src"
  crictl_ rmi "$py1" "$py2" "mirror.gcr.io/library/${py1##*/}" "mirror.gcr.io/library/${py2##*/}" >/dev/null 2>&1
}

measure() { # $1: round (0 is not counted), $2: last round (1) or not
  local round="$1" last="$2" base_held base_used times t0 t1 i pod
  res="$out/$rt.round$round"
  [ "$round" -gt 0 ] || res="$out/$rt.warmup"
  : >"$res"
  drop_caches
  settle
  base_held="$(held_kb)"
  base_used="$(used_kb)"
  record held_base_kb "$base_held"
  log "$rt round $round: start"
  start_rt || return 1
  [ "$round" -gt 0 ] || versions
  sleep "${BENCH_CALM:-30}"

  # At rest: only the system pods (CoreDNS).
  drop_caches
  settle
  record held_rest_kb $(($(held_kb) - base_held))
  record used_rest_kb $(($(used_kb) - base_used))
  record pss_rest_kb "$(rt_pss_kb)"
  record pss_anon_rest_kb "$(rt_pss_kb Pss_Anon)"
  record procs_rest "$(rt_procs)"
  record pause_rest "$(pause_procs)"
  record k3s_pss_rest_kb "$(pss_of "$k3s_pid" Pss)"

  # One pod, from 0 to 1: one run first that is not counted.
  watch_start
  log "$rt round $round: one pod"
  times=()
  for i in $(seq 0 "$runs"); do
    t0="$(now_us)"
    scale 1
    t1="$(wait_ready 1 "$t0")" || { log "a pod did not become ready"; return 1; }
    [ "$i" -eq 0 ] || times+=($((t1 - t0)))
    scale 0
    wait_gone || { log "the pod did not go"; return 1; }
  done
  record start_one_median_ms "$(printf '%s\n' "${times[@]}" | pct 50)"
  record start_one_p90_ms "$(printf '%s\n' "${times[@]}" | pct 90)"
  record start_one_all_ms "\"$(printf '%s\n' "${times[@]}" | awk '{printf "%s%.0f", (NR > 1 ? " " : ""), $1/1000}')\""

  # 30 pods, from 0 to 30.
  log "$rt round $round: $count pods"
  t0="$(now_us)"
  scale "$count"
  t1="$(wait_ready "$count" "$t0")" || { log "the pods did not become ready"; return 1; }
  record start_many_ms $(((t1 - t0) / 1000))
  sleep 10
  drop_caches
  settle
  record held_many_kb $(($(held_kb) - base_held))
  record used_many_kb $(($(used_kb) - base_used))
  record pss_many_kb "$(rt_pss_kb)"
  record pss_anon_many_kb "$(rt_pss_kb Pss_Anon)"
  record procs_many "$(rt_procs)"
  record pause_many "$(pause_procs)"
  record k3s_pss_many_kb "$(pss_of "$k3s_pid" Pss)"

  # kubectl exec: one run first that is not counted.
  pod="$(k -n bench get pods -o jsonpath='{.items[0].metadata.name}')"
  times=()
  for i in $(seq 0 "$runs"); do
    t0="$(now_us)"
    k -n bench exec "$pod" -- true || { log "exec failed"; return 1; }
    t1="$(now_us)"
    [ "$i" -eq 0 ] || times+=($((t1 - t0)))
  done
  record exec_median_ms "$(printf '%s\n' "${times[@]}" | pct 50)"
  scale 0
  wait_gone
  watch_stop
  if [ "$last" = 1 ]; then disk; fi
  return 0
}

# --- the session ------------------------------------------------------------------------
paused=()
pause_units() {
  local unit
  for unit in lima-guestagent.service apt-daily.timer apt-daily-upgrade.timer \
    fwupd-refresh.timer motd-news.timer man-db.timer; do
    if systemctl is-active --quiet "$unit"; then
      systemctl stop "$unit" && paused+=("$unit")
    fi
  done
}
resume_units() { [ "${#paused[@]}" -eq 0 ] || systemctl start "${paused[@]}"; paused=(); }
locked=0
lock() {
  [ "${BENCH_LOCK:-1}" = 1 ] || return 0
  exec 8</tmp/podbag-bench.lock 9</tmp/podbag-root-tests.lock
  log "waiting for the VM's locks"
  flock 8 && flock 9 && locked=1
}
unlock() { [ "$locked" = 1 ] && { flock -u 9; flock -u 8; exec 8<&- 9<&-; locked=0; }; }
finish() {
  watch_stop
  [ -z "$k3s_pid" ] || stop_rt
  resume_units
  unlock
}
trap finish EXIT

failed=0
if [ "${BENCH_ONLY:-}" = disk ]; then
  lock
  pause_units
  for rt in "${runtimes[@]}"; do
    setup_rt
    res="$out/$rt.disk"
    : >"$res"
    if start_rt && disk; then :; else echo "FAILED disk $rt"; failed=1; fi
    stop_rt images
    clean_store
  done
  rm -rf "$k3s_data"
  resume_units
  unlock
  exit "$failed"
fi

rm -f "$out"/*.round* "$out"/*.warmup "$out"/*.env "$out"/*.disk
n="${#runtimes[@]}"
for round in 0 $(seq "$rounds"); do
  lock
  pause_units
  for j in $(seq 0 $((n - 1))); do
    rt="${runtimes[$(((j + round) % n))]}"
    setup_rt
    last=0
    [ "$round" -lt "$rounds" ] || last=1
    if ! measure "$round" "$last"; then
      echo "FAILED $rt round $round (see $out/logs)"
      failed=1
      # Keep the logs of the failure.
      cp "$out/logs/k3s-$rt.log" "$out/logs/k3s-$rt.round$round.failed.log" 2>/dev/null
    fi
    watch_stop
    if [ "$last" = 1 ]; then
      stop_rt images
      clean_store
    else
      stop_rt
    fi
  done
  resume_units
  unlock
done
rm -rf "$k3s_data"

# Each figure: the median of its rounds (the rounds' lists are kept from the first).
for rt in "${runtimes[@]}"; do
  ls "$out/$rt".round* >/dev/null 2>&1 || continue
  cat "$out/versions.env.base" "$out/versions.env.$rt" >"$out/$rt.env" 2>/dev/null
  awk -F= '
    { if (!($1 in n)) order[++keys] = $1; v[$1, ++n[$1]] = $2 }
    END {
      for (i = 1; i <= keys; i++) {
        key = order[i]; c = n[key]; numbers = 1
        for (j = 1; j <= c; j++) if (v[key, j] !~ /^-?[0-9.]+$/) numbers = 0
        if (!numbers) { print key "=" v[key, 1]; continue }
        for (a = 1; a <= c; a++) for (b = a + 1; b <= c; b++)
          if (v[key, b] + 0 < v[key, a] + 0) { t = v[key, a]; v[key, a] = v[key, b]; v[key, b] = t }
        print key "=" v[key, int((c + 1) / 2)]
      }
    }' "$out/$rt".round* >>"$out/$rt.env"
done
log "done; paused meanwhile: lima-guestagent and the system's timers"
exit "$failed"
