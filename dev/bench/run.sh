#!/usr/bin/env bash
# Measure one container tool in the podbag-dev VM:
#   - start time of `run --rm --network none alpine true` (median and p90),
#   - memory for N sleeping containers, without a network and on the default network
#     (the system's available memory, the memory held by category, the tool's processes),
#   - processes the tool keeps with no containers,
#   - disk use of two Python images that share their Python files but not their base.
# Results go to <results-dir>/<tool>.env as key=value lines (dev/bench/report.sh reads them).
#
# usage: dev/bench/run.sh podbag|podbag-root|podman|docker|kiri [results-dir]
# (PODBAG_IMAGE_MOUNT=hardlink with podbag-root: root mode without composefs;
#  PODBAG_KEEPER=0 with podbag or podbag-root: a shim per container, no keeper)
# Needs `dev/vm.sh bench-setup` (Docker, Podman, Kiri) and `dev/vm.sh static` (podbag).
set -euo pipefail

tool="${1:?usage: dev/bench/run.sh podbag|podbag-root|podman|docker|kiri [results-dir]}"
out="${2:-$HOME/bench-results}"
runs="${BENCH_RUNS:-100}"
count="${BENCH_COUNT:-50}"
here="$(cd "$(dirname "$0")" && pwd)"
alpine="docker.io/library/alpine:3.22"
py1="docker.io/library/python:3.12-slim-bookworm"
py2="docker.io/library/python:3.12-slim-trixie"
static="${CARGO_TARGET_DIR:-$HOME/podbag-target}/$(uname -m)-unknown-linux-musl/release/podbag"
mkdir -p "$out"

case "$tool" in
  podbag)
    export PODBAG_DATA_DIR=/var/tmp/podbag-bench/data
    export PODBAG_RUNTIME_DIR="$XDG_RUNTIME_DIR/podbag-bench"
    cli=("$static")
    # The keeper (and a shim for each container that still needs one), the session
    # holder and pasta.
    proc_pattern='^(podbag.*|pasta.*)$'
    store=("$PODBAG_DATA_DIR")
    version="$("$static" --version)${PODBAG_KEEPER:+ (keeper $PODBAG_KEEPER)}"
    ;;
  podbag-root)
    # PODBAG_IMAGE_MOUNT=hardlink measures root mode without composefs.
    cli=(sudo -n env PODBAG_DATA_DIR=/var/tmp/podbag-bench-root/data
      PODBAG_RUNTIME_DIR=/run/podbag-bench-root
      ${PODBAG_IMAGE_MOUNT:+PODBAG_IMAGE_MOUNT=$PODBAG_IMAGE_MOUNT}
      ${PODBAG_KEEPER:+PODBAG_KEEPER=$PODBAG_KEEPER} "$static")
    proc_pattern='^(podbag.*)$'
    store=(/var/tmp/podbag-bench-root/data)
    version="$("$static" --version) (root, ${PODBAG_IMAGE_MOUNT:-composefs}${PODBAG_KEEPER:+, keeper $PODBAG_KEEPER})"
    ;;
  podman)
    cli=(podman)
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
    # Kiri on a Linux host: one root daemon serving the Docker Engine API, driven by the
    # same docker CLI as Docker's column. Runs only while it is measured.
    sudo -n systemctl start kiri.service
    trap 'sudo -n systemctl stop kiri.service' EXIT
    cli=(sudo -n docker -H unix:///run/kiri/docker.sock)
    for _ in $(seq 50); do "${cli[@]}" version >/dev/null 2>&1 && break; sleep 0.2; done
    proc_pattern='^(kiri|kiri-init)$'
    store=(/var/lib/kiri /var/lib/kiri-daemon)
    # The kiri binary has no --version: ask the package (kiri-lite or the full kiri).
    version="Kiri $(dpkg-query -W -f='${Version}' kiri-lite 2>/dev/null ||
      dpkg-query -W -f='${Version}' kiri 2>/dev/null) (root daemon)"
    ;;
  *) echo "unknown tool $tool" >&2; exit 2 ;;
