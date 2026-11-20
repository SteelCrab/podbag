#!/usr/bin/env bash
# Install the .deb on this Ubuntu machine with AppArmor's user namespace restriction on,
# run a rootless container, and purge the package.
#
#   packaging/test/deb.sh dist/podbag_0.1.0_amd64.deb
set -euo pipefail

deb=$(realpath "$1")
image=mirror.gcr.io/library/alpine:3.22
step() { printf '\n== %s\n' "$*"; }
loaded() { sudo grep -q '^podbag ' /sys/kernel/security/apparmor/profiles; }
fail() {
  echo "deb: $*" >&2
  exit 1
}

step "Restrict user namespaces, as Ubuntu does by default"
sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=1
for f in /etc/subuid /etc/subgid; do
  grep -q "^$USER:" "$f" || echo "$USER:100000:65536" | sudo tee -a "$f" >/dev/null
done

step "Install"
sudo apt-get update -qq
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y "$deb"
dpkg -L podbag
[ "$(command -v podbag)" = /usr/bin/podbag ]
podbag --version
[ -f /usr/share/doc/podbag/LICENSE ]
[ -f /usr/share/doc/podbag/README.md ]
dpkg -s uidmap passt >/dev/null # recommended, so installed

step "AppArmor profile"
dpkg-query -W -f='${Conffiles}\n' podbag | grep -q ' /etc/apparmor.d/podbag '
diff -u /etc/apparmor.d/podbag <(podbag check --apparmor-profile)
loaded || fail "the profile is not loaded"
[ ! -e /etc/apparmor.d/disable/podbag ]

step "Rootless container"
podbag check || true
XDG_RUNTIME_DIR=$(mktemp -d)
export XDG_RUNTIME_DIR
[ "$(podbag run --rm --network none "$image" echo rootless)" = rootless ]
podbag rmi "$image" >/dev/null

step "Purge"
sudo DEBIAN_FRONTEND=noninteractive apt-get purge -y podbag
! dpkg -s podbag >/dev/null 2>&1 || fail "still installed"
[ ! -e /usr/bin/podbag ]
[ ! -e /etc/apparmor.d/podbag ]
[ ! -e /usr/share/doc/podbag ]
! loaded || fail "the profile is still loaded"
echo "deb: all good"
