#!/usr/bin/env bash
# One measurement session in the openpod-dev VM: every tool with run.sh, openpod without
# the keeper and with hard links (for the notes), then scale.sh for openpod, openpod as
# root and Kiri: one round that is not counted, then three (a figure is the median of its
# rounds). Nothing else may run in the VM meanwhile (no build, no tests).
#
# lima's guest agent is stopped for the session: it reads every network interface again
# at each change and took a fifth of the CPU time while containers started. A Linux host
# has no such agent.
#
# usage: dev/bench/session.sh [results-dir]     (then dev/bench/report.sh)
# (BENCH_ONLY=scale: only the scale runs, into the results of an earlier session;
#  BENCH_SCALE_ROUNDS: how many counted rounds of them, default 3)
# Needs `dev/vm.sh bench-setup` and `dev/vm.sh static`.
set -u

here="$(cd "$(dirname "$0")" && pwd)"
out="${1:-$HOME/bench-results}"
mkdir -p "$out" "$out-nokeeper" "$out-hardlink"

# Stopped for the session, started again at its end: the guest agent, and the timers of
# the system's own jobs (package lists, firmware data), which would run in the middle.
paused=()
for unit in lima-guestagent.service apt-daily.timer apt-daily-upgrade.timer \
  fwupd-refresh.timer motd-news.timer man-db.timer; do
  if systemctl is-active --quiet "$unit"; then
    sudo -n systemctl stop "$unit" && paused+=("$unit")
  fi
done
trap '[ "${#paused[@]}" -eq 0 ] || sudo -n systemctl start "${paused[@]}"' EXIT

failed=0
run() { # $1 what, the rest: the command; its output goes to <dir>/<what>.out and .log
  local what="$1" dir="$2"
  shift 2
  echo "=== $what $(date +%T)"
  "$@" >"$dir/$what.out" 2>"$dir/$what.log" || { echo "FAILED $what (see $dir/$what.log)"; failed=1; }
}

if [ "${BENCH_ONLY:-}" != scale ]; then
  for tool in openpod openpod-root kiri podman docker; do
    run "$tool" "$out" "$here/run.sh" "$tool" "$out"
  done
  for tool in openpod openpod-root; do
    run "$tool" "$out-nokeeper" env OPENPOD_KEEPER=0 "$here/run.sh" "$tool" "$out-nokeeper"
  done
  run openpod-root "$out-hardlink" env OPENPOD_IMAGE_MOUNT=hardlink "$here/run.sh" openpod-root "$out-hardlink"
fi
# One run of 300 containers differs from the next (a `ps` on a cold cache most): every
# tool in each round, and of each figure the median.
#
# The first runs after a boot hold much more than later ones, for whichever tool runs
# first (as the first tool after a restart of the VM: 269 MiB for rootless openpod, 292 MiB
# for Kiri; after some runs 172 and 187 MiB): the kernel keeps slab memory that those runs
# make it allocate, and later runs use it again. So a round that is not counted comes
# first, and every tool is measured in the same state.
rounds="${BENCH_SCALE_ROUNDS:-3}"
rm -f "$out"/scale-*.env "$out"/scale-*.round* "$out"/scale-*.warmup
for round in 0 $(seq "$rounds"); do
  for tool in openpod openpod-root kiri; do
    run "scale-$tool" "$out" "$here/scale.sh" "$tool" "$out"
    [ -f "$out/scale-$tool.env" ] || continue
    if [ "$round" -eq 0 ]; then
      mv "$out/scale-$tool.env" "$out/scale-$tool.warmup"
    else
      mv "$out/scale-$tool.env" "$out/scale-$tool.round$round"
    fi
  done
done
for tool in openpod openpod-root kiri; do
  ls "$out"/scale-"$tool".round* >/dev/null 2>&1 || continue
  awk -F= '
    { if (!($1 in n)) order[++keys] = $1; v[$1, ++n[$1]] = $2 }
    END {
      for (i = 1; i <= keys; i++) {
        key = order[i]; c = n[key]; numbers = 1
        for (j = 1; j <= c; j++) if (v[key, j] !~ /^-?[0-9.]+$/) numbers = 0
        if (!numbers) { print key "=" v[key, 1]; continue }
        for (a = 1; a <= c; a++) for (b = a + 1; b <= c; b++)
          if (v[key, b] + 0 < v[key, a] + 0) { t = v[key, a]; v[key, a] = v[key, b]; v[key, b] = t }
        print key "=" v[key, int((c + 1) / 2)]
      }
    }' "$out"/scale-"$tool".round* >"$out/scale-$tool.env"
done
echo "=== done $(date +%T); paused meanwhile: ${paused[*]:-nothing}"
exit "$failed"
