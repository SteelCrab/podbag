#!/usr/bin/env bash
# Many containers on one user network (design 9: 300 containers): does starting get slower
# as the network fills (locks, hosts files, the shared session), and what do 300 sleeping
# containers cost? Times every start; memory at 50 and at the end. Results go to
# <results-dir>/scale-<tool>.env (dev/bench/report.sh reads them).
#
# usage: dev/bench/scale.sh openpod|openpod-root|kiri [results-dir]
# (BENCH_SCALE: how many, default 300)
set -euo pipefail

tool="${1:?usage: dev/bench/scale.sh openpod|openpod-root|kiri [results-dir]}"
out="${2:-$HOME/bench-results}"
total="${BENCH_SCALE:-300}"
alpine="docker.io/library/alpine:3.22"
bin="${CARGO_TARGET_DIR:-$HOME/openpod-target}/$(uname -m)-unknown-linux-musl/release/openpod"
mkdir -p "$out"

case "$tool" in
  openpod)
    export OPENPOD_DATA_DIR=/var/tmp/openpod-scale/data
    export OPENPOD_RUNTIME_DIR="$XDG_RUNTIME_DIR/openpod-scale"
    cli=("$bin")
    proc_pattern='^(openpod.*|pasta.*)$'
    ;;
  openpod-root)
    cli=(sudo -n env OPENPOD_DATA_DIR=/var/tmp/openpod-scale-root/data
      OPENPOD_RUNTIME_DIR=/run/openpod-scale-root "$bin")
    proc_pattern='^(openpod.*)$'
    ;;
  kiri)
    # The daemon runs only while it is measured (as in run.sh).
    sudo -n systemctl start kiri.service
    trap 'sudo -n systemctl stop kiri.service' EXIT
    cli=(sudo -n docker -H unix:///run/kiri/docker.sock)
    for _ in $(seq 50); do "${cli[@]}" version >/dev/null 2>&1 && break; sleep 0.2; done
    proc_pattern='^(kiri|kiri-init)$'
    ;;
  *) echo "unknown tool $tool" >&2; exit 2 ;;
esac

# sudo costs a few milliseconds per call: time root tools from a root shell instead.
timed_prefix=()
inner=("${cli[@]}")
if [ "${cli[0]}" = sudo ]; then
  timed_prefix=(sudo -n)
  inner=("${cli[@]:2}")
fi

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
# The memory of removed containers (namespaces, cgroups) is freed later, for hundreds of
# containers over many seconds: wait until what the system holds has stopped falling for
# three readings in a row (at most 90 s), so that one run's leftovers do not count in the
# next one.
settle() {
  local prev cur still=0
  prev="$(held_kb)"
  for _ in $(seq 45); do
    sleep 2
    cur="$(held_kb)"
    if [ $((prev - cur)) -gt 256 ]; then still=0; else still=$((still + 1)); fi
    [ "$still" -lt 3 ] || return 0
    prev="$cur"
  done
}
pss_kb() {
  local sum=0 pid kb
  for pid in $(ps -eo pid=,comm= | awk -v re="$proc_pattern" '$2 ~ re {print $1}'); do
    kb="$(sudo -n awk '/^Pss:/{print $2}' "/proc/$pid/smaps_rollup" 2>/dev/null || echo 0)"
    sum=$((sum + ${kb:-0}))
  done
  echo "$sum"
}
procs() { ps -eo pid=,comm= | awk -v re="$proc_pattern" '$2 ~ re' | wc -l; }
times="$out/scale-times-$tool.txt"
window() { sed -n "$1,$2p" "$times" | sort -n | awk '{v[NR]=$1} END{printf "%.1f", v[int((NR+1)/2)]/1000}'; }
# Start containers c$1..c$2, one after another; each start's time (µs) goes to $times.
start_range() {
  "${timed_prefix[@]}" bash -c '
    first=$1 last=$2 image=$3 times=$4; shift 4
    for i in $(seq "$first" "$last"); do
      t0="${EPOCHREALTIME/./}"
      "$@" run -d --network bench --name "c$i" "$image" sleep 36000 >/dev/null
      t1="${EPOCHREALTIME/./}"
      echo $((t1 - t0)) >>"$times"
    done' _ "$1" "$2" "$alpine" "$times" "${inner[@]}"
}
# The time of one command, in milliseconds.
timed_ms() {
  "${timed_prefix[@]}" bash -c '
    t0="${EPOCHREALTIME/./}"; "$@" >/dev/null; t1="${EPOCHREALTIME/./}"
    echo $(((t1 - t0) / 1000))' _ "$@"
}

echo "== scale: $tool, $total containers" >&2
"${cli[@]}" pull "$alpine" >/dev/null
# A /22: a /24 network holds 253 containers.
"${cli[@]}" network create --subnet 10.90.0.0/22 bench >/dev/null
drop_caches
settle
before="$(used_kb)"
held_before="$(held_kb)"
bystander_before="$(bystander_kb)"
: >"$times"
start_range 1 50
sleep 3
drop_caches
mem_50=$(($(used_kb) - before))
held_50=$(($(held_kb) - held_before))
pss_50="$(pss_kb)"
start_range 51 "$total"
sleep 3
drop_caches
mem_total=$(($(used_kb) - before))
held_total=$(($(held_kb) - held_before))
pss_total="$(pss_kb)"
procs_total="$(procs)"
bystander_total=$(($(bystander_kb) - bystander_before))
# The last container reaches the first by name.
"${cli[@]}" exec "c$total" ping -c 1 -W 2 c1 >/dev/null
# The caches were dropped just now for the memory reading: the first `ps -a` reads every
# container's record from the disk (a tool with a daemon has them in memory). Then five
# more, for the usual case.
ps_first_ms="$(timed_ms "${inner[@]}" ps -a)"
ps_ms="$(for _ in 1 2 3 4 5; do timed_ms "${inner[@]}" ps -a; done | sort -n | sed -n 3p)"
rm_ms="$("${timed_prefix[@]}" bash -c '
  t0="${EPOCHREALTIME/./}"; "$@" rm -f $("$@" ps -aq) >/dev/null; t1="${EPOCHREALTIME/./}"
  echo $(((t1 - t0) / 1000))' _ "${inner[@]}")"
"${cli[@]}" network rm bench >/dev/null

{
  echo "tool=$tool"
  echo "total=$total"
  echo "start_first10_ms=$(window 1 10)"
  echo "start_at50_ms=$(window 41 50)"
  echo "start_last10_ms=$(window $((total - 9)) "$total")"
  echo "mem_50_kb=$mem_50"
  echo "held_50_kb=$held_50"
  echo "pss_50_kb=$pss_50"
  echo "mem_total_kb=$mem_total"
  echo "held_total_kb=$held_total"
  echo "pss_total_kb=$pss_total"
  echo "procs_total=$procs_total"
  echo "bystander_total_kb=$bystander_total"
  echo "ps_first_ms=$ps_first_ms"
  echo "ps_ms=$ps_ms"
  echo "rm_all_ms=$rm_ms"
} >"$out/scale-$tool.env"
cat "$out/scale-$tool.env"
