#!/bin/sh
# Check a disk image from dev/macos/dmg.sh: it holds a validly signed podbag.app, whose
# podbag has the virtualization entitlement, and a link to /Applications.
# Usage: dev/macos/check-dmg.sh path/to/podbag-desktop-<arch>-macos.dmg
set -eu
[ "$#" -eq 1 ] || {
  echo "usage: dev/macos/check-dmg.sh DMG" >&2
  exit 2
}
dmg=$1
mnt=$(mktemp -d)
hdiutil attach -quiet -nobrowse -readonly -mountpoint "$mnt" "$dmg"
trap 'hdiutil detach -quiet "$mnt" || hdiutil detach -quiet -force "$mnt"; rmdir "$mnt"' EXIT
[ "$(readlink "$mnt/Applications")" = /Applications ] || {
  echo "$dmg: no link to /Applications" >&2
  exit 1
}
codesign --verify --strict --deep "$mnt/podbag.app"
codesign -d --entitlements - "$mnt/podbag.app/Contents/MacOS/podbag" 2> /dev/null |
  grep -F com.apple.security.virtualization > /dev/null || {
  echo "$dmg: the app's podbag has no virtualization entitlement" >&2
  exit 1
}
echo "$dmg: podbag.app and a link to /Applications"
