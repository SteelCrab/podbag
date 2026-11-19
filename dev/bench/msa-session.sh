#!/usr/bin/env bash
# One MSA measurement session in the podbag-dev VM: the registry of msa-setup.sh, then
# dev/bench/msa.sh for every tool, one after another, with the guest agent and the system's
# timers stopped (as dev/bench/session.sh does). Nothing else may run in the VM meanwhile.
#
# usage: dev/bench/msa-session.sh [results-dir]   (BENCH_TOOLS: which tools, in order)
set -u
here="$(cd "$(dirname "$0")" && pwd)"
out="${1:-$HOME/bench-results}"
mkdir -p "$out"

paused=()
for unit in lima-guestagent.service apt-daily.timer apt-daily-upgrade.timer \
  fwupd-refresh.timer motd-news.timer man-db.timer; do
  if systemctl is-active --quiet "$unit"; then
    sudo -n systemctl stop "$unit" && paused+=("$unit")
  fi
done
trap '[ "${#paused[@]}" -eq 0 ] || sudo -n systemctl start "${paused[@]}"' EXIT

"$here/msa-setup.sh" >&2
failed=0
for tool in ${BENCH_TOOLS:-podbag podbag-root podman docker kiri}; do
  echo "=== $tool $(date +%T)"
  "$here/msa.sh" "$tool" "$out" >"$out/msa-$tool.out" 2>"$out/msa-$tool.log" ||
    { echo "FAILED $tool (see $out/msa-$tool.log)"; failed=1; }
  df -h / | tail -1
done
exit "$failed"
