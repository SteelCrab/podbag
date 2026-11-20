#!/usr/bin/env bash
# Screenshots of the desktop window on a mocked API (preview.html), in headless Chrome.
#   desktop/preview/screenshots.sh <output-folder> [views] [languages] [themes]
# Needs Google Chrome (or CHROME=path) and python3 (a local web server).
set -euo pipefail

out="${1:?usage: screenshots.sh <output-folder> [views] [languages] [themes]}"
views="${2:-containers images pods services volumes networks detail logs inspect settings settings-vm settings-updates update-admin collapsed docker docker-down docker-settings settings-docker banner pull pull-locations pull-login import empty offline vm confirm}"
langs="${3:-en ko}"
themes="${4:-light dark}"
here="$(cd "$(dirname "$0")/.." && pwd)"
chrome="${CHROME:-}"
if [ -z "$chrome" ]; then
  for c in "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" google-chrome chromium chromium-browser; do
    if command -v "$c" >/dev/null 2>&1 || [ -x "$c" ]; then chrome="$c"; break; fi
  done
fi
[ -n "$chrome" ] || { echo "no Chrome found; set CHROME" >&2; exit 1; }

mkdir -p "$out"
port="$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')"
python3 -m http.server "$port" --bind 127.0.0.1 --directory "$here" >/dev/null 2>&1 &
server=$!
trap 'kill $server' EXIT
sleep 0.5

for lang in $langs; do
  for theme in $themes; do
    for view in $views; do
      file="$out/$view-$lang-$theme.png"
      profile="$(mktemp -d)"
      "$chrome" --headless=new --disable-gpu --hide-scrollbars --no-first-run \
        --user-data-dir="$profile" --window-size="${SIZE:-1100,720}" \
        --virtual-time-budget=3000 --screenshot="$file" \
        "http://127.0.0.1:$port/preview/preview.html?view=$view&lang=$lang&theme=$theme" \
        >/dev/null 2>&1
      rm -rf "$profile"
      echo "$file"
    done
  done
done
