#!/usr/bin/env bash
# Write docs/benchmarks.md from dev/bench/run.sh and scale.sh results, followed by the
# reading of them in dev/bench/notes.md.
#
# usage: dev/bench/report.sh [results-dir]
set -euo pipefail

in="${1:-$HOME/bench-results}"
repo="$(cd "$(dirname "$0")/../.." && pwd)"
doc="$repo/docs/benchmarks.md"
tools=(openpod openpod-root podman docker)
titles=("openpod (rootless)" "openpod (root)" "Podman (rootless)" "Docker (root daemon)")

get() { sed -n "s/^$2=//p" "$in/$1.env" 2>/dev/null | head -1; }
mb() { awk -v k="${1:-0}" 'BEGIN{printf "%.1f MiB", k/1024}'; }
# The system-wide change: values below 1 MiB (even below zero) are within its noise.
sys_mb() { awk -v k="${1:-0}" 'BEGIN{if (k < 1024) print "within noise"; else printf "%.1f MiB", k/1024}'; }
# What each container adds: the total less what the tool uses with no containers.
per() { awk -v k="${1:-0}" -v idle="${2:-0}" -v n="${3:-1}" 'BEGIN{printf "%.0f KiB", (k-idle)/n}'; }
row() {
  local label="$1"; shift
  local line="| $label |"
  for t in "${tools[@]}"; do line="$line $("$@" "$t") |"; done
  echo "$line"
}
start() { echo "$(get "$1" start_median_ms) / $(get "$1" start_p90_ms)"; }
mem_none() { sys_mb "$(get "$1" mem_none_kb)"; }
mem_default() { sys_mb "$(get "$1" mem_default_kb)"; }
pss_none() { echo "$(mb "$(get "$1" pss_none_kb)") ($(per "$(get "$1" pss_none_kb)" "$(get "$1" idle_pss_kb)" "$(get "$1" count)") per container)"; }
pss_default() { echo "$(mb "$(get "$1" pss_default_kb)") ($(per "$(get "$1" pss_default_kb)" "$(get "$1" idle_pss_kb)" "$(get "$1" count)") per container)"; }
procs() { echo "$(get "$1" procs_none) / $(get "$1" procs_default)"; }
idle() {
  if [ "$(get "$1" idle_procs)" = 0 ]; then echo "none"; else echo "$(get "$1" idle_procs), $(mb "$(get "$1" idle_pss_kb)")"; fi
}
disk_first() { mb "$(get "$1" disk_first_kb)"; }
disk_both() { mb "$(get "$1" disk_both_kb)"; }
disk_second() { mb $(($(get "$1" disk_both_kb) - $(get "$1" disk_first_kb))); }
version() { get "$1" version; }

count="$(get openpod count)"
runs="$(get openpod runs)"
{
  echo "# Benchmarks"
  echo
  echo "Measured with \`dev/bench/run.sh\` and \`dev/bench/scale.sh\`; this file is written by"
  echo "\`dev/bench/report.sh\`. All tools ran one after another in the same VM:"
  echo "$(. /etc/os-release && echo "$PRETTY_NAME"), kernel $(uname -r), $(uname -m),"
  echo "$(nproc) CPUs, $(awk '/^MemTotal/{printf "%.1f GiB", $2/1048576}' /proc/meminfo) of memory (lima on macOS)."
  echo
  echo "| | ${titles[0]} | ${titles[1]} | ${titles[2]} | ${titles[3]} |"
  echo "|---|---|---|---|---|"
  row "Version" version
  row "Start: \`run --rm --network none alpine true\`, median / p90 (ms, $runs runs)" start
  row "$count sleeping containers, no network: change in the system's used memory" mem_none
  row "$count sleeping containers, no network: the tool's processes (PSS)" pss_none
  row "$count sleeping containers, default network: change in the system's used memory" mem_default
  row "$count sleeping containers, default network: the tool's processes (PSS)" pss_default
  row "Tool processes with $count containers: no network / default network" procs
  row "Tool processes with no containers: count, PSS" idle
  row "Disk: \`python:3.12-slim-bookworm\`" disk_first
  row "Disk: then \`python:3.12-slim-trixie\` adds" disk_second
  row "Disk: both images" disk_both
  echo
  echo "- \"Change in the system's used memory\" is MemTotal − MemAvailable after minus before (caches"
  echo "  dropped), the median of three rounds. It includes the containers' own processes and"
  echo "  the kernel's memory for namespaces and cgroups; it moves by 10–20 MiB between runs. It does not"
  echo "  include daemons that were already running (Docker's)."
  echo "- \"The tool's processes\": openpod's shims, session holder and pasta; Podman's"
  echo "  conmon, pasta and helpers; Docker's dockerd, containerd and shims. PSS divides shared"
  echo "  pages among the processes that share them. \"Per container\" is (total − the tool's"
  echo "  processes with no containers) / $count: a container's own helper plus any growth of the"
  echo "  daemons. The two memory rows are measured differently and are not parts of each other:"
  echo "  for Docker, the PSS row counts dockerd and containerd, the system row does not."
  echo "- Disk is \`du\` of the tool's whole store (hard links counted once)."
  if [ -f "$in/scale.env" ]; then
    s() { sed -n "s/^$1=//p" "$in/scale.env"; }
    echo
    echo "## $(s total) containers on one network (openpod, rootless)"
    echo
    echo "\`dev/bench/scale.sh\`: \`run -d --network bench --name cN alpine sleep\`, one after another."
    echo
    echo "| | |"
    echo "|---|---|"
    echo "| Start, containers 1–10 (median) | $(s start_first10_ms) ms |"
    echo "| Start, containers 41–50 (median) | $(s start_at50_ms) ms |"
    echo "| Start, the last 10 (median) | $(s start_last10_ms) ms |"
    echo "| Memory at 50: change in the system's used memory / openpod's processes (PSS) | $(sys_mb "$(s mem_50_kb)") / $(mb "$(s pss_50_kb)") |"
    echo "| Memory at $(s total): change in the system's used memory / openpod's processes (PSS) | $(sys_mb "$(s mem_total_kb)") / $(mb "$(s pss_total_kb)") |"
    echo "| \`ps -a\` with $(s total) containers | $(s ps_ms) ms |"
    echo "| \`rm -f\` of all $(s total) | $(s rm_all_ms) ms |"
  fi
  if [ -f "$repo/dev/bench/notes.md" ]; then
    echo
    cat "$repo/dev/bench/notes.md"
  fi
} >"$doc"
echo "wrote $doc"
