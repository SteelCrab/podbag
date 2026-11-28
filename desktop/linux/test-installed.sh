#!/bin/sh
# Open the installed app and check that its window draws and that it starts its engine.
# Run as a desktop user under a fresh D-Bus session and Xvfb, with ImageMagick and xdotool.
#   desktop/linux/test-installed.sh [SCREENSHOT.png]
set -eu
shot=${1:-$(mktemp -d)/podbag-desktop.png}

entry=/usr/share/applications/podbag-desktop.desktop
desktop-file-validate "$entry"
test -s /usr/share/icons/hicolor/128x128/apps/podbag-desktop.png
/usr/lib/podbag-desktop/podbag --version

XDG_RUNTIME_DIR=$(mktemp -d)
export XDG_RUNTIME_DIR
unset DOCKER_HOST PODBAG_BIN PODBAG_RUNTIME_DIR
# Saved answers: no first-launch question covers the window, and English text.
config=${XDG_CONFIG_HOME:-$HOME/.config}/io.github.steelcrab.podbag
mkdir -p "$config"
printf '%s\n' '{"language":"en","cliNoticeDismissed":true,"compressionPromptSeen":true}' > "$config/settings.json"
# A closed local address: the Training view does not sync the live catalog.
export PODBAG_TRAINING_CATALOG=http://127.0.0.1:9/training/catalog.json

gio launch "$entry"
window=$(timeout 60 xdotool search --sync --onlyvisible --name '^Podbag Desktop$' | head -n 1)

# The first frames can come before the fonts and the views: wait for content.
colors=0
for _ in $(seq 1 60); do
    if import -window "$window" "$shot" 2>/dev/null; then
        colors=$(identify -format '%k' "$shot")
        [ "$colors" -lt 8 ] || break
    fi
    sleep 1
done
if [ "$colors" -lt 8 ]; then
    echo "The window stayed blank ($colors colors, see $shot)" >&2
    exit 1
fi
echo "The window draws: $colors colors."

# The app starts the local engine after loading its settings.
socket=$XDG_RUNTIME_DIR/podbag/docker.sock
for _ in $(seq 1 60); do
    if [ "$(curl -fs --max-time 2 --unix-socket "$socket" http://localhost/_ping)" = OK ]; then
        exit 0
    fi
    sleep 1
done
echo "The desktop app did not start its bundled engine" >&2
exit 1
