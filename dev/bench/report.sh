#!/usr/bin/env bash
# Write docs/benchmarks.md: the Linux tables from dev/bench/run.sh and scale.sh results (run
# in the VM), the macOS table from dev/bench/mac.py results (bench-results/mac in the
# repository, measured on the Mac), then the reading of them in dev/bench/notes.md.
#
# usage: dev/bench/report.sh [results-dir]      (run it in the VM)
set -euo pipefail

in="${1:-$HOME/bench-results}"
repo="$(cd "$(dirname "$0")/../.." && pwd)"
doc="$repo/docs/benchmarks.md"
all_tools=(podbag podbag-root podman docker kiri)
all_titles=("podbag (rootless)" "podbag (root)" "Podman (rootless)" "Docker (root daemon)" "Kiri (root daemon)")
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
# The first start when nothing of the tool runs: podbag starts its keeper then. Podman has
# nothing to start first; Docker's and Kiri's daemons stay, so they have no such start.
cold() {
  local v
  v="$(get "$1" start_cold_median_ms)"
  if [ -n "$v" ]; then
    echo "$v"
  elif [ "$1" = podman ]; then
    echo "$(get "$1" start_median_ms) (no daemon)"
  else
    echo "— (its daemon stays)"
  fi
}
# The three rounds of one result, in MiB.
rounds() { get "$1" "$2" | tr -d '"' | awk '{for (i = 1; i <= NF; i++) printf "%s%.1f", (i > 1 ? ", " : ""), $i/1024}'; }
mem_none() { sys_mb "$(get "$1" mem_none_kb)"; }
mem_default() { sys_mb "$(get "$1" mem_default_kb)"; }
held_none() { mb "$(get "$1" held_none_kb)"; }
held_default() { mb "$(get "$1" held_default_kb)"; }
# The largest difference between the three rounds of one tool, over the given tools (MiB).
held_spread() {
  for t in "$@"; do
    for net in none default; do get "$t" "held_${net}_all_kb" | tr -d '"'; done
  done | awk '{lo=$1; hi=$1; for(i=2;i<=NF;i++){if($i<lo)lo=$i; if($i>hi)hi=$i}
      # As the rounds are printed: each rounded to a tenth of a MiB.
      d=int(hi/1024*10+0.5)/10 - int(lo/1024*10+0.5)/10; if(d>m)m=d} END{printf "%.1f", m}'
}
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

