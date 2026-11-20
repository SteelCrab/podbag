#!/bin/sh
# Install the .rpm, run podbag, and remove it. Runs as root in a container, for example:
#
#   docker run --rm -v "$PWD:/src:ro" -w /src fedora:44 packaging/test/rpm.sh dist/podbag-0.1.0-1.x86_64.rpm
set -eu

rpm=$1
step() { printf '\n== %s\n' "$*"; }

step "Install"
dnf install -y "$rpm"
rpm -qi podbag
rpm -ql podbag
rpm -V podbag
[ -x /usr/bin/podbag ]
podbag --version
[ -f /usr/share/doc/podbag/LICENSE ]
[ -f /usr/share/doc/podbag/README.md ]
[ ! -e /etc/apparmor.d/podbag ]
podbag check || true

step "Remove"
dnf remove -y podbag
! rpm -q podbag >/dev/null 2>&1 || exit 1
[ ! -e /usr/bin/podbag ]
[ ! -e /usr/share/doc/podbag ]
echo "rpm: all good"
