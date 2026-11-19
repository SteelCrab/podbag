#!/usr/bin/env bash
# Write docs/benchmarks-k8s.md from the results of dev/bench/k8s/session.sh, then the
# reading of them in dev/bench/k8s/notes.md.
#
# usage: dev/bench/k8s/report.sh [results-dir]
set -euo pipefail

in="${1:-$HOME/bench-results/k8s}"
repo="$(cd "$(dirname "$0")/../../.." && pwd)"
doc="$repo/docs/benchmarks-k8s.md"
all_rts=(containerd crio podbag)
all_titles=("containerd (k3s's own)" "CRI-O" "podbag")
rts=()
titles=()
for i in "${!all_rts[@]}"; do
  if [ -f "$in/${all_rts[$i]}.env" ]; then
    rts+=("${all_rts[$i]}")
    titles+=("${all_titles[$i]}")
  fi
done
[ "${#rts[@]}" -gt 0 ] || { echo "no results in $in" >&2; exit 1; }

# A value of a runtime: from a later disk-only run if there is one, else from the session.
get() {
  local v=""
  [ ! -f "$in/$1.disk" ] || v="$(sed -n "s/^$2=//p" "$in/$1.disk" | head -1)"
  [ -n "$v" ] || v="$(sed -n "s/^$2=//p" "$in/$1.env" | head -1)"
  echo "$v"
}
sys() { sed -n "s/^$1=//p" "$in/${rts[0]}.env" | head -1; }
mb() { awk -v k="${1:-0}" 'BEGIN{printf "%.1f MiB", k/1024}'; }
row() {
  local label="$1"; shift
  local line="| $label |"
  for r in "${rts[@]}"; do line="$line $("$@" "$r") |"; done
  echo "$line"
}
version() {
  case "$1" in
    containerd) echo "containerd $(get "$1" containerd), runc $(get "$1" containerd_runc)" ;;
    crio) echo "CRI-O $(get "$1" crio), $(get "$1" crio_runtime), conmon $(get "$1" crio_conmon)" ;;
    podbag) echo "podbag $(get "$1" podbag) (no OCI runtime)" ;;
  esac
}
one() { echo "$(get "$1" start_one_median_ms) / $(get "$1" start_one_p90_ms)"; }
many() { awk -v ms="$(get "$1" start_many_ms)" 'BEGIN{printf "%.1f s", ms/1000}'; }
exec_() { echo "$(get "$1" exec_median_ms)"; }
procs() { echo "$(get "$1" "procs_$2") ($(get "$1" "pause_$2") pause)"; }
procs_rest() { procs "$1" rest; }
procs_many() { procs "$1" many; }
pss_rest() { mb "$(get "$1" pss_rest_kb)"; }
pss_many() { mb "$(get "$1" pss_many_kb)"; }
pss_anon_many() { mb "$(get "$1" pss_anon_many_kb)"; }
pss_per() { awk -v a="$(get "$1" pss_many_kb)" -v b="$(get "$1" pss_rest_kb)" -v n="$count" 'BEGIN{printf "%.0f KiB", (a-b)/n}'; }
held_rest() { mb "$(get "$1" held_rest_kb)"; }
held_add() { mb $(($(get "$1" held_many_kb) - $(get "$1" held_rest_kb))); }
held_per() { awk -v a="$(get "$1" held_many_kb)" -v b="$(get "$1" held_rest_kb)" -v n="$count" 'BEGIN{printf "%.0f KiB", (a-b)/n}'; }
used_add() { mb $(($(get "$1" used_many_kb) - $(get "$1" used_rest_kb))); }
k3s_pss() { echo "$(mb "$(get "$1" k3s_pss_rest_kb)") / $(mb "$(get "$1" k3s_pss_many_kb)")"; }
disk_first() { mb "$(get "$1" disk_first_kb)"; }
disk_second() { mb $(($(get "$1" disk_both_kb) - $(get "$1" disk_first_kb))); }
disk_both() { mb "$(get "$1" disk_both_kb)"; }
# The rounds of one figure of a runtime, in the rounds' order.
rounds() { # $1 runtime, $2 key, $3 kb|ms|s
  local out="" f v
  for f in "$in/$1".round*; do
    v="$(sed -n "s/^$2=//p" "$f" | head -1)"
    case "$3" in
      kb) v="$(awk -v k="$v" 'BEGIN{printf "%.1f", k/1024}')" ;;
      s) v="$(awk -v k="$v" 'BEGIN{printf "%.1f", k/1000}')" ;;
    esac
    out="$out${out:+, }$v"
  done
  echo "$out"
}
# What 30 pods add to the system, in each round.
add_rounds() {
  local out="" f a b
  for f in "$in/$1".round*; do
    a="$(sed -n 's/^held_many_kb=//p' "$f")"
    b="$(sed -n 's/^held_rest_kb=//p' "$f")"
    out="$out${out:+, }$(awk -v k=$((a - b)) 'BEGIN{printf "%.1f", k/1024}')"
  done
  echo "$out"
}
image() { grep -m1 "$2" "$in/images-$1.txt" 2>/dev/null | awk '{print $1}'; }
# One figure for every runtime: "a, b, c; d, e, f; ..." (the arguments after the runtime).
per_rt() {
  local fn="$1" out="" r
  shift
  for r in "${rts[@]}"; do out="$out${out:+; }$("$fn" "$r" "$@")"; done
  echo "$out"
}

