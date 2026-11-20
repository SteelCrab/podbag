#!/bin/sh
# Run as a desktop user under a fresh D-Bus session and Xvfb.
set -eu

entry=/usr/share/applications/podbag-desktop.desktop
desktop-file-validate "$entry"
test -s /usr/share/icons/hicolor/128x128/apps/podbag-desktop.png
/usr/lib/podbag-desktop/podbag --version

XDG_RUNTIME_DIR=$(mktemp -d)
export XDG_RUNTIME_DIR
unset DOCKER_HOST PODBAG_BIN PODBAG_RUNTIME_DIR
gio launch "$entry"
timeout 60 xdotool search --sync --onlyvisible --name '^podbag$'

# The embedded UI starts the local engine after loading its settings.
socket=$XDG_RUNTIME_DIR/podbag/docker.sock
for _ in $(seq 1 60); do
    if [ "$(curl -fs --max-time 2 --unix-socket "$socket" http://localhost/_ping)" = OK ]; then
        exit 0
    fi
    sleep 1
done
echo "The desktop app did not start its bundled engine" >&2
exit 1
