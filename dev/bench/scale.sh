#!/usr/bin/env bash
# Many containers on one user network, rootless (design 9: 300 containers): does starting
# get slower as the network fills (locks, hosts files, the shared session)? Times every
# start; memory at 50 and at the end. Results go to <results-dir>/scale.env.
#
# usage: dev/bench/scale.sh [results-dir]      (BENCH_SCALE: how many, default 300)
set -euo pipefail

out="${1:-$HOME/bench-results}"
total="${BENCH_SCALE:-300}"
alpine="docker.io/library/alpine:3.22"
bin="${CARGO_TARGET_DIR:-$HOME/openpod-target}/$(uname -m)-unknown-linux-musl/release/openpod"
export OPENPOD_DATA_DIR=/var/tmp/openpod-scale/data
export OPENPOD_RUNTIME_DIR="$XDG_RUNTIME_DIR/openpod-scale"
mkdir -p "$out"

drop_caches() { sync; sudo -n sh -c 'echo 3 > /proc/sys/vm/drop_caches'; sleep 2; }
used_kb() { awk '/^MemTotal/{t=$2} /^MemAvailable/{a=$2} END{print t-a}' /proc/meminfo; }
pss_kb() {
  local sum=0 pid kb
  for pid in $(ps -eo pid=,comm= | awk '$2 ~ /^(openpod|pasta.*)$/ {print $1}'); do
    kb="$(sudo -n awk '/^Pss:/{print $2}' "/proc/$pid/smaps_rollup" 2>/dev/null || echo 0)"
    sum=$((sum + ${kb:-0}))
  done
  echo "$sum"
}
# Median start time (ms) of containers from..to.
window() { sed -n "$1,$2p" "$out/scale-times.txt" | sort -n | awk '{v[NR]=$1} END{printf "%.1f", v[int((NR+1)/2)]/1000}'; }

"$bin" pull "$alpine" >/dev/null
# A /22: a /24 network holds 253 containers.
"$bin" network create --subnet 10.90.0.0/22 bench >/dev/null
drop_caches
before="$(used_kb)"
: >"$out/scale-times.txt"
for i in $(seq "$total"); do
  t0="${EPOCHREALTIME/./}"
  "$bin" run -d --network bench --name "c$i" "$alpine" sleep 36000 >/dev/null
  t1="${EPOCHREALTIME/./}"
  echo $((t1 - t0)) >>"$out/scale-times.txt"
  if [ "$i" -eq 50 ]; then
    sleep 3
    drop_caches
    echo "mem_50_kb=$(($(used_kb) - before))" >"$out/scale.env.part"
    echo "pss_50_kb=$(pss_kb)" >>"$out/scale.env.part"
  fi
  [ $((i % 50)) -ne 0 ] || echo "started $i" >&2
done
sleep 3
drop_caches
mem_total=$(($(used_kb) - before))
pss_total="$(pss_kb)"
# The last container reaches the first by name.
"$bin" exec "c$total" ping -c 1 -W 2 c1 >/dev/null
t0="${EPOCHREALTIME/./}"
"$bin" ps -a >/dev/null
t1="${EPOCHREALTIME/./}"
ps_ms=$(((t1 - t0) / 1000))
t0="${EPOCHREALTIME/./}"
"$bin" rm -f $("$bin" ps -aq) >/dev/null
t1="${EPOCHREALTIME/./}"
rm_ms=$(((t1 - t0) / 1000))
"$bin" network rm bench >/dev/null

{
  echo "total=$total"
  echo "start_first10_ms=$(window 1 10)"
  echo "start_at50_ms=$(window 41 50)"
  echo "start_last10_ms=$(window $((total - 9)) "$total")"
  cat "$out/scale.env.part"
  echo "mem_total_kb=$mem_total"
  echo "pss_total_kb=$pss_total"
  echo "ps_ms=$ps_ms"
  echo "rm_all_ms=$rm_ms"
} >"$out/scale.env"
rm -f "$out/scale.env.part"
cat "$out/scale.env"
