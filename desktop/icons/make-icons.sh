#!/bin/sh
# Rebuild the launcher, tray and in-app icons on macOS.
#   desktop/icons/make-icons.sh [preview.png]
set -eu
cd "$(dirname "$0")/../.."
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
swift desktop/icons/make-icons.swift desktop/icons/bulk-bag.png "$work" ${1:+"$1"}
iconutil -c icns "$work/icon.iconset" -o desktop/icons/icon.icns
for f in icon.ico icon.png 32x32.png 64x64.png 128x128.png 128x128@2x.png tray-template.png; do
  cp "$work/$f" "desktop/icons/$f"
done
cp "$work/logo-small.png" desktop/assets/logo.png
cp "$work/128x128.png" desktop/assets/icon-128.png
cp "$work/64x64.png" desktop/assets/icon-64.png
cp "$work/tray-template.png" desktop/assets/tray-template.png
cp "$work/logo-small.png" docs/images/logo-small.png
cp "$work/icon.png" docs/images/logo.png
