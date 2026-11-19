#!/usr/bin/env bash
# An enterprise MSA node: the eight Spring Cloud services of Spring PetClinic Microservices
# in two releases (n as published, n1 rebuilt by CI on a newer base), pulled from the
# registry of dev/bench/msa-setup.sh. For one tool:
#  1. pull: all of release n, then all of n1 — the time of each and the store's size;
#  2. a rolling start of the 16 services from a cold page cache, one after another: each
#     JVM runs until Spring reports "Started" — the tool's part of each start (`run -d`),
#     the JVM's own uptime when ready, and what the page cache holds after all of them (the
#     JRE and the jars each service reads). Twice: right after the pull (a tool that unpacks
#     a layer when a container first uses it, as Kiri does, unpacks now), then again;
#  3. density: 100 containers of both releases on one network, replicas spread as a
#     deployment spreads them, each running `sleep infinity` instead of its JVM (the node's
#     memory cannot hold 100 JVMs): start time of each, the memory the system holds, the
#     tool's processes; then a forced remove of all of them (killed at once, by every tool);
#  4. the store after use.
# Results go to <results-dir>/msa-<tool>.env as key=value lines.
#
# usage: dev/bench/msa.sh podbag|podbag-root|podman|docker|kiri [results-dir]
set -euo pipefail

tool="${1:?usage: dev/bench/msa.sh podbag|podbag-root|podman|docker|kiri [results-dir]}"
out="${2:-$HOME/bench-results}"
here="$(cd "$(dirname "$0")" && pwd)"
reg=127.0.0.1:5000/petclinic
static="${CARGO_TARGET_DIR:-$HOME/podbag-target}/$(uname -m)-unknown-linux-musl/release/podbag"
services=(config-server discovery-server api-gateway customers-service vets-service
  visits-service admin-server genai-service)
# Replicas of each service in one release (50 per release, 100 in all).
declare -A replicas=([config-server]=2 [discovery-server]=3 [api-gateway]=6
  [customers-service]=12 [vets-service]=9 [visits-service]=12 [admin-server]=2
  [genai-service]=4)
releases=(n n1)
mkdir -p "$out"
pull_opts=()
jvm_opts=()
rm_opts=()

case "$tool" in
  podbag)
    export PODBAG_DATA_DIR=/var/tmp/podbag-msa/data
    export PODBAG_RUNTIME_DIR="$XDG_RUNTIME_DIR/podbag-msa"
    cli=("$static")
    proc_pattern='^(podbag.*|pasta.*)$'
    store=("$PODBAG_DATA_DIR")
    version="$("$static" --version) (rootless)"
    ;;
  podbag-root)
    cli=(sudo -n env PODBAG_DATA_DIR=/var/tmp/podbag-msa-root/data
      PODBAG_RUNTIME_DIR=/run/podbag-msa-root "$static")
    proc_pattern='^(podbag.*)$'
    store=(/var/tmp/podbag-msa-root/data)
    version="$("$static" --version) (root, composefs)"
    ;;
  podman)
    cli=(podman)
    pull_opts=(--tls-verify=false)
    # Its default log driver is journald: each `podman logs` below would read the system's
    # journal files into the page cache, which is no part of running the service.
    jvm_opts=(--log-driver k8s-file)
    # Its `rm -f` sends SIGTERM and waits 10 s before SIGKILL (`sleep` as PID 1 ignores
    # SIGTERM); the other tools kill at once.
    rm_opts=(-t 0)
    proc_pattern='^(conmon|pasta.*|aardvark-dns|catatonit|podman|netavark|rootlessport.*)$'
    store=("$(podman info --format '{{.Store.GraphRoot}}')")
    version="$(podman --version) (rootless)"
    ;;
  docker)
    sudo -n systemctl start docker.service
    trap '"$here/docker-off.sh"' EXIT
    cli=(sudo -n docker)
    proc_pattern='^(dockerd|containerd.*|docker-proxy|runc)$'
    store=(/var/lib/docker /var/lib/containerd)
    version="$(docker --version | cut -d, -f1) (root daemon)"
    ;;
  kiri)
    sudo -n systemctl start kiri.service
    trap 'sudo -n systemctl stop kiri.service' EXIT
    cli=(sudo -n docker -H unix:///run/kiri/docker.sock)
    for _ in $(seq 50); do "${cli[@]}" version >/dev/null 2>&1 && break; sleep 0.2; done
    proc_pattern='^(kiri|kiri-init)$'
    store=(/var/lib/kiri /var/lib/kiri-daemon)
    version="Kiri $(dpkg-query -W -f='${Version}' kiri-lite 2>/dev/null ||
      dpkg-query -W -f='${Version}' kiri 2>/dev/null) (root daemon)"
    ;;
  *) echo "unknown tool $tool" >&2; exit 2 ;;
esac

