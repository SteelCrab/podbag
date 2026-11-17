#!/usr/bin/env bash
# Measure one container tool in the openpod-dev VM (design 9):
#   - start time of `run --rm --network none alpine true` (median and p90),
#   - memory for N sleeping containers, without a network and on the default network,
#   - processes the tool keeps with no containers,
#   - disk use of two Python images that share their Python files but not their base.
# Results go to <results-dir>/<tool>.env as key=value lines (dev/bench/report.sh reads them).
#
# usage: dev/bench/run.sh openpod|openpod-root|podman|docker [results-dir]
# Needs `dev/vm.sh bench-setup` (Docker, Podman) and `dev/vm.sh static` (openpod).
set -euo pipefail

tool="${1:?usage: dev/bench/run.sh openpod|openpod-root|podman|docker [results-dir]}"
out="${2:-$HOME/bench-results}"
runs="${BENCH_RUNS:-100}"
count="${BENCH_COUNT:-50}"
here="$(cd "$(dirname "$0")" && pwd)"
alpine="docker.io/library/alpine:3.22"
py1="docker.io/library/python:3.12-slim-bookworm"
py2="docker.io/library/python:3.12-slim-trixie"
static="${CARGO_TARGET_DIR:-$HOME/openpod-target}/$(uname -m)-unknown-linux-musl/release/openpod"
mkdir -p "$out"

case "$tool" in
  openpod)
    export OPENPOD_DATA_DIR=/var/tmp/openpod-bench/data
    export OPENPOD_RUNTIME_DIR="$XDG_RUNTIME_DIR/openpod-bench"
    cli=("$static")
    # The shims, the session holder and pasta.
    proc_pattern='^(openpod|pasta.*)$'
    store=("$OPENPOD_DATA_DIR")
    version="$("$static" --version)"
    ;;
  openpod-root)
    cli=(sudo -n env OPENPOD_DATA_DIR=/var/tmp/openpod-bench-root/data
      OPENPOD_RUNTIME_DIR=/run/openpod-bench-root "$static")
    proc_pattern='^(openpod)$'
    store=(/var/tmp/openpod-bench-root/data)
    version="$("$static" --version) (root)"
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
used_kb() { awk '/^MemTotal/{t=$2} /^MemAvailable/{a=$2} END{print t-a}' /proc/meminfo; }
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

results="$out/$tool.env"
{
  echo "tool=$tool"
  echo "version=$version"
  echo "runs=$runs"
  echo "count=$count"
  echo "idle_procs=$(tool_procs)"
  echo "idle_pss_kb=$(tool_pss_kb)"
} >"$results"

# 1. Start time. The first runs warm caches and are not counted.
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

# 2. Memory for $count sleeping containers. The system-wide difference is noisy (other
# processes, the kernel's caches), so it is measured three times; PSS is exact.
for net in none default; do
  net_args=()
  [ "$net" = default ] || net_args=(--network none)
  deltas=()
  for round in 1 2 3; do
    echo "memory: $count containers, network $net, round $round" >&2
    drop_caches
    before="$(used_kb)"
    for i in $(seq "$count"); do
      "${timed_prefix[@]}" "${cli[@]}" run -d "${net_args[@]}" "$alpine" sleep 3600 >/dev/null
    done
    sleep 5
    drop_caches
    deltas+=($(($(used_kb) - before)))
    pss="$(tool_pss_kb)"
    nprocs="$(tool_procs)"
    remove_all
  done
  {
    echo "mem_${net}_kb=$(printf '%s\n' "${deltas[@]}" | sort -n | sed -n 2p)"
    echo "pss_${net}_kb=$pss"
    echo "procs_${net}=$nprocs"
  } >>"$results"
done

# 3. Disk: two images that differ in their Debian base but share Python's files.
echo "disk: $py1, then $py2" >&2
prune() {
  case "$tool" in
    openpod*) "${timed_prefix[@]}" "${cli[@]}" system prune -f ;;
    *) "${timed_prefix[@]}" "${cli[@]}" system prune -af ;;
  esac >/dev/null
  sleep 5
}
prune
base="$(store_kb)"
"${timed_prefix[@]}" "${cli[@]}" pull "$py1" >/dev/null
one="$(store_kb)"
"${timed_prefix[@]}" "${cli[@]}" pull "$py2" >/dev/null
two="$(store_kb)"
{
  echo "disk_first_kb=$((one - base))"
  echo "disk_both_kb=$((two - base))"
} >>"$results"
prune

cat "$results"
