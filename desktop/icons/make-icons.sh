#!/bin/sh
# Make the desktop app's icons from docs/images/logo.png (macOS only: Swift and iconutil).
#   desktop/icons/make-icons.sh [preview.png]
# Writes desktop/icons/{icon.icns, icon.ico, icon.png, 32x32.png, 64x64.png, 128x128.png,
# 128x128@2x.png}. The tray icon (tray-template-44.png) is drawn apart.
set -eu
cd "$(dirname "$0")/../.."
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
swift desktop/icons/make-icons.swift docs/images/logo.png "$work" ${1:+"$1"}
iconutil -c icns "$work/icon.iconset" -o desktop/icons/icon.icns
for f in icon.ico icon.png 32x32.png 64x64.png 128x128.png 128x128@2x.png; do
  cp "$work/$f" "desktop/icons/$f"
done
echo "desktop/icons: icon.icns and the PNG and ICO files"
