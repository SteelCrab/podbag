#!/usr/bin/env bash
# Write docs/benchmarks.md: the Linux table from dev/bench/run.sh and scale.sh results (run
# in the VM), the macOS table from dev/bench/mac.py results (bench-results/mac in the
# repository, measured on the Mac), then the reading of them in dev/bench/notes.md.
#
# usage: dev/bench/report.sh [results-dir]      (run it in the VM)
set -euo pipefail

in="${1:-$HOME/bench-results}"
repo="$(cd "$(dirname "$0")/../.." && pwd)"
doc="$repo/docs/benchmarks.md"
all_tools=(openpod openpod-root podman docker kiri)
all_titles=("openpod (rootless)" "openpod (root)" "Podman (rootless)" "Docker (root daemon)" "Kiri (root daemon)")
# Only the tools that were measured.
tools=()
titles=()
for i in "${!all_tools[@]}"; do
  if [ -f "$in/${all_tools[$i]}.env" ]; then
    tools+=("${all_tools[$i]}")
    titles+=("${all_titles[$i]}")
  fi
done

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
mac="$repo/bench-results/mac"
mac_tools=(docker-desktop orbstack kiri podman colima openpod)
{
  echo "# Benchmarks"
  echo
  echo "Two comparisons, written by \`dev/bench/report.sh\`: container engines on one Linux host"
  echo "(no VM; the same kernel for all), and the tools Mac users run, each with its own Linux VM."
  echo "Numbers compare only within one table: each table was measured in one session."
  echo
  echo "## Linux host, no VM"
  echo
  echo "Measured with \`dev/bench/run.sh\` and \`dev/bench/scale.sh\`, one tool after another in the"
  echo "same VM: $(. /etc/os-release && echo "$PRETTY_NAME"), kernel $(uname -r), $(uname -m),"
  echo "$(nproc) CPUs, $(awk '/^MemTotal/{printf "%.1f GiB", $2/1048576}' /proc/meminfo) of memory (lima on macOS)."
  echo "OrbStack has no Linux version; Docker Desktop runs this Docker Engine in its VM."
  echo
  header="| |"
  rule="|---|"
  for t in "${titles[@]}"; do
    header="$header $t |"
    rule="$rule---|"
  done
  echo "$header"
  echo "$rule"
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
  echo "  conmon, pasta and helpers; Docker's dockerd, containerd and shims; Kiri's daemon and"
  echo "  the one init process all its containers share. PSS divides shared"
  echo "  pages among the processes that share them. \"Per container\" is (total − the tool's"
  echo "  processes with no containers) / $count: a container's own helper plus any growth of the"
  echo "  daemons. The two memory rows are measured differently and are not parts of each other:"
  echo "  for Docker, the PSS row counts dockerd and containerd, the system row does not."
  echo "- Disk is what the images add to \`du\` of the tool's whole store (hard links counted once),"
  echo "  each image pulled and then used by one container; the store already holds alpine."
  if [ -f "$mac/mac.env" ]; then
    m() { sed -n "s/^$2=//p" "$mac/$1.env" 2>/dev/null | head -1; }
    mem_h() { awk -v k="${1:-0}" 'BEGIN{if (k >= 1048576) printf "%.2f GiB", k/1048576; else if (k >= 10240) printf "%.0f MiB", k/1024; else printf "%.1f MiB", k/1024}'; }
    mrow() {
      local label="$1" key="$2"; shift 2
      local line="| $label |"
      for t in "${present[@]}"; do line="$line $("$key" "$t") |"; done
      echo "$line"
    }
    present=()
    for t in "${mac_tools[@]}"; do [ -f "$mac/$t.env" ] && present+=("$t"); done
    m_version() { m "$1" version; }
    m_vm() { echo "$(m "$1" vm_cpus) CPUs, $(mem_h "$(m "$1" vm_mem_kb)")"; }
    m_start() { echo "$(m "$1" start_median_ms) / $(m "$1" start_p90_ms)"; }
    m_wake() { m "$1" wake_ms; }
    m_idle() { mem_h "$(m "$1" mem_idle_kb)"; }
    m_busy() { mem_h "$(m "$1" mem_busy_kb)"; }
    m_after30() { mem_h "$(m "$1" mem_after30_kb)"; }
    m_after6m() { mem_h "$(m "$1" mem_after6m_kb)"; }
    vm_h() { if [ "${1:-0}" = 0 ]; then echo "VM stopped"; else mem_h "$1"; fi; }
    m_vm_mem() { echo "$(vm_h "$(m "$1" vm_busy_kb)") / $(vm_h "$(m "$1" vm_after6m_kb)")"; }
    m_procs() { echo "$(m "$1" procs_idle) / $(m "$1" procs_busy)"; }
    mruns="$(m "${present[0]}" runs)"
    mcount="$(m "${present[0]}" count)"
    echo
    echo "## macOS, each tool with its own VM"
    echo
    echo "Measured with \`dev/bench/mac.py\` from the Mac: $(sed -n 's/^chip=//p' "$mac/mac.env"),"
    echo "$(sed -n 's/^cpus=//p' "$mac/mac.env") CPUs, $(awk -v k="$(sed -n 's/^mem_kb=//p' "$mac/mac.env")" 'BEGIN{printf "%.0f GiB", k/1048576}'),"
    echo "macOS $(sed -n 's/^macos=//p' "$mac/mac.env"), $(sed -n 's/^date=//p' "$mac/mac.env"). One tool ran at a time; the others were stopped."
    echo
    header="| |"
    rule="|---|"
    for t in "${present[@]}"; do
      header="$header $(m "$t" title) |"
      rule="$rule---|"
    done
    echo "$header"
    echo "$rule"
    mrow "Version" m_version
    mrow "VM size" m_vm
    mrow "Start: \`run --rm --network none alpine true\`, median / p90 (ms, $mruns runs)" m_start
    mrow "First \`run\` after 6 minutes without containers (ms)" m_wake
    mrow "Memory on the Mac (the tool's processes, VM included): just started" m_idle
    mrow "… with $mcount sleeping containers" m_busy
    mrow "… 30 s after they are removed" m_after30
    mrow "… 6 minutes after they are removed" m_after6m
    mrow "Of that, the VM: $mcount containers / 6 minutes after" m_vm_mem
    mrow "Processes on the Mac: just started / $mcount containers" m_procs
    echo
    echo "- Every VM was set to 10 CPUs and 8 GiB, Docker Desktop's default here (the script"
    echo "  cannot change Docker Desktop's settings); OrbStack's VM reports more than the 8 GiB"
    echo "  it was limited to. The size is an upper limit: the memory rows show what each VM"
    echo "  takes from the Mac."
    echo "- Docker Desktop, OrbStack, Kiri and Colima were driven by the same docker CLI through"
    echo "  their sockets, Podman by its own CLI. openpod has no macOS front end yet (design 7):"
    echo "  it runs in lima's Ubuntu VM and its CLI is reached over ssh, which alone takes"
    echo "  $(m openpod transport_ms) ms of each start; its memory is mostly that Ubuntu system."
    echo "- \"Memory on the Mac\" is the sum of macOS physical footprint (\`footprint\`) of the"
    echo "  tool's app, helpers and VM process; root helpers by resident size. Other VMs on the Mac"
    echo "  are not counted (another project's VM ran during the measurements)."
    echo "- Containers on the default network; the script removes only the containers it labels,"
    echo "  so Docker Desktop's existing images and containers stayed. For that reason disk use is"
    echo "  compared in the Linux table only."
  fi
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