# Root tools are timed from one root shell: sudo costs milliseconds per call.
timed_prefix=()
if [ "${cli[0]}" = sudo ]; then
  timed_prefix=(sudo -n)
  cli=("${cli[@]:2}")
fi
c() { "${timed_prefix[@]}" "${cli[@]}" "$@"; }

now_us() { local t="$EPOCHREALTIME"; echo "${t/./}"; }
drop_caches() { sync; sudo -n sh -c 'echo 3 > /proc/sys/vm/drop_caches'; sleep 2; }
bystanders="${BENCH_BYSTANDERS:-^(lima-guestagent|fwupd|fwupdmgr|registry)$}"
bystander_kb() {
  local sum=0 pid kb
  for pid in $(ps -eo pid=,comm= | awk -v re="$bystanders" '$2 ~ re {print $1}'); do
    kb="$(sudo -n awk '/^Pss_Anon:/{print $2}' "/proc/$pid/smaps_rollup" 2>/dev/null || echo 0)"
    sum=$((sum + ${kb:-0}))
  done
  echo "$sum"
}
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
cached_kb() { awk '/^Cached:/{print $2}' /proc/meminfo; }
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
tool_pids() { ps -eo pid=,comm= | awk -v re="$proc_pattern" '$2 ~ re {print $1}'; }
tool_procs() { tool_pids | wc -l; }
tool_pss_kb() {
  local sum=0 pid kb
  for pid in $(tool_pids); do
    kb="$(sudo -n awk '/^Pss:/{print $2}' "/proc/$pid/smaps_rollup" 2>/dev/null || echo 0)"
    sum=$((sum + ${kb:-0}))
  done
  echo "$sum"
}
store_kb() { sudo -n du -scxk "${store[@]}" 2>/dev/null | tail -1 | cut -f1; }
pct() { sort -n | awk -v p="$1" '{v[NR]=$1} END{i=int((NR*p+99)/100); if(i<1)i=1; printf "%.1f", v[i]/1000}'; }
# Run commands (one per line on stdin) in one shell of the tool's user; print the
# microseconds each took.
batch() {
  local f
  f="$(mktemp)"
  cat >"$f"
  chmod 644 "$f"
  "${timed_prefix[@]}" bash -c '
    while IFS= read -r line; do
      t0=${EPOCHREALTIME/./}
      eval "$line" >/dev/null 2>&1 || echo "FAILED: $line" >&2
      t1=${EPOCHREALTIME/./}
      echo $((t1 - t0))
    done <"$1"' _ "$f"
  rm -f "$f"
}
cli_line="${cli[*]}"
remove_all() {
  local ids
  ids="$(c ps -aq)"
  [ -z "$ids" ] || c rm -f "${rm_opts[@]}" $ids >/dev/null 2>&1 || true
}
# One pass of the rolling start, in one shell of the tool's user: each of the 16 services in
# turn runs until Spring reports "Started", and is then removed. One line per service:
#   <name> <run_ms> <ready_ms> <jvm_ms>
# run_ms: until `run -d` returned (the tool's part: create, unpack if lazy, start);
# ready_ms: until its log showed "Started" (polled every 0.5 s);
# jvm_ms: the JVM's own uptime then ("process running for"), to the millisecond.
# A service that did not start has "-" for the values it lacks.
jvm_pass() {
  local f
  f="$(mktemp)"
  for r in "${releases[@]}"; do
    for s in "${services[@]}"; do echo "$s $r"; done
  done >"$f"
  chmod 644 "$f"
  "${timed_prefix[@]}" bash -c '
    list=$1 reg=$2 nopts=$3 nrm=$4
    shift 4
    opts=("${@:1:nopts}")
    rmo=("${@:nopts+1:nrm}")
    cli=("${@:nopts+nrm+1}")
    while read -r s r; do
      name="jvm-$s-$r"
      t0=${EPOCHREALTIME/./}
      if ! "${cli[@]}" run -d --network none --name "$name" "${opts[@]}" \
        -e SPRING_PROFILES_ACTIVE=default "$reg/$s:$r" >/dev/null 2>&1; then
        echo "$name - - -"
        continue
      fi
      t1=${EPOCHREALTIME/./}
      line=""
      for _ in $(seq 180); do
        logs="$("${cli[@]}" logs "$name" 2>&1)"
        line="$(grep -m1 -oE "Started [A-Za-z]+ in [0-9.]+ seconds \((process|JVM) running for [0-9.]+\)" <<<"$logs")"
        [ -n "$line" ] && break
        grep -q "APPLICATION FAILED TO START" <<<"$logs" && break
        sleep 0.5
      done
      t2=${EPOCHREALTIME/./}
      if [ -n "$line" ]; then
        up="${line##*running for }"
        echo "$name $(((t1 - t0) / 1000)) $(((t2 - t0) / 1000)) $(awk -v u="${up%)}" "BEGIN{printf \"%.0f\", u*1000}")"
      else
        echo "$name $(((t1 - t0) / 1000)) - -"
      fi
      "${cli[@]}" rm -f "${rmo[@]}" "$name" >/dev/null 2>&1
    done <"$list"' _ "$f" "$reg" "${#jvm_opts[@]}" "${#rm_opts[@]}" \
    "${jvm_opts[@]}" "${rm_opts[@]}" "${cli[@]}"
  rm -f "$f"
}

