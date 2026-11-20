#!/bin/sh
# On an old distribution: run the glibc program, then install and remove the package.
# Runs as root in a container, for example:
#
#   docker run --rm -v "$PWD:/src:ro" -w /src almalinux:8 \
#     packaging/test/old.sh dist/podbag-x86_64-linux-gnu dist/podbag-0.1.0-1.x86_64.rpm
set -eu

pkg=$2
step() { printf '\n== %s\n' "$*"; }

# podbag starts helpers from the program named podbag next to itself.
mkdir -p /tmp/gnu
install -m 0755 "$1" /tmp/gnu/podbag
gnu=/tmp/gnu/podbag

step "glibc program"
ldd --version | head -n 1
"$gnu" --version
"$gnu" check || true
# TLS (ring), the registry client and the store.
"$gnu" pull mirror.gcr.io/library/alpine:3.22
"$gnu" images

case $pkg in
  *.deb)
    step "Install the .deb, with an AppArmor older than 4.0"
    apt-get update -qq
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq apparmor >/dev/null
    DEBIAN_FRONTEND=noninteractive apt-get install -y "./$pkg"
    podbag --version
    # This AppArmor cannot read the profile, so it skips it.
    [ "$(readlink /etc/apparmor.d/disable/podbag)" = ../podbag ]
    step "Purge"
    DEBIAN_FRONTEND=noninteractive apt-get purge -y podbag
    [ ! -e /etc/apparmor.d/disable/podbag ]
    [ ! -e /etc/apparmor.d/podbag ]
    ;;
  *.rpm)
    step "Install the .rpm"
    dnf install -y "$pkg"
    podbag --version
    step "Remove"
    dnf remove -y podbag
    ;;
esac
[ ! -e /usr/bin/podbag ]
echo "old: all good"