count=30
nrounds="$(ls "$in/${rts[0]}".round* | wc -l | tr -d ' ')"
{
  cat <<'EOF'
# Benchmarks: a Kubernetes node

podbag as the container runtime of a Kubernetes node, against the runtimes nodes use today:
containerd and CRI-O. All three run under the same k3s, in the same VM.

- Other benchmarks: [benchmarks.md](benchmarks.md) (container engines) and
  [benchmarks-msa.md](benchmarks-msa.md) (a node of Spring microservices).
- `dev/bench/k8s/session.sh` measures; `dev/bench/k8s/report.sh` writes this page.

## Contents

- [Summary](#summary)
- [Setup](#setup)
- [Results](#results)
- [Reading the numbers](#reading-the-numbers)
- [Caveats](#caveats)
- [Run it yourself](#run-it-yourself)

## Summary

EOF
  # The summary's figures: containerd, CRI-O, podbag.
  three() { # $1 function: "containerd A, CRI-O B, podbag C"
    echo "containerd $("$1" containerd), CRI-O $("$1" crio), podbag $("$1" podbag)"
  }
  procs30() { get "$1" procs_many; }
  start1() { echo "$(get "$1" start_one_median_ms) ms"; }
  exec_ms() { echo "$(get "$1" exec_median_ms) ms"; }
  echo "- **The runtime's processes with $count pods:**"
  echo "  - count: $(three procs30),"
  echo "  - memory (PSS): $(three pss_many)."
  echo "- **What $count pods add to the memory the system holds:** $(three held_add)."
  echo "- **One pod ready (median):** $(three start1)."
  echo "- **$count pods ready:** $(three many)."
  echo "  - Most of the start time is Kubernetes itself, the same for all three."
  echo "- **Disk for the two Python images:** $(three disk_both)."
  echo "- **\`kubectl exec\` (median):** $(three exec_ms)."
  echo
  cat <<'EOF'
## Setup

### Machine

EOF
  echo "- One VM: $(sys os), kernel $(sys kernel), $(sys arch), $(sys cpus) CPUs,"
  echo "  $(awk -v k="$(sys mem_kb)" 'BEGIN{printf "%.1f GiB", k/1048576}') of memory (lima on macOS)."
  echo "- One runtime at a time. No builds or tests ran in the VM meanwhile."
  echo
  echo "### Kubernetes"
  echo
  echo "- k3s $(sys k3s), a single node: the same binary and the same flags for all three"
  echo "  runtimes. Only \`--container-runtime-endpoint\` differs (none for containerd)."
  echo "- k3s's own add-ons are off (\`--disable traefik,servicelb,metrics-server,local-storage\`)."
  echo "  CoreDNS runs. At start, k3s also runs one short Helm job (\`helm-install-gateway-api-crd\`);"
  echo "  it ends before anything is measured."
  echo "- The kubelet uses the cgroupfs driver, and so do the three runtimes."
  echo "- The VM's disk is nearly full. So eviction for disk space and image garbage collection"
  echo "  are off (\`--kubelet-arg\`), the same for all three."
  echo "- The network: k3s's flannel (VXLAN), through its CNI plugins and its CNI"
  echo "  configuration, for all three."
  echo "- crictl $(sys crictl)."
  echo
  echo "### The runtimes"
  echo
  echo "- **containerd:** k3s's embedded containerd $(get containerd containerd), with the runc it"
  echo "  ships ($(get containerd containerd_runc)). One shim process per pod (\`containerd-shim-runc-v2\`),"
  echo "  and one pause container per pod ($(image containerd pause))."
  echo "- **CRI-O:** $(get crio crio), the static release bundle. Its default OCI runtime:"
  echo "  $(get crio crio_runtime). One conmon process per container. Its default configuration"
  echo "  drops the pause container (\`drop_infra_ctr\`) when a pod does not share one PID"
  echo "  namespace, as here: it still pulls the pause image ($(image crio pause))."
  echo "  Started with \`--cgroup-manager cgroupfs --conmon-cgroup pod\` and k3s's CNI folders."
  echo "- **podbag:** \`podbag cri serve\`, podbag $(get podbag podbag), a static release build of this"
  echo "  repository. No pause container, and no process per pod or container: one keeper"
  echo "  process watches all containers."
  echo
  echo "### What was measured"
  echo
  echo "- Each runtime gets a fresh cluster in every round. Its images stay from round to round."
  echo "- A Deployment of \`busybox:1.36\` running \`sleep\`. The image is pulled before anything"
  echo "  is timed."
  echo "  - **One pod:** scaled from 0 to 1, until the pod is ready; then back to 0, until the"
  echo "    runtime has removed it. 10 runs, after one that is not counted."
  echo "  - **$count pods:** scaled from 0 to $count, until all are ready."
  echo "  - The time runs from the \`kubectl scale\` command until a watch of the pods"
  echo "    (\`kubectl get pods -w\`) reports the pod ready."
  echo "- **\`kubectl exec POD -- true\`:** 10 runs, with $count pods running."
  echo "- **Memory:** at rest (only CoreDNS), and with the $count pods running."
  echo "- **Disk:** \`crictl pull\` of \`python:3.12-slim-bookworm\`, then \`python:3.12-slim-trixie\`,"
  echo "  as in [benchmarks.md](benchmarks.md#disk)."
  echo "- One round that is not counted, then $nrounds rounds. The order of the runtimes turns"
  echo "  from round to round, so that each one comes first once. Each figure is the median of"
  echo "  the $nrounds rounds."
  echo
  echo "## Results"
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
  row "One pod: scale 0 → 1 until ready, median / p90 (ms, 10 runs)" one
  row "$count pods: scale 0 → $count until all are ready" many
  row "\`kubectl exec POD -- true\`, median (ms)" exec_
  row "At rest: the runtime's processes (of them pause)" procs_rest
  row "At rest: the runtime's processes, PSS" pss_rest
  row "At rest: memory the system holds, by category (more than before k3s started)" held_rest
  row "$count pods: the runtime's processes (of them pause)" procs_many
  row "$count pods: the runtime's processes, PSS" pss_many
  row "… of it, anonymous memory (Pss_Anon: heaps and stacks, no program code)" pss_anon_many
  row "… per pod (PSS)" pss_per
  row "$count pods: what they add to the memory the system holds, by category" held_add
  row "… per pod" held_per
  row "$count pods: change in MemTotal − MemAvailable (noisy)" used_add
  row "The k3s server (the same for all): PSS at rest / with $count pods" k3s_pss
  row "Disk: \`python:3.12-slim-bookworm\`" disk_first
  row "Disk: then \`python:3.12-slim-trixie\` adds" disk_second
  row "Disk: both images" disk_both
  echo
  echo "- \"The runtime's processes\":"
  echo "  - containerd: containerd itself, one \`containerd-shim-runc-v2\` per pod, and the pause"
  echo "    processes. (k3s starts containerd as a child process.)"
  echo "  - CRI-O: crio, one conmon per container, and the pause processes (none here)."
  echo "  - podbag: \`podbag cri serve\` and the keeper (and \`podbag __pause\` for a pod that"
  echo "    shares one PID namespace; none here)."
  echo "  - The pods' own \`sleep\` processes are not counted: they are the same for all."
  echo "- PSS divides shared pages among the processes that share them. \"Per pod\" is (with $count"
  echo "  pods − at rest) / $count."
  echo "- \"Memory the system holds, by category\" is the change in AnonPages + Shmem + Slab +"
  echo "  KernelStack + PageTables + Percpu + VmallocUsed of \`/proc/meminfo\`, as in"
  echo "  [benchmarks.md](benchmarks.md). Caches are dropped before each reading. It includes"
  echo "  the runtime, the pods' processes, CoreDNS, and the kernel's memory for namespaces,"
  echo "  cgroups, network devices and mounts."
  echo "  - The k3s server's own anonymous memory is left out: it is the same program for all"
  echo "    three, and its Go heap moves by tens of MiB. Its PSS has its own row."
  echo "  - lima's guest agent is left out too (it was stopped for the session)."
  echo "  - What $count pods add, in each round, MiB: $(per_rt add_rounds)."
  echo "- The rounds, in the order of the columns:"
  echo "  - one pod, median ms: $(per_rt rounds start_one_median_ms ms),"
  echo "  - one pod, p90 ms: $(per_rt rounds start_one_p90_ms ms),"
  echo "  - $count pods, s: $(per_rt rounds start_many_ms s)."
  echo "- About one start in ten took 0.3 s longer than the others, with every runtime. The"
  echo "  p90 shows it in some rounds."
  echo "- Disk is what the two images add to \`du\` of the runtime's image store (hard links"
  echo "  counted once): \`/var/lib/rancher/k3s/agent/containerd\` (containerd),"
  echo "  \`/var/lib/containers/storage\` (CRI-O), podbag's data folder."
  if [ -f "$repo/dev/bench/k8s/notes.md" ]; then
    echo
    cat "$repo/dev/bench/k8s/notes.md"
  fi
} >"$doc"
echo "wrote $doc"