results="$out/msa-$tool.env"
echo "== msa $tool: $version" >&2
remove_all
c network rm msa >/dev/null 2>&1 || true
settle
base_store="$(store_kb)"
{
  echo "tool=$tool"
  echo "version=$version"
  echo "date=$(date +%F)"
} >"$results"

# 1. Pull each release.
for r in "${releases[@]}"; do
  drop_caches
  t0="$(now_us)"
  for s in "${services[@]}"; do
    c pull "${pull_opts[@]}" "$reg/$s:$r" >/dev/null
  done
  t1="$(now_us)"
  echo "pull_${r}_ms=$(((t1 - t0) / 1000))" >>"$results"
  echo "store_after_${r}_kb=$(($(store_kb) - base_store))" >>"$results"
  echo "  pulled $r in $(((t1 - t0) / 1000)) ms" >&2
done

# 2. A rolling start of all 16 services from a cold page cache, twice. Without the config
#    server and Eureka of a full deployment: the default profile, whose configuration import
#    is optional, starts each service on its own.
for pass in 1 2; do
  settle
  drop_caches
  cache0="$(cached_kb)"
  lines="$(jvm_pass)"
  cache1="$(cached_kb)"
  sed "s/^/  pass $pass: /" <<<"$lines" >&2
  ok="$(awk '$4 != "-"' <<<"$lines")"
  {
    echo "jvm${pass}_started=$(grep -c . <<<"$ok" || true)"
    echo "jvm${pass}_failed=$(awk '$4 == "-"' <<<"$lines" | grep -c . || true)"
    echo "jvm${pass}_run_median_ms=$(awk '{print $2*1000}' <<<"$ok" | pct 50)"
    echo "jvm${pass}_run_sum_ms=$(awk '{s+=$2} END{print s}' <<<"$ok")"
    echo "jvm${pass}_jvm_median_ms=$(awk '{print $4*1000}' <<<"$ok" | pct 50)"
    echo "jvm${pass}_jvm_sum_ms=$(awk '{s+=$4} END{print s}' <<<"$ok")"
    echo "jvm${pass}_ready_median_ms=$(awk '{print $3*1000}' <<<"$ok" | pct 50)"
    echo "jvm${pass}_page_cache_kb=$((cache1 - cache0))"
  } >>"$results"
done
echo "store_after_use_kb=$(($(store_kb) - base_store))" >>"$results"

# 3. Density: 100 sleeping containers of both releases on one network.
c network create msa >/dev/null
settle
held0="$(held_kb)"
used0="$(used_kb)"
pss0="$(tool_pss_kb)"
times="$(
  for r in "${releases[@]}"; do
    for s in "${services[@]}"; do
      for i in $(seq "${replicas[$s]}"); do
        echo "$cli_line run -d --network msa --name $s-$r-$i --entrypoint sleep $reg/$s:$r infinity"
      done
    done
  done | batch
)"
sleep 10
{
  echo "density_count=$(c ps -q | wc -l)"
  echo "density_start_median_ms=$(pct 50 <<<"$times")"
  echo "density_start_p90_ms=$(pct 90 <<<"$times")"
  echo "density_start_total_ms=$(awk '{s+=$1} END{printf "%d", s/1000}' <<<"$times")"
  echo "density_held_kb=$(($(held_kb) - held0))"
  echo "density_used_kb=$(($(used_kb) - used0))"
  echo "density_tool_pss_kb=$(tool_pss_kb)"
  echo "density_tool_pss_idle_kb=$pss0"
  echo "density_tool_procs=$(tool_procs)"
} >>"$results"
ids="$(c ps -aq | tr '\n' ' ')"
rm_ms="$(echo "$cli_line rm -f ${rm_opts[*]} $ids" | batch)"
echo "density_rm_all_ms=$((rm_ms / 1000))" >>"$results"
c network rm msa >/dev/null 2>&1 || true

# 4. Leave nothing for the next tool.
case "$tool" in
  podbag | podbag-root)
    for r in "${releases[@]}"; do
      for s in "${services[@]}"; do c rmi "$reg/$s:$r" >/dev/null 2>&1 || true; done
    done
    c system prune -f >/dev/null 2>&1 || true
    ;;
  *) c system prune -a -f >/dev/null 2>&1 || true ;;
esac
echo "store_after_prune_kb=$(($(store_kb) - base_store))" >>"$results"
cat "$results" >&2