esac

# sudo costs a few milliseconds per call: time root tools from a root shell instead.
timed_prefix=()
if [ "${cli[0]}" = sudo ]; then
  timed_prefix=(sudo -n)
  cli=("${cli[@]:2}")
fi

now_us() { local t="$EPOCHREALTIME"; echo "${t/./}"; }
drop_caches() { sync; sudo -n sh -c 'echo 3 > /proc/sys/vm/drop_caches'; sleep 2; }
# What processes and the kernel hold, without the page cache: anonymous and shared memory,
# slab, kernel stacks, page tables, per-CPU and vmalloc memory. Steadier than available
# memory. (Pages the kernel takes straight from the page allocator are in no category.)
# The VM's own helper (lima's guest agent, a Go program that follows network changes) grows
# and shrinks by 10 MiB and more while containers come and go; a Linux host has no such
# process, so its anonymous memory is left out of both system-wide figures, like that of
# the firmware updater a timer starts now and then (BENCH_BYSTANDERS: a pattern of names).
bystanders="${BENCH_BYSTANDERS:-^(lima-guestagent|fwupd|fwupdmgr)$}"
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
# The system's used memory: MemTotal - MemAvailable, less the bystanders.
used_kb() {
  local all
  all="$(awk '/^MemTotal/{t=$2} /^MemAvailable/{a=$2} END{print t-a}' /proc/meminfo)"
  echo $((all - $(bystander_kb)))
}
# The memory of removed containers (namespaces, cgroups) is freed a little later: wait
# until what the system holds stops falling, so that one round's leftovers do not count
# in the next one.
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
remove_all() {
  local ids
  ids="$("${timed_prefix[@]}" "${cli[@]}" ps -aq)"
  [ -z "$ids" ] || "${timed_prefix[@]}" "${cli[@]}" rm -f $ids >/dev/null
  sleep 3
}
# Percentile p (0-100) of the numbers on stdin, in milliseconds with one decimal.
pct() { sort -n | awk -v p="$1" '{v[NR]=$1} END{i=int((NR*p+99)/100); if(i<1)i=1; printf "%.1f", v[i]/1000}'; }

echo "== $tool: $version" >&2
"${timed_prefix[@]}" "${cli[@]}" pull "$alpine" >/dev/null
remove_all

# BENCH_PARTS picks what to measure (default: start memory disk). A run of some parts
# replaces only their lines in an earlier results file.
parts="${BENCH_PARTS:-start memory disk}"
want() { [[ " $parts " == *" $1 "* ]]; }
results="$out/$tool.env"
if want start && want memory && want disk; then
  # What a tool keeps with no containers: podbag's keeper leaves 10 s after the last one.
  sleep "${BENCH_IDLE_WAIT:-12}"
  {
    echo "tool=$tool"
    echo "version=$version"
    echo "runs=$runs"
    echo "count=$count"
    echo "idle_procs=$(tool_procs)"
    echo "idle_pss_kb=$(tool_pss_kb)"
  } >"$results"
else
  [ -f "$results" ] || { echo "BENCH_PARTS=$parts needs an earlier $results" >&2; exit 2; }
  if want start; then sed -i '/^start_/d' "$results"; fi
  if want memory; then sed -i -E '/^(mem|pss|procs|held)_/d' "$results"; fi
  if want disk; then sed -i '/^disk_/d' "$results"; fi
fi

# 1. Start time. The first runs warm caches and are not counted.
if want start; then
echo "start time: $runs runs" >&2
times="$("${timed_prefix[@]}" bash -c '
  for i in $(seq $((3 + $1))); do
    t0="${EPOCHREALTIME/./}"
    "${@:3}" run --rm --network none "$2" true >/dev/null
    t1="${EPOCHREALTIME/./}"
    [ "$i" -le 3 ] || echo $((t1 - t0))
  done' _ "$runs" "$alpine" "${cli[@]}")"