count="$(get podbag count)"
runs="$(get podbag runs)"
mac="$repo/bench-results/mac"
mac_tools=(docker-desktop orbstack kiri podman colima podbag-mac podbag-mac-default)
{
  echo "# Benchmarks"
  echo
  cat <<'EOF'
- Two comparisons:
  - container engines on one Linux host (no VM; the same kernel for all),
  - the tools Mac users run, each with its own Linux VM.
- Numbers compare only within one table: each table was measured in one session.
- A node of Spring microservices in two releases: [benchmarks-msa.md](benchmarks-msa.md).
- `dev/bench/report.sh` writes this page.

## Contents

- [Linux host, no VM](#linux-host-no-vm)
- [macOS, each tool with its own VM](#macos-each-tool-with-its-own-vm)
- [300 containers on one network](#300-containers-on-one-network)
- [Reading the numbers](#reading-the-numbers)
EOF
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
  row "The same start when nothing of the tool runs yet, median (ms)" cold
  row "$count sleeping containers, no network: memory the system holds, by category" held_none
  row "$count sleeping containers, no network: the tool's processes (PSS)" pss_none
  row "$count sleeping containers, no network: change in MemTotal − MemAvailable (noisy, see below)" mem_none
  row "$count sleeping containers, default network: memory the system holds, by category" held_default
  row "$count sleeping containers, default network: the tool's processes (PSS)" pss_default
  row "$count sleeping containers, default network: change in MemTotal − MemAvailable (noisy, see below)" mem_default
  row "Tool processes with $count containers: no network / default network" procs
  row "Tool processes with no containers: count, PSS" idle
  row "Disk: \`python:3.12-slim-bookworm\`" disk_first
  row "Disk: then \`python:3.12-slim-trixie\` adds" disk_second
  row "Disk: both images" disk_both
  echo
  echo "- \"Memory the system holds, by category\" is the change in AnonPages + Shmem + Slab +"
  echo "  KernelStack + PageTables + Percpu + VmallocUsed of \`/proc/meminfo\`: what processes and"
  echo "  the kernel hold, without the page cache. Caches are dropped before each reading; the"
  echo "  value is the median of three rounds; the rounds of one tool differ by at most"
  echo "  $(held_spread podbag podbag-root kiri) MiB for podbag and Kiri ($(held_spread podman docker) MiB for Podman and Docker). It includes"
  echo "  the containers' own processes (the same $count \`sleep\` for every tool) and the kernel's"
  echo "  memory for their namespaces and cgroups, not daemons that already ran (Docker's), and"
  echo "  not pages the kernel takes straight from its page allocator (they are in no category)."
  echo "- \"Change in MemTotal − MemAvailable\" is the kernel's estimate of memory that is no longer"
  echo "  available, read at the same moments. It cannot tell the small tools apart. The three"
  echo "  rounds without a network gave, in MiB: podbag rootless $(rounds podbag mem_none_all_kb); podbag as root"
  echo "  $(rounds podbag-root mem_none_all_kb); Kiri $(rounds kiri mem_none_all_kb). The sums by category of the same rounds:"
  echo "  $(rounds podbag held_none_all_kb); $(rounds podbag-root held_none_all_kb); $(rounds kiri held_none_all_kb). Where this row and the sum"
  echo "  by category disagree, no counter of the kernel says where the memory would be; in a"
  echo "  separate reading three identical rounds of Kiri moved free memory by −9.5, −17.3 and"
  echo "  +6.4 MiB. The row stays for the large numbers."
  echo "- lima's guest agent was stopped for the session (\`dev/bench/session.sh\`): it grew by"
  echo "  23 MiB during one run of 150 containers, shrank by 46 MiB during the next, and took a"
  echo "  fifth of the CPU time while containers started. A Linux host has no guest agent. Both"
  echo "  system-wide rows also leave its anonymous memory out, should it run."
  echo "- \"The tool's processes\": podbag's keeper (and a helper for each container that is"
  echo "  attached or publishes ports), session holder and pasta; Podman's"
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
    m_own() { local v; v="$(m "$1" own_start_median_ms)"; if [ -n "$v" ]; then echo "$v / $(m "$1" own_start_p90_ms)"; else echo "—"; fi; }
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
    # Each column's date; results from before the date was recorded per tool carry mac.env's.
    m_date() { local d; d="$(m "$1" date)"; echo "${d:-$(sed -n 's/^date=//p' "$mac/mac.env")}"; }
    od="$(m_date podbag-mac)"; kd="$(m_date kiri)"
    if [ "$od" = "$kd" ]; then
      echo "macOS $(sed -n 's/^macos=//p' "$mac/mac.env"), $od. One tool ran at a time; the others were stopped."
    else
      echo "macOS $(sed -n 's/^macos=//p' "$mac/mac.env"). The other tools on $kd, one at a time with the others stopped;"
      echo "podbag on $od, while Docker Desktop's app (its VM stopped) and the lima VM podbag is developed in ran."
    fi
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
    mrow "… through podbag's own CLI (\`podbag run\`), median / p90 (ms)" m_own
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
    echo "  takes from the Mac. podbag is shown at that size and at its own default (the"
    echo "  Mac's CPUs up to 8, 2 GiB): what its VM holds grows with the size, as the guest"
    echo "  kernel sets up its bookkeeping for all of the VM's memory when it boots."
    echo "- Docker Desktop, OrbStack, Kiri, Colima and podbag were driven by the same docker"
    echo "  CLI through their sockets, Podman by its own CLI. podbag runs a VM of its own"
    echo "  (Virtualization.framework, Linux 6.18 with podbag as its init); it powers the VM"
    echo "  off 60 s after the last container and leaves a small standby process, which starts"
    echo "  the VM again for the next client."
    echo "- \"Memory on the Mac\" is the sum of macOS physical footprint (\`footprint\`) of the"
    echo "  tool's app, helpers and VM process; root helpers by resident size. Other VMs on the Mac"
    echo "  are not counted (another project's VM ran during the measurements)."
    echo "- Containers on the default network; the script removes only the containers it labels"
    echo "  (podbag's are in a home of its own), so Docker Desktop's existing images and"
    echo "  containers stayed. For that reason disk use is compared in the Linux table only."
  fi
  # The many-containers run: a column for each tool that was measured.
  scale_tools=()
  scale_titles=()
  for i in "${!all_tools[@]}"; do
    if [ -f "$in/scale-${all_tools[$i]}.env" ]; then
      scale_tools+=("${all_tools[$i]}")
      scale_titles+=("${all_titles[$i]}")
    fi
  done
  if [ "${#scale_tools[@]}" -gt 0 ]; then
    sg() { sed -n "s/^$2=//p" "$in/scale-$1.env" | head -1; }
    srow() {
      local label="$1"; shift
      local line="| $label |"
      for t in "${scale_tools[@]}"; do line="$line $("$@" "$t") |"; done
      echo "$line"
    }
    s_ms() { echo "$(sg "$2" "$1") ms"; }
    s_mem() { echo "$(mb "$(sg "$2" "held_$1_kb")") / $(mb "$(sg "$2" "pss_$1_kb")")"; }
    s_procs() { sg "$1" procs_total; }
    total="$(sg "${scale_tools[0]}" total)"
    echo
    echo "## $total containers on one network"
    echo
    echo "\`dev/bench/scale.sh\`: \`run -d --network bench --name cN alpine sleep\`, one after another,"
    echo "on a network made with \`network create --subnet 10.90.0.0/22 bench\`; the same program and"
    echo "VM as the table above. \`rm -f\` gets all $total containers on one command line."
    scale_rounds="$(ls "$in"/scale-"${scale_tools[0]}".round* 2>/dev/null | wc -l | tr -d ' ')"
    if [ "$scale_rounds" -gt 1 ]; then
      # One figure of every round of a tool, in the round's order.
      s_rounds() { # $1 tool, $2 key, $3 kb|ms
        local out="" f v
        for f in "$in"/scale-"$1".round*; do
          v="$(sed -n "s/^$2=//p" "$f" | head -1)"
          [ "$3" = kb ] && v="$(awk -v k="$v" 'BEGIN{printf "%.0f", k/1024}')"
          out="$out${out:+, }$v"
        done
        echo "$out"
      }
      echo
      echo "Every tool was run $scale_rounds times, in turn, after one round that is not counted"
      echo "(see \"The state of the system\" below); each figure is the median of its"
      echo "$scale_rounds readings. The single runs of three rows, in the order of the columns:"
      echo
      line="- what the system holds at $total, MiB:"
      for t in "${scale_tools[@]}"; do line="$line $(s_rounds "$t" held_total_kb kb);"; done
      echo "${line%;}"
      line="- the first \`ps -a\` after the caches were dropped, ms:"
      for t in "${scale_tools[@]}"; do line="$line $(s_rounds "$t" ps_first_ms ms);"; done
      echo "${line%;}"
      line="- \`rm -f\` of all $total, ms:"
      for t in "${scale_tools[@]}"; do line="$line $(s_rounds "$t" rm_all_ms ms);"; done
      echo "${line%;}"
    fi
    echo
    header="| |"
    rule="|---|"
    for t in "${scale_titles[@]}"; do
      header="$header $t |"
      rule="$rule---|"
    done
    echo "$header"
    echo "$rule"
    srow "Start, containers 1–10 (median)" s_ms start_first10_ms
    srow "Start, containers 41–50 (median)" s_ms start_at50_ms
    srow "Start, the last 10 (median)" s_ms start_last10_ms
    srow "Memory at 50: the system holds, by category / the tool's processes (PSS)" s_mem 50
    srow "Memory at $total: the system holds, by category / the tool's processes (PSS)" s_mem total
    srow "Tool processes at $total" s_procs
    s_ps() { echo "$(sg "$1" ps_ms) ms ($(sg "$1" ps_first_ms) ms)"; }
    srow "\`ps -a\` with $total containers (the first one after the caches were dropped)" s_ps
    srow "\`rm -f\` of all $total" s_ms rm_all_ms
  fi
  if [ -f "$repo/dev/bench/notes.md" ]; then
    echo
    cat "$repo/dev/bench/notes.md"
  fi
} >"$doc"
echo "wrote $doc"