echo "start_median_ms=$(pct 50 <<<"$times")" >>"$results"
echo "start_p90_ms=$(pct 90 <<<"$times")" >>"$results"
case "$tool" in
  podbag*)
    # A start that finds no keeper (the first after 10 s without containers): with no
    # linger the keeper leaves after each container, so every run starts its own.
    cold="$("${timed_prefix[@]}" bash -c '
      for i in $(seq 33); do
        t0="${EPOCHREALTIME/./}"
        PODBAG_KEEPER_LINGER_MS=0 "${@:2}" run --rm --network none "$1" true >/dev/null
        t1="${EPOCHREALTIME/./}"
        [ "$i" -le 3 ] || echo $((t1 - t0))
        sleep 0.3
      done' _ "$alpine" "${cli[@]}")"
    echo "start_cold_median_ms=$(pct 50 <<<"$cold")" >>"$results"
    ;;
esac
fi

# 2. Memory for $count sleeping containers. The system-wide difference is noisy (other
# processes, the kernel's caches), so it is measured three times; PSS is exact.
if want memory; then
for net in none default; do
  net_args=()
  [ "$net" = default ] || net_args=(--network none)
  deltas=()
  helds=()
  for round in 1 2 3; do
    echo "memory: $count containers, network $net, round $round" >&2
    drop_caches
    settle
    before="$(used_kb)"
    held_before="$(held_kb)"
    for i in $(seq "$count"); do
      "${timed_prefix[@]}" "${cli[@]}" run -d "${net_args[@]}" "$alpine" sleep 3600 >/dev/null
    done
    sleep 5
    drop_caches
    deltas+=($(($(used_kb) - before)))
    helds+=($(($(held_kb) - held_before)))
    pss="$(tool_pss_kb)"
    nprocs="$(tool_procs)"
    remove_all
  done
  {
    echo "mem_${net}_kb=$(printf '%s\n' "${deltas[@]}" | sort -n | sed -n 2p)"
    echo "mem_${net}_all_kb=\"${deltas[*]}\""
    echo "held_${net}_kb=$(printf '%s\n' "${helds[@]}" | sort -n | sed -n 2p)"
    echo "held_${net}_all_kb=\"${helds[*]}\""
    echo "pss_${net}_kb=$pss"
    echo "procs_${net}=$nprocs"
  } >>"$results"
done
fi

# 3. Disk: two images that differ in their Debian base but share Python's files. Each image
# is pulled and then used once: Kiri unpacks layers when a container first uses them, the
# other tools while pulling.
if want disk; then
echo "disk: $py1, then $py2" >&2
use() { "${timed_prefix[@]}" "${cli[@]}" run --rm --network none "$1" true >/dev/null; }
prune() {
  case "$tool" in
    podbag*) "${timed_prefix[@]}" "${cli[@]}" system prune -f ;;
    kiri) "${timed_prefix[@]}" "${cli[@]}" system prune -af --volumes ;;
    *) "${timed_prefix[@]}" "${cli[@]}" system prune -af ;;
  esac >/dev/null
  sleep 5
}
prune
# The base holds alpine, pulled and used once, in every store: Kiri deletes the layers of
# removed images only when it next creates a container, so this also clears what earlier
# runs left. The rows are what the two Python images add.
"${timed_prefix[@]}" "${cli[@]}" pull "$alpine" >/dev/null
use "$alpine"
base="$(store_kb)"
"${timed_prefix[@]}" "${cli[@]}" pull "$py1" >/dev/null
use "$py1"
one="$(store_kb)"
"${timed_prefix[@]}" "${cli[@]}" pull "$py2" >/dev/null
use "$py2"
two="$(store_kb)"
{
  echo "disk_first_kb=$((one - base))"
  echo "disk_both_kb=$((two - base))"
} >>"$results"
prune
fi

cat "$results"
